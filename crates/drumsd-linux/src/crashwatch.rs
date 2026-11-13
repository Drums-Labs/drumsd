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
//! The connector reports an exit after the task is already a zombie, and the reader gets to it later; by then the
//! parent may have reaped it, and a whole tree that dies at once (a Chrome browser with its zygote and renderers) takes
//! the parent with it, so /proc can no longer answer. A census thread therefore reads every slot cgroup's
//! `cgroup.procs` every [`CENSUS_EVERY_MS`] and remembers which tgid lived in which cgroup (and its comm) for
//! [`CENSUS_KEEP_S`] after it was last seen; an exit /proc cannot place is attributed from the census (the task, else
//! its parent). The watch also remembers the cgroup of every plain (non-crash) process exit it saw for a few seconds
//! ([`EXITED_KEEP_S`]): when a whole tree is torn down the kernel reports the exits in any order, and a renderer that
//! dies of a crash signal after its parent is gone and reaped is placed through that remembered parent.
//!
//! An exit that neither /proc nor the census can place counts against every episode whose tree ran Chrome in the
//! last fresh census pass (the crash signals seen are Chrome's), and is recorded as not counted for the others. With no
//! fresh census, or no Chrome anywhere, it counts against every episode running at the time; so does a stretch of
//! events the kernel dropped (the socket's buffer overflowed: `ENOBUFS`). Fail closed: a crash is never silently
//! treated as nobody's.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
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
/// How many plain exits' cgroups are remembered, and for how long.
const EXITED_KEEP: usize = 20_000;
const EXITED_KEEP_S: f64 = 15.0;

/// How often the census reads the slots' cgroups (which process lives in which cgroup).
pub const CENSUS_EVERY_MS: u64 = 200;
/// How long the census still knows a process after it stopped appearing in a pass (it may have crashed and been reaped
/// between two passes). Far below the time the kernel takes to hand the same pid out again: pids are allocated
/// cyclically up to pid_max, so reuse needs a full cycle of forks.
pub const CENSUS_KEEP_S: f64 = 10.0;
/// A census pass older than this when a crash is read cannot say which slots ran Chrome at that moment.
pub const CENSUS_FRESH_S: f64 = 2.0;

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
    /// Where the cgroup came from: "task" or "parent" (read from /proc at once), "census" or "census-parent" (the
    /// census pass that last saw the task or its parent), or "remembered exit" (the plain exit of the task or its
    /// parent that the watch saw a moment before).
    pub cgroup_from: Option<String>,
    /// Only for an unattributed crash: the cgroups that ran Chrome in the last census pass, or None when there was no
    /// fresh pass (then the crash counts against every episode).
    pub chrome_cgroups: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
struct CensusEntry {
    cgroup: String,
    comm: Option<String>,
    seen: f64,
}

#[derive(Default)]
struct Census {
    procs: HashMap<i32, CensusEntry>,
    /// The cgroups with a Chrome process in the last pass.
    chrome: BTreeSet<String>,
    last_pass: Option<f64>,
    passes: u64,
}

/// A Chrome (or Chromium) process by comm: the browser, its zygotes, renderers, GPU and utility processes are all
/// `chrome`; crashpad's handler is `chrome_crashpad` (comm is cut at 15 bytes).
pub fn is_chrome(comm: &str) -> bool {
    comm == "chrome" || comm.starts_with("chrome_") || comm.starts_with("chromium")
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Overflow {
    pub epoch: f64,
}

#[derive(Default)]
struct Inner {
    events: VecDeque<CrashEvent>,
    dropped_by_age: u64,
    /// cgroup of every process whose exit was seen while /proc still had it, kept for a few seconds: when a whole env
    /// tree is killed (close, reset) the kernel reports the exits in any order, and a renderer that died of a crash
    /// signal a few ms after its zygote and browser are gone can only be placed through them (fresh-host replication of
    /// T2b W1p: one such unattributed SIGTRAP marked every running episode incomplete).
    exited_cg: VecDeque<(f64, i32, String)>,
    /// Epochs at which the kernel reported lost events (ENOBUFS).
    overflows: Vec<f64>,
    /// When the watch stopped (a fatal socket error): every episode since then is unjudgeable.
    stopped: Option<(f64, String)>,
    exits_seen: u64,
    /// How each kept crash was attributed: task, parent, census, census-parent, remembered exit, chrome-fallback, none.
    attributed: std::collections::BTreeMap<String, u64>,
}

/// Shared state of the watch. Cheap to clone (Arc).
#[derive(Clone, Default)]
pub struct CrashWatch {
    inner: Arc<Mutex<Inner>>,
    census: Arc<Mutex<Census>>,
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
        CrashWatch { inner: Arc::default(), census: Arc::default(), started_epoch: crate::now_epoch() }
    }

    /// Record an exit (the watch thread, and tests). Only crash signals are kept.
    pub fn record_exit(&self, r: &ExitRecord, epoch: f64, read_cgroup: impl Fn(i32) -> Option<String>, read_comm: impl Fn(i32) -> Option<String>) {
        let mut g = self.inner.lock().unwrap();
        g.exits_seen += 1;
        let Some((sig, core)) = crash_signal(r.exit_code) else {
            // a plain exit: remember where the process was, for crashes of its children reported after it is gone
            if r.pid == r.tgid {
                drop(g);
                let c = read_cgroup(r.tgid);
                let mut g = self.inner.lock().unwrap();
                if let Some(c) = c {
                    g.exited_cg.push_back((epoch, r.tgid, c));
                }
                while g.exited_cg.len() > EXITED_KEEP || g.exited_cg.front().map(|(t, _, _)| epoch - *t > EXITED_KEEP_S).unwrap_or(false) {
                    g.exited_cg.pop_front();
                }
            }
            return;
        };
        // a fatal signal ends every thread of the group, each with the same status: keep one event per process
        if g.events.iter().rev().take(64).any(|e| e.tgid == r.tgid && e.signal == sig && (epoch - e.epoch).abs() < 5.0) {
            return;
        }
        let remembered = |g: &Inner, tg: i32| g.exited_cg.iter().rev().find(|(_, t, _)| *t == tg).map(|(_, _, c)| c.clone());
        let (rem_self, rem_parent) = (remembered(&g, r.tgid), remembered(&g, r.parent_tgid));
        drop(g);
        let (census_task, census_parent, chrome, fresh) = {
            let c = self.census.lock().unwrap();
            let known = |p: i32| c.procs.get(&p).filter(|e| epoch - e.seen <= CENSUS_KEEP_S).cloned();
            let fresh = c.last_pass.map(|t| epoch - t <= CENSUS_FRESH_S).unwrap_or(false);
            (known(r.tgid), known(r.parent_tgid), c.chrome.iter().cloned().collect::<Vec<_>>(), fresh)
        };
        let (cgroup, from) = match read_cgroup(r.tgid).or_else(|| read_cgroup(r.pid)) {
            Some(c) => (Some(c), Some("task".to_string())),
            None => match read_cgroup(r.parent_tgid).or_else(|| read_cgroup(r.parent_pid)) {
                Some(c) => (Some(c), Some("parent".to_string())),
                None => match (&census_task, &census_parent) {
                    (Some(e), _) => (Some(e.cgroup.clone()), Some("census".to_string())),
                    (None, Some(e)) => (Some(e.cgroup.clone()), Some("census-parent".to_string())),
                    // neither /proc nor the census knows the task or its parent: a parent whose plain exit the watch
                    // saw a moment ago (a tree torn down by close or reset, exits reported in any order)
                    _ => match rem_self.or(rem_parent) {
                        Some(c) => (Some(c), Some("remembered exit".to_string())),
                        None => (None, None),
                    },
                },
            },
        };
        let chrome_cgroups = if cgroup.is_none() && fresh { Some(chrome) } else { None };
        let how = match (&from, &chrome_cgroups) {
            (Some(f), _) => f.clone(),
            (None, Some(c)) if !c.is_empty() => "chrome-fallback".to_string(),
            _ => "none".to_string(),
        };
        let ev = CrashEvent {
            epoch,
            pid: r.pid,
            tgid: r.tgid,
            parent_tgid: r.parent_tgid,
            signal: sig,
            signal_name: signal_name(sig),
            core_dumped: core,
            comm: read_comm(r.tgid).or_else(|| read_comm(r.pid)).or_else(|| census_task.as_ref().and_then(|e| e.comm.clone())),
            parent_comm: read_comm(r.parent_tgid).or_else(|| census_parent.as_ref().and_then(|e| e.comm.clone())),
            cgroup,
            cgroup_from: from,
            chrome_cgroups,
        };
        tracing::warn!(
            "crash: {} pid {} ({:?}, parent {:?}) died of {} in {:?} (attributed: {how})",
            ev.tgid,
            ev.pid,
            ev.comm,
            ev.parent_comm,
            ev.signal_name,
            ev.cgroup
        );
        let mut g = self.inner.lock().unwrap();
        *g.attributed.entry(how).or_default() += 1;
        g.events.push_back(ev);
        while g.events.len() > KEEP {
            g.events.pop_front();
            g.dropped_by_age += 1;
        }
    }

    /// One census pass: every (cgroup, tgids) read below the census roots at `epoch`. A tgid new to the census, or
    /// seen in another cgroup than before, has its comm read once; tgids not seen for [`CENSUS_KEEP_S`] are forgotten.
    pub fn record_census(&self, observed: &[(String, Vec<i32>)], epoch: f64, read_comm: impl Fn(i32) -> Option<String>) {
        let mut c = self.census.lock().unwrap();
        let mut chrome = BTreeSet::new();
        for (cg, tgids) in observed {
            for &t in tgids {
                let e = match c.procs.get_mut(&t) {
                    Some(e) if e.cgroup == *cg => {
                        e.seen = epoch;
                        e.clone()
                    }
                    _ => {
                        let e = CensusEntry { cgroup: cg.clone(), comm: read_comm(t), seen: epoch };
                        c.procs.insert(t, e.clone());
                        e
                    }
                };
                if e.comm.as_deref().map(is_chrome).unwrap_or(false) {
                    chrome.insert(cg.clone());
                }
            }
        }
        c.procs.retain(|_, e| epoch - e.seen <= CENSUS_KEEP_S);
        c.chrome = chrome;
        c.last_pass = Some(epoch);
        c.passes += 1;
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
        let mut excused = vec![];
        for e in &unattributed {
            let what = format!(
                "{} (pid {}) died of {} at {:.3} and could not be attributed to a cgroup",
                e.comm.as_deref().unwrap_or("?"),
                e.tgid,
                e.signal_name,
                e.epoch
            );
            match &e.chrome_cgroups {
                Some(c) if !c.is_empty() => {
                    if c.iter().any(|x| mine(x)) {
                        incomplete.push(format!("{what}; counted against this episode: its tree ran Chrome at that moment"));
                    } else {
                        excused.push(format!("{what}; not counted: this episode's tree ran no Chrome at that moment"));
                    }
                }
                // no fresh census, or no Chrome anywhere: it may have been anyone's
                _ => incomplete.push(what),
            }
        }
        if aged_out {
            incomplete.push("crash events of this episode were dropped from the watch's buffer".into());
        }
        Judgment { crashes, incomplete, excused }
    }

    pub fn summary(&self) -> serde_json::Value {
        let c = self.census.lock().unwrap();
        let census = serde_json::json!({"passes": c.passes, "last_pass_epoch": c.last_pass, "procs": c.procs.len(), "chrome_cgroups": c.chrome.len()});
        drop(c);
        let g = self.inner.lock().unwrap();
        serde_json::json!({"census": census, "attributed": g.attributed, "running": g.stopped.is_none(), "stopped": g.stopped.as_ref().map(|(t, w)| serde_json::json!({"epoch": t, "why": w})),
                           "crash_events": g.events.len() as u64 + g.dropped_by_age, "unattributed": g.events.iter().filter(|e| e.cgroup.is_none()).count(),
                           "overflows": g.overflows.len(), "exits_seen": g.exits_seen, "started_epoch": self.started_epoch})
    }
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Judgment {
    pub crashes: Vec<CrashEvent>,
    pub incomplete: Vec<String>,
    /// Unattributed crashes that were not counted against this episode, in words (its tree ran no Chrome then).
    pub excused: Vec<String>,
}

impl Judgment {
    pub fn clean(&self) -> bool {
        self.crashes.is_empty() && self.incomplete.is_empty()
    }
}

/// Every cgroup at or below `roots` with the tgids in its cgroup.procs, as paths relative to `cg_root` (the
/// /proc/PID/cgroup form). A root that does not exist (yet) gives nothing.
pub fn census_scan(roots: &[PathBuf], cg_root: &Path) -> Vec<(String, Vec<i32>)> {
    let mut out = vec![];
    let mut todo: Vec<PathBuf> = roots.to_vec();
    while let Some(d) = todo.pop() {
        let Ok(procs) = std::fs::read_to_string(d.join("cgroup.procs")) else { continue };
        let rel = d.strip_prefix(cg_root).map(|p| format!("/{}", p.display())).unwrap_or_else(|_| d.display().to_string());
        out.push((rel, procs.split_whitespace().filter_map(|x| x.parse().ok()).collect()));
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    todo.push(e.path());
                }
            }
        }
    }
    out.sort();
    out
}

fn check(r: libc::c_int, what: &str) -> io::Result<()> {
    if r < 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(e.kind(), format!("{what}: {e}")));
    }
    Ok(())
}

/// Open the connector socket, subscribe to non-zero exits, and start the reader thread and the census of the cgroups
/// at or below `census_roots` (the slots' pool). Fails when the kernel has no process events connector or refuses the
/// subscription (drumsd must be root in the initial user namespace).
pub fn start(census_roots: Vec<PathBuf>) -> io::Result<CrashWatch> {
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
    let w = CrashWatch { inner: Arc::default(), census: Arc::default(), started_epoch: crate::now_epoch() };
    // the first pass before the first exit is read
    w.record_census(&census_scan(&census_roots, Path::new(crate::cgroup::CG_ROOT)), crate::now_epoch(), crate::procfs::comm);
    let w2 = w.clone();
    std::thread::Builder::new().name("crashwatch".into()).spawn(move || reader(fd, w2))?;
    let w3 = w.clone();
    std::thread::Builder::new().name("crashcensus".into()).spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(CENSUS_EVERY_MS));
        let seen = census_scan(&census_roots, Path::new(crate::cgroup::CG_ROOT));
        w3.record_census(&seen, crate::now_epoch(), crate::procfs::comm);
    })?;
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
        assert_eq!(
            r,
            vec![
                ExitRecord { pid: 10, tgid: 10, exit_code: 4, parent_pid: 5, parent_tgid: 5 },
                ExitRecord { pid: 12, tgid: 10, exit_code: 0x84, parent_pid: 5, parent_tgid: 5 }
            ]
        );
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
    fn a_crash_after_its_tree_is_gone_is_placed_through_a_remembered_exit() {
        let w = CrashWatch::new_for_test();
        let t0 = w.started_epoch;
        // the browser (pid 7, in /a) exits normally while /proc still has it; then its renderer dies of SIGTRAP after
        // every /proc entry of the tree is gone
        w.record_exit(&ExitRecord { pid: 7, tgid: 7, exit_code: 0, parent_pid: 1, parent_tgid: 1 }, t0 + 1.0, |p| (p == 7).then(|| "/a".to_string()), |_| None);
        w.record_exit(&ExitRecord { pid: 9, tgid: 9, exit_code: 5, parent_pid: 7, parent_tgid: 7 }, t0 + 1.01, |_| None, |_| None);
        let j = w.judge("/a", t0);
        assert_eq!(j.crashes.len(), 1);
        assert_eq!(j.crashes[0].cgroup_from.as_deref(), Some("remembered exit"));
        assert!(j.incomplete.is_empty(), "{:?}", j.incomplete);
        // an episode in another cgroup is untouched
        let k = w.judge("/b", t0);
        assert!(k.crashes.is_empty() && k.incomplete.is_empty());
        // a remembered exit older than the window is forgotten
        w.record_exit(&ExitRecord { pid: 30, tgid: 30, exit_code: 0, parent_pid: 1, parent_tgid: 1 }, t0 + 1.0, |_| Some("/c".into()), |_| None);
        w.record_exit(&ExitRecord { pid: 31, tgid: 31, exit_code: 0, parent_pid: 1, parent_tgid: 1 }, t0 + 100.0, |_| Some("/d".into()), |_| None);
        w.record_exit(&ExitRecord { pid: 32, tgid: 32, exit_code: 5, parent_pid: 30, parent_tgid: 30 }, t0 + 100.0, |_| None, |_| None);
        assert_eq!(w.judge("/c", t0 + 50.0).incomplete.len(), 1);
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
    const A: &str = "/system.slice/drumsbench-pool.scope/drumsbench-m-1-pool.scope/drumsbench-m-1-r1.scope";
    const B: &str = "/system.slice/drumsbench-pool.scope/drumsbench-m-2-pool.scope/drumsbench-m-2-r1.scope";
    fn comm_of(p: i32) -> Option<String> {
        Some(
            match p {
                40..=49 => "chrome",
                _ => "soffice.bin",
            }
            .to_string(),
        )
    }

    #[test]
    fn a_crash_reaped_with_its_parent_is_attributed_by_the_census_to_its_own_slot_only() {
        // T6: a Chrome process died of SIGTRAP and it and its parent were gone from /proc before the watch read them;
        // the old watch called it unattributed and failed every running episode, Chrome-less ones included
        let w = CrashWatch::new_for_test();
        let t0 = w.started_epoch;
        w.record_census(&[(A.to_string(), vec![40, 41]), (B.to_string(), vec![50])], t0 + 0.5, comm_of);
        w.record_exit(&ExitRecord { pid: 40, tgid: 40, exit_code: 5, parent_pid: 41, parent_tgid: 41 }, t0 + 1.0, |_| None, |_| None);
        let a = w.judge(A, t0);
        assert_eq!(a.crashes.len(), 1, "{a:?}");
        assert_eq!(a.crashes[0].signal_name, "SIGTRAP");
        assert_eq!(a.crashes[0].cgroup_from.as_deref(), Some("census"));
        assert_eq!(a.crashes[0].comm.as_deref(), Some("chrome"), "the census remembers the comm the dead task had");
        assert!(a.incomplete.is_empty(), "{:?}", a.incomplete);
        let b = w.judge(B, t0);
        assert!(b.clean(), "a crash attributed to slot 1 must not touch slot 2: {b:?}");
        assert_eq!(w.summary()["unattributed"], 0);
    }

    #[test]
    fn a_crash_of_a_task_the_census_never_saw_is_attributed_through_its_parent_in_the_census() {
        let w = CrashWatch::new_for_test();
        let t0 = w.started_epoch;
        w.record_census(&[(A.to_string(), vec![41]), (B.to_string(), vec![50])], t0 + 0.5, comm_of);
        // a renderer forked after the last pass, reaped together with its zygote (41)
        w.record_exit(&ExitRecord { pid: 77, tgid: 77, exit_code: 5, parent_pid: 41, parent_tgid: 41 }, t0 + 0.6, |_| None, |_| None);
        let a = w.judge(A, t0);
        assert_eq!(a.crashes.len(), 1);
        assert_eq!(a.crashes[0].cgroup_from.as_deref(), Some("census-parent"));
        assert_eq!(a.crashes[0].parent_comm.as_deref(), Some("chrome"));
        assert!(w.judge(B, t0).clean());
    }

    #[test]
    fn an_unattributed_crash_counts_only_against_episodes_that_ran_chrome_and_says_so() {
        let w = CrashWatch::new_for_test();
        let t0 = w.started_epoch;
        // slot 1 runs Chrome, slot 2 only LibreOffice; the crashed task and its parent were never seen by any pass
        w.record_census(&[(A.to_string(), vec![40]), (B.to_string(), vec![50])], t0 + 0.5, comm_of);
        w.record_exit(&ExitRecord { pid: 99, tgid: 99, exit_code: 5, parent_pid: 98, parent_tgid: 98 }, t0 + 0.7, |_| None, |_| None);
        let a = w.judge(A, t0);
        assert!(!a.clean(), "never silently valid: the Chrome slot cannot be vouched for");
        assert!(a.incomplete[0].contains("could not be attributed") && a.incomplete[0].contains("Chrome"), "{:?}", a.incomplete);
        let b = w.judge(B, t0);
        assert!(b.clean(), "a slot with no Chrome is not invalidated by an unattributed Chrome-era crash: {b:?}");
        assert_eq!(b.excused.len(), 1, "the episode's report still records the unattributed crash");
        assert!(b.excused[0].contains("not counted"), "{:?}", b.excused);
    }

    #[test]
    fn an_unattributed_crash_without_a_fresh_census_or_any_chrome_fails_every_episode_closed() {
        // no census at all
        let w = CrashWatch::new_for_test();
        let t0 = w.started_epoch;
        w.record_exit(&ExitRecord { pid: 99, tgid: 99, exit_code: 5, parent_pid: 98, parent_tgid: 98 }, t0 + 1.0, |_| None, |_| None);
        assert!(!w.judge(B, t0).clean());
        // a stale census (the census thread stalled): cannot say which slots ran Chrome
        let w = CrashWatch::new_for_test();
        w.record_census(&[(A.to_string(), vec![40]), (B.to_string(), vec![50])], t0, comm_of);
        w.record_exit(&ExitRecord { pid: 99, tgid: 99, exit_code: 5, parent_pid: 98, parent_tgid: 98 }, t0 + CENSUS_FRESH_S + 1.0, |_| None, |_| None);
        assert!(!w.judge(B, t0).clean());
        // a fresh census but no Chrome anywhere: the crash is not Chrome's, so no slot can be excused
        let w = CrashWatch::new_for_test();
        w.record_census(&[(A.to_string(), vec![50]), (B.to_string(), vec![51])], t0 + 0.5, comm_of);
        w.record_exit(&ExitRecord { pid: 99, tgid: 99, exit_code: 5, parent_pid: 98, parent_tgid: 98 }, t0 + 0.7, |_| None, |_| None);
        assert!(!w.judge(A, t0).clean() && !w.judge(B, t0).clean());
    }

    #[test]
    fn the_census_follows_moves_and_forgets_gone_processes_after_the_keep_window() {
        let w = CrashWatch::new_for_test();
        let t0 = w.started_epoch;
        w.record_census(&[(A.to_string(), vec![40])], t0, comm_of);
        // 40 moved to slot 2 (a restore into the other leaf keeps no pid, but a cgroup move must win)
        w.record_census(&[(B.to_string(), vec![40])], t0 + 0.2, comm_of);
        w.record_exit(&ExitRecord { pid: 40, tgid: 40, exit_code: 5, parent_pid: 1, parent_tgid: 1 }, t0 + 0.3, |_| None, |_| None);
        assert!(w.judge(A, t0).clean() && w.judge(B, t0).crashes.len() == 1);
        // a process last seen longer ago than CENSUS_KEEP_S is forgotten (its pid could be reused)
        let w = CrashWatch::new_for_test();
        w.record_census(&[(A.to_string(), vec![60])], t0, comm_of);
        w.record_census(&[(A.to_string(), vec![])], t0 + CENSUS_KEEP_S + 1.0, comm_of);
        w.record_exit(&ExitRecord { pid: 60, tgid: 60, exit_code: 11, parent_pid: 1, parent_tgid: 1 }, t0 + CENSUS_KEEP_S + 1.1, |_| None, |_| None);
        assert!(w.judge(A, t0).crashes.is_empty());
    }

    #[test]
    fn census_scan_reads_every_cgroup_procs_below_the_roots() {
        let d = std::env::temp_dir().join(format!("crashwatch-census-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let pool = d.join("system.slice/drumsbench-pool.scope");
        std::fs::create_dir_all(pool.join("drumsbench-m-1-pool.scope/drumsbench-m-1-r1.scope")).unwrap();
        std::fs::write(pool.join("cgroup.procs"), "").unwrap();
        std::fs::write(pool.join("drumsbench-m-1-pool.scope/cgroup.procs"), "").unwrap();
        std::fs::write(pool.join("drumsbench-m-1-pool.scope/drumsbench-m-1-r1.scope/cgroup.procs"), "40\n41\n").unwrap();
        let mut got = census_scan(&[pool.clone()], &d);
        got.retain(|(_, p)| !p.is_empty());
        assert_eq!(got, vec![(A.to_string(), vec![40, 41])]);
        assert!(census_scan(&[d.join("missing")], &d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
