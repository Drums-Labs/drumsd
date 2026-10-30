//! Capture readiness: is a slot's process tree in a state that is safe to checkpoint?
//!
//! A checkpoint taken while a process is on its way out (a Chrome renderer tearing down after its tab closed, a
//! child whose parent has not reaped it yet) carries that exit into every restore: the process finishes exiting in
//! each child, and the alive gate (rightly) cannot tell that from a bad restore. So before a dump the tree must be
//! *quiescent*, and after the dump drumsd checks that what CRIU actually dumped is the tree it judged quiescent.
//!
//! The decisions live here and are pure (the samples are read from /proc by `drumsd_linux::quiesce`):
//!
//! * [`kernel_issues`]: a task the kernel already marks as leaving: state `Z`/`X`, the `PF_EXITING` flag, or a
//!   zombie child nobody has reaped.
//! * [`changes`]: what differs between two samples: a process or thread that started or exited (pid + start time
//!   name one incarnation, so a reused pid is a change too).
//! * [`Waiter`]: quiescent = no kernel issue and no change for a whole window of consecutive samples.
//! * [`parse_dump_log`] + [`compare_dump`]: the tasks and threads CRIU seized (its `-v4` dump log), compared with the
//!   last quiescent sample. Any difference (a process or thread that came or went between the sample and the freeze,
//!   or a task CRIU found dead) rejects the dump.
//! * [`dumped_reference`]: the alive gate's reference set taken from the dump itself.
//! * [`source_after_dump`]: the source tree keeps running after a `--leave-running` dump with no action applied to
//!   it; a dumped process that exits there right after the dump was already exiting in the captured state.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// `PF_EXITING` (include/linux/sched.h): set at the start of do_exit().
pub const PF_EXITING: u64 = 0x0000_0004;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInfo {
    pub tgid: i32,
    /// starttime in clock ticks (field 22 of /proc/PID/stat).
    pub start: u64,
    pub state: char,
    pub flags: u64,
    pub comm: String,
}

/// One read of a slot's tree: every thread of every process in the slot's cgroup, keyed by host tid, and the
/// zombie children found in the tree's `children` lists (they have left the cgroup but are not reaped).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    pub tasks: BTreeMap<i32, TaskInfo>,
    /// host pid -> (parent host pid, comm)
    pub zombies: BTreeMap<i32, (i32, String)>,
}

impl Sample {
    /// host pid -> comm of the thread-group leaders.
    pub fn processes(&self) -> BTreeMap<i32, String> {
        self.tasks.iter().filter(|(tid, t)| **tid == t.tgid).map(|(tid, t)| (*tid, t.comm.clone())).collect()
    }
    pub fn threads_of(&self, tgid: i32) -> BTreeSet<i32> {
        self.tasks.iter().filter(|(_, t)| t.tgid == tgid).map(|(tid, _)| *tid).collect()
    }
    pub fn n_threads(&self) -> usize {
        self.tasks.len()
    }
}

/// Why the tree is not (yet) safe to capture.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
pub struct Unstable {
    /// zombie | exiting | dead | process_started | process_exited | thread_started | thread_exited |
    /// not_dumped | dumped_not_sampled | dumped_dead | thread_not_dumped | thread_dumped_not_sampled | exited_after_dump
    pub kind: String,
    pub pid: i32,
    pub tid: Option<i32>,
    pub comm: String,
}

fn u(kind: &str, pid: i32, tid: Option<i32>, comm: &str) -> Unstable {
    Unstable { kind: kind.into(), pid, tid, comm: comm.into() }
}

/// Tasks the kernel already marks as leaving.
pub fn kernel_issues(s: &Sample) -> Vec<Unstable> {
    let mut v = vec![];
    for (tid, t) in &s.tasks {
        let tid_opt = (*tid != t.tgid).then_some(*tid);
        if t.state == 'Z' {
            v.push(u("zombie", t.tgid, tid_opt, &t.comm));
        } else if t.state == 'X' || t.state == 'x' {
            v.push(u("dead", t.tgid, tid_opt, &t.comm));
        } else if t.flags & PF_EXITING != 0 {
            v.push(u("exiting", t.tgid, tid_opt, &t.comm));
        }
    }
    for (pid, (_, comm)) in &s.zombies {
        v.push(u("zombie", *pid, None, comm));
    }
    v
}

/// Processes and threads that started or exited between `a` and `b` (pid + start time = one incarnation).
pub fn changes(a: &Sample, b: &Sample) -> Vec<Unstable> {
    let mut v = vec![];
    for (tid, t) in &a.tasks {
        match b.tasks.get(tid) {
            Some(t2) if t2.start == t.start && t2.tgid == t.tgid => {}
            _ => v.push(if *tid == t.tgid { u("process_exited", *tid, None, &t.comm) } else { u("thread_exited", t.tgid, Some(*tid), &t.comm) }),
        }
    }
    for (tid, t) in &b.tasks {
        match a.tasks.get(tid) {
            Some(t1) if t1.start == t.start && t1.tgid == t.tgid => {}
            _ => v.push(if *tid == t.tgid { u("process_started", *tid, None, &t.comm) } else { u("thread_started", t.tgid, Some(*tid), &t.comm) }),
        }
    }
    v
}

/// Quiescence parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Params {
    /// The tree must show no kernel issue and no change for this long (seconds of consecutive samples).
    pub window_s: f64,
    /// Give up after this long (seconds) and report what was unstable.
    pub timeout_s: f64,
    /// Sample every this many milliseconds.
    pub poll_ms: u64,
}

impl Default for Params {
    fn default() -> Self {
        Params { window_s: 1.0, timeout_s: 30.0, poll_ms: 50 }
    }
}

/// What a quiescence wait saw.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub quiescent: bool,
    pub waited_s: f64,
    pub samples: u64,
    pub window_s: f64,
    pub timeout_s: f64,
    /// How many times the window restarted because something changed or was leaving.
    pub restarts: u64,
    pub processes: usize,
    pub threads: usize,
    /// When not quiescent: what was unstable during the last window (at most 50 entries).
    pub unstable: Vec<Unstable>,
    /// Every change or issue that restarted the window (at most 50 entries, the first ones), for the record.
    pub seen: Vec<Unstable>,
    /// Mean milliseconds per sample (the cost of reading the tree).
    pub sample_ms_mean: f64,
}

/// Feed samples in time order; it says when the window is complete.
pub struct Waiter {
    pub params: Params,
    last: Option<Sample>,
    stable_since: f64,
    started: f64,
    pub report: Report,
    window_issues: Vec<Unstable>,
    sample_ms_total: f64,
}

pub enum Verdict {
    /// Quiescent: the last sample is the tree the window proved stable.
    Quiescent(Sample),
    Wait,
    TimedOut,
}

impl Waiter {
    pub fn new(params: Params, now: f64) -> Self {
        let report = Report { window_s: params.window_s, timeout_s: params.timeout_s, ..Default::default() };
        Waiter { params, last: None, stable_since: now, started: now, report, window_issues: vec![], sample_ms_total: 0.0 }
    }

    /// `now` in seconds on any monotonic clock; `sample_ms` how long the read took.
    pub fn feed(&mut self, s: Sample, now: f64, sample_ms: f64) -> Verdict {
        self.report.samples += 1;
        self.sample_ms_total += sample_ms;
        self.report.sample_ms_mean = self.sample_ms_total / self.report.samples as f64;
        self.report.waited_s = now - self.started;
        self.report.processes = s.processes().len();
        self.report.threads = s.n_threads();
        let mut bad = kernel_issues(&s);
        if let Some(prev) = &self.last {
            bad.extend(changes(prev, &s));
        }
        if s.tasks.is_empty() {
            bad.push(u("empty", 0, None, "no task in the slot's cgroup"));
        }
        let first = self.last.is_none();
        let clean = bad.is_empty();
        if !clean {
            self.report.restarts += 1;
            for b in &bad {
                if self.report.seen.len() < 50 {
                    self.report.seen.push(b.clone());
                }
            }
            // what restarted the window most recently is the reason reported on a timeout
            self.window_issues = bad;
        }
        if !clean || first {
            self.stable_since = now;
        }
        self.last = Some(s);
        if clean && !first && now - self.stable_since >= self.params.window_s {
            self.report.quiescent = true;
            self.report.unstable.clear();
            return Verdict::Quiescent(self.last.clone().unwrap());
        }
        if now - self.started >= self.params.timeout_s {
            let mut un = self.window_issues.clone();
            un.truncate(50);
            self.report.unstable = un;
            return Verdict::TimedOut;
        }
        Verdict::Wait
    }
}

/// One task as CRIU's dump log records it (`-v4`): seized state, pid at every namespace level, comm, threads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpedTask {
    /// host pid
    pub pid: i32,
    /// compel task state: 1 alive, 2 dead (zombie), 3 stopped
    pub state: u32,
    /// pid at every pid namespace level, outermost (host) first
    pub levels: Vec<i32>,
    pub comm: Option<String>,
    /// host tids, the main thread included
    pub threads: BTreeSet<i32>,
}

pub const TASK_ALIVE: u32 = 1;
pub const TASK_DEAD: u32 = 2;
pub const TASK_STOPPED: u32 = 3;

/// Parse the seize and dump lines of a CRIU dump log (formats of drums-criu app-compat 72dd49a5, criu/seize.c and
/// criu/cr-dump.c): `Seized task P, state S`, `\tTask P: N pid namespace level(s): P ... p`, `\tSeizing P's T thread`,
/// `Dumping task (pid: P comm: C)`. The root task is listed by `Seized task` like every other one.
pub fn parse_dump_log(text: &str) -> Result<BTreeMap<i32, DumpedTask>, String> {
    let mut out: BTreeMap<i32, DumpedTask> = BTreeMap::new();
    let mut dumped_ok = false;
    for line in text.lines() {
        // strip the "(00.001726) " timestamp
        let l = match line.find(") ") {
            Some(i) if line.starts_with('(') => &line[i + 2..],
            _ => line,
        };
        let lt = l.trim_start();
        if let Some(rest) = lt.strip_prefix("Seized task ") {
            if let Some((p, s)) = rest.split_once(", state ") {
                if let (Ok(p), Ok(s)) = (p.trim().parse::<i32>(), s.trim().parse::<u32>()) {
                    let e = out.entry(p).or_insert_with(|| DumpedTask { pid: p, ..Default::default() });
                    e.state = s;
                    e.threads.insert(p);
                }
            }
        } else if let Some(rest) = lt.strip_prefix("Task ") {
            if let Some((p, r)) = rest.split_once(": ") {
                if let (Ok(p), Some((_, lv))) = (p.trim().parse::<i32>(), r.split_once("pid namespace level(s):")) {
                    let levels: Vec<i32> = lv.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                    if let Some(e) = out.get_mut(&p) {
                        e.levels = levels;
                    }
                }
            }
        } else if let Some(rest) = lt.strip_prefix("Seizing ") {
            // "13493's 13494 thread"
            if let Some((p, r)) = rest.split_once("'s ") {
                if let (Ok(p), Some(t)) = (p.trim().parse::<i32>(), r.strip_suffix(" thread").and_then(|t| t.trim().parse::<i32>().ok())) {
                    out.entry(p).or_insert_with(|| DumpedTask { pid: p, ..Default::default() }).threads.insert(t);
                }
            }
        } else if let Some(rest) = lt.strip_prefix("Dumping task (pid: ") {
            if let Some((p, c)) = rest.split_once(" comm: ") {
                if let Ok(p) = p.trim().parse::<i32>() {
                    let c = c.strip_suffix(')').unwrap_or(c);
                    if let Some(e) = out.get_mut(&p) {
                        e.comm = Some(c.to_string());
                    }
                }
            }
        } else if lt.starts_with("Dumping finished successfully") {
            dumped_ok = true;
        }
    }
    if out.is_empty() {
        return Err("the dump log lists no seized task (is it a -v4 log?)".into());
    }
    if !dumped_ok {
        return Err("the dump log does not say 'Dumping finished successfully'".into());
    }
    Ok(out)
}

/// Differences between the tree judged quiescent (`s`, the last sample before the dump) and what CRIU dumped.
pub fn compare_dump(s: &Sample, dumped: &BTreeMap<i32, DumpedTask>) -> Vec<Unstable> {
    let mut v = vec![];
    let procs = s.processes();
    for (pid, d) in dumped {
        let comm = d.comm.clone().unwrap_or_default();
        if d.state != TASK_ALIVE && d.state != TASK_STOPPED {
            v.push(u("dumped_dead", *pid, None, &comm));
            continue;
        }
        if !procs.contains_key(pid) {
            v.push(u("dumped_not_sampled", *pid, None, &comm));
            continue;
        }
        let st = s.threads_of(*pid);
        for t in d.threads.difference(&st) {
            v.push(u("thread_dumped_not_sampled", *pid, Some(*t), &comm));
        }
        for t in st.difference(&d.threads) {
            v.push(u("thread_not_dumped", *pid, Some(*t), &comm));
        }
    }
    for (pid, comm) in &procs {
        if !dumped.contains_key(pid) {
            v.push(u("not_dumped", *pid, None, comm));
        }
    }
    v
}

/// The alive gate's reference set from the dump: {namespace pid: comm} of every live dumped process in the root
/// task's own pid namespace (same population as the pre-resume census: processes in pid namespaces nested below
/// the slot's are not in it). Zombies in the dump are excluded (they were already dead when captured).
pub fn dumped_reference(dumped: &BTreeMap<i32, DumpedTask>) -> Result<BTreeMap<String, String>, String> {
    let root = dumped.values().find(|d| d.levels.last() == Some(&1)).ok_or("no dumped task is pid 1 of its namespace")?;
    let n = root.levels.len();
    let mut out = BTreeMap::new();
    for d in dumped.values() {
        if d.levels.len() != n || !(d.state == TASK_ALIVE || d.state == TASK_STOPPED) {
            continue;
        }
        let comm = d.comm.clone().ok_or_else(|| format!("dumped task {} has no comm in the log", d.pid))?;
        out.insert(d.levels.last().unwrap().to_string(), comm);
    }
    Ok(out)
}

/// After a `--leave-running` dump, the source tree runs on with no action applied to it (the slot's operation lock
/// is held). Every live dumped process must still be there (same pid and start time) and not leaving.
pub fn source_after_dump(before: &Sample, dumped: &BTreeMap<i32, DumpedTask>, after: &Sample) -> Vec<Unstable> {
    let mut v = vec![];
    let leaving: BTreeSet<i32> = kernel_issues(after).into_iter().filter(|x| x.tid.is_none()).map(|x| x.pid).collect();
    for (pid, d) in dumped {
        if !(d.state == TASK_ALIVE || d.state == TASK_STOPPED) {
            continue;
        }
        let comm = d.comm.clone().unwrap_or_default();
        let start = before.tasks.get(pid).map(|t| t.start);
        match (after.tasks.get(pid), start) {
            (Some(t), Some(s0)) if t.start == s0 && !leaving.contains(pid) => {}
            _ => v.push(u("exited_after_dump", *pid, None, &comm)),
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(tgid: i32, start: u64, comm: &str) -> TaskInfo {
        TaskInfo { tgid, start, state: 'S', flags: 0x400000, comm: comm.into() }
    }
    fn sample(v: &[(i32, i32, u64, &str)]) -> Sample {
        Sample { tasks: v.iter().map(|(tid, tgid, st, c)| (*tid, t(*tgid, *st, c))).collect(), zombies: BTreeMap::new() }
    }

    #[test]
    fn a_stable_tree_becomes_quiescent_after_the_window() {
        let s = sample(&[(10, 10, 100, "python3"), (11, 10, 101, "python3"), (20, 20, 200, "chrome")]);
        let mut w = Waiter::new(Params { window_s: 0.5, timeout_s: 5.0, poll_ms: 50 }, 0.0);
        assert!(matches!(w.feed(s.clone(), 0.0, 1.0), Verdict::Wait));
        assert!(matches!(w.feed(s.clone(), 0.3, 1.0), Verdict::Wait));
        assert!(matches!(w.feed(s.clone(), 0.5, 1.0), Verdict::Quiescent(_)));
        assert!(w.report.quiescent);
        assert_eq!(w.report.restarts, 0);
    }

    #[test]
    fn a_thread_exiting_restarts_the_window() {
        let a = sample(&[(20, 20, 200, "chrome"), (21, 20, 201, "chrome"), (22, 20, 202, "chrome")]);
        let b = sample(&[(20, 20, 200, "chrome"), (21, 20, 201, "chrome")]);
        let mut w = Waiter::new(Params { window_s: 0.5, timeout_s: 5.0, poll_ms: 50 }, 0.0);
        w.feed(a, 0.0, 1.0);
        assert!(matches!(w.feed(b.clone(), 0.4, 1.0), Verdict::Wait));
        assert_eq!(w.report.seen[0].kind, "thread_exited");
        assert!(matches!(w.feed(b.clone(), 0.8, 1.0), Verdict::Wait), "window restarted at 0.4");
        assert!(matches!(w.feed(b, 0.9, 1.0), Verdict::Quiescent(_)));
    }

    #[test]
    fn exiting_and_zombie_tasks_block_and_time_out_with_the_reason() {
        let mut s = sample(&[(20, 20, 200, "chrome"), (30, 30, 300, "bash")]);
        s.tasks.get_mut(&20).unwrap().flags |= PF_EXITING;
        s.zombies.insert(31, (30, "sleep".into()));
        let mut w = Waiter::new(Params { window_s: 0.2, timeout_s: 1.0, poll_ms: 50 }, 0.0);
        let mut v = Verdict::Wait;
        let mut now = 0.0;
        while matches!(v, Verdict::Wait) {
            v = w.feed(s.clone(), now, 1.0);
            now += 0.1;
        }
        assert!(matches!(v, Verdict::TimedOut));
        let kinds: BTreeSet<String> = w.report.unstable.iter().map(|x| x.kind.clone()).collect();
        assert!(kinds.contains("exiting") && kinds.contains("zombie"), "{:?}", w.report.unstable);
        assert!(!w.report.quiescent);
    }

    #[test]
    fn a_reused_pid_is_a_change() {
        let a = sample(&[(20, 20, 200, "chrome")]);
        let b = sample(&[(20, 20, 999, "chrome")]);
        let c = changes(&a, &b);
        assert_eq!(c.len(), 2);
        assert!(c.iter().any(|x| x.kind == "process_exited") && c.iter().any(|x| x.kind == "process_started"));
    }

    const LOG: &str = "(00.000529) Dumping processes (pid: 12804 comm: python3)
(00.001726) Seized task 12804, state 1
(00.001728) \tTask 12804: 2 pid namespace level(s): 12804 1
(00.006578) Seized task 13493, state 1
(00.006579) \tTask 13493: 2 pid namespace level(s): 13493 552
(00.006585) \tSeizing 13493's 13494 thread
(00.006626) \tThread 13494: 2 pid namespace level(s): 13494 553
(00.006700) Seized task 13500, state 2
(00.006701) \tTask 13500: 2 pid namespace level(s): 13500 560
(00.006800) Seized task 13600, state 1
(00.006801) \tTask 13600: 3 pid namespace level(s): 13600 600 1
(00.098559) Dumping task (pid: 12804 comm: python3)
(01.950846) Dumping task (pid: 13493 comm: chrome)
(01.950900) Dumping task (pid: 13600 comm: chrome)
(02.000000) Dumping finished successfully
";

    #[test]
    fn dump_log_is_parsed_and_the_reference_comes_from_it() {
        let d = parse_dump_log(LOG).unwrap();
        assert_eq!(d.len(), 4);
        assert_eq!(d[&13493].threads, [13493, 13494].into_iter().collect());
        assert_eq!(d[&13493].levels, vec![13493, 552]);
        assert_eq!(d[&13500].state, TASK_DEAD);
        let r = dumped_reference(&d).unwrap();
        // the zombie (560) and the nested-namespace task (13600) are not in the reference
        assert_eq!(r, [("1".to_string(), "python3".to_string()), ("552".to_string(), "chrome".to_string())].into_iter().collect());
        assert!(parse_dump_log("(00.1) nothing here").is_err());
        assert!(parse_dump_log(&LOG.replace("Dumping finished successfully", "Dumping FAILED")).is_err());
    }

    #[test]
    fn dump_and_sample_must_match() {
        let d = parse_dump_log(LOG).unwrap();
        let good = sample(&[(12804, 12804, 1, "python3"), (13493, 13493, 2, "chrome"), (13494, 13493, 3, "chrome"), (13600, 13600, 4, "chrome")]);
        // the dead task in the dump is reported (it was exiting when frozen)
        let v = compare_dump(&good, &d);
        assert_eq!(v, vec![u("dumped_dead", 13500, None, "")]);
        let mut d2 = d.clone();
        d2.remove(&13500);
        assert!(compare_dump(&good, &d2).is_empty());
        // a thread that exited between the last sample and the freeze
        let mut d3 = d2.clone();
        d3.get_mut(&13493).unwrap().threads.remove(&13494);
        assert_eq!(compare_dump(&good, &d3), vec![u("thread_not_dumped", 13493, Some(13494), "chrome")]);
        // a process that started after the sample
        let mut d4 = d2.clone();
        d4.insert(14000, DumpedTask { pid: 14000, state: 1, levels: vec![14000, 700], comm: Some("sh".into()), threads: [14000].into_iter().collect() });
        assert_eq!(compare_dump(&good, &d4), vec![u("dumped_not_sampled", 14000, None, "sh")]);
    }

    #[test]
    fn a_process_that_exits_in_the_source_after_the_dump_is_caught() {
        let mut d = parse_dump_log(LOG).unwrap();
        d.remove(&13500);
        let before = sample(&[(12804, 12804, 1, "python3"), (13493, 13493, 2, "chrome"), (13494, 13493, 3, "chrome"), (13600, 13600, 4, "chrome")]);
        assert!(source_after_dump(&before, &d, &before).is_empty());
        let after = sample(&[(12804, 12804, 1, "python3"), (13600, 13600, 4, "chrome")]);
        assert_eq!(source_after_dump(&before, &d, &after), vec![u("exited_after_dump", 13493, None, "chrome")]);
        let mut leaving = before.clone();
        leaving.tasks.get_mut(&13493).unwrap().flags |= PF_EXITING;
        assert_eq!(source_after_dump(&before, &d, &leaving), vec![u("exited_after_dump", 13493, None, "chrome")]);
    }
}
