//! Template servers: one `criu restore --template-server SOCK -D IMAGES` per checkpoint, kept warm for the
//! checkpoint's life. Started with the same command line as `fc_drums.serve` (setsid, pidfile next to the socket), so
//! it survives a drumsd restart and is adopted again from its pidfile.
//!
//! One server serves every restore of its checkpoint, so it must be the OOM killer's last choice: it runs in its own
//! cgroup (`drums.slice/drums-template.scope/<name>`, [`cgroup::server_cg`]) with memory.min/low set to its built size,
//! at oom_score_adj [`cgroup::TEMPLATE_OOM_SCORE_ADJ`]. Its forked restores take the client's oom_score_adj (drums-criu
//! pageshare fix) and every client runs at [`cgroup::SLOT_OOM_SCORE_ADJ`], so slot processes are the preferred victims.
//! (Before: the pool's `template` cgroup, no protection, oom_score_adj 0, and the server was the largest process on
//! the host: drums-lab reset-failures 20261030T0218Z and oom-inotify 20261030T0344Z.)

use crate::cgroup;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Server {
    pub socket: PathBuf,
    pub pidfile: PathBuf,
    pub pid: i32,
    pub images: PathBuf,
    pub log: PathBuf,
    pub ready_s: f64,
    #[serde(default)]
    pub cgroup: PathBuf,
    #[serde(default)]
    pub memory_current: u64,
}

pub fn pidfile_for(sock: &Path) -> PathBuf {
    let s = sock.display().to_string();
    PathBuf::from(format!("{}.pid", s.strip_suffix(".sock").unwrap_or(&s)))
}

/// The pid in the pidfile, if it is alive and really a template server for `images`.
pub fn adopt(sock: &Path, images: &Path) -> Option<i32> {
    let pid = server_pid(sock, images)?;
    if exe_replaced(&fs::read_link(format!("/proc/{pid}/exe")).ok()?) {
        return None; // a server of a criu binary that has since been replaced: not adopted, see start()
    }
    crate::restore::socket_exists(sock).then_some(pid)
}

/// The pid in the socket's pidfile, if it is a template server of these images.
fn server_pid(sock: &Path, images: &Path) -> Option<i32> {
    let pid: i32 = fs::read_to_string(pidfile_for(sock)).ok()?.trim().parse().ok()?;
    let cl = crate::procfs::cmdline(pid)?;
    let img = images.display().to_string();
    (cl.join(" ").contains("--template-server") && cl.iter().any(|a| a == &img)).then_some(pid)
}

/// Template servers of this state dir that no live checkpoint owns: their pid is not the pidfile pid of an adopted
/// checkpoint. They come from a daemon killed between starting a capture's server and journaling the checkpoint,
/// or from a server whose socket was taken over; each holds its checkpoint's pages in memory (scale runs: 10 such
/// servers had piled up after 6 drumsd kills). `owned` is the set of adopted server pids.
pub fn orphans(state_dir: &Path, owned: &std::collections::BTreeSet<i32>) -> Vec<(i32, String)> {
    let sd = state_dir.display().to_string();
    let mut out = vec![];
    let Ok(rd) = fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
        let Some(cl) = crate::procfs::cmdline(pid) else { continue };
        // a restore child forked by an owned server has the server's argv: not an orphan
        let ppid = fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|st| st.rsplit(')').next().and_then(|r| r.split_whitespace().nth(1)).and_then(|x| x.parse::<i32>().ok()));
        if ppid.map(|pp| owned.contains(&pp)).unwrap_or(false) {
            continue;
        }
        if is_orphan_server(&cl, &sd, pid, owned) {
            out.push((pid, cl.join(" ").chars().take(200).collect()));
        }
    }
    out
}

pub fn is_orphan_server(cl: &[String], state_dir: &str, pid: i32, owned: &std::collections::BTreeSet<i32>) -> bool {
    let Some(i) = cl.iter().position(|a| a == "--template-server") else { return false };
    let Some(sock) = cl.get(i + 1) else { return false };
    cl.first().map(|a| a.ends_with("criu")).unwrap_or(false) && sock.starts_with(&format!("{state_dir}/")) && !owned.contains(&pid)
}

/// The running binary was replaced on disk (an install upgraded criu): /proc/PID/exe reads "<path> (deleted)".
/// A reinstall from runtime-v0.2 to v0.3 kept the old template server: its restored inits came up at
/// oom_score_adj 0 instead of +300 and the cgroup gate quarantined every env (scale run L128v3).
pub fn exe_replaced(exe: &Path) -> bool {
    exe.to_string_lossy().ends_with(" (deleted)")
}

pub fn start(criu: &Path, sock: &Path, images: &Path, log_dir: &Path, log_level: u8) -> io::Result<Server> {
    cgroup::pool_init()?;
    if let Some(pid) = adopt(sock, images) {
        return Ok(Server {
            socket: sock.into(),
            pidfile: pidfile_for(sock),
            pid,
            images: images.into(),
            log: PathBuf::new(),
            ready_s: 0.0,
            cgroup: PathBuf::new(),
            memory_current: 0,
        });
    }
    if let Some(stale) = server_pid(sock, images) {
        // still running on the replaced binary: stop it before its socket is taken over
        unsafe { libc::kill(stale, libc::SIGKILL) };
    }
    let _ = fs::remove_file(sock);
    let pidf = pidfile_for(sock);
    let _ = fs::remove_file(&pidf);
    let stamp = (crate::now_epoch() * 1000.0) as u64;
    let log = log_dir.join(format!("template-server-{stamp}.log"));
    let out = fs::File::create(log_dir.join(format!("template-server-{stamp}.out")))?;
    cgroup::template_init()?;
    let tcg = cgroup::server_cg(sock);
    fs::create_dir_all(&tcg)?;
    if !cgroup::procs(&tcg)?.is_empty() {
        return Err(io::Error::other(format!("template server cgroup {} still holds processes {:?}", tcg.display(), cgroup::procs(&tcg)?)));
    }
    for f in ["memory.min", "memory.low"] {
        fs::write(tcg.join(f), "0")?;
    }
    let t0 = Instant::now();
    let mut cmd = Command::new(criu);
    cmd.args(["restore", "--template-server"])
        .arg(sock)
        .arg("-D")
        .arg(images)
        .arg("-o")
        .arg(&log)
        .arg(format!("-v{}", log_level.clamp(1, 4)))
        .arg("--pidfile")
        .arg(&pidf);
    cmd.stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out);
    let procs = tcg.join("cgroup.procs");
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            fs::write("/proc/self/oom_score_adj", cgroup::TEMPLATE_OOM_SCORE_ADJ.to_string())?;
            fs::write(&procs, std::process::id().to_string())
        });
    }
    let mut child = cmd.spawn()?;
    loop {
        if crate::restore::socket_exists(sock) && pidf.exists() {
            break;
        }
        if let Some(st) = child.try_wait()? {
            return Err(io::Error::other(format!("template server exited before listening: {st}; see {}", log.display())));
        }
        if t0.elapsed() > Duration::from_secs(120) {
            return Err(io::Error::other(format!("template server did not open {} in 120 s", sock.display())));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid: i32 = fs::read_to_string(&pidf)?.trim().parse().map_err(io::Error::other)?;
    // the server runs on after drumsd: reap it from a thread so it never lingers as our zombie
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    let adj: i32 = fs::read_to_string(format!("/proc/{pid}/oom_score_adj"))?.trim().parse().map_err(io::Error::other)?;
    if adj != cgroup::TEMPLATE_OOM_SCORE_ADJ {
        return Err(io::Error::other(format!("template server {pid} has oom_score_adj {adj}, want {}", cgroup::TEMPLATE_OOM_SCORE_ADJ)));
    }
    let want_cg = format!("/{}", tcg.strip_prefix(cgroup::CG_ROOT).unwrap().display());
    if crate::procfs::cgroup_path(pid).as_deref() != Some(want_cg.as_str()) {
        return Err(io::Error::other(format!("template server {pid} is in {:?}, want {want_cg}", crate::procfs::cgroup_path(pid))));
    }
    let cur = cgroup::protect_server(&tcg)?;
    cgroup::protect_all()?;
    Ok(Server { socket: sock.into(), pidfile: pidf, pid, images: images.into(), log, ready_s: t0.elapsed().as_secs_f64(), cgroup: tcg, memory_current: cur })
}

/// Files any process on the host holds open (from /proc/*/fd).
fn open_files() -> std::collections::BTreeSet<PathBuf> {
    let mut out = std::collections::BTreeSet::new();
    let Ok(rd) = fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        if !e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = fs::read_dir(e.path().join("fd")) else { continue };
        for f in fds.flatten() {
            if let Ok(t) = fs::read_link(f.path()) {
                out.insert(t);
            }
        }
    }
    out
}

/// Template-server logs (`template-server-<ms>.{log,out}`) that drumsd < 0.4.2 wrote into the state dir itself, where
/// nothing removed them when their checkpoint was deleted. Those no process holds open are moved into one new
/// directory `<state_dir>/template-logs-<now_ms>` (same filesystem: a rename) for the caller to pack to disk.
/// Returns (that dir, files, bytes), or None when there was nothing to move.
pub fn collect_loose_logs(state_dir: &Path, now_ms: u64) -> io::Result<Option<(PathBuf, usize, u64)>> {
    let held = open_files();
    let mut loose = vec![];
    for e in fs::read_dir(state_dir)?.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with("template-server-") && (n.ends_with(".log") || n.ends_with(".out")) && e.path().is_file() && !held.contains(&e.path()) {
            loose.push(e.path());
        }
    }
    if loose.is_empty() {
        return Ok(None);
    }
    let dir = state_dir.join(format!("template-logs-{now_ms}"));
    fs::create_dir_all(&dir)?;
    let mut bytes = 0;
    for p in &loose {
        bytes += fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        fs::rename(p, dir.join(p.file_name().unwrap()))?;
    }
    Ok(Some((dir, loose.len(), bytes)))
}

pub fn alive(s: &Server) -> bool {
    adopt(&s.socket, &s.images) == Some(s.pid)
}

pub fn stop(s: &Server) -> io::Result<()> {
    if adopt(&s.socket, &s.images) == Some(s.pid) {
        let fd = crate::pidfd::open(s.pid)?;
        crate::pidfd::send(&fd, libc::SIGTERM)?;
        if !crate::pidfd::wait_exit(&fd, Duration::from_secs(5)) {
            crate::pidfd::send(&fd, libc::SIGKILL)?;
        }
    }
    // its cgroup and its share of the protection go with it
    let cg = cgroup::server_cg(&s.socket);
    if cg.is_dir() {
        let t0 = Instant::now();
        while cgroup::populated(&cg).unwrap_or(false) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = fs::remove_dir(&cg);
        let _ = cgroup::protect_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pidfile_next_to_socket() {
        assert_eq!(pidfile_for(Path::new("/s/ps-0.sock")), PathBuf::from("/s/ps-0.pid"));
        assert_eq!(pidfile_for(Path::new("/s/x")), PathBuf::from("/s/x.pid"));
    }
}

#[cfg(test)]
mod upgrade_tests {
    use super::*;
    #[test]
    fn only_unowned_servers_of_this_state_dir_are_orphans() {
        let cl = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        let owned: std::collections::BTreeSet<i32> = [10].into();
        let srv = cl("/opt/drums/criu/bin/criu restore --template-server /var/lib/drums/state/ps-301.sock -D /var/lib/drums/state/slot301/images");
        assert!(is_orphan_server(&srv, "/var/lib/drums/state", 11, &owned));
        assert!(!is_orphan_server(&srv, "/var/lib/drums/state", 10, &owned), "an adopted server is not an orphan");
        assert!(!is_orphan_server(&srv, "/var/lib/other", 11, &owned), "another state dir is not ours");
        assert!(!is_orphan_server(&cl("/opt/drums/criu/bin/criu restore -D x"), "/var/lib/drums/state", 11, &owned));
    }
    #[test]
    fn loose_template_logs_are_collected_and_open_ones_left() {
        let d = tempfile::tempdir().unwrap();
        for n in ["template-server-1.log", "template-server-1.out", "template-server-2.log"] {
            fs::write(d.path().join(n), vec![b'x'; 100]).unwrap();
        }
        fs::write(d.path().join("other.log"), b"keep").unwrap();
        fs::create_dir(d.path().join("slot100")).unwrap();
        // one log still written by a live process (this test): stays
        let _held = fs::OpenOptions::new().append(true).open(d.path().join("template-server-2.log")).unwrap();
        let (dir, n, bytes) = collect_loose_logs(d.path(), 7).unwrap().unwrap();
        assert_eq!((n, bytes), (2, 200));
        assert_eq!(dir, d.path().join("template-logs-7"));
        assert!(dir.join("template-server-1.out").is_file());
        assert!(d.path().join("template-server-2.log").is_file(), "an open log is not moved");
        assert!(d.path().join("other.log").is_file());
        drop(_held);
        assert_eq!(collect_loose_logs(d.path(), 8).unwrap().map(|x| x.1), Some(1));
        assert!(collect_loose_logs(d.path(), 9).unwrap().is_none(), "nothing left");
    }

    #[test]
    fn a_server_of_a_replaced_criu_binary_is_not_adopted() {
        assert!(exe_replaced(Path::new("/opt/drums/criu/bin/criu (deleted)")));
        assert!(!exe_replaced(Path::new("/opt/drums/criu/bin/criu")));
    }
}
