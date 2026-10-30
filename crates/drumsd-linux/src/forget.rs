//! Records of destroyed envs, deleted states and rejected dumps leave the state tmpfs.
//!
//! Every destroyed slot keeps its records and logs (CRIU restore logs at -v4 are most of it, 20-30 MB per env). They
//! used to be moved to `state_dir/forgotten/`, on the same 15 GiB tmpfs that holds every running env's upper layer and
//! images, so ~700 destroyed envs filled it and every later restore failed with ENOSPC. Now a directory to forget is
//! renamed to `state_dir/.forgetting/` (same filesystem: instant), packed into one `<name>.tar.zst` in the on-disk
//! `forgotten_dir`, and only then removed from the tmpfs. Nothing is deleted unarchived: if packing fails, the
//! directory stays (under `state_dir/forgotten/`, as before) and the failure is reported.
//!
//! Retention: everything is kept by default. With `max_bytes`, the oldest archives beyond it are deleted (and listed).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Free bytes for unprivileged use on the filesystem holding `p` (statvfs f_bavail * f_frsize).
pub fn free_bytes(p: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

/// Total bytes of the filesystem holding `p`.
pub fn total_bytes(p: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_blocks as u64 * st.f_frsize as u64)
}

pub fn dir_bytes(p: &Path) -> u64 {
    let Ok(m) = p.symlink_metadata() else { return 0 };
    if !m.is_dir() {
        return m.len();
    }
    fs::read_dir(p).map(|d| d.flatten().map(|e| dir_bytes(&e.path())).sum()).unwrap_or(0)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Forgotten {
    /// Where the records are now (the archive, or the fallback directory on the tmpfs if packing failed).
    pub at: PathBuf,
    pub archived: bool,
    pub bytes_on_tmpfs: u64,
    pub archive_bytes: u64,
    pub error: Option<String>,
}

/// Move `dir` (on the state tmpfs) out of the way under the name `name`, and pack it into `<forgotten_dir>/<name>.tar.zst`.
/// `state_dir` is the tmpfs root (for the staging rename and the fallback).
pub fn forget(state_dir: &Path, forgotten_dir: &Path, dir: &Path, name: &str) -> io::Result<Forgotten> {
    let staging = state_dir.join(".forgetting");
    fs::create_dir_all(&staging)?;
    let staged = staging.join(name);
    fs::rename(dir, &staged)?;
    let bytes = dir_bytes(&staged);
    match pack(&staged, forgotten_dir, name) {
        Ok((archive, n)) => {
            fs::remove_dir_all(&staged)?;
            Ok(Forgotten { at: archive, archived: true, bytes_on_tmpfs: bytes, archive_bytes: n, error: None })
        }
        Err(e) => {
            // keep it where it used to go; nothing is deleted unarchived
            let fallback = state_dir.join("forgotten").join(name);
            fs::create_dir_all(fallback.parent().unwrap())?;
            fs::rename(&staged, &fallback)?;
            Ok(Forgotten { at: fallback, archived: false, bytes_on_tmpfs: bytes, archive_bytes: 0, error: Some(e.to_string()) })
        }
    }
}

/// tar + zstd of `src` into `<dest>/<name>.tar.zst` (written as .partial, then renamed, so an archive is complete).
fn pack(src: &Path, dest: &Path, name: &str) -> io::Result<(PathBuf, u64)> {
    fs::create_dir_all(dest)?;
    let out = dest.join(format!("{name}.tar.zst"));
    let part = dest.join(format!("{name}.tar.zst.partial"));
    let parent = src.parent().ok_or_else(|| io::Error::other("no parent"))?;
    let base = src.file_name().ok_or_else(|| io::Error::other("no name"))?;
    let o = Command::new("tar")
        .arg("--zstd")
        .arg("-cf")
        .arg(&part)
        .arg("-C")
        .arg(parent)
        .arg(base)
        .output()?;
    if !o.status.success() {
        let _ = fs::remove_file(&part);
        return Err(io::Error::other(format!("tar --zstd: {}", String::from_utf8_lossy(&o.stderr).trim())));
    }
    fs::rename(&part, &out)?;
    let n = fs::metadata(&out)?.len();
    Ok((out, n))
}

/// Pack everything left in `state_dir/forgotten` and `state_dir/.forgetting` (older daemons, a crash mid-forget).
pub fn sweep(state_dir: &Path, forgotten_dir: &Path) -> Vec<Forgotten> {
    let mut out = vec![];
    for d in ["forgotten", ".forgetting"] {
        let Ok(rd) = fs::read_dir(state_dir.join(d)) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Ok(f) = forget(state_dir, forgotten_dir, &e.path(), &name) {
                out.push(f);
            }
        }
    }
    out
}

/// Archives in `dir`, oldest first: (path, bytes).
pub fn archives(dir: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".tar.zst"))
                .filter_map(|e| e.metadata().ok().map(|m| (e.path(), m.len(), m.modified().unwrap_or(std::time::UNIX_EPOCH))))
                .collect()
        })
        .unwrap_or_default();
    v.sort_by_key(|x| x.2);
    v
}

/// Retention: delete the oldest archives until the total is at most `max_bytes`. Returns what was deleted.
pub fn prune(dir: &Path, max_bytes: u64) -> Vec<PathBuf> {
    let a = archives(dir);
    let mut total: u64 = a.iter().map(|x| x.1).sum();
    let mut gone = vec![];
    for (p, n, _) in a {
        if total <= max_bytes {
            break;
        }
        if fs::remove_file(&p).is_ok() {
            total -= n;
            gone.push(p);
        }
    }
    gone
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(d: &Path, kb: usize) {
        fs::create_dir_all(d.join("lock-hook-1")).unwrap();
        fs::write(d.join("restore.log"), vec![b'x'; kb * 1024]).unwrap();
        fs::write(d.join("lock-hook-1/procs-pre-resume.json"), b"{}").unwrap();
    }

    #[test]
    fn forgetting_moves_records_off_the_state_dir_and_keeps_them() {
        let t = tempfile::tempdir().unwrap();
        let state = t.path().join("state");
        let disk = t.path().join("disk");
        for i in 0..20 {
            let d = state.join(format!("slot{i}"));
            mk(&d, 256);
            let f = forget(&state, &disk, &d, &format!("slot{i}-1")).unwrap();
            assert!(f.archived, "{:?}", f.error);
            assert!(!d.exists());
        }
        // nothing of the 20 destroyed slots stays in the state dir
        assert!(dir_bytes(&state) < 4096, "state dir holds {} bytes", dir_bytes(&state));
        assert_eq!(archives(&disk).len(), 20);
        // an archive holds the records
        let o = Command::new("tar").arg("--zstd").arg("-tf").arg(disk.join("slot3-1.tar.zst")).output().unwrap();
        let list = String::from_utf8_lossy(&o.stdout);
        assert!(list.contains("slot3-1/restore.log") && list.contains("procs-pre-resume.json"), "{list}");
    }

    #[test]
    fn leftovers_are_swept_and_retention_prunes_oldest_only_when_set() {
        let t = tempfile::tempdir().unwrap();
        let state = t.path().join("state");
        let disk = t.path().join("disk");
        mk(&state.join("forgotten/old-a"), 64);
        mk(&state.join(".forgetting/old-b"), 64);
        let s = sweep(&state, &disk);
        assert_eq!(s.len(), 2);
        assert!(s.iter().all(|f| f.archived));
        assert_eq!(archives(&disk).len(), 2);
        // max large enough: nothing deleted
        assert!(prune(&disk, u64::MAX).is_empty());
        let gone = prune(&disk, 0);
        assert_eq!(gone.len(), 2);
    }

    #[test]
    fn a_failed_pack_keeps_the_directory() {
        let t = tempfile::tempdir().unwrap();
        let state = t.path().join("state");
        let d = state.join("slot9");
        mk(&d, 8);
        // the archive dir is a file: packing must fail and the records stay on the state dir
        let bad = t.path().join("notadir");
        fs::write(&bad, b"").unwrap();
        let f = forget(&state, &bad, &d, "slot9-1").unwrap();
        assert!(!f.archived && f.error.is_some());
        assert!(state.join("forgotten/slot9-1/restore.log").exists());
    }
}
