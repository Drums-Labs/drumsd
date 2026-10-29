//! Slot cgroups: one persistent cgroup per slot inside the delegated pool scope, created once with the configured
//! limits and reused by every restore (pageshare.py's layout, so the Python verbs recognise it). A reset empties it
//! with `cgroup.kill` and waits for `cgroup.events` to report `populated 0`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const CG_ROOT: &str = "/sys/fs/cgroup";
pub const POOL_UNIT: &str = "drumsbench-pool.scope";

pub fn pool() -> PathBuf {
    Path::new(CG_ROOT).join("system.slice").join(POOL_UNIT)
}
pub fn template_cg() -> PathBuf {
    pool().join("template")
}
pub fn slot_cg(marker: &str) -> PathBuf {
    pool().join(format!("drumsbench-{marker}-pool.scope"))
}
/// The two reusable leaves of a slot cgroup. A reset kills the tree in one leaf and restores into the other at
/// once; the killed leaf drains in the background and is reused at the reset after. The names match the Python
/// runtime's scope pattern (`drumsbench-<marker>-r<N>.scope`), so its tools still recognise the tree.
pub fn slot_leaf(marker: &str, leaf: u8) -> PathBuf {
    slot_cg(marker).join(format!("drumsbench-{marker}-r{leaf}.scope"))
}

fn read(p: &Path) -> io::Result<String> {
    Ok(fs::read_to_string(p)?.trim().to_string())
}
fn write(p: &Path, v: &str) -> io::Result<()> {
    fs::write(p, v)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Limits {
    pub cpu_max: String,
    pub memory_max: String,
    pub swap_max: String,
}

pub fn expected_limits(vcpus: u32, memory_mib: u64) -> Limits {
    Limits { cpu_max: format!("{} 100000", vcpus * 100000), memory_max: (memory_mib * 1024 * 1024).to_string(), swap_max: "0".into() }
}

/// The delegated pool scope with a keeper process (same as pageshare.pool_init); idempotent.
pub fn pool_init() -> io::Result<()> {
    let p = pool();
    if !p.is_dir() {
        let st = Command::new("systemd-run")
            .args(["--scope", "--quiet", "--unit", POOL_UNIT, "-p", "Delegate=yes", "--", "setsid", "-f", "sleep", "infinity"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !st.success() {
            return Err(io::Error::other("systemd-run for the pool scope failed"));
        }
        let t0 = Instant::now();
        while !p.is_dir() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let keeper = p.join("keeper");
    fs::create_dir_all(&keeper)?;
    for pid in read(&p.join("cgroup.procs"))?.split_whitespace() {
        write(&keeper.join("cgroup.procs"), pid)?;
    }
    let sc = read(&p.join("cgroup.subtree_control"))?;
    if !(sc.contains("cpu") && sc.contains("memory") && sc.contains("pids")) {
        write(&p.join("cgroup.subtree_control"), "+cpu +memory +pids")?;
    }
    fs::create_dir_all(template_cg())?;
    Ok(())
}

pub fn read_limits(d: &Path) -> io::Result<Limits> {
    Ok(Limits { cpu_max: read(&d.join("cpu.max"))?, memory_max: read(&d.join("memory.max"))?, swap_max: read(&d.join("memory.swap.max"))? })
}

fn set_limits(d: &Path, want: &Limits) -> io::Result<()> {
    write(&d.join("cpu.max"), &want.cpu_max)?;
    write(&d.join("memory.max"), &want.memory_max)?;
    write(&d.join("memory.swap.max"), &want.swap_max)?;
    write(&d.join("pids.max"), "8192")
}

/// Create the slot cgroup once with the limits; on reuse, prove the limits are unchanged.
pub fn ensure_slot_cg(marker: &str, want: &Limits) -> io::Result<PathBuf> {
    let d = slot_cg(marker);
    if !d.is_dir() {
        fs::create_dir(&d)?;
        set_limits(&d, want)?;
    }
    if read_limits(&d)? != *want {
        set_limits(&d, want)?;
    }
    Ok(d)
}

/// The slot cgroup (limits for the whole slot, both leaves together) with its two leaves (the same limits again, so
/// the restored tree's own cgroup reads back the configured values). Created once; a slot cgroup that still holds
/// processes directly (the single-cgroup layout) is emptied first.
pub fn ensure_slot_leaves(marker: &str, want: &Limits) -> io::Result<(PathBuf, PathBuf, PathBuf)> {
    let d = ensure_slot_cg(marker, want)?;
    let (a, b) = (slot_leaf(marker, 1), slot_leaf(marker, 2));
    if !(a.is_dir() && b.is_dir()) {
        if !procs(&d)?.is_empty() {
            kill_and_wait(&d, Duration::from_secs(20))?;
        }
        let sc = read(&d.join("cgroup.subtree_control"))?;
        if !(sc.contains("cpu") && sc.contains("memory") && sc.contains("pids")) {
            write(&d.join("cgroup.subtree_control"), "+cpu +memory +pids")?;
        }
        for l in [&a, &b] {
            if !l.is_dir() {
                fs::create_dir(l)?;
            }
            set_limits(l, want)?;
        }
    }
    // a restore boost that never reached its pre-resume reset (crash) leaves cpu.max raised: put it back
    for l in [&d, &a, &b] {
        if read_limits(l)? != *want {
            set_limits(l, want)?;
        }
    }
    Ok((d, a, b))
}

/// Send SIGKILL to every process in the cgroup (cgroup.kill) without waiting.
pub fn kill_nowait(d: &Path) -> io::Result<bool> {
    if d.is_dir() && populated(d)? {
        write(&d.join("cgroup.kill"), "1")?;
        return Ok(true);
    }
    Ok(false)
}

/// Wait until the cgroup is empty (it was killed earlier); kill again if something is still there at the end.
pub fn wait_empty(d: &Path, timeout: Duration) -> io::Result<f64> {
    let t0 = Instant::now();
    while d.is_dir() && populated(d)? {
        if t0.elapsed() > timeout {
            return kill_and_wait(d, Duration::from_secs(5)).map(|r| r.seconds + t0.elapsed().as_secs_f64());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(t0.elapsed().as_secs_f64())
}

pub fn procs(d: &Path) -> io::Result<Vec<i32>> {
    Ok(read(&d.join("cgroup.procs"))?.split_whitespace().filter_map(|x| x.parse().ok()).collect())
}

pub fn populated(d: &Path) -> io::Result<bool> {
    let ev = read(&d.join("cgroup.events"))?;
    Ok(ev.lines().any(|l| l.trim() == "populated 1"))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KillReport {
    pub had_procs: usize,
    pub killed: bool,
    pub seconds: f64,
}

/// Kill everything in the cgroup and wait until it is empty (`populated 0`). Fails closed after `timeout`.
pub fn kill_and_wait(d: &Path, timeout: Duration) -> io::Result<KillReport> {
    let t0 = Instant::now();
    if !d.is_dir() {
        return Ok(KillReport { had_procs: 0, killed: false, seconds: 0.0 });
    }
    let had = procs(d)?.len();
    let mut killed = false;
    if populated(d)? {
        write(&d.join("cgroup.kill"), "1")?;
        killed = true;
    }
    while populated(d)? {
        if t0.elapsed() > timeout {
            return Err(io::Error::other(format!("cgroup {} still populated after cgroup.kill: {:?}", d.display(), procs(d)?)));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(KillReport { had_procs: had, killed, seconds: t0.elapsed().as_secs_f64() })
}

pub fn set_cpu_max(d: &Path, v: &str) -> io::Result<()> {
    write(&d.join("cpu.max"), v)
}

pub fn join(d: &Path, pid: i32) -> io::Result<()> {
    write(&d.join("cgroup.procs"), &pid.to_string())
}

pub fn memory_current(d: &Path) -> Option<u64> {
    read(&d.join("memory.current")).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn limits_match_launch_py() {
        assert_eq!(
            expected_limits(4, 4096),
            Limits { cpu_max: "400000 100000".into(), memory_max: "4294967296".into(), swap_max: "0".into() }
        );
        assert!(slot_cg("rootfs-0734af49-2").ends_with("drumsbench-pool.scope/drumsbench-rootfs-0734af49-2-pool.scope"));
    }
}
