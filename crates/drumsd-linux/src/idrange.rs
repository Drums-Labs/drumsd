//! A host uid/gid range of its own for every env of the hardened profile.
//!
//! A hardened slot runs in its own user namespace: slot ids 0..65535 are host ids `base..base+65535` (drums-lab
//! harden.py). CRIU restores that map from the checkpoint, so before this module every env created, forked or reset
//! from one checkpoint ran in the checkpoint's range, and a process that escaped one env owned every sibling's files.
//!
//! Now each env made from a checkpoint gets a free range of its own when its directory is made ([`assign`]):
//! - its copy of `upper.snap` is shifted from the checkpoint's range to the new one ([`shift_tree`]: owners, the
//!   rootid of v3 file capabilities and the ids of POSIX ACLs; `user.overlay.*` xattrs are kept as they are). Every
//!   later fresh upper (the warm spare, a cold reset copy) is a copy of that shifted `upper.snap`;
//! - its plan names the new range and an idmapped read-only view of the image for it (`/run/drums-idmap/<h>-<base>`,
//!   the layout of harden.ensure_idmapped_lower);
//! - its directory is `0710 root:<base>`, so only its own mapped root group can enter it;
//! - every restore passes `--userns-remap FROM:TO:65536` to CRIU ([`restore_args`]): FROM is the range of the
//!   checkpoint the images come from, TO the env's own.
//!
//! A range is held by every slot directory whose plan names it: live envs, saved states (whose images and upper.snap
//! are in the range of the env they were captured from) and the installed origin. A new env never gets a held range,
//! so the range of a saved state is never reused by a live sibling while the state exists. A saved state made by a
//! capture is also sealed (`0700 root:root`, [`seal_checkpoint`]), so the env it was captured from, which keeps
//! running in the same range, cannot reach its files by path.
//!
//! The pool is the root delegation in /etc/subuid and /etc/subgid (install.sh writes `root:100000:<n*65536>`).
//! Ranges outside it are never handed out. An unhardened checkpoint (no userns in its plan) gets none of this.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

pub const SUBID_BASE: u32 = 100000;
pub const SUBID_COUNT: u32 = 65536;
pub const IDMAP_ROOT: &str = "/run/drums-idmap";
/// What [`assign`] did, in the env's directory.
pub const RECORD: &str = "idrange.json";
const PLAN: &str = "service-plan.json";

/// One `--userns-remap FROM:TO:COUNT`: host ids `from..from+count` become `to..to+count` (uids and gids).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remap {
    pub from: u32,
    pub to: u32,
    pub count: u32,
}

impl Remap {
    /// The id `id` moves to, if it is in the `from` range.
    pub fn map(&self, id: u32) -> Option<u32> {
        (id >= self.from && (id as u64) < self.from as u64 + self.count as u64).then(|| id - self.from + self.to)
    }

    pub fn criu_args(&self) -> Vec<String> {
        vec!["--userns-remap".into(), format!("{}:{}:{}", self.from, self.to, self.count)]
    }
}

// ------------------------------------------------------------------------------------------------ the pool

/// `(start, count)` of every `user:start:count` line of an /etc/subuid or /etc/subgid text.
pub fn delegations(text: &str, user: &str) -> Vec<(u64, u64)> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.trim().split(':').collect();
            (f.len() == 3 && f[0] == user).then(|| Some((f[1].parse().ok()?, f[2].parse().ok()?))).flatten()
        })
        .collect()
}

fn covered(d: &[(u64, u64)], base: u64, count: u64) -> bool {
    d.iter().any(|&(s, c)| s <= base && base + count <= s + c)
}

/// The range bases `SUBID_BASE + i*SUBID_COUNT` that root may map as uids AND as gids, in order.
pub fn pool(subuid: &str, subgid: &str) -> Vec<u32> {
    let (u, g) = (delegations(subuid, "root"), delegations(subgid, "root"));
    let mut v = vec![];
    let mut b = SUBID_BASE as u64;
    while b + SUBID_COUNT as u64 <= u32::MAX as u64 {
        if !covered(&u, b, SUBID_COUNT as u64) || !covered(&g, b, SUBID_COUNT as u64) {
            break;
        }
        v.push(b as u32);
        b += SUBID_COUNT as u64;
    }
    v
}

pub fn host_pool() -> io::Result<Vec<u32>> {
    let p = pool(&fs::read_to_string("/etc/subuid").unwrap_or_default(), &fs::read_to_string("/etc/subgid").unwrap_or_default());
    if p.is_empty() {
        return Err(io::Error::other(format!("/etc/subuid and /etc/subgid delegate no range at {SUBID_BASE} to root (install.sh writes root:{SUBID_BASE}:<n*{SUBID_COUNT}>)")));
    }
    Ok(p)
}

/// The lowest base of the pool no slot directory holds.
pub fn pick(pool: &[u32], held: &BTreeSet<u32>) -> Option<u32> {
    pool.iter().copied().find(|b| !held.contains(b))
}

// ------------------------------------------------------------------------------------------------ plans

/// `(base, count)` of a slot directory's plan; None for an unhardened one (no userns) or no plan.
pub fn plan_userns(dir: &Path) -> io::Result<Option<(u32, u32)>> {
    let b = match fs::read(dir.join(PLAN)) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let v: Value = serde_json::from_slice(&b)?;
    userns_of(&v)
}

fn userns_of(plan: &Value) -> io::Result<Option<(u32, u32)>> {
    let u = &plan["userns"];
    if u.is_null() {
        return Ok(None);
    }
    let get = |k: &str| u[k].as_u64().filter(|x| *x <= u32::MAX as u64).map(|x| x as u32);
    match (get("base"), get("count")) {
        (Some(b), Some(c)) => Ok(Some((b, c))),
        _ => Err(io::Error::other(format!("the plan's userns {u} has no base or count"))),
    }
}

/// Bases named by the plan of every `slot<N>` directory under `state_dir` (live envs, saved states, the origin).
/// A plan that cannot be read holds nothing it could name, so it is an error: never hand out a range blindly.
pub fn held_bases(state_dir: &Path) -> io::Result<BTreeSet<u32>> {
    let mut held = BTreeSet::new();
    for e in fs::read_dir(state_dir)? {
        let e = e?;
        let n = e.file_name().to_string_lossy().into_owned();
        if !n.strip_prefix("slot").map(|x| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit())).unwrap_or(false) {
            continue;
        }
        if !e.file_type()?.is_dir() {
            continue;
        }
        match plan_userns(&e.path()) {
            Ok(Some((b, _))) => {
                held.insert(b);
            }
            Ok(None) => {}
            Err(err) => return Err(io::Error::other(format!("{}: {err}", e.path().join(PLAN).display()))),
        }
    }
    Ok(held)
}

/// The plan with its userns base and lower layer replaced.
pub fn replan(plan: &Value, base: u32, lower: &Path) -> Value {
    let mut p = plan.clone();
    p["userns"]["base"] = json!(base);
    p["lower"] = json!(lower);
    p
}

// ------------------------------------------------------------------------------------------------ the shift

const XATTR_CAPS: &str = "security.capability";
const XATTR_ACLS: [&str; 2] = ["system.posix_acl_access", "system.posix_acl_default"];
const VFS_CAP_REVISION_MASK: u32 = 0xFF00_0000;
const VFS_CAP_REVISION_3: u32 = 0x0300_0000;
const ACL_USER: u16 = 0x02;
const ACL_GROUP: u16 = 0x08;

/// A `security.capability` value with its rootid moved (v3, `vfs_ns_cap_data`), or None if it needs no change
/// (v2 has no rootid; a rootid outside the range stays).
pub fn shift_cap(v: &[u8], r: &Remap) -> Option<Vec<u8>> {
    if v.len() != 24 || u32::from_le_bytes(v[0..4].try_into().unwrap()) & VFS_CAP_REVISION_MASK != VFS_CAP_REVISION_3 {
        return None;
    }
    let to = r.map(u32::from_le_bytes(v[20..24].try_into().unwrap()))?;
    let mut o = v.to_vec();
    o[20..24].copy_from_slice(&to.to_le_bytes());
    Some(o)
}

/// A POSIX ACL xattr value (`posix_acl_xattr_header` + entries of tag, perm, id) with the ids of its named user and
/// group entries moved, or None if none moved. A value of another layout is an error: it is not shifted blindly.
pub fn shift_acl(v: &[u8], r: &Remap) -> io::Result<Option<Vec<u8>>> {
    if v.len() < 4 || (v.len() - 4) % 8 != 0 || u32::from_le_bytes(v[0..4].try_into().unwrap()) != 2 {
        return Err(io::Error::other(format!("not a version 2 POSIX ACL ({} bytes)", v.len())));
    }
    let mut o = v.to_vec();
    let mut moved = false;
    for e in o[4..].chunks_mut(8) {
        let tag = u16::from_le_bytes([e[0], e[1]]);
        if tag == ACL_USER || tag == ACL_GROUP {
            if let Some(to) = r.map(u32::from_le_bytes(e[4..8].try_into().unwrap())) {
                e[4..8].copy_from_slice(&to.to_le_bytes());
                moved = true;
            }
        }
    }
    Ok(moved.then_some(o))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ShiftReport {
    /// Every inode visited (a hard-linked one once).
    pub entries: u64,
    /// Entries whose owner or group moved.
    pub moved: u64,
    /// Entries with an owner or group outside the range (left as they are: host root's overlay whiteouts, ...).
    pub outside: u64,
    pub caps: u64,
    pub acls: u64,
    pub seconds: f64,
}

fn cpath(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_encoded_bytes()).map_err(|_| io::Error::other("NUL in a path"))
}

fn lgetxattr(p: &CString, name: &str) -> io::Result<Option<Vec<u8>>> {
    let n = CString::new(name).unwrap();
    let mut buf = vec![0u8; 256];
    loop {
        let r = unsafe { libc::lgetxattr(p.as_ptr(), n.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if r >= 0 {
            buf.truncate(r as usize);
            return Ok(Some(buf));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ENODATA) | Some(libc::ENOTSUP) => return Ok(None),
            Some(libc::ERANGE) if buf.len() < 1 << 20 => buf.resize(buf.len() * 4, 0),
            _ => return Err(e),
        }
    }
}

fn lsetxattr(p: &CString, name: &str, v: &[u8]) -> io::Result<()> {
    let n = CString::new(name).unwrap();
    if unsafe { libc::lsetxattr(p.as_ptr(), n.as_ptr(), v.as_ptr() as *const libc::c_void, v.len(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn shift_one(p: &Path, m: &fs::Metadata, r: &Remap, rep: &mut ShiftReport) -> io::Result<()> {
    rep.entries += 1;
    let (u, g) = (r.map(m.uid()), r.map(m.gid()));
    if u.is_none() || g.is_none() {
        rep.outside += 1;
    }
    let c = cpath(p)?;
    let link = m.file_type().is_symlink();
    // chown drops a file capability and the set-id bits: read the first, keep the mode
    let cap = if !link && m.is_file() { lgetxattr(&c, XATTR_CAPS)? } else { None };
    if u.is_some() || g.is_some() {
        let (nu, ng) = (u.unwrap_or(m.uid()), g.unwrap_or(m.gid()));
        if unsafe { libc::lchown(c.as_ptr(), nu, ng) } < 0 {
            return Err(io::Error::other(format!("lchown {}: {}", p.display(), io::Error::last_os_error())));
        }
        rep.moved += 1;
        if !link && m.mode() & 0o6000 != 0 {
            fs::set_permissions(p, fs::Permissions::from_mode(m.mode() & 0o7777))?;
        }
    }
    if let Some(v) = cap {
        lsetxattr(&c, XATTR_CAPS, &shift_cap(&v, r).unwrap_or(v))?;
        rep.caps += 1;
    }
    if !link {
        for a in XATTR_ACLS {
            if let Some(v) = lgetxattr(&c, a)? {
                if let Some(nv) = shift_acl(&v, r).map_err(|e| io::Error::other(format!("{a} of {}: {e}", p.display())))? {
                    lsetxattr(&c, a, &nv)?;
                    rep.acls += 1;
                }
            }
        }
    }
    Ok(())
}

/// Move the host ids of every entry under `root` (itself included, symlinks not followed) from `r.from..` to `r.to..`.
pub fn shift_tree(root: &Path, r: &Remap) -> io::Result<ShiftReport> {
    let t0 = Instant::now();
    let mut rep = ShiftReport::default();
    let mut todo = vec![root.to_path_buf()];
    let mut linked = std::collections::HashSet::new();
    while let Some(p) = todo.pop() {
        let m = fs::symlink_metadata(&p)?;
        if m.is_dir() {
            for e in fs::read_dir(&p)? {
                todo.push(e?.path());
            }
        } else if m.nlink() > 1 && !linked.insert((m.dev(), m.ino())) {
            continue; // moved at its first name
        }
        shift_one(&p, &m, r, &mut rep)?;
    }
    rep.seconds = (t0.elapsed().as_secs_f64() * 1e6).round() / 1e6;
    Ok(rep)
}

// ------------------------------------------------------------------------------------------------ the idmapped view

/// `harden.idmapped_lower_path`: `/run/drums-idmap/<sha256(realpath(lower))[:8]>-<base>`.
pub fn view_path(lower: &Path, base: u32) -> io::Result<PathBuf> {
    let real = fs::canonicalize(lower)?;
    let h = hex::encode(Sha256::digest(real.as_os_str().as_encoded_bytes()));
    Ok(Path::new(IDMAP_ROOT).join(format!("{}-{base}", &h[..8])))
}

/// The mount options of the top mount at `path` in mountinfo text, if one is mounted there.
pub fn mount_options<'a>(mountinfo: &'a str, path: &str) -> Option<&'a str> {
    mountinfo.lines().filter_map(|l| {
        let f: Vec<&str> = l.split(" - ").next()?.split_whitespace().collect();
        (f.len() >= 6 && f[4] == path).then_some(f[5])
    }).last()
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
mod nr {
    pub const OPEN_TREE: libc::c_long = 428;
    pub const MOVE_MOUNT: libc::c_long = 429;
    pub const MOUNT_SETATTR: libc::c_long = 442;
}
const OPEN_TREE_CLONE: libc::c_uint = 1;
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x4;
const MOUNT_ATTR_RDONLY: u64 = 0x1;
const MOUNT_ATTR_IDMAP: u64 = 0x0010_0000;

/// An fd of a new user namespace with uid and gid maps `0 base count`: a short-lived `sleep` unshares it, host root
/// writes the maps and opens it, and the helper is killed (harden._userns_fd_for).
fn userns_fd(base: u32, count: u32) -> io::Result<libc::c_int> {
    let mut cmd = std::process::Command::new("/bin/sleep");
    cmd.arg("30").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    unsafe {
        cmd.pre_exec(|| if libc::unshare(libc::CLONE_NEWUSER) == 0 { Ok(()) } else { Err(io::Error::last_os_error()) });
    }
    let mut child = cmd.spawn()?; // returns after the exec, so after the unshare
    let pid = child.id();
    let r = (|| {
        fs::write(format!("/proc/{pid}/uid_map"), format!("0 {base} {count}\n"))?;
        fs::write(format!("/proc/{pid}/setgroups"), "deny\n")?;
        fs::write(format!("/proc/{pid}/gid_map"), format!("0 {base} {count}\n"))?;
        let c = CString::new(format!("/proc/{pid}/ns/user")).unwrap();
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    })();
    let _ = child.kill();
    let _ = child.wait();
    r
}

static VIEWS: Mutex<()> = Mutex::new(());

/// Host root: an idmapped read-only view of `lower` in which on-disk id u reads as host id base+u. Idempotent: an
/// existing mount at the path is used only if mountinfo says it is idmapped (harden.ensure_idmapped_lower).
pub fn ensure_view(lower: &Path, base: u32) -> io::Result<PathBuf> {
    let dest = view_path(lower, base)?;
    let ds = dest.display().to_string();
    let _g = VIEWS.lock().unwrap();
    if let Some(o) = mount_options(&fs::read_to_string("/proc/self/mountinfo")?, &ds) {
        if !o.split(',').any(|x| x == "idmapped") {
            return Err(io::Error::other(format!("{ds} is mounted but not idmapped: refusing to use it as a slot's lower layer")));
        }
        return Ok(dest);
    }
    fs::create_dir_all(&dest)?;
    let ufd = userns_fd(base, SUBID_COUNT)?;
    let r = (|| {
        let src = cpath(&fs::canonicalize(lower)?)?;
        let tfd = unsafe { libc::syscall(nr::OPEN_TREE, libc::AT_FDCWD, src.as_ptr(), OPEN_TREE_CLONE | libc::O_CLOEXEC as libc::c_uint) };
        if tfd < 0 {
            return Err(io::Error::other(format!("open_tree({}): {}", lower.display(), io::Error::last_os_error())));
        }
        let tfd = tfd as libc::c_int;
        let r = (|| {
            // struct mount_attr { u64 attr_set, attr_clr, propagation, userns_fd }
            let attr: [u64; 4] = [MOUNT_ATTR_IDMAP | MOUNT_ATTR_RDONLY, 0, 0, ufd as u64];
            let empty = CString::new("").unwrap();
            if unsafe { libc::syscall(nr::MOUNT_SETATTR, tfd, empty.as_ptr(), libc::AT_EMPTY_PATH, attr.as_ptr(), std::mem::size_of_val(&attr)) } < 0 {
                return Err(io::Error::other(format!("mount_setattr(MOUNT_ATTR_IDMAP): {}", io::Error::last_os_error())));
            }
            let d = cpath(&dest)?;
            if unsafe { libc::syscall(nr::MOVE_MOUNT, tfd, empty.as_ptr(), libc::AT_FDCWD, d.as_ptr(), MOVE_MOUNT_F_EMPTY_PATH) } < 0 {
                return Err(io::Error::other(format!("move_mount({ds}): {}", io::Error::last_os_error())));
            }
            Ok(())
        })();
        unsafe { libc::close(tfd) };
        r
    })();
    unsafe { libc::close(ufd) };
    r?;
    match mount_options(&fs::read_to_string("/proc/self/mountinfo")?, &ds) {
        Some(o) if o.split(',').any(|x| x == "idmapped") => Ok(dest),
        o => Err(io::Error::other(format!("{ds} did not come up as an idmapped mount: {o:?}"))),
    }
}

// ------------------------------------------------------------------------------------------------ the calls

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Assigned {
    pub remap: Remap,
    pub view: PathBuf,
    pub shift: ShiftReport,
    pub pool: usize,
    pub held: usize,
}

static ASSIGN: Mutex<()> = Mutex::new(());

/// Give the env directory `dir` (just made from a checkpoint: its plan, images and upper.snap are the checkpoint's) a
/// range of its own. None for an unhardened checkpoint: nothing is changed.
pub fn assign(state_dir: &Path, dir: &Path, image_lower: &Path) -> io::Result<Option<Assigned>> {
    let plan: Value = match fs::read(dir.join(PLAN)) {
        Ok(b) => serde_json::from_slice(&b)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let Some((from, count)) = userns_of(&plan)? else { return Ok(None) };
    if count != SUBID_COUNT {
        return Err(io::Error::other(format!("the checkpoint's userns has {count} ids, not {SUBID_COUNT}")));
    }
    let pool = host_pool()?;
    let view;
    let remap;
    let held_n;
    {
        // scan, pick and write the plan as one step: the plan is what holds the range for every later scan
        let _g = ASSIGN.lock().unwrap();
        let held = held_bases(state_dir)?;
        held_n = held.len();
        let to = pick(&pool, &held).ok_or_else(|| io::Error::other(format!("no free uid range: all {} ranges of the root delegation are held", pool.len())))?;
        remap = Remap { from, to, count };
        view = view_path(image_lower, to)?;
        write_plan(dir, &replan(&plan, to, &view), to)?;
    }
    ensure_view(image_lower, remap.to)?;
    let shift = shift_tree(&dir.join("upper.snap"), &remap)?;
    // only the env's own mapped root group may enter its directory (harden.prepare_slot_access)
    std::os::unix::fs::chown(dir, Some(0), Some(remap.to))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o710))?;
    let a = Assigned { remap, view, shift, pool: pool.len(), held: held_n };
    fs::write(dir.join(RECORD), serde_json::to_vec_pretty(&a)?)?;
    Ok(Some(a))
}

fn write_plan(dir: &Path, plan: &Value, base: u32) -> io::Result<()> {
    let tmp = dir.join("service-plan.json.drumsd-tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(plan)?)?;
    std::os::unix::fs::chown(&tmp, Some(0), Some(base))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o640))?;
    fs::rename(&tmp, dir.join(PLAN))
}

/// The CRIU restore arguments of the env in `env_dir` restored from the checkpoint in `ckpt_dir`: `--userns-remap
/// FROM:TO:65536` when both are hardened and in different ranges, else none (CRIU then behaves as before).
pub fn restore_args(env_dir: &Path, ckpt_dir: &Path) -> io::Result<Vec<String>> {
    match (plan_userns(ckpt_dir)?, plan_userns(env_dir)?) {
        (Some((from, c1)), Some((to, c2))) if from != to => {
            if c1 != c2 {
                return Err(io::Error::other(format!("the checkpoint maps {c1} ids and the env {c2}")));
            }
            Ok(Remap { from, to, count: c1 }.criu_args())
        }
        _ => Ok(vec![]),
    }
}

/// A saved state made by a capture: only root may enter its directory. The env it was captured from keeps running in
/// the range its files are in; no task runs in the state's directory (its children restore from their own).
pub fn seal_checkpoint(dir: &Path) -> io::Result<bool> {
    if plan_userns(dir)?.is_none() {
        return Ok(false);
    }
    std::os::unix::fs::chown(dir, Some(0), Some(0))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: Remap = Remap { from: 100000, to: 165536, count: 65536 };

    fn root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn map_moves_only_the_range() {
        assert_eq!(R.map(100000), Some(165536));
        assert_eq!(R.map(101000), Some(166536));
        assert_eq!(R.map(165535), Some(231071));
        assert_eq!(R.map(165536), None);
        assert_eq!(R.map(99999), None);
        assert_eq!(R.map(0), None);
        assert_eq!(R.criu_args(), vec!["--userns-remap", "100000:165536:65536"]);
    }

    #[test]
    fn the_pool_is_the_root_delegation_of_both_files() {
        let u = "ubuntu:100000:65536\nroot:100000:6553600\n";
        let p = pool(u, u);
        assert_eq!(p.len(), 100);
        assert_eq!((p[0], p[1], p[99]), (100000, 165536, 100000 + 99 * 65536));
        // gids delegate fewer: the smaller one counts
        assert_eq!(pool(u, "root:100000:196608\n").len(), 3);
        // no root line, or one that does not start at SUBID_BASE
        assert!(pool("ubuntu:100000:65536\n", u).is_empty());
        assert!(pool("root:200000:6553600\n", "root:200000:6553600\n").is_empty());
        // two adjacent lines count as one delegation only where each range is inside one line
        assert_eq!(pool("root:100000:65536\nroot:165536:65536\n", "root:100000:131072\n").len(), 2);
        assert_eq!(delegations("# x\nroot:1:2\nbad\nroot:a:3\n", "root"), vec![(1, 2)]);
    }

    #[test]
    fn pick_takes_the_lowest_free_range() {
        let p = vec![100000, 165536, 231072];
        assert_eq!(pick(&p, &BTreeSet::from([100000])), Some(165536));
        assert_eq!(pick(&p, &BTreeSet::from([100000, 165536])), Some(231072));
        assert_eq!(pick(&p, &BTreeSet::from([100000, 165536, 231072])), None);
    }

    fn plan(dir: &Path, base: Option<u32>) {
        fs::create_dir_all(dir).unwrap();
        let v = match base {
            Some(b) => json!({"lower": format!("/run/drums-idmap/ab-{b}"), "userns": {"base": b, "count": 65536}, "x": 1}),
            None => json!({"lower": "/mnt/osworld", "x": 1}),
        };
        fs::write(dir.join(PLAN), serde_json::to_vec(&v).unwrap()).unwrap();
    }

    #[test]
    fn every_slot_dir_holds_the_range_its_plan_names() {
        let t = tempfile::tempdir().unwrap();
        plan(&t.path().join("slot0"), Some(100000));
        plan(&t.path().join("slot101"), Some(165536));
        plan(&t.path().join("slot102"), None);
        plan(&t.path().join("forgotten-slot7"), Some(231072)); // not a slot dir
        fs::create_dir(t.path().join("slot103")).unwrap(); // no plan yet: holds nothing
        assert_eq!(held_bases(t.path()).unwrap(), BTreeSet::from([100000, 165536]));
        fs::create_dir(t.path().join("slot104")).unwrap();
        fs::write(t.path().join("slot104").join(PLAN), b"{").unwrap();
        assert!(held_bases(t.path()).is_err(), "an unreadable plan must not be skipped");
    }

    #[test]
    fn restore_args_remap_only_between_different_hardened_ranges() {
        let t = tempfile::tempdir().unwrap();
        let (c, e, u) = (t.path().join("c"), t.path().join("e"), t.path().join("u"));
        plan(&c, Some(100000));
        plan(&e, Some(231072));
        plan(&u, None);
        assert_eq!(restore_args(&e, &c).unwrap(), vec!["--userns-remap", "100000:231072:65536"]);
        assert!(restore_args(&c, &c).unwrap().is_empty());
        assert!(restore_args(&u, &u).unwrap().is_empty());
        assert!(restore_args(&e, &u).unwrap().is_empty());
        assert!(restore_args(&t.path().join("none"), &c).unwrap().is_empty());
    }

    #[test]
    fn replan_changes_only_the_range_and_the_lower() {
        let p = json!({"lower": "/run/drums-idmap/ab-100000", "userns": {"base": 100000, "count": 65536}, "harden": true, "services": [1]});
        let n = replan(&p, 231072, Path::new("/run/drums-idmap/ab-231072"));
        assert_eq!(n["userns"], json!({"base": 231072, "count": 65536}));
        assert_eq!(n["lower"], "/run/drums-idmap/ab-231072");
        assert_eq!((n["harden"].clone(), n["services"].clone()), (json!(true), json!([1])));
        assert!(userns_of(&json!({"userns": {"base": 1}})).is_err());
        assert_eq!(userns_of(&json!({"userns": null})).unwrap(), None);
    }

    #[test]
    fn file_capabilities_v3_get_the_new_rootid_and_v2_stay() {
        let mut v3 = vec![0u8; 24];
        v3[0..4].copy_from_slice(&(VFS_CAP_REVISION_3 | 1).to_le_bytes());
        v3[4..8].copy_from_slice(&0x2000u32.to_le_bytes()); // cap_net_raw
        v3[20..24].copy_from_slice(&100000u32.to_le_bytes());
        let n = shift_cap(&v3, &R).unwrap();
        assert_eq!(u32::from_le_bytes(n[20..24].try_into().unwrap()), 165536);
        assert_eq!(&n[..20], &v3[..20]);
        v3[20..24].copy_from_slice(&0u32.to_le_bytes());
        assert!(shift_cap(&v3, &R).is_none(), "a host-root rootid is not in the range");
        let mut v2 = vec![0u8; 20];
        v2[0..4].copy_from_slice(&0x0200_0001u32.to_le_bytes());
        assert!(shift_cap(&v2, &R).is_none());
    }

    #[test]
    fn acl_named_entries_move_and_others_stay() {
        let ent = |tag: u16, perm: u16, id: u32| [tag.to_le_bytes().to_vec(), perm.to_le_bytes().to_vec(), id.to_le_bytes().to_vec()].concat();
        let v = [2u32.to_le_bytes().to_vec(), ent(0x01, 6, u32::MAX), ent(ACL_USER, 6, 101000), ent(0x04, 4, u32::MAX), ent(ACL_GROUP, 4, 101001),
                 ent(ACL_GROUP, 4, 50), ent(0x10, 6, u32::MAX), ent(0x20, 4, u32::MAX)].concat();
        let n = shift_acl(&v, &R).unwrap().unwrap();
        let id = |b: &[u8], i: usize| u32::from_le_bytes(b[4 + i * 8 + 4..4 + i * 8 + 8].try_into().unwrap());
        assert_eq!((id(&n, 1), id(&n, 3), id(&n, 4)), (166536, 166537, 50));
        assert_eq!((id(&n, 0), id(&n, 2), id(&n, 5)), (u32::MAX, u32::MAX, u32::MAX));
        let none = [2u32.to_le_bytes().to_vec(), ent(0x01, 6, u32::MAX)].concat();
        assert!(shift_acl(&none, &R).unwrap().is_none());
        assert!(shift_acl(&[1, 0, 0, 0], &R).is_err());
        assert!(shift_acl(&v[..10], &R).is_err());
    }

    #[test]
    fn mount_options_read_the_top_mount() {
        let mi = "30 1 0:5 / /run rw,nosuid - tmpfs tmpfs rw\n\
                  40 30 259:1 / /run/drums-idmap/ab-100000 ro,relatime,idmapped - ext4 /dev/nbd0p3 ro\n\
                  41 30 259:1 / /run/drums-idmap/ab-165536 ro,relatime - ext4 /dev/nbd0p3 ro\n";
        assert_eq!(mount_options(mi, "/run/drums-idmap/ab-100000"), Some("ro,relatime,idmapped"));
        assert_eq!(mount_options(mi, "/run/drums-idmap/ab-165536"), Some("ro,relatime"));
        assert_eq!(mount_options(mi, "/run/drums-idmap/ab-231072"), None);
    }

    // root only (as UsernsSlotTests in drums-lab): the shift on a real tree
    #[test]
    fn shift_tree_moves_owners_keeps_modes_caps_acls_and_overlay_xattrs() {
        if !root() {
            eprintln!("skipped: needs root");
            return;
        }
        use std::os::unix::fs::{symlink, chown, lchown};
        let t = tempfile::tempdir().unwrap();
        let s = t.path().join("upper.snap");
        fs::create_dir_all(s.join("home/user")).unwrap();
        fs::write(s.join("home/user/a.txt"), b"a").unwrap();
        fs::write(s.join("suid"), b"#!").unwrap();
        fs::write(s.join("capped"), b"x").unwrap();
        fs::write(s.join("hostroot"), b"x").unwrap();
        symlink("home/user/a.txt", s.join("link")).unwrap();
        fs::hard_link(s.join("home/user/a.txt"), s.join("hard")).unwrap();
        chown(&s, Some(100000), Some(100000)).unwrap();
        chown(s.join("home"), Some(100000), Some(100000)).unwrap();
        chown(s.join("home/user"), Some(101000), Some(101000)).unwrap();
        chown(s.join("home/user/a.txt"), Some(101000), Some(101005)).unwrap();
        chown(s.join("suid"), Some(100000), Some(100050)).unwrap();
        fs::set_permissions(s.join("suid"), fs::Permissions::from_mode(0o6755)).unwrap();
        chown(s.join("capped"), Some(100000), Some(100000)).unwrap();
        lchown(s.join("link"), Some(101000), Some(101000)).unwrap();
        let c = cpath(&s.join("capped")).unwrap();
        let mut v3 = vec![0u8; 24];
        v3[0..4].copy_from_slice(&(VFS_CAP_REVISION_3 | 1).to_le_bytes());
        v3[4..8].copy_from_slice(&0x2000u32.to_le_bytes());
        v3[20..24].copy_from_slice(&100000u32.to_le_bytes());
        let cap_ok = lsetxattr(&c, XATTR_CAPS, &v3).is_ok();
        let d = cpath(&s.join("home/user")).unwrap();
        lsetxattr(&d, "user.overlay.opaque", b"y").unwrap();
        let ent = |tag: u16, perm: u16, id: u32| [tag.to_le_bytes().to_vec(), perm.to_le_bytes().to_vec(), id.to_le_bytes().to_vec()].concat();
        let acl = [2u32.to_le_bytes().to_vec(), ent(0x01, 7, u32::MAX), ent(ACL_USER, 7, 101002), ent(0x04, 5, u32::MAX), ent(0x10, 7, u32::MAX), ent(0x20, 5, u32::MAX)].concat();
        let acl_ok = lsetxattr(&d, "system.posix_acl_access", &acl).is_ok();

        assert!(cap_ok && acl_ok, "the test filesystem takes file capabilities and ACLs");
        let rep = shift_tree(&s, &R).unwrap();
        let o = |p: &str| { let m = fs::symlink_metadata(s.join(p)).unwrap(); (m.uid(), m.gid()) };
        assert_eq!(o(""), (165536, 165536));
        assert_eq!(o("home/user"), (166536, 166536));
        assert_eq!(o("home/user/a.txt"), (166536, 166541));
        assert_eq!(o("hard"), (166536, 166541), "a hard link is moved once, not twice");
        assert_eq!(o("link"), (166536, 166536));
        assert_eq!(o("hostroot"), (0, 0));
        assert_eq!(o("suid"), (165536, 165586));
        assert_eq!(fs::metadata(s.join("suid")).unwrap().mode() & 0o7777, 0o6755);
        assert_eq!(lgetxattr(&d, "user.overlay.opaque").unwrap().as_deref(), Some(&b"y"[..]));
        let n = lgetxattr(&c, XATTR_CAPS).unwrap().unwrap();
        assert_eq!(u32::from_le_bytes(n[20..24].try_into().unwrap()), 165536, "the v3 rootid follows the range");
        assert_eq!(rep.caps, 1);
        let n = lgetxattr(&d, "system.posix_acl_access").unwrap().unwrap();
        assert_eq!(u32::from_le_bytes(n[4 + 8 + 4..4 + 16].try_into().unwrap()), 166538, "the named ACL user follows the range");
        assert_eq!(rep.acls, 1);
        assert_eq!(rep.outside, 1, "hostroot");
        assert_eq!(rep.entries, 8, "9 names, a.txt and hard are one inode");
        // a second shift finds nothing in the old range
        let again = shift_tree(&s, &R).unwrap();
        assert_eq!(again.moved, 0);
    }

    #[test]
    fn a_hardened_saved_state_is_sealed_and_an_unhardened_one_left() {
        if !root() {
            eprintln!("skipped: needs root");
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let (h, u) = (t.path().join("h"), t.path().join("u"));
        plan(&h, Some(165536));
        plan(&u, None);
        fs::set_permissions(&u, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(seal_checkpoint(&h).unwrap());
        assert_eq!(fs::metadata(&h).unwrap().mode() & 0o7777, 0o700);
        assert!(!seal_checkpoint(&u).unwrap());
        assert_eq!(fs::metadata(&u).unwrap().mode() & 0o7777, 0o755);
    }
}
