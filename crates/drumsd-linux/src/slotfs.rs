//! Slot directories in the Python runtime's layout, plus the warm overlay scaffolding.
//!
//! Layout (`launch.slot_paths`): `<state>/slot<N>/{upper,work,images,upper.snap,slot.json,.candidate-owner.json,...}`.
//!
//! Warm scaffolding: after every restore a background job prepares `upper.next` (a copy of `upper.snap`) and
//! `work.next` (empty). A reset then swaps them in with two renames while the old tree's overlay may still be
//! tearing down, instead of deleting and copying the upper layer on the critical path. The old directories go to
//! `trash/` and are removed in the background. A spare is only used when its `.drumsd-complete` stamp exists, so a
//! copy interrupted by a crash is never mistaken for a full one.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

pub const OWNER_RUNTIME: &str = "sharedos-rootfs-candidate";
pub const SLOT_MARKER: &str = ".candidate-owner.json";
pub const ROOT_MARKER: &str = ".sharedos-candidate-owner.json";
pub const SOURCE_FILES: [&str; 7] = ["tree.py", "supervisor.py", "launch.py", "criu_root.py", "locks.py", "candidate.py", "proxy.py"];
pub const REQUIRED_RECORDS: [&str; 3] = ["netstate-pre-dump.json", "mounts-pre-dump.json", "locks-dumped.json"];
pub const COPIED_RECORDS: [&str; 8] = [
    "netstate-pre-dump.json",
    "mounts-pre-dump.json",
    "locks-dumped.json",
    "locks-census.json",
    "images-manifest.json",
    "dump.log",
    "service-plan.json",
    "alive-reference.json",
];
const COMPLETE: &str = ".drumsd-complete";

/// The host device nodes a hardened slot (own user namespace, drums-lab harden.DEV_BIND_NODES) binds into its /dev: a
/// tmpfs mounted inside a user namespace cannot hold usable nodes. Each is a CRIU external mount `drums-dev-<name>`.
pub const DEV_BIND_NODES: [&str; 6] = ["null", "zero", "full", "random", "urandom", "tty"];

/// `criu_root.restore_externals`' device half: `--external mnt[drums-dev-<n>]:/dev/<n>` for each host device bind in a
/// checkpoint's dumped mount table (`mounts-pre-dump.json` rows [mountpoint, fstype, root]); none for an unhardened slot.
pub fn dev_restore_externals(rows: &[[String; 3]]) -> Vec<String> {
    let mut v = vec![];
    for n in DEV_BIND_NODES {
        if rows.iter().any(|r| r[0] == format!("/dev/{n}") && r[1] == "devtmpfs" && r[2] == format!("/{n}")) {
            v.push("--external".into());
            v.push(format!("mnt[drums-dev-{n}]:/dev/{n}"));
        }
    }
    v
}

/// The lower layer a checkpoint's slot ran on and whether it ran in its own user namespace, from its plan
/// (`service-plan.json`, copied with every checkpoint; `candidate._restore_lower`). A hardened slot's lower layer is the
/// idmapped read-only view of the image the provisioning made (`/run/drums-idmap/...`): it must still be mounted, or the
/// restore is refused (a host restart removes it; the origin is then re-provisioned). No plan userns: `default_lower`.
pub fn restore_lower(slot_dir: &Path, default_lower: &Path, is_mounted: impl Fn(&Path) -> bool) -> io::Result<(PathBuf, bool)> {
    let plan = read_json(&slot_dir.join("service-plan.json"))
        .map_err(|e| io::Error::other(format!("{}/service-plan.json is missing or unreadable ({e}): the restore cannot tell which lower layer the checkpoint ran on", slot_dir.display())))?;
    if plan.get("userns").map(|u| u.is_null()).unwrap_or(true) {
        return Ok((default_lower.to_path_buf(), false));
    }
    let lower = plan["lower"].as_str().map(PathBuf::from).ok_or_else(|| io::Error::other("the plan of a user-namespace slot names no lower layer"))?;
    if !is_mounted(&lower) {
        return Err(io::Error::other(format!("the idmapped lower layer {} of this checkpoint is not mounted (host restarted?): re-provision the origin", lower.display())));
    }
    Ok((lower, true))
}

/// `launch.marker_for`: rootfs-<sha256(abspath(state_dir))[:8]>-<slot>.
pub fn marker_for(state_dir: &Path, slot: u32) -> String {
    let h = hex::encode(Sha256::digest(state_dir.to_string_lossy().as_bytes()));
    format!("rootfs-{}-{slot}", &h[..8])
}

/// `candidate.source_sha256`: one digest over the runtime sources (names and bytes, fixed order).
pub fn source_sha256(pkg_dir: &Path) -> io::Result<String> {
    let mut h = Sha256::new();
    for n in SOURCE_FILES {
        let b = fs::read(pkg_dir.join(n))?;
        h.update(n.as_bytes());
        h.update([0u8]);
        h.update(&b);
        h.update([0u8]);
    }
    Ok(hex::encode(h.finalize()))
}

#[derive(Clone, Debug)]
pub struct SlotPaths {
    pub dir: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
    pub images: PathBuf,
    pub snap: PathBuf,
    pub slot_json: PathBuf,
    pub croot: PathBuf,
    pub upper_next: PathBuf,
    pub work_next: PathBuf,
    pub trash: PathBuf,
}

pub fn paths(state_dir: &Path, slot: u32) -> SlotPaths {
    let d = state_dir.join(format!("slot{slot}"));
    SlotPaths {
        upper: d.join("upper"),
        work: d.join("work"),
        images: d.join("images"),
        snap: d.join("upper.snap"),
        slot_json: d.join("slot.json"),
        croot: d.join("criu-root"),
        upper_next: d.join("upper.next"),
        work_next: d.join("work.next"),
        trash: d.join("trash"),
        dir: d,
    }
}

pub fn read_json(p: &Path) -> io::Result<Value> {
    Ok(serde_json::from_slice(&fs::read(p)?)?)
}

pub fn write_json_atomic(p: &Path, v: &Value) -> io::Result<()> {
    let tmp = p.with_extension("json.drumsd-tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    fs::rename(&tmp, p)
}

/// `arm._record`: merge fields into slot.json.
pub fn record(sp: &SlotPaths, fields: Value) -> io::Result<Value> {
    let mut cur = read_json(&sp.slot_json).unwrap_or_else(|_| json!({}));
    if let (Some(c), Some(f)) = (cur.as_object_mut(), fields.as_object()) {
        for (k, v) in f {
            c.insert(k.clone(), v.clone());
        }
    }
    write_json_atomic(&sp.slot_json, &cur)?;
    Ok(cur)
}

/// The tree marker inside the checkpoint's argv: the origin's for a branched slot, else the slot's own.
pub fn argv_marker(state_dir: &Path, slot: u32) -> String {
    let sp = paths(state_dir, slot);
    read_json(&sp.slot_json)
        .ok()
        .and_then(|v| v["branched_from"]["tree_marker"].as_str().map(String::from))
        .unwrap_or_else(|| marker_for(state_dir, slot))
}

/// Give `dst` the owner, group and mode bits of `src` (directories of a branch mirror its origin's).
pub fn copy_access(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let m = fs::metadata(src)?;
    std::os::unix::fs::chown(dst, Some(m.uid()), Some(m.gid()))?;
    fs::set_permissions(dst, fs::Permissions::from_mode(m.mode() & 0o7777))
}

/// Hard-link every file of `src` into a new `dst` (same inodes: the template server accepts only its own inodes).
/// `stats-restore` is not linked, CRIU rewrites it on every restore.
pub fn link_images(src: &Path, dst: &Path) -> io::Result<usize> {
    fs::DirBuilder::new().mode(0o700).create(dst)?;
    let mut n = 0;
    for e in fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name();
        if name == "stats-restore" {
            continue;
        }
        let ft = e.file_type()?;
        if !ft.is_file() {
            return Err(io::Error::other(format!("unexpected non-file {:?} in images", e.path())));
        }
        fs::hard_link(e.path(), dst.join(&name))?;
        n += 1;
    }
    Ok(n)
}

fn cp_a(src: &Path, dst: &Path) -> io::Result<()> {
    let st = Command::new("cp").arg("-a").arg("--reflink=auto").arg(src).arg(dst).status()?;
    if st.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("cp -a {} {} failed: {st}", src.display(), dst.display())))
    }
}

/// Inputs a new slot is made from: the origin checkpoint's slot directory.
pub struct BranchSpec<'a> {
    pub state_dir: &'a Path,
    pub origin: u32,
    pub slot: u32,
    pub lower: &'a Path,
    pub image_sha256: &'a str,
    pub source_sha256: &'a str,
    pub socket: &'a Path,
    pub port: u32,
}

/// `pageshare.PageshareArm.branch` minus the restore: marker, hard-linked images, upper.snap copy, records, slot.json.
pub fn create_branch_dir(b: &BranchSpec) -> io::Result<SlotPaths> {
    let po = paths(b.state_dir, b.origin);
    let p = paths(b.state_dir, b.slot);
    if !po.snap.is_dir() || fs::read_dir(&po.images)?.next().is_none() {
        return Err(io::Error::other(format!("slot {} has no snapshot (upper.snap + images)", b.origin)));
    }
    for n in REQUIRED_RECORDS {
        if !po.dir.join(n).is_file() {
            return Err(io::Error::other(format!("slot {} lacks {n}: the restore gates would have nothing to compare against", b.origin)));
        }
    }
    if p.dir.symlink_metadata().is_ok() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("slot {} already exists at {}", b.slot, p.dir.display())));
    }
    fs::DirBuilder::new().mode(0o700).create(&p.dir)?;
    let tree_marker = argv_marker(b.state_dir, b.origin);
    let state_real = fs::canonicalize(b.state_dir)?;
    let marker = json!({
        "runtime": OWNER_RUNTIME, "protocol": 1, "slot": b.slot, "tree_marker": marker_for(b.state_dir, b.slot),
        "state_dir_real": state_real, "slot_dir_real": fs::canonicalize(&p.dir)?, "lower_real": fs::canonicalize(b.lower)?,
        "image_sha256": b.image_sha256, "created_epoch": crate::now_epoch(), "source_sha256": b.source_sha256,
        "branched_from": {"slot": b.origin, "tree_marker": tree_marker}, "created_by": "drumsd",
    });
    fs::write(p.dir.join(SLOT_MARKER), serde_json::to_vec_pretty(&marker)?)?;
    let t0 = Instant::now();
    link_images(&po.images, &p.images)?;
    // A slot of the hardened profile restores inside its own user namespace, where CRIU reads the images as the slot's mapped
    // root: the branch gets the origin's access to its slot and images directories (drums-lab harden.prepare_slot_access,
    // 0710 root:<slot base>), not drumsd's root-only 0700 (rc1 dev host: "Unable to open tty-info.img: Permission denied").
    copy_access(&po.dir, &p.dir)?;
    copy_access(&po.images, &p.images)?;
    cp_a(&po.snap, &p.snap)?;
    for n in COPIED_RECORDS {
        let s = po.dir.join(n);
        if s.is_file() {
            fs::copy(&s, p.dir.join(n))?;
        }
    }
    fs::create_dir(&p.upper)?;
    fs::create_dir(&p.work)?;
    let so = read_json(&po.slot_json).unwrap_or_else(|_| json!({}));
    record(
        &p,
        json!({
            "port": b.port, "file_locks": so["file_locks"].as_bool().unwrap_or(false), "state": "dumped", "restore_generation": 0,
            "pageshare": {"mode": "template", "socket": b.socket, "criu": null},
            "branched_from": {"slot": b.origin, "tree_marker": tree_marker, "origin_restore_generation": so["restore_generation"],
                              "copy_seconds": (t0.elapsed().as_secs_f64() * 1000.0).round() / 1000.0, "epoch": crate::now_epoch(), "images": "hard links"},
            "created_by": "drumsd",
        }),
    )?;
    Ok(p)
}

/// Prepare `upper.next` / `work.next` from `upper.snap` (background job). Idempotent.
pub fn prepare_spare(p: &SlotPaths) -> io::Result<f64> {
    let t0 = Instant::now();
    if p.upper_next.join(COMPLETE).exists() && p.work_next.is_dir() {
        return Ok(0.0);
    }
    for d in [&p.upper_next, &p.work_next] {
        if d.symlink_metadata().is_ok() {
            fs::remove_dir_all(d)?;
        }
    }
    let tmp = p.dir.join("upper.next.tmp");
    if tmp.symlink_metadata().is_ok() {
        fs::remove_dir_all(&tmp)?;
    }
    cp_a(&p.snap, &tmp)?;
    fs::rename(&tmp, &p.upper_next)?;
    fs::create_dir(&p.work_next)?;
    // the stamp is outside the copied tree's content: written last, removed at swap time before the tree sees it
    fs::write(p.upper_next.join(COMPLETE), b"")?;
    Ok(t0.elapsed().as_secs_f64())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SwapReport {
    pub warm: bool,
    pub seconds: f64,
}

/// Put a fresh upper/work in place: the warm spare if complete (2 renames), else a synchronous copy (recorded as cold).
pub fn swap_in_fresh_upper(p: &SlotPaths) -> io::Result<SwapReport> {
    let t0 = Instant::now();
    fs::create_dir_all(&p.trash)?;
    let stamp = format!("{}", (crate::now_epoch() * 1e6) as u64);
    for (cur, name) in [(&p.upper, "upper"), (&p.work, "work")] {
        if cur.symlink_metadata().is_ok() {
            fs::rename(cur, p.trash.join(format!("{name}.{stamp}")))?;
        }
    }
    let warm = p.upper_next.join(COMPLETE).exists() && p.work_next.is_dir();
    if warm {
        fs::remove_file(p.upper_next.join(COMPLETE))?;
        fs::rename(&p.upper_next, &p.upper)?;
        fs::rename(&p.work_next, &p.work)?;
    } else {
        cp_a(&p.snap, &p.upper)?;
        fs::create_dir(&p.work)?;
    }
    Ok(SwapReport { warm, seconds: t0.elapsed().as_secs_f64() })
}

/// Remove everything under trash/ (background job).
pub fn empty_trash(p: &SlotPaths) -> io::Result<usize> {
    let mut n = 0;
    if let Ok(rd) = fs::read_dir(&p.trash) {
        for e in rd.flatten() {
            let path = e.path();
            // overlay work dirs contain a root-only 0-mode 'work' subdir; remove_dir_all handles it as root
            if fs::remove_dir_all(&path).is_ok() {
                n += 1;
            }
        }
    }
    Ok(n)
}

/// Inode identity of every image file (what the template server checks).
pub fn image_inodes(images: &Path) -> io::Result<Vec<(String, u64, u64)>> {
    let mut v = vec![];
    for e in fs::read_dir(images)? {
        let e = e?;
        let m = e.metadata()?;
        v.push((e.file_name().to_string_lossy().into_owned(), m.dev(), m.ino()));
    }
    v.sort();
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_branch_gets_the_access_of_its_origin_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        fs::DirBuilder::new().mode(0o700).create(&a).unwrap();
        fs::set_permissions(&a, fs::Permissions::from_mode(0o710)).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&b).unwrap();
        copy_access(&a, &b).unwrap();
        assert_eq!(fs::metadata(&b).unwrap().mode() & 0o7777, 0o710);
        assert_eq!(fs::metadata(&b).unwrap().gid(), fs::metadata(&a).unwrap().gid());
    }

    #[test]
    fn device_binds_of_a_hardened_checkpoint_are_restored_from_the_host_nodes() {
        let r = |a: &str, b: &str, c: &str| [a.to_string(), b.to_string(), c.to_string()];
        let rows = vec![r("/", "overlay", "/"), r("/dev", "tmpfs", "/"), r("/dev/null", "devtmpfs", "/null"), r("/dev/tty", "devtmpfs", "/tty"),
                        r("/dev/zero", "tmpfs", "/zero"), r("/dev/sda", "devtmpfs", "/sda")];
        assert_eq!(dev_restore_externals(&rows), vec!["--external", "mnt[drums-dev-null]:/dev/null", "--external", "mnt[drums-dev-tty]:/dev/tty"]);
        assert!(dev_restore_externals(&[r("/", "overlay", "/")]).is_empty());
    }

    #[test]
    fn restore_lower_follows_the_checkpoint_plan() {
        let d = tempfile::tempdir().unwrap();
        let img = Path::new("/mnt/osworld");
        assert!(restore_lower(d.path(), img, |_| true).is_err());                      // no plan: refuse, never guess
        fs::write(d.path().join("service-plan.json"), br#"{"lower": "/mnt/osworld"}"#).unwrap();
        assert_eq!(restore_lower(d.path(), img, |_| true).unwrap(), (img.to_path_buf(), false));
        fs::write(d.path().join("service-plan.json"), br#"{"lower": "/run/drums-idmap/ab-100000", "userns": {"base": 100000, "count": 65536}}"#).unwrap();
        assert_eq!(restore_lower(d.path(), img, |_| true).unwrap(), (PathBuf::from("/run/drums-idmap/ab-100000"), true));
        assert!(restore_lower(d.path(), img, |_| false).is_err());                     // the idmapped view is gone
    }

    fn origin(state: &Path) -> SlotPaths {
        let p = paths(state, 0);
        fs::create_dir_all(p.images.clone()).unwrap();
        fs::write(p.images.join("pages-1.img"), b"pages").unwrap();
        fs::write(p.images.join("stats-restore"), b"s").unwrap();
        fs::create_dir_all(p.snap.join("home/user/Desktop")).unwrap();
        fs::write(p.snap.join("home/user/Desktop/a.txt"), b"hello").unwrap();
        for n in REQUIRED_RECORDS {
            fs::write(p.dir.join(n), b"[]").unwrap();
        }
        fs::write(&p.slot_json, br#"{"file_locks": true, "restore_generation": 1, "branched_from": {"tree_marker": "rootfs-aaaa-0"}}"#).unwrap();
        p
    }

    #[test]
    fn marker_matches_python() {
        // python3 -c 'import hashlib;print(hashlib.sha256(b"/var/lib/drumsd-tmpfs/state").hexdigest()[:8])'
        let h = hex::encode(Sha256::digest(b"/var/lib/drumsd-tmpfs/state"));
        assert_eq!(marker_for(Path::new("/var/lib/drumsd-tmpfs/state"), 7), format!("rootfs-{}-7", &h[..8]));
    }

    #[test]
    fn branch_dir_links_images_and_copies_state() {
        let t = tempfile::tempdir().unwrap();
        let state = t.path();
        let lower = t.path().join("lower");
        fs::create_dir(&lower).unwrap();
        let po = origin(state);
        let sock = state.join("ps-0.sock");
        let p = create_branch_dir(&BranchSpec {
            state_dir: state, origin: 0, slot: 5, lower: &lower, image_sha256: "6bf6", source_sha256: "abc", socket: &sock, port: 25005,
        })
        .unwrap();
        assert_eq!(fs::metadata(p.images.join("pages-1.img")).unwrap().ino(), fs::metadata(po.images.join("pages-1.img")).unwrap().ino());
        assert!(!p.images.join("stats-restore").exists());
        assert_eq!(fs::read(p.snap.join("home/user/Desktop/a.txt")).unwrap(), b"hello");
        let sj = read_json(&p.slot_json).unwrap();
        assert_eq!(sj["file_locks"], true);
        assert_eq!(sj["branched_from"]["tree_marker"], "rootfs-aaaa-0");
        assert_eq!(sj["pageshare"]["mode"], "template");
        assert_eq!(argv_marker(state, 5), "rootfs-aaaa-0");
        let m = read_json(&p.dir.join(SLOT_MARKER)).unwrap();
        assert_eq!(m["runtime"], OWNER_RUNTIME);
        assert_eq!(m["tree_marker"], marker_for(state, 5));
        // a second branch into the same slot is refused
        assert!(create_branch_dir(&BranchSpec {
            state_dir: state, origin: 0, slot: 5, lower: &lower, image_sha256: "6bf6", source_sha256: "abc", socket: &sock, port: 25005,
        })
        .is_err());
    }

    #[test]
    fn warm_spare_swaps_in_and_cold_fallback_copies() {
        let t = tempfile::tempdir().unwrap();
        let state = t.path();
        let lower = t.path().join("lower");
        fs::create_dir(&lower).unwrap();
        origin(state);
        let p = create_branch_dir(&BranchSpec {
            state_dir: state, origin: 0, slot: 3, lower: &lower, image_sha256: "x", source_sha256: "y", socket: Path::new("/s"), port: 1,
        })
        .unwrap();
        fs::write(p.upper.join("dirty"), b"agent wrote this").unwrap();
        // no spare yet: cold copy
        let r = swap_in_fresh_upper(&p).unwrap();
        assert!(!r.warm);
        assert!(!p.upper.join("dirty").exists());
        assert!(p.upper.join("home/user/Desktop/a.txt").exists());
        // prepared spare: warm swap, stamp not visible in the new upper
        prepare_spare(&p).unwrap();
        fs::write(p.upper.join("dirty2"), b"x").unwrap();
        let r = swap_in_fresh_upper(&p).unwrap();
        assert!(r.warm);
        assert!(!p.upper.join("dirty2").exists());
        assert!(!p.upper.join(COMPLETE).exists());
        assert!(p.upper.join("home/user/Desktop/a.txt").exists());
        assert!(p.work.is_dir());
        // an interrupted spare (no stamp) is not used
        fs::create_dir(&p.upper_next).unwrap();
        fs::create_dir(&p.work_next).unwrap();
        let r = swap_in_fresh_upper(&p).unwrap();
        assert!(!r.warm);
        assert!(empty_trash(&p).unwrap() >= 4);
    }

    #[test]
    fn source_hash_is_the_python_one() {
        let t = tempfile::tempdir().unwrap();
        for n in SOURCE_FILES {
            fs::write(t.path().join(n), n.as_bytes()).unwrap();
        }
        let mut h = Sha256::new();
        for n in SOURCE_FILES {
            h.update(format!("{n}\0{n}\0").as_bytes());
        }
        assert_eq!(source_sha256(t.path()).unwrap(), hex::encode(h.finalize()));
    }
}
