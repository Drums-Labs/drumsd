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

/// Create the slot cgroup once with the limits; on reuse, prove the limits are unchanged.
pub fn ensure_slot_cg(marker: &str, want: &Limits) -> io::Result<PathBuf> {
    let d = slot_cg(marker);
    if !d.is_dir() {
        fs::create_dir(&d)?;
        write(&d.join("cpu.max"), &want.cpu_max)?;
        write(&d.join("memory.max"), &want.memory_max)?;
        write(&d.join("memory.swap.max"), &want.swap_max)?;
        write(&d.join("pids.max"), "8192")?;
    }
    let got = read_limits(&d)?;
    if &got != want {
        return Err(io::Error::other(format!("slot cgroup {} limits {:?} differ from the configured {:?}", d.display(), got, want)));
    }
    Ok(d)
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
