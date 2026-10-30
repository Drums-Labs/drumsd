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
    let pid: i32 = fs::read_to_string(pidfile_for(sock)).ok()?.trim().parse().ok()?;
    let cl = crate::procfs::cmdline(pid)?;
    let joined = cl.join(" ");
    let img = images.display().to_string();
    if joined.contains("--template-server") && cl.iter().any(|a| a == &img) && crate::restore::socket_exists(sock) {
        Some(pid)
    } else {
        None
    }
}

pub fn start(criu: &Path, sock: &Path, images: &Path, log_dir: &Path) -> io::Result<Server> {
    cgroup::pool_init()?;
    if let Some(pid) = adopt(sock, images) {
        return Ok(Server { socket: sock.into(), pidfile: pidfile_for(sock), pid, images: images.into(), log: PathBuf::new(), ready_s: 0.0, cgroup: PathBuf::new(), memory_current: 0 });
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
    cmd.args(["restore", "--template-server"]).arg(sock).arg("-D").arg(images).arg("-o").arg(&log).args(["-v4", "--pidfile"]).arg(&pidf);
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
