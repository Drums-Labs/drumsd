//! The CRIU restore, driven exactly like the Python path, without the Python processes around it.
//!
//! Python path (`pageshare.PageshareArm._rewrite` of `criu_root.restore_argv`):
//! `sh -c 'echo $$ > CG/cgroup.procs && exec "$@"' CG unshare --mount --propagation private -- sh -c 'mount -t overlay ...; exec "$@"'
//!   python3 -I pageshare_client.py SOCK criu restore -D IMAGES --root CROOT --restore-detached [--file-locks] --action-script HOOK
//!   --shell-job --tcp-close --manage-cgroups=ignore -v4 --pidfile PIDFILE -o LOG`
//!
//! drumsd: one short-lived helper process (`drumsd restore-client`, this module's [`client_main`]) does the same four
//! steps itself: join the slot cgroup, unshare a private mount namespace, mount the overlay at CROOT, then either send
//! the identical `criu restore ...` command line with its stdio to the template server (SCM_RIGHTS, same protocol as
//! pageshare_client.py) or exec the CRIU binary for a private restore. The CRIU flags are unchanged.

use std::ffi::CString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const CRIU_COMMON: [&str; 4] = ["--shell-job", "--tcp-close", "--manage-cgroups=ignore", "-v4"];

#[derive(Clone, Debug)]
pub struct RestoreSpec {
    pub images: PathBuf,
    pub croot: PathBuf,
    pub pidfile: PathBuf,
    pub log: PathBuf,
    pub hook: Option<PathBuf>,
    pub file_locks: bool,
    pub lower: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
    pub cgroup: PathBuf,
    /// Some(socket): template restore through that server. None: private restore with `criu_bin`.
    pub socket: Option<PathBuf>,
    pub criu_bin: PathBuf,
    /// stdout/stderr of the restore (CRIU's own messages; the log is `-o`).
    pub out: PathBuf,
    /// `--empty-ns net`: CRIU leaves the restored network namespace empty and the setup-namespaces action brings
    /// `lo` up (labelled variant; see hook.rs). Off = CRIU restores the namespace from the images.
    pub empty_net: bool,
    /// `--nested-pidns` (drums-criu app-compat): restore the pid/user/net namespaces sandboxed apps nest below the
    /// slot's, as launch.py's CRIU_COMMON does on that branch.
    pub nested_pidns: bool,
    /// `--external mnt[KEY]:PATH` pairs: the host device binds of a hardened slot (slotfs::dev_restore_externals) and the
    /// per-env view binds (procview::restore_externals) the checkpoint's mount table holds.
    pub externals: Vec<String>,
    /// The checkpoint's slot ran in its own user namespace (the hardened profile, drums-lab harden.py): its overlay keeps
    /// its metadata in user.overlay xattrs, so the restore root is mounted with `userxattr` (criu_root.overlay_options).
    pub userxattr: bool,
    /// CRIU's log level (`-vN`). 4 logs every debug line (about 150k lines per desktop restore, ~0.1 s of it); 3
    /// keeps every pr_info line, which is everything the gates read (lock lines, `Restore finished successfully`).
    pub log_level: u8,
}

/// The `criu restore ...` argv (argv[0] = "criu"), identical to criu_root.restore_argv's CRIU part.
pub fn criu_argv(s: &RestoreSpec) -> Vec<String> {
    let mut v: Vec<String> = vec!["criu".into(), "restore".into(), "-D".into(), s.images.display().to_string(), "--root".into(), s.croot.display().to_string(), "--restore-detached".into()];
    if s.file_locks {
        v.push("--file-locks".into());
    }
    if s.empty_net {
        v.push("--empty-ns".into());
        v.push("net".into());
    }
    if let Some(h) = &s.hook {
        v.push("--action-script".into());
        v.push(h.display().to_string());
    }
    v.extend(s.externals.iter().cloned());
    for x in CRIU_COMMON {
        // launch.py (app-compat): ("--shell-job", "--tcp-close", "--manage-cgroups=ignore", "--nested-pidns", "-v4")
        if x == "-v4" && s.nested_pidns {
            v.push("--nested-pidns".into());
        }
        if x == "-v4" {
            v.push(format!("-v{}", s.log_level.clamp(1, 4)));
            continue;
        }
        v.push(x.to_string());
    }
    v.extend(["--pidfile".into(), s.pidfile.display().to_string(), "-o".into(), s.log.display().to_string()]);
    v
}

/// `criu_root.overlay_options`: refuse paths that cannot be overlay mount options.
pub fn overlay_options(lower: &Path, upper: &Path, work: &Path, userxattr: bool) -> io::Result<String> {
    for (what, p) in [("lower", lower), ("upper", upper), ("work", work)] {
        let s = p.display().to_string();
        if s.contains(',') || s.contains(':') || s.chars().any(|c| c.is_whitespace()) {
            return Err(io::Error::other(format!("the {what} path {s:?} contains a comma, colon or space")));
        }
    }
    Ok(format!("lowerdir={},upperdir={},workdir={}{}", lower.display(), upper.display(), work.display(), if userxattr { ",userxattr" } else { "" }))
}

/// Arguments for the helper: `drumsd restore-client --cgroup CG --overlay OPTS --root CROOT (--socket S | --criu BIN) -- criu restore ...`
pub fn client_args(s: &RestoreSpec) -> io::Result<Vec<String>> {
    let mut a = vec![
        "restore-client".to_string(),
        "--cgroup".into(),
        s.cgroup.display().to_string(),
        "--overlay".into(),
        overlay_options(&s.lower, &s.upper, &s.work, s.userxattr)?,
        "--root".into(),
        s.croot.display().to_string(),
    ];
    match &s.socket {
        Some(sock) => a.extend(["--socket".into(), sock.display().to_string()]),
        None => a.extend(["--criu".into(), s.criu_bin.display().to_string()]),
    }
    a.push("--".into());
    a.extend(criu_argv(s));
    Ok(a)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RestoreRun {
    pub ok: bool,
    pub seconds: f64,
    pub exit: Option<i32>,
    pub out_tail: String,
}

/// Run one restore through the helper (blocking). `self_exe` is the drumsd binary.
pub fn run(self_exe: &Path, s: &RestoreSpec, timeout: Duration) -> io::Result<RestoreRun> {
    let t0 = Instant::now();
    let out = fs::File::create(&s.out)?;
    let err = out.try_clone()?;
    let mut child = Command::new(self_exe).args(client_args(s)?).stdin(Stdio::null()).stdout(out).stderr(err).spawn()?;
    // Wait on a pidfd (one poll, woken at the exit) instead of polling waitid every 2 ms: at 128 desktops with 16
    // restores in flight that polling was ~8,000 waitid/s, each taking the kernel's global tasklist_lock for reading,
    // which every fork and exit on the host (CRIU's own restore forks included) must take for writing (perf, T2b W2:
    // 3.9 % of all host CPU spinning on it from drumsd's Child::try_wait).
    let status = match crate::pidfd::open(child.id() as i32) {
        Ok(fd) => {
            if crate::pidfd::wait_exit(&fd, timeout.saturating_sub(t0.elapsed())) {
                Some(child.wait()?)
            } else {
                let _ = child.kill();
                let _ = child.wait();
                None
            }
        }
        Err(_) => loop {
            if let Some(st) = child.try_wait()? {
                break Some(st);
            }
            if t0.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(2));
        },
    };
    let text = fs::read_to_string(&s.out).unwrap_or_default();
    let tail: String = text.chars().rev().take(1500).collect::<Vec<_>>().into_iter().rev().collect();
    Ok(RestoreRun { ok: status.map(|s| s.success()).unwrap_or(false), seconds: t0.elapsed().as_secs_f64(), exit: status.and_then(|s| s.code()), out_tail: tail })
}

/// Last bytes of the CRIU log (errors are at the end).
pub fn log_tail(log: &Path, n: usize) -> String {
    let t = fs::read(log).unwrap_or_default();
    let s = String::from_utf8_lossy(&t[t.len().saturating_sub(n)..]).into_owned();
    s
}

/// `(seconds)` of CRIU's "Restore finished successfully" line, CRIU's own clock.
pub fn criu_clock(log_text: &str) -> Option<f64> {
    log_text.lines().rev().find(|l| l.contains("Restore finished successfully")).and_then(|l| {
        let l = l.trim_start();
        l.strip_prefix('(').and_then(|r| r.split(')').next()).and_then(|x| x.trim().parse().ok())
    })
}

fn cstr(s: &str) -> CString {
    CString::new(s).expect("no NUL in argument")
}

fn check(r: libc::c_int, what: &str) -> io::Result<()> {
    if r < 0 {
        Err(io::Error::new(io::Error::last_os_error().kind(), format!("{what}: {}", io::Error::last_os_error())))
    } else {
        Ok(())
    }
}

/// Send argv + our stdio to the template server and wait for its 4-byte result (pageshare_client.py's protocol).
fn template_request(sock: &Path, argv: &[String]) -> io::Result<i32> {
    let t0 = Instant::now();
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    check(fd, "socket")?;
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let p = sock.as_os_str().as_encoded_bytes();
    if p.len() >= addr.sun_path.len() {
        return Err(io::Error::other("socket path too long"));
    }
    for (i, b) in p.iter().enumerate() {
        addr.sun_path[i] = *b as libc::c_char;
    }
    // the server may be starting: retry like pageshare_client.py, but bounded at 10 s
    loop {
        let r = unsafe { libc::connect(fd, &addr as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_un>() as u32) };
        if r == 0 {
            break;
        }
        if t0.elapsed() > Duration::from_secs(10) {
            return Err(io::Error::other(format!("no template server at {}: {}", sock.display(), io::Error::last_os_error())));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut payload: Vec<u8> = vec![];
    for a in argv {
        payload.extend_from_slice(a.as_bytes());
        payload.push(0);
    }
    let fds: [libc::c_int; 3] = [0, 1, 2];
    let cmsg_space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(&fds) as u32) } as usize;
    let mut cbuf = vec![0u8; cmsg_space];
    let mut iov = libc::iovec { iov_base: payload.as_mut_ptr() as *mut libc::c_void, iov_len: payload.len() };
    let mut mh: libc::msghdr = unsafe { std::mem::zeroed() };
    mh.msg_iov = &mut iov;
    mh.msg_iovlen = 1;
    mh.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
    mh.msg_controllen = cmsg_space as _;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&mh);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(&fds) as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(c), std::mem::size_of_val(&fds));
    }
    let n = unsafe { libc::sendmsg(fd, &mh, 0) };
    check(n as i32, "sendmsg")?;
    stamp("sent");
    let mut rc = [0u8; 4];
    let got = unsafe { libc::recv(fd, rc.as_mut_ptr() as *mut libc::c_void, 4, 0) };
    unsafe { libc::close(fd) };
    if got != 4 {
        return Err(io::Error::other("the template server closed the connection without a result"));
    }
    Ok(i32::from_ne_bytes(rc))
}

/// The helper's main (runs as `drumsd restore-client ...`, as root). Returns the process exit code.
pub fn client_main(args: &[String]) -> i32 {
    match client_inner(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("drumsd restore-client: {e}");
            97
        }
    }
}

fn stamp(what: &str) {
    // one line per step on the client's stderr (the slot's restore-*.out): where the time outside CRIU's clock goes
    eprintln!("drumsd-client {what} {:.6}", crate::now_epoch());
}

fn client_inner(args: &[String]) -> io::Result<i32> {
    stamp("start");
    let opt = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned();
    let sep = args.iter().position(|a| a == "--").ok_or_else(|| io::Error::other("missing -- before the criu argv"))?;
    let criu: Vec<String> = args[sep + 1..].to_vec();
    if criu.len() < 2 || criu[1] != "restore" {
        return Err(io::Error::other("the command after -- must be `criu restore ...`"));
    }
    let cg = opt("--cgroup").ok_or_else(|| io::Error::other("--cgroup required"))?;
    let ovl = opt("--overlay").ok_or_else(|| io::Error::other("--overlay required"))?;
    let root = opt("--root").ok_or_else(|| io::Error::other("--root required"))?;
    // 0. the slot's OOM priority: CRIU (or the template server's child, which copies ours) and every restored task whose
    //    image holds 0 run at SLOT_OOM_SCORE_ADJ, so the OOM killer takes slot processes before the template server
    fs::write("/proc/self/oom_score_adj", crate::cgroup::SLOT_OOM_SCORE_ADJ.to_string())?;
    // 1. the slot's persistent cgroup: CRIU (or the template server's child, which joins our cgroup) and every restored
    //    task stay in it, so the limits hold from the first instruction
    fs::write(Path::new(&cg).join("cgroup.procs"), std::process::id().to_string())?;
    // 2. a private mount namespace (unshare --mount --propagation private)
    check(unsafe { libc::unshare(libc::CLONE_NEWNS) }, "unshare(CLONE_NEWNS)")?;
    check(
        unsafe { libc::mount(std::ptr::null(), cstr("/").as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) },
        "make / private",
    )?;
    // 3. the overlay root CRIU restores into (--root), visible only in this namespace
    fs::create_dir_all(&root)?;
    let o = cstr(&ovl);
    check(
        unsafe { libc::mount(cstr("overlay").as_ptr(), cstr(&root).as_ptr(), cstr("overlay").as_ptr(), 0, o.as_ptr() as *const libc::c_void) },
        "mount overlay",
    )?;
    stamp("mounted");
    // 4. restore
    if let Some(sock) = opt("--socket") {
        let rc = template_request(Path::new(&sock), &criu)?;
        stamp("result");
        return Ok(if rc == 0 { 0 } else { 1 });
    }
    let bin = opt("--criu").ok_or_else(|| io::Error::other("--socket or --criu required"))?;
    let argv_c: Vec<CString> = std::iter::once(cstr(&bin)).chain(criu[1..].iter().map(|a| cstr(a))).collect();
    let mut ptrs: Vec<*const libc::c_char> = argv_c.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe { libc::execv(argv_c[0].as_ptr(), ptrs.as_ptr()) };
    Err(io::Error::other(format!("execv {bin}: {}", io::Error::last_os_error())))
}

/// The socket file exists and is a socket. Nothing is sent: the server's accept loop is serial, so a probe that
/// connected and held the connection would block every other slot's restore.
pub fn socket_exists(sock: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    fs::symlink_metadata(sock).map(|m| m.file_type().is_socket()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(socket: Option<&str>) -> RestoreSpec {
        RestoreSpec {
            images: "/s/slot5/images".into(),
            croot: "/s/slot5/criu-root".into(),
            pidfile: "/s/slot5/restored.pid".into(),
            log: "/s/slot5/restore-drumsd-3.log".into(),
            hook: Some("/s/slot5/lock-hook-3/hook.sh".into()),
            file_locks: true,
            lower: "/mnt/osworld".into(),
            upper: "/s/slot5/upper".into(),
            work: "/s/slot5/work".into(),
            cgroup: "/sys/fs/cgroup/system.slice/drumsbench-pool.scope/drumsbench-m-pool.scope".into(),
            socket: socket.map(PathBuf::from),
            criu_bin: "/opt/drumsd/criu-bin/criu".into(),
            out: "/s/slot5/restore-drumsd-3.out".into(),
            empty_net: false,
            nested_pidns: false,
            externals: vec![],
            userxattr: false,
            log_level: 4,
        }
    }

    #[test]
    fn criu_argv_is_the_python_one() {
        // criu_root.restore_argv(..., file_locks=True, action_script=hook)'s part after `criu`
        let want = "criu restore -D /s/slot5/images --root /s/slot5/criu-root --restore-detached --file-locks --action-script /s/slot5/lock-hook-3/hook.sh --shell-job --tcp-close --manage-cgroups=ignore -v4 --pidfile /s/slot5/restored.pid -o /s/slot5/restore-drumsd-3.log";
        assert_eq!(criu_argv(&spec(None)).join(" "), want);
        let mut s = spec(None);
        s.file_locks = false;
        s.hook = None;
        assert!(!criu_argv(&s).contains(&"--file-locks".to_string()));
        s.empty_net = true;
        assert!(criu_argv(&s).windows(2).any(|w| w[0] == "--empty-ns" && w[1] == "net"));
        s.log_level = 3;
        assert!(criu_argv(&s).contains(&"-v3".to_string()) && !criu_argv(&s).contains(&"-v4".to_string()));
    }

    #[test]
    fn view_externals_go_after_the_action_script_as_criu_root_puts_them() {
        let mut s = spec(None);
        s.externals = vec!["--external".into(), "mnt[drums-pv-meminfo]:/run/drums/procview/meminfo".into()];
        let j = criu_argv(&s).join(" ");
        assert!(j.contains("--action-script /s/slot5/lock-hook-3/hook.sh --external mnt[drums-pv-meminfo]:/run/drums/procview/meminfo --shell-job"), "{j}");
    }

    #[test]
    fn nested_pidns_sits_where_launch_py_puts_it() {
        let mut s = spec(None);
        s.nested_pidns = true;
        let j = criu_argv(&s).join(" ");
        assert!(j.contains("--shell-job --tcp-close --manage-cgroups=ignore --nested-pidns -v4 --pidfile"), "{j}");
    }

    #[test]
    fn client_args_pick_template_or_private() {
        let t = client_args(&spec(Some("/s/ps-0.sock"))).unwrap();
        assert!(t.windows(2).any(|w| w[0] == "--socket" && w[1] == "/s/ps-0.sock"));
        assert!(!t.contains(&"--criu".to_string()));
        let p = client_args(&spec(None)).unwrap();
        assert!(p.windows(2).any(|w| w[0] == "--criu"));
        let i = p.iter().position(|a| a == "--").unwrap();
        assert_eq!(&p[i + 1..i + 3], &["criu".to_string(), "restore".to_string()]);
        assert!(p.contains(&"lowerdir=/mnt/osworld,upperdir=/s/slot5/upper,workdir=/s/slot5/work".to_string()));
    }

    #[test]
    fn overlay_options_refuse_bad_paths() {
        assert!(overlay_options(Path::new("/a,b"), Path::new("/u"), Path::new("/w"), false).is_err());
        assert!(overlay_options(Path::new("/a"), Path::new("/u:x"), Path::new("/w"), false).is_err());
    }

    #[test]
    fn a_hardened_checkpoint_is_restored_on_user_overlay_xattrs() {
        let mut s = spec(None);
        s.userxattr = true;
        s.lower = "/run/drums-idmap/0f3a9c1e-100000".into();
        let a = client_args(&s).unwrap();
        assert!(a.contains(&"lowerdir=/run/drums-idmap/0f3a9c1e-100000,upperdir=/s/slot5/upper,workdir=/s/slot5/work,userxattr".to_string()), "{a:?}");
    }

    #[test]
    fn criu_clock_reads_the_finish_line() {
        assert_eq!(criu_clock("(00.1) x\n(01.691861) Restore finished successfully. Tasks resumed.\n(01.69) Running post-resume scripts"), Some(1.691861));
        assert_eq!(criu_clock("nothing"), None);
    }
}
