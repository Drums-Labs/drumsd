//! Reads a slot's tree for the capture-readiness check (`drumsd_core::quiesce` decides).
//!
//! The population is the slot's cgroup (every descendant cgroup included): that holds processes in pid namespaces
//! nested below the slot's too (Chrome's sandbox), which a pid-namespace walk would miss. Threads come from
//! `/proc/PID/task`, and zombies from the tree's `children` lists (a zombie has left its cgroup but is not reaped).

use drumsd_core::quiesce::{Params, Report, Sample, TaskInfo, Verdict, Waiter};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// (comm, state, flags, ppid, start ticks) from a `/proc/.../stat` text.
pub fn parse_stat(s: &str) -> Option<(String, char, u64, i32, u64)> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s[open + 1..close].to_string();
    let f: Vec<&str> = s[close + 1..].split_whitespace().collect();
    // after ')': state(0) ppid(1) pgrp(2) session(3) tty(4) tpgid(5) flags(6) ... starttime(19)
    let state = f.first()?.chars().next()?;
    let ppid = f.get(1)?.parse().ok()?;
    let flags = f.get(6)?.parse().ok()?;
    let start = f.get(19)?.parse().ok()?;
    Some((comm, state, flags, ppid, start))
}

fn cgroup_procs(cg: &Path, out: &mut BTreeSet<i32>) -> io::Result<()> {
    let s = fs::read_to_string(cg.join("cgroup.procs"))?;
    out.extend(s.split_whitespace().filter_map(|x| x.parse::<i32>().ok()));
    for e in fs::read_dir(cg)?.flatten() {
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let _ = cgroup_procs(&e.path(), out);
        }
    }
    Ok(())
}

/// One read of every thread of every process in `cg` (and its descendants), plus unreaped zombie children.
/// A task that vanishes while it is read is simply absent from this sample (the next one shows the change).
pub fn sample(cg: &Path) -> io::Result<Sample> {
    let mut pids = BTreeSet::new();
    cgroup_procs(cg, &mut pids)?;
    let mut s = Sample::default();
    let mut children: BTreeSet<i32> = BTreeSet::new();
    for pid in &pids {
        let Ok(rd) = fs::read_dir(format!("/proc/{pid}/task")) else { continue };
        for e in rd.flatten() {
            let Some(tid) = e.file_name().to_str().and_then(|x| x.parse::<i32>().ok()) else { continue };
            let Ok(st) = fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat")) else { continue };
            let Some((comm, state, flags, _ppid, start)) = parse_stat(&st) else { continue };
            s.tasks.insert(tid, TaskInfo { tgid: *pid, start, state, flags, comm });
            if let Ok(c) = fs::read_to_string(format!("/proc/{pid}/task/{tid}/children")) {
                children.extend(c.split_whitespace().filter_map(|x| x.parse::<i32>().ok()));
            }
        }
    }
    // children that are not in the cgroup: zombies (left the cgroup at exit, not reaped yet)
    for c in children.difference(&pids) {
        if let Ok(st) = fs::read_to_string(format!("/proc/{c}/stat")) {
            if let Some((comm, state, _, ppid, _)) = parse_stat(&st) {
                if state == 'Z' || state == 'X' {
                    s.zombies.insert(*c, (ppid, comm));
                }
            }
        }
    }
    Ok(s)
}

/// Sample `cg` until it is quiescent or `p.timeout_s` passes. Returns the report and, when quiescent, the sample
/// the window ended on (the tree a following dump must match).
pub fn wait(cg: &Path, p: &Params) -> io::Result<(Report, Option<Sample>)> {
    let t0 = Instant::now();
    let mut w = Waiter::new(p.clone(), 0.0);
    loop {
        let ts = Instant::now();
        let s = sample(cg)?;
        let ms = ts.elapsed().as_secs_f64() * 1000.0;
        match w.feed(s, t0.elapsed().as_secs_f64(), ms) {
            Verdict::Quiescent(s) => return Ok((w.report, Some(s))),
            Verdict::TimedOut => return Ok((w.report, None)),
            Verdict::Wait => std::thread::sleep(Duration::from_millis(p.poll_ms.max(1))),
        }
    }
}

/// Event-assisted wait. The activity watch says how long no task of the tree has started or exited; once that covers
/// the window, one sample must show no task on its way out (`PF_EXITING`, state X) and a non-empty tree, and that
/// sample is the tree a following dump must match. A task found leaving keeps the wait going (its exit is an event
/// and restarts the window). When the watch can not vouch for the window (it stopped, dropped or could not attribute
/// an event, or started too recently) the sampler (`wait`) runs for the remaining time instead; the report says so.
pub fn wait_with(cg: &Path, p: &Params, watch: Option<&crate::activity::ActivityWatch>) -> io::Result<(Report, Option<Sample>)> {
    let Some(w) = watch else { return wait(cg, p) };
    let t0 = Instant::now();
    let cgs = cg.display().to_string();
    let mut rep = Report { window_s: p.window_s, timeout_s: p.timeout_s, basis: Some("events".into()), ..Default::default() };
    let mut ms_total = 0.0;
    let mut last_issues: Vec<drumsd_core::quiesce::Unstable> = vec![];
    loop {
        let left = p.timeout_s - t0.elapsed().as_secs_f64();
        match w.quiet_for(&cgs, crate::now_epoch(), p.window_s) {
            Err(why) => {
                let mut p2 = p.clone();
                p2.timeout_s = left.max(0.0);
                let before = t0.elapsed().as_secs_f64();
                let (mut r, s) = wait(cg, &p2)?;
                r.waited_s += before;
                r.events_fallback = Some(why);
                return Ok((r, s));
            }
            Ok(q) if q >= p.window_s => {
                let ts = Instant::now();
                let s = sample(cg)?;
                ms_total += ts.elapsed().as_secs_f64() * 1000.0;
                rep.samples += 1;
                rep.sample_ms_mean = ms_total / rep.samples as f64;
                rep.processes = s.processes().len();
                rep.threads = s.n_threads();
                let mut bad = drumsd_core::quiesce::kernel_issues(&s);
                if s.tasks.is_empty() {
                    bad.push(drumsd_core::quiesce::Unstable { kind: "empty".into(), pid: 0, tid: None, comm: "no task in the slot's cgroup".into() });
                }
                rep.waited_s = t0.elapsed().as_secs_f64();
                if bad.is_empty() {
                    rep.quiescent = true;
                    rep.quiet_for_s = Some((q * 1000.0).round() / 1000.0);
                    return Ok((rep, Some(s)));
                }
                rep.restarts += 1;
                for b in &bad {
                    if rep.seen.len() < 50 {
                        rep.seen.push(b.clone());
                    }
                }
                last_issues = bad;
                if left <= 0.0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(p.poll_ms.max(1)));
            }
            Ok(q) => {
                if left <= 0.0 {
                    break;
                }
                std::thread::sleep(Duration::from_secs_f64((p.window_s - q).clamp(0.001, left.max(0.001))));
            }
        }
    }
    rep.waited_s = t0.elapsed().as_secs_f64();
    last_issues.truncate(50);
    rep.unstable = last_issues;
    Ok((rep, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_fields() {
        let s = "13493 (Chrome_ChildIOT) S 13224 13224 13224 0 -1 4194624 100 0 0 0 1 2 0 0 20 0 2 0 123456 1000 10 18446744073709551615";
        let (comm, state, flags, ppid, start) = parse_stat(s).unwrap();
        assert_eq!((comm.as_str(), state, flags, ppid, start), ("Chrome_ChildIOT", 'S', 4194624, 13224, 123456));
        // a comm with spaces and parens
        let s2 = "7 (a (b) c) Z 1 7 7 0 -1 4194308 0 0 0 0 0 0 0 0 20 0 1 0 99 0 0 0";
        let (comm, state, flags, _, start) = parse_stat(s2).unwrap();
        assert_eq!((comm.as_str(), state, flags & drumsd_core::quiesce::PF_EXITING, start), ("a (b) c", 'Z', 4, 99));
    }

    #[test]
    fn reads_own_threads() {
        // this process's own cgroup holds at least this test's threads
        let cg = crate::procfs::cgroup_path(std::process::id() as i32).unwrap();
        let p = Path::new(crate::cgroup::CG_ROOT).join(cg.trim_start_matches('/'));
        if let Ok(s) = sample(&p) {
            let me = std::process::id() as i32;
            assert!(s.tasks.contains_key(&me), "own pid in the sample of its own cgroup");
        }
    }

    /// A zombie child (exited, not reaped) is in the sample, and its appearance is a change.
    #[test]
    fn a_zombie_child_is_seen() {
        let cg = crate::procfs::cgroup_path(std::process::id() as i32).unwrap();
        let p = Path::new(crate::cgroup::CG_ROOT).join(cg.trim_start_matches('/'));
        if fs::read_to_string(p.join("cgroup.procs")).is_err() {
            eprintln!("skipped: no readable cgroup");
            return;
        }
        let s0 = sample(&p).unwrap();
        let child = std::process::Command::new("true").spawn().unwrap();
        let cpid = child.id() as i32;
        // wait for it to become a zombie (not reaped: no wait() yet)
        for _ in 0..200 {
            if fs::read_to_string(format!("/proc/{cpid}/stat")).ok().and_then(|s| parse_stat(&s)).map(|x| x.1) == Some('Z') {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let s = sample(&p).unwrap();
        let in_cg = s.tasks.get(&cpid).map(|t| t.state);
        assert!(s.zombies.contains_key(&cpid) || in_cg == Some('Z'), "zombie {cpid} seen: zombies {:?} task {:?}", s.zombies, in_cg);
        let ch = drumsd_core::quiesce::changes(&s0, &s);
        assert!(ch.iter().any(|x| x.pid == cpid), "the new zombie is a change: {ch:?}");
        let mut child = child;
        let _ = child.wait();
    }
}
