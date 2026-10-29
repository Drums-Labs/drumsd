//! Template servers: one `criu restore --template-server SOCK -D IMAGES` per checkpoint, kept warm for the
//! checkpoint's life. Started with the same command line as `fc_drums.serve` (in the pool's `template` cgroup,
//! setsid, pidfile next to the socket), so it survives a drumsd restart and is adopted again from its pidfile.

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
        return Ok(Server { socket: sock.into(), pidfile: pidfile_for(sock), pid, images: images.into(), log: PathBuf::new(), ready_s: 0.0 });
    }
    let _ = fs::remove_file(sock);
    let pidf = pidfile_for(sock);
    let _ = fs::remove_file(&pidf);
    let stamp = (crate::now_epoch() * 1000.0) as u64;
    let log = log_dir.join(format!("template-server-{stamp}.log"));
    let out = fs::File::create(log_dir.join(format!("template-server-{stamp}.out")))?;
    let tcg = cgroup::template_cg();
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
    Ok(Server { socket: sock.into(), pidfile: pidf, pid, images: images.into(), log, ready_s: t0.elapsed().as_secs_f64() })
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
