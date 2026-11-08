//! Activity watch: when did a task (process or thread) of a slot's tree last start or exit?
//!
//! The capture-readiness rule (drumsd_core::quiesce) asks for a window, 1 s by default, in which no task of the tree
//! starts, exits or is on its way out. The sampler proves that window by reading the whole tree every 50 ms for the
//! length of the window, so every capture paid the full window on its critical path even when the tree had been quiet
//! for a minute. This watch keeps the same record continuously and cheaply: the kernel's process events connector
//! (netlink `NETLINK_CONNECTOR`, `CN_IDX_PROC`, filtered in the kernel to fork and exit events) reports every task
//! that starts or ends on the host; each one is attributed to a cgroup at once and the newest event time is kept per
//! cgroup. A capture then needs the window only when the tree was actually busy within it.
//!
//! Fail closed: a stretch the kernel dropped (`ENOBUFS`), an event the watch could not attribute, a stopped watch or a
//! window that started before the watch did all make the watch unable to vouch for a window (`quiet_for` is None)
//! and the caller falls back to the sampler.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use crate::crashwatch::{CN_IDX_PROC, CN_VAL_PROC, NETLINK_CONNECTOR, PROC_CN_MCAST_LISTEN, PROC_EVENT_EXIT};

pub const PROC_EVENT_FORK: u32 = 0x0000_0001;

/// One fork or exit record of the connector: the task that started or ended and its thread group.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Event {
    /// `fork_proc_event`: parent pid/tgid, child pid/tgid. A new thread has child_tgid == parent_tgid.
    Fork { parent_tgid: i32, child_pid: i32, child_tgid: i32 },
    /// `exit_proc_event`: process pid/tgid, ..., parent pid/tgid.
    Exit { pid: i32, tgid: i32, parent_tgid: i32 },
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|x| u32::from_ne_bytes([x[0], x[1], x[2], x[3]]))
}

/// Parse one datagram (nlmsghdr 16 + cn_msg 20 + proc_event header 16 + event data) into fork and exit events.
pub fn parse_datagram(buf: &[u8]) -> Vec<Event> {
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
        let d = ev + 16;
        if let (Some(idx), Some(val), Some(what)) = (u32_at(m, cn), u32_at(m, cn + 4), u32_at(m, ev)) {
            if idx == CN_IDX_PROC && val == CN_VAL_PROC {
                if what == PROC_EVENT_FORK {
                    if let (Some(_ppid), Some(ptgid), Some(cpid), Some(ctgid)) = (u32_at(m, d), u32_at(m, d + 4), u32_at(m, d + 8), u32_at(m, d + 12)) {
                        out.push(Event::Fork { parent_tgid: ptgid as i32, child_pid: cpid as i32, child_tgid: ctgid as i32 });
                    }
                } else if what == PROC_EVENT_EXIT {
                    if let (Some(pid), Some(tgid), Some(ptgid)) = (u32_at(m, d), u32_at(m, d + 4), u32_at(m, d + 20)) {
                        out.push(Event::Exit { pid: pid as i32, tgid: tgid as i32, parent_tgid: ptgid as i32 });
                    }
                }
            }
        }
        off += (len + 3) & !3;
    }
    out
}

#[derive(Default)]
struct Inner {
    /// cgroup path (the /proc/PID/cgroup form, e.g. `/drums.slice/slot-101/l1`) -> newest event epoch in it
    last: HashMap<String, f64>,
    /// thread group -> its cgroup, read when the group's first task was seen (threads share it)
    tg_cg: HashMap<i32, String>,
    /// epochs of events the watch could not attribute to any cgroup
    unattributed: Vec<f64>,
    /// epochs at which the kernel reported lost events (ENOBUFS)
    overflows: Vec<f64>,
    stopped: Option<(f64, String)>,
    events: u64,
}

/// Shared state of the watch. Cheap to clone (Arc).
#[derive(Clone)]
pub struct ActivityWatch {
    inner: Arc<Mutex<Inner>>,
    pub started_epoch: f64,
}

const KEEP_MARKS: usize = 1024;

fn push_mark(v: &mut Vec<f64>, t: f64) {
    v.push(t);
    if v.len() > KEEP_MARKS {
        v.remove(0);
    }
}

impl ActivityWatch {
    pub fn new_for_test(started_epoch: f64) -> ActivityWatch {
        ActivityWatch { inner: Arc::default(), started_epoch }
    }

    /// Record one event read at `epoch`. `read_cgroup(pid)` reads /proc/PID/cgroup (None when the task is gone).
    pub fn record(&self, e: &Event, epoch: f64, read_cgroup: impl Fn(i32) -> Option<String>) {
        let cached = |g: &Inner, tg: i32| g.tg_cg.get(&tg).cloned();
        let mut g = self.inner.lock().unwrap();
        g.events += 1;
        // attribution order: the group's cached cgroup, /proc of the task, then its parent's (a task starts in its
        // parent's cgroup; a short-lived child and its parent can both be gone before this reads them)
        let cg = match *e {
            Event::Fork { parent_tgid, child_pid, child_tgid } => {
                let fresh = child_pid == child_tgid;
                let c0 = if fresh { None } else { cached(&g, child_tgid) };
                let pc = cached(&g, parent_tgid);
                let c = match c0 {
                    Some(c) => Some(c),
                    None => {
                        drop(g);
                        let c = read_cgroup(child_tgid).or(pc).or_else(|| read_cgroup(parent_tgid));
                        g = self.inner.lock().unwrap();
                        c
                    }
                };
                if let Some(c) = &c {
                    g.tg_cg.insert(child_tgid, c.clone());
                }
                c
            }
            Event::Exit { pid, tgid, parent_tgid } => {
                let c = match cached(&g, tgid) {
                    Some(c) => Some(c),
                    None => {
                        let pc = cached(&g, parent_tgid);
                        drop(g);
                        let c = read_cgroup(tgid).or_else(|| read_cgroup(pid)).or(pc).or_else(|| read_cgroup(parent_tgid));
                        g = self.inner.lock().unwrap();
                        c
                    }
                };
                if pid == tgid {
                    g.tg_cg.remove(&tgid);
                }
                c
            }
        };
        match cg {
            Some(c) => {
                let slot = g.last.entry(c).or_insert(epoch);
                if epoch > *slot {
                    *slot = epoch;
                }
            }
            None => push_mark(&mut g.unattributed, epoch),
        }
        // bound the caches: a host runs millions of short tasks in a day
        if g.tg_cg.len() > 200_000 {
            g.tg_cg.clear();
        }
        if g.last.len() > 50_000 {
            let cut = epoch - 3600.0;
            g.last.retain(|_, t| *t >= cut);
        }
    }

    /// Remember the cgroup of every thread group running now (the watch's start), so the exit of a process that
    /// started before the watch is attributed even when it is reaped before the event is read.
    pub fn seed(&self, read_cgroup: impl Fn(i32) -> Option<String>) -> usize {
        let pids: Vec<i32> = std::fs::read_dir("/proc").map(|d| d.flatten().filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok())).collect()).unwrap_or_default();
        let found: Vec<(i32, String)> = pids.into_iter().filter_map(|p| read_cgroup(p).map(|c| (p, c))).collect();
        let n = found.len();
        let mut g = self.inner.lock().unwrap();
        for (p, c) in found {
            g.tg_cg.entry(p).or_insert(c);
        }
        n
    }

    pub fn record_overflow(&self, epoch: f64) {
        push_mark(&mut self.inner.lock().unwrap().overflows, epoch);
    }

    pub fn record_stop(&self, epoch: f64, why: String) {
        self.inner.lock().unwrap().stopped = Some((epoch, why));
    }

    /// How long the tree in cgroup `cg` (a path under /sys/fs/cgroup, or the /proc form) and every cgroup below it has
    /// had no task start or exit, as of `now`, capped at `now - started_epoch`. Err(reason) when the watch can not
    /// vouch for the last `need_s` seconds: it stopped, the kernel dropped events, an event could not be attributed,
    /// or the watch started less than `need_s` ago.
    pub fn quiet_for(&self, cg: &str, now: f64, need_s: f64) -> Result<f64, String> {
        let cg = cg.strip_prefix(crate::cgroup::CG_ROOT).unwrap_or(cg).to_string();
        let g = self.inner.lock().unwrap();
        if let Some((t, why)) = &g.stopped {
            return Err(format!("the activity watch stopped at {t:.3}: {why}"));
        }
        let from = now - need_s;
        if self.started_epoch > from {
            return Err(format!("the activity watch started {:.3} s ago, under the {need_s} s window", now - self.started_epoch));
        }
        if let Some(t) = g.overflows.iter().rev().find(|t| **t >= from) {
            return Err(format!("the kernel dropped process events at {t:.3} (socket overflow)"));
        }
        if let Some(t) = g.unattributed.iter().rev().find(|t| **t >= from) {
            return Err(format!("a task event at {t:.3} could not be attributed to a cgroup"));
        }
        let mine = |c: &str| c == cg || c.starts_with(&format!("{cg}/"));
        let newest = g.last.iter().filter(|(c, _)| mine(c)).map(|(_, t)| *t).fold(f64::NEG_INFINITY, f64::max);
        let since = if newest.is_finite() { newest.max(self.started_epoch) } else { self.started_epoch };
        Ok((now - since).max(0.0))
    }

    pub fn summary(&self) -> serde_json::Value {
        let g = self.inner.lock().unwrap();
        serde_json::json!({"running": g.stopped.is_none(), "events": g.events, "cgroups": g.last.len(), "groups_cached": g.tg_cg.len(),
                           "unattributed": g.unattributed.len(), "overflows": g.overflows.len(), "started_epoch": self.started_epoch})
    }
}

fn check(r: libc::c_int, what: &str) -> io::Result<()> {
    if r < 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(e.kind(), format!("{what}: {e}")));
    }
    Ok(())
}

/// Open a connector socket subscribed to fork and exit events and start the reader thread. Fails when the kernel has
/// no process events connector or refuses the subscription (drumsd must be root in the initial user namespace).
pub fn start() -> io::Result<ActivityWatch> {
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, NETLINK_CONNECTOR) };
    check(fd, "socket(NETLINK_CONNECTOR)")?;
    // every thread start and end on the host comes through here: a large buffer, and an overflow only disables the
    // shortcut for the windows it touches (the sampler runs instead)
    let size: libc::c_int = 32 << 20;
    unsafe {
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUFFORCE, &size as *const _ as *const libc::c_void, std::mem::size_of::<libc::c_int>() as u32);
    }
    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as u16;
    sa.nl_groups = CN_IDX_PROC;
    let r = unsafe { libc::bind(fd, &sa as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_nl>() as u32) };
    if r < 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(io::Error::new(e.kind(), format!("bind(CN_IDX_PROC): {e}")));
    }
    // nlmsghdr + cn_msg + proc_input {mcast_op, event_type}: the kernel passes only fork and exit events
    let mut msg = vec![0u8; 16 + 20 + 8];
    let total = msg.len() as u32;
    msg[0..4].copy_from_slice(&total.to_ne_bytes());
    msg[4..6].copy_from_slice(&(libc::NLMSG_DONE as u16).to_ne_bytes());
    msg[12..16].copy_from_slice(&std::process::id().to_ne_bytes());
    msg[16..20].copy_from_slice(&CN_IDX_PROC.to_ne_bytes());
    msg[20..24].copy_from_slice(&CN_VAL_PROC.to_ne_bytes());
    msg[32..34].copy_from_slice(&8u16.to_ne_bytes());
    msg[36..40].copy_from_slice(&PROC_CN_MCAST_LISTEN.to_ne_bytes());
    msg[40..44].copy_from_slice(&(PROC_EVENT_FORK | PROC_EVENT_EXIT).to_ne_bytes());
    let n = unsafe { libc::send(fd, msg.as_ptr() as *const libc::c_void, msg.len(), 0) };
    if n < 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(io::Error::new(e.kind(), format!("PROC_CN_MCAST_LISTEN: {e}")));
    }
    let w = ActivityWatch { inner: Arc::default(), started_epoch: crate::now_epoch() };
    let w2 = w.clone();
    std::thread::Builder::new().name("activity".into()).spawn(move || reader(fd, w2))?;
    Ok(w)
}

fn reader(fd: i32, w: ActivityWatch) {
    // events queue in the socket meanwhile
    let n = w.seed(crate::procfs::cgroup_path);
    tracing::info!("activity watch: {n} running thread groups seeded");
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        let now = crate::now_epoch();
        if n < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::ENOBUFS) => {
                    tracing::warn!("activity watch: the kernel dropped process events (ENOBUFS)");
                    w.record_overflow(now);
                    continue;
                }
                _ => {
                    tracing::error!("activity watch stopped: {e}");
                    w.record_stop(now, e.to_string());
                    return;
                }
            }
        }
        for e in parse_datagram(&buf[..n as usize]) {
            w.record(&e, now, crate::procfs::cgroup_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datagram(recs: &[(u32, [u32; 6])]) -> Vec<u8> {
        let mut out = vec![];
        for (what, d) in recs {
            let mut m = vec![0u8; 16 + 20 + 16 + 24];
            let len = m.len() as u32;
            m[0..4].copy_from_slice(&len.to_ne_bytes());
            m[16..20].copy_from_slice(&CN_IDX_PROC.to_ne_bytes());
            m[20..24].copy_from_slice(&CN_VAL_PROC.to_ne_bytes());
            m[36..40].copy_from_slice(&what.to_ne_bytes());
            for (i, v) in d.iter().enumerate() {
                m[52 + 4 * i..56 + 4 * i].copy_from_slice(&v.to_ne_bytes());
            }
            out.extend(m);
        }
        out
    }

    #[test]
    fn parses_fork_and_exit_and_skips_others() {
        let d = datagram(&[(PROC_EVENT_FORK, [5, 5, 11, 11, 0, 0]), (0x2, [1, 1, 0, 0, 0, 0]), (PROC_EVENT_EXIT, [12, 10, 0, 17, 5, 5])]);
        assert_eq!(parse_datagram(&d), vec![Event::Fork { parent_tgid: 5, child_pid: 11, child_tgid: 11 }, Event::Exit { pid: 12, tgid: 10, parent_tgid: 5 }]);
        assert!(parse_datagram(&d[..20]).is_empty());
    }

    fn cg_of(map: &'static [(i32, &'static str)]) -> impl Fn(i32) -> Option<String> {
        move |p| map.iter().find(|(q, _)| *q == p).map(|(_, c)| c.to_string())
    }

    #[test]
    fn quiet_time_is_per_tree_and_below() {
        let w = ActivityWatch::new_for_test(100.0);
        let cg = cg_of(&[(20, "/s/slot-1/l1"), (30, "/s/slot-2/l1"), (40, "/s/slot-1/l1/nested")]);
        w.record(&Event::Fork { parent_tgid: 1, child_pid: 20, child_tgid: 20 }, 200.0, &cg);
        w.record(&Event::Fork { parent_tgid: 1, child_pid: 30, child_tgid: 30 }, 205.0, &cg);
        assert_eq!(w.quiet_for("/s/slot-1", 210.0, 1.0), Ok(10.0));
        assert_eq!(w.quiet_for("/sys/fs/cgroup/s/slot-2", 210.0, 1.0), Ok(5.0));
        // a nested cgroup's event counts for the tree
        w.record(&Event::Fork { parent_tgid: 20, child_pid: 40, child_tgid: 40 }, 209.5, &cg);
        assert_eq!(w.quiet_for("/s/slot-1", 210.0, 1.0), Ok(0.5));
        // a prefix that is not a parent cgroup does not match (slot-1 vs slot-10)
        assert_eq!(w.quiet_for("/s/slot-10", 210.0, 1.0), Ok(110.0));
    }

    #[test]
    fn a_thread_and_an_exit_use_the_groups_cgroup_after_the_task_is_gone() {
        let w = ActivityWatch::new_for_test(0.0);
        w.record(&Event::Fork { parent_tgid: 1, child_pid: 20, child_tgid: 20 }, 10.0, cg_of(&[(20, "/a")]));
        // the thread and its exit: /proc no longer answers, the cache does
        w.record(&Event::Fork { parent_tgid: 20, child_pid: 21, child_tgid: 20 }, 11.0, cg_of(&[]));
        w.record(&Event::Exit { pid: 21, tgid: 20, parent_tgid: 1 }, 12.0, cg_of(&[]));
        assert_eq!(w.quiet_for("/a", 13.0, 1.0), Ok(1.0));
        w.record(&Event::Exit { pid: 20, tgid: 20, parent_tgid: 1 }, 12.5, cg_of(&[]));
        assert_eq!(w.quiet_for("/a", 13.0, 1.0), Ok(0.5));
    }

    #[test]
    fn a_short_lived_child_is_attributed_to_its_known_parent() {
        let w = ActivityWatch::new_for_test(0.0);
        w.record(&Event::Fork { parent_tgid: 1, child_pid: 20, child_tgid: 20 }, 10.0, cg_of(&[(20, "/a")]));
        // 20 forks 30, and both are gone before the watch reads /proc
        w.record(&Event::Fork { parent_tgid: 20, child_pid: 30, child_tgid: 30 }, 11.0, cg_of(&[]));
        w.record(&Event::Exit { pid: 30, tgid: 30, parent_tgid: 20 }, 11.5, cg_of(&[]));
        assert_eq!(w.quiet_for("/a", 12.0, 1.0), Ok(0.5));
        assert_eq!(w.summary()["unattributed"], 0);
    }

    #[test]
    fn it_fails_closed() {
        let w = ActivityWatch::new_for_test(100.0);
        assert!(w.quiet_for("/a", 100.5, 1.0).is_err(), "started inside the window");
        w.record(&Event::Exit { pid: 9, tgid: 9, parent_tgid: 8 }, 150.0, cg_of(&[]));
        assert!(w.quiet_for("/a", 150.5, 1.0).unwrap_err().contains("attributed"));
        assert_eq!(w.quiet_for("/a", 152.0, 1.0), Ok(52.0), "an unattributed event outside the window does not matter");
        w.record_overflow(160.0);
        assert!(w.quiet_for("/a", 160.9, 1.0).unwrap_err().contains("dropped"));
        w.record_stop(170.0, "x".into());
        assert!(w.quiet_for("/a", 200.0, 1.0).is_err());
    }
}
