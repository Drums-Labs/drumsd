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
    let mut stray = vec![];
    for p in procfs::pids() {
        if procfs::ns_inode(p, "pid") == ns && procfs::cgroup_path(p).as_deref() != Some(rel.as_str()) && procfs::state_char(p) != Some('Z') {
            stray.push(p);
        }
    }
    let init_cg = procfs::cgroup_path(pid);
    let ok = got.as_ref().map(|g| g == want).unwrap_or(false) && init_cg.as_deref() == Some(rel.as_str()) && stray.is_empty();
    let d = json!({"cgroup": rel, "init_cgroup": init_cg, "limits": got.as_ref().map_err(|e| e.to_string()), "want": want, "members": members.len(), "outside_cgroup": stray});
    if ok {
        GateResult::pass(Gate::Cgroup, secs(t0), d)
    } else {
        GateResult::fail(Gate::Cgroup, secs(t0), d)
    }
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
