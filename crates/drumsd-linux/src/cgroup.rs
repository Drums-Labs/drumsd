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
/// Where template servers ran before fix/template-oom (one unprotected cgroup in the pool, oom_score_adj 0): kept for
/// reading old hosts only. New servers run in [`server_cg`].
pub fn template_cg() -> PathBuf {
    pool().join("template")
}

/// Template servers live outside the slots' pool, in a delegated scope of their own top-level slice, one cgroup per
/// server, so their memory protection holds from the root down (drums-lab tmplsrv.py uses the same layout).
pub const TEMPLATE_SLICE: &str = "drums.slice";
pub const TEMPLATE_UNIT: &str = "drums-template.scope";
/// The server is the last thing the OOM killer should take: one server serves every restore of its checkpoint.
pub const TEMPLATE_OOM_SCORE_ADJ: i32 = -900;
/// Every slot's restore (and so every restored task whose image holds 0) runs at this value: slot processes are the
/// preferred OOM victims. Restored tasks can raise it (Chrome's renderers do), not lower it.
pub const SLOT_OOM_SCORE_ADJ: i32 = 300;
const HEADROOM: u64 = 64 << 20;

pub fn template_root() -> PathBuf {
    Path::new(CG_ROOT).join(TEMPLATE_SLICE).join(TEMPLATE_UNIT)
}

/// One cgroup per server, named after its socket path (unique per state dir).
pub fn server_cg(sock: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let base = sock.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let base = base.strip_suffix(".sock").unwrap_or(&base).to_string();
    let h = hex::encode(Sha256::digest(sock.as_os_str().as_encoded_bytes()));
    template_root().join(format!("{base}-{}", &h[..10]))
}

/// The delegated template scope with a keeper (same as tmplsrv.init_root); idempotent.
pub fn template_init() -> io::Result<()> {
    let r = template_root();
    if !r.is_dir() {
        let st = Command::new("systemd-run")
            .args(["--scope", "--quiet", "--unit", TEMPLATE_UNIT, "--slice", TEMPLATE_SLICE, "-p", "Delegate=yes", "--", "setsid", "-f", "sleep", "infinity"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !st.success() {
            return Err(io::Error::other("systemd-run for the template scope failed"));
        }
        let t0 = Instant::now();
        while !r.is_dir() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let keeper = r.join("keeper");
    fs::create_dir_all(&keeper)?;
    for pid in read(&r.join("cgroup.procs"))?.split_whitespace() {
        write(&keeper.join("cgroup.procs"), pid)?;
    }
    let sc = read(&r.join("cgroup.subtree_control"))?;
    if !(sc.contains("memory") && sc.contains("pids")) {
        write(&r.join("cgroup.subtree_control"), "+memory +pids")?;
    }
    Ok(())
}

fn mem_total() -> u64 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| m.lines().find(|l| l.starts_with("MemTotal:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok()))
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

/// Protect a just-built server: memory.min = its memory now + headroom, memory.low = + 2 x headroom.
pub fn protect_server(cg: &Path) -> io::Result<u64> {
    let cur: u64 = read(&cg.join("memory.current"))?.parse().map_err(io::Error::other)?;
    write(&cg.join("memory.min"), &(cur + HEADROOM).to_string())?;
    write(&cg.join("memory.low"), &(cur + 2 * HEADROOM).to_string())?;
    Ok(cur)
}

/// Grant the sum of the servers' protection on the scope and the slice (capped at half the host's memory), so it is
/// effective: cgroup v2 protection only holds as far as every ancestor grants it.
pub fn protect_all() -> io::Result<serde_json::Value> {
    let r = template_root();
    let (mut tmin, mut tlow) = (0u64, 0u64);
    for e in fs::read_dir(&r)?.flatten() {
        if !e.path().is_dir() || e.file_name() == "keeper" {
            continue;
        }
        tmin += read(&e.path().join("memory.min")).ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        tlow += read(&e.path().join("memory.low")).ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    }
    let cap = mem_total() / 2;
    let (pmin, plow) = (tmin.min(cap), tlow.min(cap));
    for unit in [TEMPLATE_SLICE, TEMPLATE_UNIT] {
        let o = Command::new("systemctl").args(["set-property", "--runtime", unit, &format!("MemoryMin={pmin}"), &format!("MemoryLow={plow}")]).output()?;
        if !o.status.success() {
            return Err(io::Error::other(format!("systemctl set-property {unit}: {}", String::from_utf8_lossy(&o.stderr).trim())));
        }
    }
    Ok(serde_json::json!({"servers_min": tmin, "servers_low": tlow, "cap": cap, "capped": tlow > cap}))
}

/// The cgroup's oom_kill counter (hierarchical memory.events: global and cgroup OOM kills of its tasks).
pub fn oom_kills(d: &Path) -> Option<u64> {
    read(&d.join("memory.events")).ok()?.lines().find_map(|l| l.strip_prefix("oom_kill ").and_then(|v| v.trim().parse().ok()))
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

/// The slot limits: `vcpus` CPUs of quota, `memory_mib` of memory and `swap_mib` of swap (0 = none, the default;
/// a VM-equivalent envelope such as 4096 + 3220 needs host swap, e.g. a zram device).
pub fn expected_limits(vcpus: u32, memory_mib: u64, swap_mib: u64) -> Limits {
    Limits { cpu_max: format!("{} 100000", vcpus * 100000), memory_max: (memory_mib * 1024 * 1024).to_string(), swap_max: (swap_mib * 1024 * 1024).to_string() }
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
    // a restore boost that never reached its pre-resume reset (crash) leaves cpu.max raised: put it back; a kill that
    // drumsd did not see through (a restart) can leave a cgroup frozen
    for l in [&d, &a, &b] {
        thaw(l)?;
        if read_limits(l)? != *want {
            set_limits(l, want)?;
        }
    }
    Ok((d, a, b))
}

/// Freeze the tree before it is killed. cgroup.kill sends SIGKILL to one task after another, and a Chrome thread
/// that sees its peer die first (a broken IPC channel) hits a CHECK and dies of SIGTRAP before its own SIGKILL lands:
/// an app crash during a teardown, with its whole tree reaped before anyone can read which cgroup it was in (T6).
/// A frozen task runs no user code, and a fatal signal still ends it. Best effort: the kill does not depend on it.
fn freeze_for_kill(d: &Path) {
    let _ = write(&d.join("cgroup.freeze"), "1");
}

/// Undo [`freeze_for_kill`] once the cgroup is empty: the leaf is the next restore's target, and a frozen cgroup
/// would freeze the restored tree.
pub fn thaw(d: &Path) -> io::Result<()> {
    let f = d.join("cgroup.freeze");
    if f.exists() && read(&f)? != "0" {
        write(&f, "0")?;
    }
    Ok(())
}

/// Send SIGKILL to every process in the cgroup (cgroup.kill) without waiting; the tree is frozen first.
pub fn kill_nowait(d: &Path) -> io::Result<bool> {
    if d.is_dir() && populated(d)? {
        freeze_for_kill(d);
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
    if d.is_dir() {
        thaw(d)?;
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
        freeze_for_kill(d);
        write(&d.join("cgroup.kill"), "1")?;
        killed = true;
    }
    while populated(d)? {
        if t0.elapsed() > timeout {
            return Err(io::Error::other(format!("cgroup {} still populated after cgroup.kill: {:?}", d.display(), procs(d)?)));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    thaw(d)?;
    Ok(KillReport { had_procs: had, killed, seconds: t0.elapsed().as_secs_f64() })
}

pub fn set_cpu_max(d: &Path, v: &str) -> io::Result<()> {
    write(&d.join("cpu.max"), v)
}

/// The cgroup v2 default cpu.weight; slots never set another one except during a boost.
pub const DEFAULT_CPU_WEIGHT: u32 = 100;

pub fn set_cpu_weight(d: &Path, w: u32) -> io::Result<()> {
    write(&d.join("cpu.weight"), &w.clamp(1, 10000).to_string())
}

pub fn cpu_weight(d: &Path) -> Option<u32> {
    read(&d.join("cpu.weight")).ok()?.trim().parse().ok()
}

/// A cpu.weight boost of one cgroup for the life of the guard; dropping it writes the default weight back.
pub struct WeightBoost {
    path: Option<PathBuf>,
}

impl WeightBoost {
    /// weight 0 = no boost (nothing written, nothing put back).
    pub fn new(d: &Path, weight: u32) -> WeightBoost {
        if weight == 0 || weight == DEFAULT_CPU_WEIGHT || !d.is_dir() {
            return WeightBoost { path: None };
        }
        match set_cpu_weight(d, weight) {
            Ok(()) => WeightBoost { path: Some(d.to_path_buf()) },
            Err(_) => WeightBoost { path: None },
        }
    }
    pub fn active(&self) -> bool {
        self.path.is_some()
    }
}

impl Drop for WeightBoost {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            let _ = set_cpu_weight(&p, DEFAULT_CPU_WEIGHT);
        }
    }
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
    fn fake_cg(name: &str, populated: bool, frozen: bool) -> PathBuf {
        let d = std::env::temp_dir().join(format!("drumsd-cg-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("cgroup.events"), format!("populated {}\nfrozen {}\n", populated as u8, frozen as u8)).unwrap();
        fs::write(d.join("cgroup.freeze"), if frozen { "1" } else { "0" }).unwrap();
        fs::write(d.join("cgroup.kill"), "").unwrap();
        fs::write(d.join("cgroup.procs"), "").unwrap();
        d
    }

    #[test]
    fn a_kill_freezes_the_tree_first_so_no_app_sees_its_peers_die() {
        // T6: cgroup.kill SIGKILLs one task after another; a Chrome thread that saw its peer die hit a CHECK and died
        // of SIGTRAP before its own SIGKILL, a crash during teardown that no one could attribute
        let d = fake_cg("kill", true, false);
        assert!(kill_nowait(&d).unwrap());
        assert_eq!(read(&d.join("cgroup.freeze")).unwrap(), "1", "frozen before the kill");
        assert_eq!(read(&d.join("cgroup.kill")).unwrap(), "1");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn an_emptied_leaf_is_thawed_before_anything_is_restored_into_it() {
        // the leaf that was frozen and killed is the next restore's target: a frozen leaf would freeze the new tree
        let d = fake_cg("thaw", false, true);
        wait_empty(&d, Duration::from_secs(1)).unwrap();
        assert_eq!(read(&d.join("cgroup.freeze")).unwrap(), "0");
        let e = fake_cg("thaw2", false, true);
        kill_and_wait(&e, Duration::from_secs(1)).unwrap();
        assert_eq!(read(&e.join("cgroup.freeze")).unwrap(), "0");
        for x in [d, e] {
            let _ = fs::remove_dir_all(&x);
        }
    }
    #[test]
    fn limits_match_launch_py() {
        assert_eq!(expected_limits(4, 4096, 0), Limits { cpu_max: "400000 100000".into(), memory_max: "4294967296".into(), swap_max: "0".into() });
        assert!(slot_cg("rootfs-0734af49-2").ends_with("drumsbench-pool.scope/drumsbench-rootfs-0734af49-2-pool.scope"));
    }

    #[test]
    fn a_weight_boost_is_put_back_to_the_default_on_drop_and_zero_writes_nothing() {
        let d = std::env::temp_dir().join(format!("drumsd-weight-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("cpu.weight"), "100\n").unwrap();
        {
            let b = WeightBoost::new(&d, 10000);
            assert!(b.active());
            assert_eq!(cpu_weight(&d), Some(10000));
        }
        assert_eq!(cpu_weight(&d), Some(DEFAULT_CPU_WEIGHT));
        let b = WeightBoost::new(&d, 0);
        assert!(!b.active());
        drop(b);
        assert_eq!(cpu_weight(&d), Some(100));
        assert!(!WeightBoost::new(&d.join("missing"), 5000).active());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn one_server_cgroup_per_socket_outside_the_pool() {
        let a = server_cg(Path::new("/s1/ps-0.sock"));
        let b = server_cg(Path::new("/s2/ps-0.sock"));
        assert_ne!(a, b);
        assert!(a.starts_with("/sys/fs/cgroup/drums.slice/drums-template.scope"));
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("ps-0-"));
        assert!(!a.starts_with(pool()));
    }
}
