//! Gate checks against the kernel. Each returns a [`GateResult`]; none of them changes anything.

use crate::cgroup;
use crate::procfs;
use drumsd_core::gates::{Gate, GateResult};
use drumsd_core::{locks, mounts, net};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

fn secs(t0: Instant) -> f64 {
    t0.elapsed().as_secs_f64()
}

/// `candidate._lock_gates`: (1) filelocks.img unchanged and the restore applied exactly the dumped locks;
/// (2) the pre-resume census equals the dumped set.
pub fn lock_gates(slot_dir: &Path, images: &Path, hook_dir: &Path, restore_log_text: &str) -> (GateResult, GateResult) {
    let t0 = Instant::now();
    let rec: Value = match fs::read(slot_dir.join("locks-dumped.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        Some(v) => v,
        None => {
            let d = json!({"error": "locks-dumped.json missing or unreadable"});
            return (GateResult::fail(Gate::LockApplied, secs(t0), d.clone()), GateResult::fail(Gate::LockPreResume, 0.0, d));
        }
    };
    let dumped: Vec<locks::Entry> = serde_json::from_value(rec["dumped"].clone()).unwrap_or_default();
    let img = images.join("filelocks.img");
    let now_sha = fs::read(&img).ok().map(|b| hex::encode(Sha256::digest(&b)));
    let want_sha = rec["filelocks_img_sha256"].as_str().map(String::from);
    let applied = locks::applied_from_log(restore_log_text);
    let a = if now_sha != want_sha {
        GateResult::fail(Gate::LockApplied, secs(t0), json!({"error": "filelocks.img changed between the dump and the restore", "now": now_sha, "dumped": want_sha}))
    } else if !locks::same_entries(&applied, &dumped) {
        GateResult::fail(Gate::LockApplied, secs(t0), json!({"applied": applied, "dumped": dumped}))
    } else {
        GateResult::pass(Gate::LockApplied, secs(t0), json!({"n": dumped.len()}))
    };
    let t1 = Instant::now();
    let hook: Option<Value> = fs::read(hook_dir.join("locks-pre-resume.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let b = match hook {
        None => GateResult::fail(Gate::LockPreResume, secs(t1), json!({"error": "CRIU's pre-resume action left no readable census"})),
        Some(h) if h.get("error").is_some() => GateResult::fail(Gate::LockPreResume, secs(t1), json!({"error": h["error"]})),
        Some(h) => {
            let got: BTreeSet<locks::Shape> = serde_json::from_value::<Vec<locks::Shape>>(h["shape"].clone()).unwrap_or_default().into_iter().collect();
            let want: BTreeSet<locks::Shape> = locks::shape_of_dump(&dumped);
            if got == want {
                GateResult::pass(Gate::LockPreResume, secs(t1), json!({"shape": want, "tree_n": h["tree_pids"].as_array().map(|a| a.len())}))
            } else {
                GateResult::fail(Gate::LockPreResume, secs(t1), json!({"pre_resume": got, "dumped": want}))
            }
        }
    };
    (a, b)
}

/// Exactly one tree init with the checkpoint's argv marker in the slot's cgroup, pid 1 of its own pid namespace,
/// and it is the pid CRIU wrote to the pidfile.
pub fn init_identity(pid: i32, argv_marker: &str, slot_cg_rel: &str) -> GateResult {
    let t0 = Instant::now();
    let mut found = vec![];
    for p in procfs::pids() {
        if procfs::cgroup_path(p).as_deref() != Some(slot_cg_rel) {
            continue;
        }
        if let Some(a) = procfs::cmdline(p) {
            if procfs::argv_has_marker(&a, "sharedos_rootfs.tree", argv_marker) && procfs::nspid(p).map(|n| n.len() >= 2 && n.last().unwrap() == "1").unwrap_or(false) {
                found.push(p);
            }
        }
    }
    let d = json!({"pidfile_pid": pid, "inits": found, "start_ticks": procfs::start_ticks(pid)});
    if found == vec![pid] {
        GateResult::pass(Gate::InitIdentity, secs(t0), d)
    } else {
        GateResult::fail(Gate::InitIdentity, secs(t0), d)
    }
}

/// What one read of a process of the slot's pid namespace says about its cgroup membership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Membership {
    /// In the slot's cgroup, or gone from /proc (it exited between the pid listing and the read: it is in no
    /// cgroup now), or dead (`X`) or a zombie (`Z`), which hold no memory and no CPU the limits could bound.
    Fine,
    /// Still alive and in another cgroup: a stray.
    Stray,
}

/// The verdict on one process from what /proc said: `cgroup` its `0::` path (None when the read failed or the file
/// is gone), `state` its state letter (None when gone), `rel` the slot cgroup's path under the cgroup root.
///
/// A read that fails is not a stray. Before this, `cgroup_path(p) != Some(rel)` was true for a process that had
/// exited between the pid listing and the read (the read returns None), and `state_char(p) != Some('Z')` was true
/// for the same reason, so a short-lived process of the restored desktop (a shell of a panel plugin, a hook) that
/// ended during the scan counted as outside the cgroup and the env was quarantined. That is the likeliest reading of
/// "gate cgroup failed" on one env of 256 (T1-fixed host B s251; R2-3 Firecracker host B s201-2): the init was in
/// the right cgroup and had the right oom_score_adj, the limits are the same for every env, and the field that named
/// the failed check was cut off the reason (fixed alongside: `failed` comes first now). The scan over every pid of a
/// 256-env host is slow enough under load for the window to matter.
pub fn membership(cgroup: Option<&str>, state: Option<char>, rel: &str) -> Membership {
    match (cgroup, state) {
        (_, None) | (None, _) => Membership::Fine,
        (_, Some('Z')) | (_, Some('X')) => Membership::Fine,
        (Some(c), _) if c == rel => Membership::Fine,
        _ => Membership::Stray,
    }
}

/// The processes of the slot's pid namespace that are alive and outside its cgroup, each one read twice: a stray
/// of the first pass is looked at again up to `settle` later (`again` reads it anew), and only one that is still a
/// stray then counts. A process moving between cgroups (CRIU restoring a late thread group) or exiting is not an
/// escape.
pub fn confirmed_strays(first: Vec<i32>, settle: std::time::Duration, again: impl Fn(i32) -> Membership) -> Vec<i32> {
    if first.is_empty() {
        return first;
    }
    std::thread::sleep(settle);
    first.into_iter().filter(|p| again(*p) == Membership::Stray).collect()
}

/// `launch.verify_cgroup`: the tree's cgroup is the slot's, the limits are the configured ones, every process of the
/// pid namespace is in it.
pub fn cgroup_gate(pid: i32, tree_cg: &Path, limits_cg: &Path, want: &cgroup::Limits) -> GateResult {
    let t0 = Instant::now();
    let rel = tree_cg.strip_prefix(cgroup::CG_ROOT).map(|p| format!("/{}", p.display())).unwrap_or_default();
    let got = cgroup::read_limits(tree_cg).and_then(|leaf| if limits_cg == tree_cg { Ok(leaf) } else {
        let slot = cgroup::read_limits(limits_cg)?;
        if slot == leaf { Ok(leaf) } else { Err(std::io::Error::other(format!("slot cgroup limits {slot:?} != tree cgroup limits {leaf:?}"))) }
    });
    let members = procfs::ns_procs(pid);
    let ns = procfs::ns_inode(pid, "pid");
    let read = |p: i32| membership(procfs::cgroup_path(p).as_deref(), procfs::state_char(p), &rel);
    let mut stray = vec![];
    for p in procfs::pids() {
        if procfs::ns_inode(p, "pid") == ns && read(p) == Membership::Stray {
            stray.push(p);
        }
    }
    // a second read, 50 ms later, of each candidate: it must still be in the namespace and still outside the cgroup
    let stray = confirmed_strays(stray, std::time::Duration::from_millis(50), |p| if procfs::ns_inode(p, "pid") == ns { read(p) } else { Membership::Fine });
    let stray_comm: Vec<(i32, Option<String>, Option<String>)> = stray.iter().map(|p| (*p, procfs::comm(*p), procfs::cgroup_path(*p))).collect();
    let init_cg = procfs::cgroup_path(pid);
    let (init_adj, negative) = oom_priority(pid, &procfs::ns_members_and_procs(pid).0);
    let oom_ok = init_adj == Some(cgroup::SLOT_OOM_SCORE_ADJ) && negative.is_empty();
    let limits_ok = got.as_ref().map(|g| g == want).unwrap_or(false);
    let init_ok = init_cg.as_deref() == Some(rel.as_str());
    let ok = limits_ok && init_ok && stray.is_empty() && oom_ok;
    // `failed` names the checks that failed, first, so a reason cut short still says which one it was
    let failed: Vec<&str> = [(!limits_ok, "limits"), (!init_ok, "init_cgroup"), (!stray.is_empty(), "outside_cgroup"), (!oom_ok, "oom_score_adj")]
        .iter().filter(|(f, _)| *f).map(|(_, n)| *n).collect();
    let d = json!({"failed": failed, "cgroup": rel, "init_cgroup": init_cg, "limits": got.as_ref().map_err(|e| e.to_string()), "want": want, "members": members.len(),
                   "outside_cgroup": stray, "outside_cgroup_procs": stray_comm,
                   "init_oom_score_adj": init_adj, "want_oom_score_adj": cgroup::SLOT_OOM_SCORE_ADJ, "negative_oom_score_adj": negative});
    if ok {
        GateResult::pass(Gate::Cgroup, secs(t0), d)
    } else {
        GateResult::fail(Gate::Cgroup, secs(t0), d)
    }
}

/// The slot's OOM priority after a restore: the init (0 in every image) must have the client's SLOT_OOM_SCORE_ADJ, and no
/// process of the slot may be below 0. A restore whose tasks inherited the template server's -900 (a CRIU without the
/// pageshare oom fix) would protect the slot over the server: it fails the cgroup gate.
fn oom_priority(pid: i32, host_pids: &[i64]) -> (Option<i32>, Vec<(i64, i32)>) {
    let adj = |p: i64| fs::read_to_string(format!("/proc/{p}/oom_score_adj")).ok().and_then(|v| v.trim().parse::<i32>().ok());
    let neg = host_pids.iter().filter_map(|&p| adj(p).filter(|v| *v < 0).map(|v| (p, v))).collect();
    (adj(pid as i64), neg)
}

pub fn mount_gate(pid: i32, slot_dir: &Path) -> GateResult {
    let t0 = Instant::now();
    let want: Option<Vec<mounts::MountRow>> = fs::read(slot_dir.join("mounts-pre-dump.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let got = fs::read_to_string(format!("/proc/{pid}/mountinfo")).map_err(|e| e.to_string()).and_then(|t| mounts::summary(&t));
    match (want, got) {
        (Some(w), Ok(g)) => {
            let (missing, extra) = mounts::diff(&w, &g);
            if missing.is_empty() && extra.is_empty() {
                GateResult::pass(Gate::MountTable, secs(t0), json!({"n": g.len()}))
            } else {
                GateResult::fail(Gate::MountTable, secs(t0), json!({"missing": missing, "extra": extra}))
            }
        }
        (w, g) => GateResult::fail(Gate::MountTable, secs(t0), json!({"error": "unreadable", "want_ok": w.is_some(), "got": g.err()})),
    }
}

fn ip_json(pid: i32, what: &str) -> Result<Value, String> {
    let o = Command::new("nsenter").args(["-t", &pid.to_string(), "-n", "--", "ip", "-j", what, "show"]).output().map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err(format!("ip -j {what} show: rc {:?} {}", o.status.code(), String::from_utf8_lossy(&o.stderr)));
    }
    let s = String::from_utf8_lossy(&o.stdout);
    serde_json::from_str(if s.trim().is_empty() { "[]" } else { s.trim() }).map_err(|e| format!("ip -j {what}: {e}"))
}

/// The restored network namespace equals the checkpointed one (three `ip -j` readbacks run in parallel).
pub fn netns_gate(pid: i32, slot_dir: &Path) -> GateResult {
    let t0 = Instant::now();
    if procfs::ns_inode(pid, "net") == procfs::ns_inode(std::process::id() as i32, "net") {
        return GateResult::fail(Gate::Netns, secs(t0), json!({"error": "init is in the host network namespace"}));
    }
    let (l, a, r) = std::thread::scope(|s| {
        let l = s.spawn(|| ip_json(pid, "link"));
        let a = s.spawn(|| ip_json(pid, "addr"));
        let r = s.spawn(|| ip_json(pid, "route"));
        (l.join().unwrap(), a.join().unwrap(), r.join().unwrap())
    });
    let want: Option<Value> = fs::read(slot_dir.join("netstate-pre-dump.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
    match (l, a, r, want) {
        (Ok(l), Ok(a), Ok(r), Some(w)) => {
            let got = net::normalize(&l, &a, &r);
            let d = net::diff(&w, &got);
            if d.is_empty() {
                GateResult::pass(Gate::Netns, secs(t0), json!({"links": got["links"].as_object().map(|o| o.len())}))
            } else {
                GateResult::fail(Gate::Netns, secs(t0), json!({"diff": d}))
            }
        }
        (l, a, r, w) => GateResult::fail(
            Gate::Netns,
            secs(t0),
            json!({"error": [l.err(), a.err(), r.err()], "checkpoint_record": w.is_some()}),
        ),
    }
}

/// The processes of the slot cgroup outside its pid namespace that still count, and whether to look again.
/// A pid that has left /proc exited between the cgroup listing and the namespace check: it is in no namespace now
/// and is dropped (before, it was reported as foreign with comm None, failing a good reset under load). A process
/// still there whose comm cannot be read is exiting (or unreadable), like the template server's forked `criu` that
/// reports and exits: look again for up to 200 ms; if it is still there, it fails the gate.
pub fn foreign_now(foreign: Vec<(i32, Option<String>)>, exists: impl Fn(i32) -> bool) -> (Vec<(i32, Option<String>)>, bool) {
    let still: Vec<(i32, Option<String>)> = foreign.into_iter().filter(|(p, _)| exists(*p)).collect();
    let wait = !still.is_empty() && still.iter().all(|(_, c)| c.is_none() || c.as_deref() == Some("criu"));
    (still, wait)
}

pub const NS_KINDS: [&str; 5] = ["pid", "mnt", "net", "ipc", "uts"];

/// Namespaces distinct from the host and from every other live slot's init; no process of another slot or of the
/// host inside this slot's cgroup.
pub fn isolation_gate(pid: i32, others: &[i32], slot_cg: &Path) -> GateResult {
    let t0 = Instant::now();
    let mut problems = vec![];
    let mine: Vec<(String, Option<String>)> = NS_KINDS.iter().map(|k| (k.to_string(), procfs::ns_inode(pid, k))).collect();
    for (k, v) in &mine {
        if v.is_none() {
            problems.push(format!("{k} namespace of init {pid} unreadable"));
        } else if procfs::ns_inode(1, k) == *v {
            problems.push(format!("{k} namespace is the host's"));
        }
        for o in others {
            if *o != pid && procfs::ns_inode(*o, k) == *v {
                problems.push(format!("{k} namespace shared with slot init {o}"));
            }
        }
    }
    let pidns = procfs::ns_inode(pid, "pid");
    // the template server's forked criu reports the result and then exits: give an exiting `criu` up to 200 ms
    let mut tries = 0;
    loop {
        let Ok(ps) = cgroup::procs(slot_cg) else {
            problems.push("slot cgroup unreadable".into());
            break;
        };
        let foreign: Vec<(i32, Option<String>)> =
            ps.into_iter().filter(|p| !pidns.as_deref().map(|ns| procfs::pidns_within(*p, ns)).unwrap_or(false)).map(|p| (p, procfs::comm(p))).collect();
        let (foreign, wait) = foreign_now(foreign, |p| std::path::Path::new(&format!("/proc/{p}")).exists());
        if wait && tries < 40 {
            tries += 1;
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }
        for (p, c) in foreign {
            problems.push(format!("process {p} ({c:?}) in the slot cgroup is outside the slot's pid namespace (and not in one nested below it)"));
        }
        break;
    }
    let d = json!({"namespaces": mine.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<std::collections::BTreeMap<_, _>>(), "others": others.len(), "problems": problems});
    if problems.is_empty() {
        GateResult::pass(Gate::Isolation, secs(t0), d)
    } else {
        GateResult::fail(Gate::Isolation, secs(t0), d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_that_exited_mid_check_is_not_foreign() {
        // L32a: "process 731070 (None) in the slot cgroup is outside the slot's pid namespace" on a good reset; the
        // pid had exited between the cgroup listing and the namespace read
        let (f, wait) = foreign_now(vec![(731070, None)], |_| false);
        assert!(f.is_empty() && !wait);
    }

    #[test]
    fn a_process_that_exited_mid_scan_is_not_outside_the_cgroup() {
        // T1-fixed host B s251 and R2-3 host B s201-2: "gate cgroup failed" with the init in the right cgroup; the
        // membership scan had counted a process whose /proc entries were gone by the time they were read
        let rel = "/system.slice/drumsbench-pool.scope/drumsbench-rootfs-76bafd47-201-pool.scope/drumsbench-rootfs-76bafd47-201-r2.scope";
        assert_eq!(membership(None, None, rel), Membership::Fine, "gone between the listing and the read");
        assert_eq!(membership(Some(rel), None, rel), Membership::Fine, "gone during the state read");
        assert_eq!(membership(None, Some('R'), rel), Membership::Fine, "the cgroup file could not be read: nothing was observed wrong");
        assert_eq!(membership(Some("/system.slice/other.scope"), Some('Z'), rel), Membership::Fine, "a zombie holds nothing the limits bound");
        assert_eq!(membership(Some("/system.slice/other.scope"), Some('X'), rel), Membership::Fine, "dead");
        assert_eq!(membership(Some(rel), Some('S'), rel), Membership::Fine);
        assert_eq!(membership(Some("/system.slice/other.scope"), Some('S'), rel), Membership::Stray, "alive and elsewhere: a real stray");
        assert_eq!(membership(Some("/"), Some('R'), rel), Membership::Stray, "the root cgroup is not the slot's");
    }

    #[test]
    fn a_stray_counts_only_when_the_second_read_agrees() {
        use std::cell::Cell;
        let no_wait = std::time::Duration::from_millis(0);
        assert!(confirmed_strays(vec![], no_wait, |_| Membership::Stray).is_empty());
        // 10 is gone or moved by the second read; 11 is still outside
        let v = confirmed_strays(vec![10, 11], no_wait, |p| if p == 11 { Membership::Stray } else { Membership::Fine });
        assert_eq!(v, vec![11]);
        // the second read happens after the settle time, once per candidate
        let n = Cell::new(0);
        let t0 = Instant::now();
        let v = confirmed_strays(vec![1, 2, 3], std::time::Duration::from_millis(30), |_| { n.set(n.get() + 1); Membership::Stray });
        assert_eq!((v, n.get()), (vec![1, 2, 3], 3));
        assert!(t0.elapsed() >= std::time::Duration::from_millis(30));
    }

    #[test]
    fn a_live_foreign_process_still_fails_and_an_exiting_one_is_looked_at_again() {
        let (f, wait) = foreign_now(vec![(10, Some("bash".into())), (11, None)], |_| true);
        assert_eq!(f.len(), 2);
        assert!(!wait, "a named foreign process fails at once");
        let (f, wait) = foreign_now(vec![(11, None), (12, Some("criu".into()))], |_| true);
        assert_eq!(f.len(), 2);
        assert!(wait, "unreadable or criu: look again (bounded by the caller), then fail");
        let (f, _) = foreign_now(vec![(10, Some("bash".into())), (11, None)], |p| p == 10);
        assert_eq!(f, vec![(10, Some("bash".to_string()))]);
    }
}
