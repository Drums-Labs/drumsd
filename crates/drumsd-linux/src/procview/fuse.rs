//! A minimal read-only FUSE file system (raw kernel protocol 7.31 over /dev/fuse) serving the slot views of
//! [`super::view`]: one flat directory of the files in [`super::view::FILES`], mode 0444, owned by root.
//!
//! Each read at offset 0 renders the file for the task that reads (the request header's pid, in the host's pid
//! namespace, so it is right for tasks in nested namespaces too); later offsets of the same open file read that
//! rendering, as a procfs seq_file does. A task that is not in a Drums slot cgroup is refused (EACCES) at open and
//! at read. Nothing can be written, created or removed.
//!
//! The connection survives a restart of the daemon: the /dev/fuse fd is handed to systemd's file-descriptor store
//! (`FDSTORE=1`) and taken back from `LISTEN_FDS` at the next start, so live slots keep their bind mounts.

use super::view;
use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const FSTYPE: &str = "fuse.drums-procview";
pub const SOURCE: &str = "drums-procview";

const FUSE_LOOKUP: u32 = 1;
const FUSE_FORGET: u32 = 2;
const FUSE_GETATTR: u32 = 3;
const FUSE_OPEN: u32 = 14;
const FUSE_READ: u32 = 15;
const FUSE_STATFS: u32 = 17;
const FUSE_RELEASE: u32 = 18;
const FUSE_FLUSH: u32 = 25;
const FUSE_INIT: u32 = 26;
const FUSE_OPENDIR: u32 = 27;
const FUSE_READDIR: u32 = 28;
const FUSE_RELEASEDIR: u32 = 29;
const FUSE_ACCESS: u32 = 34;
const FUSE_INTERRUPT: u32 = 36;
const FUSE_DESTROY: u32 = 38;
const FUSE_BATCH_FORGET: u32 = 42;
const FOPEN_DIRECT_IO: u32 = 1;
const IN_HDR: usize = 40;
const ROOT: u64 = 1;
const BUF: usize = 1 << 17;

/// What the server needs to render: the cgroup root and /proc (injectable for tests).
#[derive(Clone)]
pub struct Ctx {
    pub cg_root: PathBuf,
    pub proc: PathBuf,
    pub host: Arc<Mutex<(std::time::Instant, view::HostFacts)>>,
    /// Last rendering per (slot cgroup, file), reused for [`RENDER_TTL`]: a slot that floods its view costs the daemon
    /// one cgroup read per request instead of a full rendering (measured before: 16 readers in one env took ~8 host
    /// CPUs of the daemon's time, outside the env's cpu.max).
    pub cache: Arc<Mutex<HashMap<(String, &'static str), (std::time::Instant, Vec<u8>)>>>,
}

/// How long a rendering is reused for the same slot and file. Values change continuously anyway; 100 ms is far below
/// what any reader polls at (top, free -s: 1 s and up).
pub const RENDER_TTL: std::time::Duration = std::time::Duration::from_millis(100);

impl Ctx {
    pub fn system() -> io::Result<Ctx> {
        let h = view::HostFacts::read(Path::new("/proc"))?;
        Ok(Ctx {
            cg_root: "/sys/fs/cgroup".into(),
            proc: "/proc".into(),
            host: Arc::new(Mutex::new((std::time::Instant::now(), h))),
            cache: Default::default(),
        })
    }
    /// Host facts, refreshed at most once a second (uptime moves; swap devices can change).
    fn host(&self) -> view::HostFacts {
        let mut g = self.host.lock().unwrap();
        if g.0.elapsed().as_secs_f64() > 1.0 {
            if let Ok(h) = view::HostFacts::read(&self.proc) {
                *g = (std::time::Instant::now(), h);
            }
        }
        g.1.clone()
    }
    /// Render `name` for task `pid`. Err(EACCES) for a non-slot reader, Err(EIO) when the view cannot be built.
    pub fn render_for(&self, name: &str, pid: u32) -> Result<Vec<u8>, i32> {
        let rel = view::cgroup_of(&self.proc, pid).map_err(|_| libc::EACCES)?;
        if !view::is_slot_path(&rel) {
            return Err(libc::EACCES);
        }
        let Some(key_name) = view::FILES.iter().map(|(n, _)| *n).find(|n| *n == name) else { return Err(libc::ENOENT) };
        let key = (rel.clone(), key_name);
        if let Some((at, d)) = self.cache.lock().unwrap().get(&key) {
            if at.elapsed() < RENDER_TTL {
                return Ok(d.clone());
            }
        }
        let d = view::render(name, &self.cg_root, &rel, &self.host()).map(String::into_bytes).map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                libc::EACCES
            } else {
                libc::EIO
            }
        })?;
        let mut c = self.cache.lock().unwrap();
        if c.len() > 8192 {
            c.retain(|_, v| v.0.elapsed() < RENDER_TTL);
        }
        c.insert(key, (std::time::Instant::now(), d.clone()));
        Ok(d)
    }
    fn is_slot(&self, pid: u32) -> bool {
        view::cgroup_of(&self.proc, pid).map(|r| view::is_slot_path(&r)).unwrap_or(false)
    }
}

fn ino_of(name: &str) -> Option<u64> {
    view::FILES.iter().position(|(n, _)| *n == name).map(|i| i as u64 + 2)
}
fn name_of(ino: u64) -> Option<&'static str> {
    (ino >= 2).then(|| view::FILES.get((ino - 2) as usize).map(|(n, _)| *n)).flatten()
}

fn attr(ino: u64) -> Vec<u8> {
    // struct fuse_attr (88 bytes)
    let mut a = Vec::with_capacity(88);
    let (mode, nlink) = if ino == ROOT { (libc::S_IFDIR | 0o555, 2u32) } else { (libc::S_IFREG | 0o444, 1u32) };
    for x in [ino, 0u64, 0, 0, 0, 0] {
        a.extend_from_slice(&x.to_ne_bytes()); // ino size blocks atime mtime ctime
    }
    for x in [0u32, 0, 0, mode, nlink, 0, 0, 0, 4096, 0] {
        a.extend_from_slice(&x.to_ne_bytes()); // atimensec mtimensec ctimensec mode nlink uid gid rdev blksize flags
    }
    a
}

fn entry_out(ino: u64) -> Vec<u8> {
    let mut e = Vec::with_capacity(128);
    for x in [ino, 0u64, 1, 1] {
        e.extend_from_slice(&x.to_ne_bytes()); // nodeid generation entry_valid attr_valid
    }
    e.extend_from_slice(&0u32.to_ne_bytes());
    e.extend_from_slice(&0u32.to_ne_bytes());
    e.extend_from_slice(&attr(ino));
    e
}

fn attr_out(ino: u64) -> Vec<u8> {
    let mut e = Vec::with_capacity(104);
    e.extend_from_slice(&1u64.to_ne_bytes());
    e.extend_from_slice(&0u32.to_ne_bytes());
    e.extend_from_slice(&0u32.to_ne_bytes());
    e.extend_from_slice(&attr(ino));
    e
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_ne_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_ne_bytes(b[o..o + 8].try_into().unwrap())
}

/// Handle table: fh -> (file, the rendering being read).
#[derive(Default)]
pub struct Handles {
    next: u64,
    open: HashMap<u64, (&'static str, Vec<u8>)>,
}

/// One request -> Some(reply) (error, payload) or None (no reply: forget, interrupt). Pure apart from the context.
pub fn handle(ctx: &Ctx, hs: &Mutex<Handles>, req: &[u8]) -> Option<(i32, Vec<u8>)> {
    if req.len() < IN_HDR {
        return Some((-libc::EINVAL, vec![]));
    }
    let op = u32_at(req, 4);
    let node = u64_at(req, 16);
    let pid = u32_at(req, 32);
    let body = &req[IN_HDR..];
    let ok = |p: Vec<u8>| Some((0, p));
    let err = |e: i32| Some((-e, vec![]));
    match op {
        FUSE_INIT => {
            let major = u32_at(body, 0);
            let max_readahead = if body.len() >= 12 { u32_at(body, 8) } else { 0 };
            if major != 7 {
                return err(libc::EPROTO);
            }
            let mut o = Vec::with_capacity(64);
            for x in [7u32, 31, max_readahead, 0] {
                o.extend_from_slice(&x.to_ne_bytes()); // major minor max_readahead flags
            }
            o.extend_from_slice(&16u16.to_ne_bytes()); // max_background
            o.extend_from_slice(&12u16.to_ne_bytes()); // congestion_threshold
            o.extend_from_slice(&4096u32.to_ne_bytes()); // max_write
            o.extend_from_slice(&1u32.to_ne_bytes()); // time_gran
            o.extend_from_slice(&0u16.to_ne_bytes()); // max_pages
            o.extend_from_slice(&0u16.to_ne_bytes()); // map_alignment
            o.extend_from_slice(&[0u8; 32]);
            ok(o)
        }
        FUSE_LOOKUP => {
            let name = body.split(|b| *b == 0).next().map(|n| String::from_utf8_lossy(n).into_owned()).unwrap_or_default();
            match (node == ROOT).then(|| ino_of(&name)).flatten() {
                Some(ino) => ok(entry_out(ino)),
                None => err(libc::ENOENT),
            }
        }
        FUSE_GETATTR => {
            if node == ROOT || name_of(node).is_some() {
                ok(attr_out(node))
            } else {
                err(libc::ENOENT)
            }
        }
        FUSE_OPEN => {
            let flags = u32_at(body, 0) as i32;
            let Some(name) = name_of(node) else { return err(libc::ENOENT) };
            if flags & libc::O_ACCMODE != libc::O_RDONLY || flags & libc::O_TRUNC != 0 {
                return err(libc::EACCES);
            }
            if !ctx.is_slot(pid) {
                return err(libc::EACCES);
            }
            let mut h = hs.lock().unwrap();
            h.next += 1;
            let fh = h.next;
            h.open.insert(fh, (name, vec![]));
            let mut o = fh.to_ne_bytes().to_vec();
            o.extend_from_slice(&FOPEN_DIRECT_IO.to_ne_bytes());
            o.extend_from_slice(&0u32.to_ne_bytes());
            ok(o)
        }
        FUSE_READ => {
            let (fh, off, size) = (u64_at(body, 0), u64_at(body, 8) as usize, u32_at(body, 16) as usize);
            let Some(name) = name_of(node) else { return err(libc::ENOENT) };
            if !ctx.is_slot(pid) {
                return err(libc::EACCES);
            }
            // a handle opened by an earlier daemon (fd store restart) is unknown: render it fresh
            let cached = hs.lock().unwrap().open.get(&fh).map(|x| x.1.clone()).filter(|c| off > 0 && !c.is_empty());
            let data = match cached {
                Some(c) => c,
                None => match ctx.render_for(name, pid) {
                    Ok(d) => {
                        if let Some(e) = hs.lock().unwrap().open.get_mut(&fh) {
                            e.1 = d.clone();
                        }
                        d
                    }
                    Err(e) => return err(e),
                },
            };
            let end = (off + size).min(data.len());
            ok(if off >= data.len() { vec![] } else { data[off..end].to_vec() })
        }
        FUSE_RELEASE => {
            hs.lock().unwrap().open.remove(&u64_at(body, 0));
            ok(vec![])
        }
        FUSE_FLUSH | FUSE_RELEASEDIR | FUSE_ACCESS => ok(vec![]),
        FUSE_OPENDIR => {
            if node != ROOT {
                return err(libc::ENOTDIR);
            }
            let mut o = 0u64.to_ne_bytes().to_vec();
            o.extend_from_slice(&0u32.to_ne_bytes());
            o.extend_from_slice(&0u32.to_ne_bytes());
            ok(o)
        }
        FUSE_READDIR => {
            let (off, size) = (u64_at(body, 8) as usize, u32_at(body, 16) as usize);
            let mut ents: Vec<(u64, &str, u32)> = vec![(ROOT, ".", libc::DT_DIR as u32), (ROOT, "..", libc::DT_DIR as u32)];
            for (n, _) in view::FILES {
                ents.push((ino_of(n).unwrap(), n, libc::DT_REG as u32));
            }
            let mut o = vec![];
            for (i, (ino, n, ty)) in ents.iter().enumerate().skip(off) {
                let mut d = vec![];
                d.extend_from_slice(&ino.to_ne_bytes());
                d.extend_from_slice(&(i as u64 + 1).to_ne_bytes());
                d.extend_from_slice(&(n.len() as u32).to_ne_bytes());
                d.extend_from_slice(&ty.to_ne_bytes());
                d.extend_from_slice(n.as_bytes());
                while d.len() % 8 != 0 {
                    d.push(0);
                }
                if o.len() + d.len() > size {
                    break;
                }
                o.extend_from_slice(&d);
            }
            ok(o)
        }
        FUSE_STATFS => {
            let mut o = vec![0u8; 80];
            o[40..44].copy_from_slice(&4096u32.to_ne_bytes()); // bsize
            o[44..48].copy_from_slice(&255u32.to_ne_bytes()); // namelen
            o[48..52].copy_from_slice(&4096u32.to_ne_bytes()); // frsize
            ok(o)
        }
        FUSE_FORGET | FUSE_BATCH_FORGET | FUSE_INTERRUPT => None,
        FUSE_DESTROY => ok(vec![]),
        _ => err(libc::ENOSYS),
    }
}

fn reply(fd: RawFd, unique: u64, (error, payload): (i32, Vec<u8>)) {
    let mut out = Vec::with_capacity(16 + payload.len());
    out.extend_from_slice(&((16 + payload.len()) as u32).to_ne_bytes());
    out.extend_from_slice(&error.to_ne_bytes());
    out.extend_from_slice(&unique.to_ne_bytes());
    out.extend_from_slice(&payload);
    // ENOENT: the request was interrupted and is gone; nothing to do
    unsafe { libc::write(fd, out.as_ptr() as *const libc::c_void, out.len()) };
}

/// Serve requests from `fd` on `threads` threads until the file system is unmounted (ENODEV).
pub fn serve(fd: RawFd, ctx: Ctx, threads: usize) {
    let hs = Arc::new(Mutex::new(Handles::default()));
    let workers: Vec<_> = (0..threads.max(1))
        .map(|_| {
            let (ctx, hs) = (ctx.clone(), hs.clone());
            std::thread::spawn(move || {
                let mut buf = vec![0u8; BUF];
                loop {
                    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                    if n < 0 {
                        match io::Error::last_os_error().raw_os_error() {
                            Some(libc::EINTR) | Some(libc::ENOENT) | Some(libc::EAGAIN) => continue,
                            _ => return, // ENODEV: unmounted
                        }
                    }
                    let req = &buf[..n as usize];
                    if req.len() < IN_HDR {
                        continue;
                    }
                    let unique = u64_at(req, 8);
                    let op = u32_at(req, 4);
                    if let Some(r) = handle(&ctx, &hs, req) {
                        reply(fd, unique, r);
                    }
                    if op == FUSE_DESTROY {
                        return;
                    }
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
}

fn cstr(s: &str) -> CString {
    CString::new(s).expect("no NUL")
}

/// Open /dev/fuse and mount it read-only at `mountpoint` (allow_other: slot users read it; default_permissions:
/// the kernel checks the 0444 mode). Returns the connection fd.
pub fn mount(mountpoint: &Path) -> io::Result<OwnedFd> {
    std::fs::create_dir_all(mountpoint)?;
    let fd = unsafe { libc::open(cstr("/dev/fuse").as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let opts = format!("fd={},rootmode=40000,user_id=0,group_id=0,allow_other,default_permissions", fd.as_raw_fd());
    let r = unsafe {
        libc::mount(
            cstr(SOURCE).as_ptr(),
            cstr(&mountpoint.display().to_string()).as_ptr(),
            cstr(FSTYPE).as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_RDONLY,
            cstr(&opts).as_ptr() as *const libc::c_void,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Whether `mountpoint` is a live procview mount (statfs works and its type is FUSE).
pub fn is_live(mountpoint: &Path) -> bool {
    let Ok(c) = CString::new(mountpoint.as_os_str().as_encoded_bytes()) else { return false };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    unsafe { libc::statfs(c.as_ptr(), &mut st) == 0 && st.f_type as i64 == 0x65735546 }
}

/// The mountinfo row of `mountpoint`, if it is a procview mount.
pub fn mounted_here(mountinfo: &str, mountpoint: &Path) -> bool {
    let mp = mountpoint.display().to_string();
    mountinfo.lines().any(|l| {
        let (pre, post) = l.split_once(" - ").unwrap_or(("", ""));
        pre.split_whitespace().nth(4) == Some(mp.as_str()) && post.split_whitespace().next() == Some(FSTYPE)
    })
}

/// sd_notify with optional fds (SCM_RIGHTS) to $NOTIFY_SOCKET. Ok(false) when not run by systemd.
pub fn sd_notify(state: &str, fds: &[RawFd]) -> io::Result<bool> {
    let Ok(sock) = std::env::var("NOTIFY_SOCKET") else { return Ok(false) };
    let s = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if s < 0 {
        return Err(io::Error::last_os_error());
    }
    let s = unsafe { OwnedFd::from_raw_fd(s) };
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let b = sock.as_bytes();
    if b.len() >= addr.sun_path.len() {
        return Err(io::Error::other("NOTIFY_SOCKET too long"));
    }
    for (i, c) in b.iter().enumerate() {
        addr.sun_path[i] = if i == 0 && *c == b'@' { 0 } else { *c as libc::c_char };
    }
    let alen = (std::mem::size_of::<libc::sa_family_t>() + b.len()) as u32;
    let mut payload = state.as_bytes().to_vec();
    let mut iov = libc::iovec { iov_base: payload.as_mut_ptr() as *mut libc::c_void, iov_len: payload.len() };
    let space = if fds.is_empty() { 0 } else { (unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) }) as usize };
    let mut cbuf = vec![0u8; space];
    let mut mh: libc::msghdr = unsafe { std::mem::zeroed() };
    mh.msg_name = &mut addr as *mut _ as *mut libc::c_void;
    mh.msg_namelen = alen;
    mh.msg_iov = &mut iov;
    mh.msg_iovlen = 1;
    if !fds.is_empty() {
        mh.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        mh.msg_controllen = space as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&mh);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(c), std::mem::size_of_val(fds));
        }
    }
    if unsafe { libc::sendmsg(s.as_raw_fd(), &mh, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

/// The fd systemd handed back from the store under `name` (LISTEN_PID/LISTEN_FDS/LISTEN_FDNAMES), if any.
pub fn stored_fd(name: &str) -> Option<RawFd> {
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.parse().ok()?;
    if pid != std::process::id() {
        return None;
    }
    let n: i32 = std::env::var("LISTEN_FDS").ok()?.parse().ok()?;
    let names = std::env::var("LISTEN_FDNAMES").unwrap_or_default();
    names.split(':').take(n as usize).position(|x| x == name).map(|i| 3 + i as RawFd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn ctx(root: &Path) -> Ctx {
        let proc = root.join("proc");
        fs::create_dir_all(proc.join("100")).unwrap();
        fs::create_dir_all(proc.join("200")).unwrap();
        fs::write(proc.join("100/cgroup"), "0::/system.slice/drumsbench-rootfs-ab-3.scope\n").unwrap();
        fs::write(proc.join("200/cgroup"), "0::/system.slice/drumsd.service\n").unwrap();
        let cg = root.join("cg");
        let s = cg.join("system.slice/drumsbench-rootfs-ab-3.scope");
        fs::create_dir_all(&s).unwrap();
        for (f, v) in [
            ("memory.max", "4294967296"),
            ("memory.swap.max", "0"),
            ("cpu.max", "400000 100000"),
            ("memory.current", "1073741824"),
            ("memory.swap.current", "0"),
            ("memory.stat", "anon 1\n"),
            ("cpu.stat", "usage_usec 1\n"),
            ("pids.current", "3"),
        ] {
            fs::write(s.join(f), v).unwrap();
        }
        let host = view::HostFacts {
            mem_total: 64 << 30,
            swap_total: 0,
            cpus: 16,
            cpuinfo: "processor\t: 0\n\n".into(),
            uptime_s: 10.0,
            btime: 1,
            overcommit_ratio: 50,
        };
        Ctx { cg_root: cg, proc, host: Arc::new(Mutex::new((std::time::Instant::now(), host))), cache: Default::default() }
    }

    fn req(op: u32, node: u64, pid: u32, body: &[u8]) -> Vec<u8> {
        let mut r = vec![];
        r.extend_from_slice(&((IN_HDR + body.len()) as u32).to_ne_bytes());
        r.extend_from_slice(&op.to_ne_bytes());
        r.extend_from_slice(&7u64.to_ne_bytes());
        r.extend_from_slice(&node.to_ne_bytes());
        r.extend_from_slice(&[0u8; 8]);
        r.extend_from_slice(&pid.to_ne_bytes());
        r.extend_from_slice(&[0u8; 4]);
        r.extend_from_slice(body);
        r
    }

    fn read_in(fh: u64, off: u64, size: u32) -> Vec<u8> {
        let mut b = fh.to_ne_bytes().to_vec();
        b.extend_from_slice(&off.to_ne_bytes());
        b.extend_from_slice(&size.to_ne_bytes());
        b.extend_from_slice(&[0u8; 20]);
        b
    }

    #[test]
    fn lookup_open_read_for_a_slot_reader_and_refusal_for_others() {
        let t = tempfile::tempdir().unwrap();
        let c = ctx(t.path());
        let hs = Mutex::new(Handles::default());
        let (e, p) = handle(&c, &hs, &req(FUSE_LOOKUP, ROOT, 100, b"meminfo\0")).unwrap();
        assert_eq!(e, 0);
        let ino = u64_at(&p, 0);
        assert_eq!(handle(&c, &hs, &req(FUSE_LOOKUP, ROOT, 100, b"passwd\0")).unwrap().0, -libc::ENOENT);
        // open for write refused; host reader refused
        assert_eq!(
            handle(&c, &hs, &req(FUSE_OPEN, ino, 100, &(libc::O_WRONLY as u32).to_ne_bytes().iter().chain([0u8; 4].iter()).copied().collect::<Vec<_>>()))
                .unwrap()
                .0,
            -libc::EACCES
        );
        assert_eq!(handle(&c, &hs, &req(FUSE_OPEN, ino, 200, &[0u8; 8])).unwrap().0, -libc::EACCES);
        let (e, o) = handle(&c, &hs, &req(FUSE_OPEN, ino, 100, &[0u8; 8])).unwrap();
        assert_eq!(e, 0);
        let fh = u64_at(&o, 0);
        assert_eq!(u32_at(&o, 8), FOPEN_DIRECT_IO);
        let (e, d) = handle(&c, &hs, &req(FUSE_READ, ino, 100, &read_in(fh, 0, 20))).unwrap();
        assert_eq!(e, 0);
        assert_eq!(&d, b"MemTotal:        419");
        // later offsets come from the same rendering
        let (_, d2) = handle(&c, &hs, &req(FUSE_READ, ino, 100, &read_in(fh, 20, 8))).unwrap();
        assert_eq!(&d2, b"4304 kB\n");
        // a reader that is not in a slot gets nothing even through a handle it did not open
        assert_eq!(handle(&c, &hs, &req(FUSE_READ, ino, 200, &read_in(fh, 0, 64))).unwrap().0, -libc::EACCES);
        // an unknown handle (opened by an earlier daemon) renders fresh
        let (e, d3) = handle(&c, &hs, &req(FUSE_READ, ino, 100, &read_in(999, 0, 8))).unwrap();
        assert_eq!((e, d3.as_slice()), (0, b"MemTotal".as_slice()));
        assert_eq!(handle(&c, &hs, &req(FUSE_RELEASE, ino, 100, &read_in(fh, 0, 0))).unwrap().0, 0);
        assert!(hs.lock().unwrap().open.is_empty());
    }

    #[test]
    fn attrs_dir_listing_and_ops_without_a_reply() {
        let t = tempfile::tempdir().unwrap();
        let c = ctx(t.path());
        let hs = Mutex::new(Handles::default());
        let (_, a) = handle(&c, &hs, &req(FUSE_GETATTR, 2, 100, &[0u8; 16])).unwrap();
        assert_eq!(a.len(), 104);
        assert_eq!(u32_at(&a, 16 + 60), libc::S_IFREG | 0o444);
        let (_, a) = handle(&c, &hs, &req(FUSE_GETATTR, ROOT, 100, &[0u8; 16])).unwrap();
        assert_eq!(u32_at(&a, 16 + 60), libc::S_IFDIR | 0o555);
        let (_, d) = handle(&c, &hs, &req(FUSE_READDIR, ROOT, 100, &read_in(0, 0, 4096))).unwrap();
        let s = String::from_utf8_lossy(&d);
        for (n, _) in view::FILES {
            assert!(s.contains(n), "{n}");
        }
        assert!(handle(&c, &hs, &req(FUSE_FORGET, 2, 100, &[0u8; 8])).is_none());
        assert_eq!(handle(&c, &hs, &req(4 /* SETATTR */, 2, 100, &[0u8; 88])).unwrap().0, -libc::ENOSYS);
        let mut init = 7u32.to_ne_bytes().to_vec();
        init.extend_from_slice(&41u32.to_ne_bytes());
        init.extend_from_slice(&65536u32.to_ne_bytes());
        init.extend_from_slice(&[0u8; 52]);
        let (e, o) = handle(&c, &hs, &req(FUSE_INIT, 0, 0, &init)).unwrap();
        assert_eq!((e, o.len(), u32_at(&o, 0), u32_at(&o, 4)), (0, 64, 7, 31));
    }

    #[test]
    fn renderings_are_reused_briefly_per_slot_and_never_across_slots() {
        let t = tempfile::tempdir().unwrap();
        let c = ctx(t.path());
        let a = c.render_for("meminfo", 100).unwrap();
        // a second slot with another limit reads its own value even right after the first
        let s2 = c.cg_root.join("system.slice/drumsbench-rootfs-cd-4.scope");
        fs::create_dir_all(&s2).unwrap();
        for (f, v) in [
            ("memory.max", "2147483648"),
            ("memory.swap.max", "0"),
            ("cpu.max", "100000 100000"),
            ("memory.current", "1"),
            ("memory.swap.current", "0"),
            ("memory.stat", "anon 1\n"),
            ("cpu.stat", "usage_usec 1\n"),
            ("pids.current", "1"),
        ] {
            fs::write(s2.join(f), v).unwrap();
        }
        fs::create_dir_all(c.proc.join("300")).unwrap();
        fs::write(c.proc.join("300/cgroup"), "0::/system.slice/drumsbench-rootfs-cd-4.scope\n").unwrap();
        let b = c.render_for("meminfo", 300).unwrap();
        assert!(String::from_utf8_lossy(&a).starts_with("MemTotal:        4194304 kB"));
        assert!(String::from_utf8_lossy(&b).starts_with("MemTotal:        2097152 kB"));
        // within the TTL the same slot gets the cached rendering; after it, a fresh one
        fs::write(c.cg_root.join("system.slice/drumsbench-rootfs-ab-3.scope/memory.max"), "3221225472").unwrap();
        assert_eq!(c.render_for("meminfo", 100).unwrap(), a);
        std::thread::sleep(RENDER_TTL + std::time::Duration::from_millis(20));
        assert!(String::from_utf8_lossy(&c.render_for("meminfo", 100).unwrap()).starts_with("MemTotal:        3145728 kB"));
        // a host reader is still refused, cache or not
        assert_eq!(c.render_for("meminfo", 200).unwrap_err(), libc::EACCES);
    }

    #[test]
    fn mountinfo_detection() {
        let mi = "50 30 0:60 / /run/drums/procview ro,nosuid,nodev,noexec,relatime - fuse.drums-procview drums-procview ro,user_id=0\n";
        assert!(mounted_here(mi, Path::new("/run/drums/procview")));
        assert!(!mounted_here(mi, Path::new("/run")));
    }
}
