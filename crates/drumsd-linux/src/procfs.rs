//! Read-only /proc access.

use drumsd_core::locks::ProcFs;
use std::collections::BTreeMap;
use std::fs;
use std::io;

pub struct RealProc;

impl ProcFs for RealProc {
    fn read(&self, path: &str) -> io::Result<String> {
        fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned())
    }
    fn listdir(&self, path: &str) -> io::Result<Vec<String>> {
        Ok(fs::read_dir(path)?.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
    }
    fn readlink(&self, path: &str) -> io::Result<String> {
        Ok(fs::read_link(path)?.to_string_lossy().into_owned())
    }
}

pub fn pids() -> Vec<i32> {
    fs::read_dir("/proc").map(|d| d.filter_map(|e| e.ok()).filter_map(|e| e.file_name().to_str().and_then(|s| s.parse().ok())).collect()).unwrap_or_default()
}

pub fn cmdline(pid: i32) -> Option<Vec<String>> {
    let b = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(b.split(|c| *c == 0).filter(|x| !x.is_empty()).map(|x| String::from_utf8_lossy(x).into_owned()).collect())
}

/// NSpid: the pid in every nested pid namespace, outermost first.
pub fn nspid(pid: i32) -> Option<Vec<String>> {
    let s = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    s.lines().find(|l| l.starts_with("NSpid:")).map(|l| l.split_whitespace().skip(1).map(String::from).collect())
}

/// Field 22 of /proc/PID/stat (start time in clock ticks): with the pid it names one process incarnation.
pub fn start_ticks(pid: i32) -> Option<String> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = s.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19).map(String::from)
}

pub fn state_char(pid: i32) -> Option<char> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    s.rsplit_once(')')?.1.split_whitespace().next()?.chars().next()
}

pub fn comm(pid: i32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim_end_matches('\n').to_string())
}

pub fn ns_inode(pid: i32, kind: &str) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/ns/{kind}")).ok().map(|p| p.to_string_lossy().into_owned())
}

/// Whether `pid`'s pid namespace is the namespace `ns` (a `pid:[N]` link) or nested below it. Sandboxed apps (Chrome,
/// Electron) run helpers in pid namespaces they create inside the slot's; those are still the slot's processes. Walks
/// up with NS_GET_PARENT; reaching the top (or a namespace we may not see) without meeting `ns` means outside.
pub fn pidns_within(pid: i32, ns: &str) -> bool {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::MetadataExt;
    let Some(want) = ns.strip_prefix("pid:[").and_then(|x| x.strip_suffix(']')).and_then(|x| x.parse::<u64>().ok()) else {
        return false;
    };
    let Ok(f) = fs::File::open(format!("/proc/{pid}/ns/pid")) else {
        return false;
    };
    let mut fd: OwnedFd = f.into();
    const NS_GET_PARENT: libc::c_ulong = 0xb702; // _IO(0xb7, 0x2)
    for _ in 0..40 {
        let ino = match fs::File::from(fd.try_clone().unwrap()).metadata() {
            Ok(m) => m.ino(),
            Err(_) => return false,
        };
        if ino == want {
            return true;
        }
        let p = unsafe { libc::ioctl(fd.as_raw_fd(), NS_GET_PARENT as _) };
        if p < 0 {
            return false;
        }
        fd = unsafe { OwnedFd::from_raw_fd(p) };
    }
    false
}

/// The unified (cgroup v2) path of a process, e.g. `/system.slice/drumsbench-pool.scope/drumsbench-x-pool.scope`.
pub fn cgroup_path(pid: i32) -> Option<String> {
    let s = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let v: Vec<&str> = s.lines().filter(|l| l.starts_with("0::")).collect();
    if v.len() == 1 {
        Some(v[0][3..].to_string())
    } else {
        None
    }
}

/// `-m <module> --marker <marker>` appear contiguously in argv (runtime.tree_argv_matches).
pub fn argv_has_marker(argv: &[String], module: &str, marker: &str) -> bool {
    argv.windows(4).any(|w| w[0] == "-m" && w[1] == module && w[2] == "--marker" && w[3] == marker)
}

/// Every user-space process in `init`'s pid namespace as {namespace pid: comm}, zombies excluded
/// (capture.py::ns_procs).
pub fn ns_procs(init: i32) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(want) = ns_inode(init, "pid") else { return out };
    for p in pids() {
        if ns_inode(p, "pid").as_deref() != Some(want.as_str()) {
            continue;
        }
        if matches!(state_char(p), Some('Z') | Some('X') | None) {
            continue;
        }
        if let (Some(ns), Some(c)) = (nspid(p), comm(p)) {
            if let Some(last) = ns.last() {
                out.insert(last.clone(), c);
            }
        }
    }
    out
}

/// One /proc pass: the host pids in `init`'s pid namespace (all states) and {namespace pid: comm} of the live ones.
pub fn ns_members_and_procs(init: i32) -> (Vec<i64>, BTreeMap<String, String>) {
    let mut members = vec![];
    let mut procs = BTreeMap::new();
    let Some(want) = ns_inode(init, "pid") else { return (members, procs) };
    for p in pids() {
        if ns_inode(p, "pid").as_deref() != Some(want.as_str()) {
            continue;
        }
        members.push(p as i64);
        if matches!(state_char(p), Some('Z') | Some('X') | None) {
            continue;
        }
        if let (Some(ns), Some(c)) = (nspid(p), comm(p)) {
            if let Some(last) = ns.last() {
                procs.insert(last.clone(), c);
            }
        }
    }
    members.sort();
    (members, procs)
}

#[cfg(test)]
mod tests {
    #[test]
    fn nested_pid_namespace_is_within_its_parent() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: needs root");
            return;
        }
        let me = std::process::id() as i32;
        let mine = super::ns_inode(me, "pid").unwrap();
        assert!(super::pidns_within(me, &mine));
        let mut c = std::process::Command::new("unshare").args(["--pid", "--fork", "sleep", "5"]).spawn().unwrap();
        let mut kid = None;
        for _ in 0..200 {
            if let Ok(s) = std::fs::read_to_string(format!("/proc/{}/task/{}/children", c.id(), c.id())) {
                if let Some(k) = s.split_whitespace().next() {
                    kid = k.parse::<i32>().ok();
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let kid = kid.expect("unshare child");
        let inner = super::ns_inode(kid, "pid").unwrap();
        assert_ne!(inner, mine);
        assert!(super::pidns_within(kid, &mine), "nested ns is within ours");
        assert!(!super::pidns_within(me, &inner), "ours is not within the nested one");
        assert!(!super::pidns_within(me, "pid:[1]"));
        let _ = c.kill();
        let _ = c.wait();
    }

    use super::*;

    #[test]
    fn reads_this_process() {
        let me = std::process::id() as i32;
        assert!(pids().contains(&me));
        assert!(start_ticks(me).is_some());
        assert!(nspid(me).is_some());
        assert!(comm(me).is_some());
        assert!(ns_inode(me, "pid").unwrap().starts_with("pid:["));
    }

    #[test]
    fn marker_match_is_exact() {
        let a: Vec<String> = ["python3", "-m", "sharedos_rootfs.tree", "--marker", "rootfs-ab-1", "--plan", "p"].iter().map(|s| s.to_string()).collect();
        assert!(argv_has_marker(&a, "sharedos_rootfs.tree", "rootfs-ab-1"));
        assert!(!argv_has_marker(&a, "sharedos_rootfs.tree", "rootfs-ab-10"));
        assert!(!argv_has_marker(&a, "sharedos_rootfs.proxy", "rootfs-ab-1"));
    }
}
