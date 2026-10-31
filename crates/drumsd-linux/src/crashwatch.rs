//! Crash watch: every process on the host that dies of a crash signal, attributed to the cgroup it ran in.
//!
//! A desktop can look alive while one of its apps is broken: when a Chrome renderer dies, the browser stays up and
//! shows "Can't open this page", so the process gates (`alive_after_resume` reads the restored tree once, 5 s after
//! ready) and the checker never notice. drumsd listens to the kernel's process events connector (netlink
//! `NETLINK_CONNECTOR`, `CN_IDX_PROC`), filtered in the kernel to exits with a non-zero status, and keeps every exit
//! caused by one of [`CRASH_SIGNALS`]. Each one is attributed to a cgroup at once (the dead task's own, else its
//! parent's: the parent of a crashed renderer is Chrome's zygote, still alive in the same slot), so the manager can ask
//! "did anything of this episode's tree crash since it started?" (`no_app_crash`).
//!
//! An exit the watch could not attribute (task and parent both gone before it read them), or a stretch of events the
//! kernel dropped (the socket's buffer overflowed: `ENOBUFS`), is kept as such; the manager treats both as a possible
//! crash of every episode that was running at the time (fail closed).

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

pub const NETLINK_CONNECTOR: i32 = 11;
pub const CN_IDX_PROC: u32 = 1;
pub const CN_VAL_PROC: u32 = 1;
pub const PROC_CN_MCAST_LISTEN: u32 = 1;
pub const PROC_EVENT_EXIT: u32 = 0x8000_0000;
/// Filter flag (Linux 6.6+): report only exits whose status is not 0.
pub const PROC_EVENT_NONZERO_EXIT: u32 = 0x2000_0000;

/// Signals whose default action is a core dump and that only a fault, a failed check or abort() sends: SIGILL (Chrome's
/// and V8's CHECK use `ud2`), SIGTRAP (`int3`), SIGABRT, SIGBUS, SIGFPE, SIGSEGV, SIGSYS (a seccomp kill). SIGKILL,
/// SIGTERM and the other termination signals are how processes are normally ended (a reset kills the old tree with
/// cgroup.kill) and are not crashes.
pub const CRASH_SIGNALS: [i32; 7] = [libc::SIGILL, libc::SIGTRAP, libc::SIGABRT, libc::SIGBUS, libc::SIGFPE, libc::SIGSEGV, libc::SIGSYS];

/// How many events are kept (oldest dropped first). An episode is judged against the events since its start, which
/// are the newest; the count of dropped-by-age events is kept so a judgment can never miss one silently.
pub const KEEP: usize = 4096;

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct CrashEvent {
    /// Seconds since the epoch, when the watch read the event.
    pub epoch: f64,
    pub pid: i32,
    pub tgid: i32,
    pub parent_tgid: i32,
    pub signal: i32,
    pub signal_name: String,
    pub core_dumped: bool,
    /// The dead task's comm if it could still be read, else None.
    pub comm: Option<String>,
    /// The parent's comm (e.g. `chrome` for a renderer's zygote).
    pub parent_comm: Option<String>,
    /// The unified cgroup path (as in /proc/PID/cgroup, e.g. `/system.slice/.../drumsbench-...-r1.scope`), or None
    /// when neither the task nor its parent could be read (unattributed).
    pub cgroup: Option<String>,
    /// Which process the cgroup was read from: "task" or "parent".
    pub cgroup_from: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Overflow {
    pub epoch: f64,
}

#[derive(Default)]
struct Inner {
    events: VecDeque<CrashEvent>,
    dropped_by_age: u64,
    /// Epochs at which the kernel reported lost events (ENOBUFS).
    overflows: Vec<f64>,
    /// When the watch stopped (a fatal socket error): every episode since then is unjudgeable.
    stopped: Option<(f64, String)>,
    exits_seen: u64,
}

/// Shared state of the watch. Cheap to clone (Arc).
#[derive(Clone, Default)]
pub struct CrashWatch {
    inner: Arc<Mutex<Inner>>,
    pub started_epoch: f64,
}

pub fn signal_name(sig: i32) -> String {
    match sig {
        libc::SIGILL => "SIGILL".into(),
        libc::SIGTRAP => "SIGTRAP".into(),
        libc::SIGABRT => "SIGABRT".into(),
        libc::SIGBUS => "SIGBUS".into(),
        libc::SIGFPE => "SIGFPE".into(),
        libc::SIGSEGV => "SIGSEGV".into(),
        libc::SIGSYS => "SIGSYS".into(),
        n => format!("signal {n}"),
    }
}

/// The kernel's exit status of a task (`task->exit_code`): the signal number in the low 7 bits when it was killed by
/// a signal, 0x80 when it dumped core, else the exit status << 8.
pub fn crash_signal(exit_code: u32) -> Option<(i32, bool)> {
    let sig = (exit_code & 0x7f) as i32;
    if sig != 0 && CRASH_SIGNALS.contains(&sig) {
        Some((sig, exit_code & 0x80 != 0))
    } else {
        None
    }
}

/// One `PROC_EVENT_EXIT` record of the connector: (pid, tgid, exit_code, parent_pid, parent_tgid).
#[derive(Debug, PartialEq)]
pub struct ExitRecord {
    pub pid: i32,
    pub tgid: i32,
    pub exit_code: u32,
    pub parent_pid: i32,
    pub parent_tgid: i32,
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|x| u32::from_ne_bytes([x[0], x[1], x[2], x[3]]))
}

/// Parse one datagram (one or more netlink messages, each an nlmsghdr + cn_msg + proc_event) into exit records.
/// Layouts: nlmsghdr 16 bytes (len u32, type u16, flags u16, seq u32, pid u32); cn_msg 20 bytes (idx, val, seq, ack
/// u32; len, flags u16); proc_event 16 bytes of header (what u32, cpu u32, timestamp_ns u64) then the exit event:
/// process_pid, process_tgid, exit_code, exit_signal, parent_pid, parent_tgid (u32 each).
pub fn parse_datagram(buf: &[u8]) -> Vec<ExitRecord> {
    let mut out = vec![];
    let mut off = 0usize;
    while off + 16 <= buf.len() {
        let Some(len) = u32_at(buf, off).map(|l| l as usize) else { break };
        if len < 16 || off + len > buf.len() {
            break;
        }
        let m = &buf[off..off + len];
        let cn = 16;
        let ev = cn + 20;
        if let (Some(idx), Some(val), Some(what)) = (u32_at(m, cn), u32_at(m, cn + 4), u32_at(m, ev)) {
            if idx == CN_IDX_PROC && val == CN_VAL_PROC && what == PROC_EVENT_EXIT {
                let d = ev + 16;
                if let (Some(pid), Some(tgid), Some(code), Some(ppid), Some(ptgid)) =
                    (u32_at(m, d), u32_at(m, d + 4), u32_at(m, d + 8), u32_at(m, d + 16), u32_at(m, d + 20))
                {
                    out.push(ExitRecord { pid: pid as i32, tgid: tgid as i32, exit_code: code, parent_pid: ppid as i32, parent_tgid: ptgid as i32 });
                }
            }
        }
        // NLMSG_ALIGN(len)
        off += (len + 3) & !3;
    }
    out
}

impl CrashWatch {
    pub fn new_for_test() -> CrashWatch {
        CrashWatch { inner: Arc::default(), started_epoch: crate::now_epoch() }
    }

    /// Record an exit (the watch thread, and tests). Only crash signals are kept.
    pub fn record_exit(&self, r: &ExitRecord, epoch: f64, read_cgroup: impl Fn(i32) -> Option<String>, read_comm: impl Fn(i32) -> Option<String>) {
        let mut g = self.inner.lock().unwrap();
        g.exits_seen += 1;
        let Some((sig, core)) = crash_signal(r.exit_code) else { return };
        // a fatal signal ends every thread of the group, each with the same status: keep one event per process
        if g.events.iter().rev().take(64).any(|e| e.tgid == r.tgid && e.signal == sig && (epoch - e.epoch).abs() < 5.0) {
            return;
        }
        drop(g);
        let (cgroup, from) = match read_cgroup(r.tgid).or_else(|| read_cgroup(r.pid)) {
            Some(c) => (Some(c), Some("task".to_string())),
            None => match read_cgroup(r.parent_tgid).or_else(|| read_cgroup(r.parent_pid)) {
                Some(c) => (Some(c), Some("parent".to_string())),
                None => (None, None),
            },
        };
        let ev = CrashEvent {
            epoch,
            pid: r.pid,
            tgid: r.tgid,
            parent_tgid: r.parent_tgid,
            signal: sig,
            signal_name: signal_name(sig),
            core_dumped: core,
            comm: read_comm(r.tgid).or_else(|| read_comm(r.pid)),
            parent_comm: read_comm(r.parent_tgid),
            cgroup,
            cgroup_from: from,
        };
        tracing::warn!("crash: {} pid {} ({:?}, parent {:?}) died of {} in {:?}", ev.tgid, ev.pid, ev.comm, ev.parent_comm, ev.signal_name, ev.cgroup);
        let mut g = self.inner.lock().unwrap();
        g.events.push_back(ev);
        while g.events.len() > KEEP {
            g.events.pop_front();
            g.dropped_by_age += 1;
        }
    }

    pub fn record_overflow(&self, epoch: f64) {
        let mut g = self.inner.lock().unwrap();
        g.overflows.push(epoch);
        if g.overflows.len() > KEEP {
            g.overflows.remove(0);
        }
    }

    pub fn record_stop(&self, epoch: f64, why: String) {
        self.inner.lock().unwrap().stopped = Some((epoch, why));
    }

    /// What happened to the tree in cgroup `cg` (a path under /sys/fs/cgroup, or the /proc form) since `since_epoch`:
    /// the crash events attributed to it or to a cgroup below it, plus every reason the answer may be incomplete
    /// (unattributed crashes, kernel overflows, a stopped watch, events aged out). Empty lists = nothing crashed.
    pub fn judge(&self, cg: &str, since_epoch: f64) -> Judgment {
        let cg = cg.strip_prefix(crate::cgroup::CG_ROOT).unwrap_or(cg).to_string();
        let g = self.inner.lock().unwrap();
        let mine = |c: &str| c == cg || c.starts_with(&format!("{cg}/"));
        let crashes: Vec<CrashEvent> = g.events.iter().filter(|e| e.epoch >= since_epoch && e.cgroup.as_deref().map(mine).unwrap_or(false)).cloned().collect();
        let unattributed: Vec<CrashEvent> = g.events.iter().filter(|e| e.epoch >= since_epoch && e.cgroup.is_none()).cloned().collect();
        let overflows: Vec<f64> = g.overflows.iter().copied().filter(|t| *t >= since_epoch).collect();
        let oldest = g.events.front().map(|e| e.epoch);
        let aged_out = g.dropped_by_age > 0 && oldest.map(|o| o > since_epoch).unwrap_or(false);
        let mut incomplete = vec![];
        if since_epoch < self.started_epoch {
            incomplete.push(format!("the episode started before the crash watch ({:.3} < {:.3})", since_epoch, self.started_epoch));
        }
        if let Some((t, why)) = &g.stopped {
            incomplete.push(format!("the crash watch stopped at {t:.3}: {why}"));
        }
        for t in &overflows {
            incomplete.push(format!("the kernel dropped process events at {t:.3} (socket overflow)"));
        }
        for e in &unattributed {
            incomplete.push(format!("{} (pid {}) died of {} at {:.3} and could not be attributed to a cgroup", e.comm.as_deref().unwrap_or("?"), e.tgid, e.signal_name, e.epoch));
        }
        if aged_out {
            incomplete.push("crash events of this episode were dropped from the watch's buffer".into());
        }
        Judgment { crashes, incomplete }
    }

    pub fn summary(&self) -> serde_json::Value {
        let g = self.inner.lock().unwrap();
        serde_json::json!({"running": g.stopped.is_none(), "stopped": g.stopped.as_ref().map(|(t, w)| serde_json::json!({"epoch": t, "why": w})),
                           "crash_events": g.events.len() as u64 + g.dropped_by_age, "unattributed": g.events.iter().filter(|e| e.cgroup.is_none()).count(),
                           "overflows": g.overflows.len(), "exits_seen": g.exits_seen, "started_epoch": self.started_epoch})
    }
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Judgment {
    pub crashes: Vec<CrashEvent>,
    pub incomplete: Vec<String>,
}

impl Judgment {
    pub fn clean(&self) -> bool {
        self.crashes.is_empty() && self.incomplete.is_empty()
    }
}

fn check(r: libc::c_int, what: &str) -> io::Result<()> {
    if r < 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(e.kind(), format!("{what}: {e}")));
    }
    Ok(())
}

/// Open the connector socket, subscribe to non-zero exits, and start the reader thread. Fails when the kernel has no
/// process events connector or refuses the subscription (drumsd must be root in the initial user namespace).
pub fn start() -> io::Result<CrashWatch> {
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, NETLINK_CONNECTOR) };
    check(fd, "socket(NETLINK_CONNECTOR)")?;
    // a large receive buffer: a reset of many envs ends thousands of processes at once (SIGKILL exits are non-zero
    // too and pass the kernel filter; they are dropped here, not there)
    let size: libc::c_int = 16 << 20;
    unsafe {
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUFFORCE, &size as *const _ as *const libc::c_void, std::mem::size_of::<libc::c_int>() as u32);
    }
    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as u16;
    sa.nl_groups = CN_IDX_PROC;
    sa.nl_pid = 0;
    let r = unsafe { libc::bind(fd, &sa as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_nl>() as u32) };
    if r < 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(io::Error::new(e.kind(), format!("bind(CN_IDX_PROC): {e}")));
    }
    // nlmsghdr + cn_msg + proc_input {mcast_op, event_type}
    let mut msg = vec![0u8; 16 + 20 + 8];
    let total = msg.len() as u32;
    msg[0..4].copy_from_slice(&total.to_ne_bytes());
    msg[4..6].copy_from_slice(&(libc::NLMSG_DONE as u16).to_ne_bytes());
    msg[12..16].copy_from_slice(&std::process::id().to_ne_bytes());
    msg[16..20].copy_from_slice(&CN_IDX_PROC.to_ne_bytes());
    msg[20..24].copy_from_slice(&CN_VAL_PROC.to_ne_bytes());
    msg[32..34].copy_from_slice(&8u16.to_ne_bytes());
    msg[36..40].copy_from_slice(&PROC_CN_MCAST_LISTEN.to_ne_bytes());
    msg[40..44].copy_from_slice(&PROC_EVENT_NONZERO_EXIT.to_ne_bytes());
    let n = unsafe { libc::send(fd, msg.as_ptr() as *const libc::c_void, msg.len(), 0) };
    if n < 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(io::Error::new(e.kind(), format!("PROC_CN_MCAST_LISTEN: {e}")));
    }
    let w = CrashWatch { inner: Arc::default(), started_epoch: crate::now_epoch() };
    let w2 = w.clone();
    std::thread::Builder::new().name("crashwatch".into()).spawn(move || reader(fd, w2))?;
    Ok(w)
}

fn reader(fd: i32, w: CrashWatch) {
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        let now = crate::now_epoch();
        if n < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::ENOBUFS) => {
                    tracing::warn!("crash watch: the kernel dropped process events (ENOBUFS)");
                    w.record_overflow(now);
                    continue;
                }
                _ => {
                    tracing::error!("crash watch stopped: {e}");
                    w.record_stop(now, e.to_string());
                    return;
                }
            }
        }
        for r in parse_datagram(&buf[..n as usize]) {
            w.record_exit(&r, now, crate::procfs::cgroup_path, crate::procfs::comm);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datagram(recs: &[(u32, u32, u32, u32, u32, u32)]) -> Vec<u8> {
        let mut out = vec![];
        for &(what, pid, tgid, code, ppid, ptgid) in recs {
            let mut m = vec![0u8; 16 + 20 + 16 + 24];
            let len = m.len() as u32;
            m[0..4].copy_from_slice(&len.to_ne_bytes());
            m[16..20].copy_from_slice(&CN_IDX_PROC.to_ne_bytes());
            m[20..24].copy_from_slice(&CN_VAL_PROC.to_ne_bytes());
            m[36..40].copy_from_slice(&what.to_ne_bytes());
            for (i, v) in [pid, tgid, code, 17, ppid, ptgid].iter().enumerate() {
                m[52 + 4 * i..56 + 4 * i].copy_from_slice(&v.to_ne_bytes());
            }
            out.extend(m);
        }
        out
    }

    #[test]
    fn parses_exit_records_and_skips_other_events() {
        let d = datagram(&[(PROC_EVENT_EXIT, 10, 10, 4, 5, 5), (0x1, 11, 11, 0, 5, 5), (PROC_EVENT_EXIT, 12, 10, 0x84, 5, 5)]);
        let r = parse_datagram(&d);
        assert_eq!(r, vec![ExitRecord { pid: 10, tgid: 10, exit_code: 4, parent_pid: 5, parent_tgid: 5 },
                           ExitRecord { pid: 12, tgid: 10, exit_code: 0x84, parent_pid: 5, parent_tgid: 5 }]);
        assert!(parse_datagram(&d[..20]).is_empty(), "a truncated message is ignored");
    }

    #[test]
    fn only_crash_signals_count() {
        assert_eq!(crash_signal(4), Some((libc::SIGILL, false)));
        assert_eq!(crash_signal(0x8b), Some((libc::SIGSEGV, true)));
        assert_eq!(crash_signal(libc::SIGKILL as u32), None, "a reset's cgroup.kill is not a crash");
        assert_eq!(crash_signal(libc::SIGTERM as u32), None);
        assert_eq!(crash_signal(1 << 8), None, "exit(1) is not a crash");
    }

    #[test]
    fn attributes_by_task_then_parent_and_judges_per_cgroup() {
        let w = CrashWatch::new_for_test();
        let t0 = w.started_epoch;
        let slot = "/system.slice/drumsbench-pool.scope/drumsbench-m-1-pool.scope/drumsbench-m-1-r1.scope";
        // a renderer reaped before the watch read it: attributed through its parent (the zygote)
        let cg = |p: i32| if p == 7 { Some(slot.to_string()) } else { None };
        let comm = |p: i32| if p == 7 { Some("chrome".to_string()) } else { None };
        w.record_exit(&ExitRecord { pid: 9, tgid: 9, exit_code: 4, parent_pid: 7, parent_tgid: 7 }, t0 + 1.0, cg, comm);
        // its other threads: one event per process
        w.record_exit(&ExitRecord { pid: 10, tgid: 9, exit_code: 4, parent_pid: 7, parent_tgid: 7 }, t0 + 1.0, cg, comm);
        // a SIGKILL of a reset: not kept
        w.record_exit(&ExitRecord { pid: 11, tgid: 11, exit_code: 9, parent_pid: 7, parent_tgid: 7 }, t0 + 1.5, cg, comm);
        let j = w.judge(&format!("/sys/fs/cgroup{slot}"), t0);
        assert_eq!(j.crashes.len(), 1);
        assert_eq!(j.crashes[0].signal_name, "SIGILL");
        assert_eq!(j.crashes[0].cgroup_from.as_deref(), Some("parent"));
        assert_eq!(j.crashes[0].parent_comm.as_deref(), Some("chrome"));
        assert!(j.incomplete.is_empty());
        // the parent slot cgroup sees its leaf's crash; another slot and a later episode do not
        assert_eq!(w.judge("/system.slice/drumsbench-pool.scope/drumsbench-m-1-pool.scope", t0).crashes.len(), 1);
        assert!(w.judge("/system.slice/drumsbench-pool.scope/drumsbench-m-2-pool.scope", t0).clean());
        assert!(w.judge(slot, t0 + 2.0).clean());
        // a similar prefix is not a sub-cgroup
        assert!(w.judge("/system.slice/drumsbench-pool.scope/drumsbench-m-1-pool.scope/drumsbench-m-1-r", t0).clean());
    }

    #[test]
    fn unattributed_crashes_overflows_and_a_stopped_watch_make_every_judgment_incomplete() {
        let w = CrashWatch::new_for_test();
        let t0 = w.started_epoch;
        w.record_exit(&ExitRecord { pid: 20, tgid: 20, exit_code: 11, parent_pid: 1, parent_tgid: 1 }, t0 + 1.0, |_| None, |_| None);
        let j = w.judge("/x", t0);
        assert!(j.crashes.is_empty());
        assert_eq!(j.incomplete.len(), 1, "{:?}", j.incomplete);
        assert!(!j.clean());
        assert!(w.judge("/x", t0 + 2.0).clean(), "an older unattributed crash is not this episode's");
        w.record_overflow(t0 + 3.0);
        assert!(!w.judge("/x", t0 + 2.0).clean());
        w.record_stop(t0 + 4.0, "EBADF".into());
        assert!(w.judge("/x", t0 + 5.0).incomplete.iter().any(|s| s.contains("stopped")));
        assert!(w.judge("/x", t0 - 10.0).incomplete.iter().any(|s| s.contains("before the crash watch")));
    }
}
