//! Records of destroyed envs, deleted states and rejected dumps leave the state tmpfs.
//!
//! Every destroyed slot keeps its records and logs (CRIU restore logs at -v4 are most of it, 20-30 MB per env). They
//! used to be moved to `state_dir/forgotten/`, on the same 15 GiB tmpfs that holds every running env's upper layer and
//! images, so ~700 destroyed envs filled it and every later restore failed with ENOSPC. Now a directory to forget is
//! renamed to `state_dir/.forgetting/` (same filesystem: instant), packed into one `<name>.tar.zst` in the on-disk
//! `forgotten_dir`, and only then removed from the tmpfs. Nothing is deleted unarchived: if packing fails, the
//! directory stays (under `state_dir/forgotten/`, as before) and the failure is reported.
//!
//! Retention. Every forgotten directory has a class, decided by the caller from what the manager knows:
//! - `full`: the complete `<name>.tar.zst`, in `<forgotten_dir>/full/`. For envs that ended failed, invalid or
//!   quarantined, rejected dumps, and anything not known to be clean. Archives written directly in `<forgotten_dir>`
//!   by older daemons count as `full`.
//! - `summary`: for envs that ended valid. One small `<name>.summary.json` in `<forgotten_dir>/summary/` with
//!   slot.json, the caller's record (journal record, gates, timings) and the name, size and sha256 of every dropped
//!   file. No logs, no images. If the summary cannot be written the full archive is written instead.
//!
//! Nothing is pruned unless a cap says so: `summary_max_bytes` prunes the oldest summaries, `full_max_bytes` the oldest
//! full records (off by default), `total_max_bytes` the oldest of everything (the old `forgotten_max_gib`). The record
//! just written is never pruned by its own call.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Failed, invalid, quarantined and rejected envs keep a full archive; valid ones keep a small summary.
    #[default]
    FailuresFull,
    /// Every env keeps a full archive.
    KeepAll,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::FailuresFull => "failures_full",
            Mode::KeepAll => "keep_all",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Full,
    Summary,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Class::Full => "full",
            Class::Summary => "summary",
        }
    }
}

/// What the caller knows about the directory being forgotten.
#[derive(Debug, Clone)]
pub enum Disposition {
    /// Keep the complete archive.
    Full,
    /// The env ended valid: keep a summary. `record` is merged into the summary (journal record, gates, timings);
    /// `reason` says why it is only a summary.
    Summary { record: Value, reason: String },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Forgotten {
    /// Where the records are now (the archive or summary, or the fallback directory on the tmpfs if packing failed).
    pub at: PathBuf,
    pub archived: bool,
    pub class: Class,
    pub bytes_on_tmpfs: u64,
    pub archive_bytes: u64,
    pub error: Option<String>,
}

/// Move `dir` (on the state tmpfs) out of the way under the name `name`, and keep it in `forgotten_dir` as `disp`
/// says. `state_dir` is the tmpfs root (for the staging rename and the fallback).
pub fn forget(state_dir: &Path, forgotten_dir: &Path, dir: &Path, name: &str, disp: Disposition) -> io::Result<Forgotten> {
    let staging = state_dir.join(".forgetting");
    fs::create_dir_all(&staging)?;
    let staged = staging.join(name);
    fs::rename(dir, &staged)?;
    let bytes = dir_bytes(&staged);
    let mut class = Class::Full;
    let mut done = None;
    let mut summary_err = None;
    if let Disposition::Summary { record, reason } = &disp {
        match summarize(&staged, forgotten_dir, name, record, reason) {
            Ok(r) => {
                class = Class::Summary;
                done = Some(Ok(r));
            }
            // fail toward keeping evidence: a summary that cannot be written becomes a full archive
            Err(e) => summary_err = Some(e.to_string()),
        }
    }
    let res = done.unwrap_or_else(|| pack(&staged, forgotten_dir, name));
    match res {
        Ok((archive, n)) => {
            fs::remove_dir_all(&staged)?;
            Ok(Forgotten { at: archive, archived: true, class, bytes_on_tmpfs: bytes, archive_bytes: n, error: summary_err })
        }
        Err(e) => {
            // keep it where it used to go; nothing is deleted unarchived
            let fallback = state_dir.join("forgotten").join(name);
            fs::create_dir_all(fallback.parent().unwrap())?;
            fs::rename(&staged, &fallback)?;
            Ok(Forgotten { at: fallback, archived: false, class: Class::Full, bytes_on_tmpfs: bytes, archive_bytes: 0, error: Some(e.to_string()) })
        }
    }
}

const MANIFEST_MAX: usize = 400;

fn walk(base: &Path, p: &Path, out: &mut Vec<Value>, total: &mut (u64, u64)) {
    let Ok(rd) = fs::read_dir(p) else { return };
    let mut es: Vec<_> = rd.flatten().collect();
    es.sort_by_key(|e| e.file_name());
    for e in es {
        let path = e.path();
        let Ok(m) = path.symlink_metadata() else { continue };
        if m.is_dir() {
            walk(base, &path, out, total);
            continue;
        }
        total.0 += 1;
        total.1 += m.len();
        if out.len() >= MANIFEST_MAX {
            continue;
        }
        let rel = path.strip_prefix(base).unwrap_or(&path).display().to_string();
        let sha = if m.file_type().is_file() { sha256_file(&path).ok() } else { None };
        out.push(json!({"path": rel, "bytes": m.len(), "sha256": sha}));
    }
}

fn sha256_file(p: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut f = fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Write `<dest>/summary/<name>.summary.json` (as .partial, then renamed).
fn summarize(src: &Path, dest: &Path, name: &str, record: &Value, reason: &str) -> io::Result<(PathBuf, u64)> {
    let dir = dest.join("summary");
    fs::create_dir_all(&dir)?;
    let mut files = vec![];
    let mut total = (0u64, 0u64);
    walk(src, src, &mut files, &mut total);
    let slot_json = fs::read(src.join("slot.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let doc = json!({
        "summary_version": 1, "name": name, "class": "summary", "reason": reason, "written_epoch": crate::now_epoch(),
        "slot_json": slot_json, "record": record,
        "dropped": {"files": total.0, "bytes": total.1, "listed": files.len(), "truncated": files.len() < total.0 as usize, "manifest": files},
    });
    let out = dir.join(format!("{name}.summary.json"));
    let part = dir.join(format!("{name}.summary.json.partial"));
    if let Err(e) = fs::write(&part, serde_json::to_vec_pretty(&doc)?) {
        let _ = fs::remove_file(&part);
        return Err(e);
    }
    fs::rename(&part, &out)?;
    let n = fs::metadata(&out)?.len();
    Ok((out, n))
}

/// tar + zstd of `src` into `<dest>/full/<name>.tar.zst` (written as .partial, then renamed, so an archive is complete).
fn pack(src: &Path, dest: &Path, name: &str) -> io::Result<(PathBuf, u64)> {
    let dest = dest.join("full");
    fs::create_dir_all(&dest)?;
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
            if let Ok(f) = forget(state_dir, forgotten_dir, &e.path(), &name, Disposition::Full) {
                out.push(f);
            }
        }
    }
    out
}

/// One kept record on disk.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub bytes: u64,
    pub modified: std::time::SystemTime,
    pub class: Class,
    /// Written directly in the forgotten dir by an older daemon.
    pub legacy: bool,
}

fn list_into(dir: &Path, class: Class, legacy: bool, suffix: &str, v: &mut Vec<Entry>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if !e.file_name().to_string_lossy().ends_with(suffix) {
            continue;
        }
        if let Ok(m) = e.metadata() {
            v.push(Entry { path: e.path(), bytes: m.len(), modified: m.modified().unwrap_or(std::time::UNIX_EPOCH), class, legacy });
        }
    }
}

/// Every kept record in `dir` with its class, oldest first. Archives directly in `dir` (older daemons) are `full`.
pub fn entries(dir: &Path) -> Vec<Entry> {
    let mut v = vec![];
    list_into(dir, Class::Full, true, ".tar.zst", &mut v);
    list_into(&dir.join("full"), Class::Full, false, ".tar.zst", &mut v);
    list_into(&dir.join("summary"), Class::Summary, false, ".summary.json", &mut v);
    v.sort_by_key(|x| x.modified);
    v
}

/// All kept records in `dir` (both classes), oldest first: (path, bytes, mtime).
pub fn archives(dir: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
    entries(dir).into_iter().map(|e| (e.path, e.bytes, e.modified)).collect()
}

/// Bytes and file count of a class.
pub fn class_totals(dir: &Path, class: Class) -> (u64, u64) {
    entries(dir).iter().filter(|e| e.class == class).fold((0, 0), |a, e| (a.0 + e.bytes, a.1 + 1))
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Caps {
    /// Oldest summaries are deleted beyond this.
    pub summary: Option<u64>,
    /// Oldest full records are deleted beyond this (None: full records are never pruned).
    pub full: Option<u64>,
    /// Oldest records of either class are deleted beyond this (the old `forgotten_max_gib`).
    pub total: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Pruned {
    pub path: PathBuf,
    pub class: Class,
    pub bytes: u64,
}

fn prune_class(mut list: Vec<Entry>, cap: u64, protect: Option<&Path>, gone: &mut Vec<Pruned>) {
    let mut total: u64 = list.iter().map(|x| x.bytes).sum();
    list.sort_by_key(|x| x.modified);
    for e in list {
        if total <= cap {
            break;
        }
        if Some(e.path.as_path()) == protect {
            continue;
        }
        if fs::remove_file(&e.path).is_ok() {
            total -= e.bytes;
            gone.push(Pruned { path: e.path, class: e.class, bytes: e.bytes });
        }
    }
}

/// Retention: apply the caps, oldest first within each class, then the total cap over everything. `protect` (the record
/// just written) is never deleted. Returns what was deleted.
pub fn prune(dir: &Path, caps: Caps, protect: Option<&Path>) -> Vec<Pruned> {
    let mut gone = vec![];
    for (class, cap) in [(Class::Summary, caps.summary), (Class::Full, caps.full)] {
        if let Some(cap) = cap {
            prune_class(entries(dir).into_iter().filter(|e| e.class == class).collect(), cap, protect, &mut gone);
        }
    }
    if let Some(cap) = caps.total {
        prune_class(entries(dir), cap, protect, &mut gone);
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
            let f = forget(&state, &disk, &d, &format!("slot{i}-1"), Disposition::Full).unwrap();
            assert!(f.archived, "{:?}", f.error);
            assert!(!d.exists());
        }
        // nothing of the 20 destroyed slots stays in the state dir
        assert!(dir_bytes(&state) < 4096, "state dir holds {} bytes", dir_bytes(&state));
        assert_eq!(archives(&disk).len(), 20);
        // an archive holds the records
        let o = Command::new("tar").arg("--zstd").arg("-tf").arg(disk.join("full/slot3-1.tar.zst")).output().unwrap();
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
        assert!(prune(&disk, Caps { total: Some(u64::MAX), ..Caps::default() }, None).is_empty());
        assert!(prune(&disk, Caps::default(), None).is_empty(), "no cap, no pruning");
        let gone = prune(&disk, Caps { total: Some(0), ..Caps::default() }, None);
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
        let f = forget(&state, &bad, &d, "slot9-1", Disposition::Full).unwrap();
        assert!(!f.archived && f.error.is_some());
        assert!(state.join("forgotten/slot9-1/restore.log").exists());
    }

    fn summ() -> Disposition {
        Disposition::Summary { record: json!({"gates": {"gates": [{"gate": "alive", "status": "pass"}]}, "timings": {"restore_s": 1.5}}), reason: "valid".into() }
    }

    fn put_old(p: &Path, bytes: usize, age_s: u64) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![b'z'; bytes]).unwrap();
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(age_s);
        fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn classes_go_to_their_own_layout() {
        let t = tempfile::tempdir().unwrap();
        let (state, disk) = (t.path().join("state"), t.path().join("disk"));
        let (a, b) = (state.join("slot1"), state.join("slot2"));
        mk(&a, 8);
        mk(&b, 8);
        let f = forget(&state, &disk, &a, "slot1-1", Disposition::Full).unwrap();
        let s = forget(&state, &disk, &b, "slot2-1", summ()).unwrap();
        assert_eq!((f.class, s.class), (Class::Full, Class::Summary));
        assert_eq!(f.at, disk.join("full/slot1-1.tar.zst"));
        assert_eq!(s.at, disk.join("summary/slot2-1.summary.json"));
        assert!(f.archived && s.archived && !a.exists() && !b.exists());
        assert_eq!(dir_bytes(&state), 0, "nothing left on the state dir");
    }

    #[test]
    fn a_summary_is_small_and_lists_what_it_dropped() {
        let t = tempfile::tempdir().unwrap();
        let (state, disk) = (t.path().join("state"), t.path().join("disk"));
        let d = state.join("slot5");
        mk(&d, 5 * 1024); // a 5 MB restore.log
        fs::write(d.join("slot.json"), br#"{"restore_generation":3}"#).unwrap();
        let f = forget(&state, &disk, &d, "slot5-9", summ()).unwrap();
        assert_eq!(f.class, Class::Summary);
        assert!(f.archive_bytes < 64 * 1024, "summary is {} bytes", f.archive_bytes);
        let v: Value = serde_json::from_slice(&fs::read(&f.at).unwrap()).unwrap();
        assert_eq!(v["reason"], "valid");
        assert_eq!(v["slot_json"]["restore_generation"], 3);
        assert_eq!(v["record"]["timings"]["restore_s"], 1.5);
        let m = v["dropped"]["manifest"].as_array().unwrap();
        let log = m.iter().find(|x| x["path"] == "restore.log").unwrap();
        assert_eq!(log["bytes"], 5 * 1024 * 1024);
        assert_eq!(log["sha256"].as_str().unwrap().len(), 64);
        assert!(m.iter().any(|x| x["path"] == "lock-hook-1/procs-pre-resume.json"));
        assert_eq!(v["dropped"]["files"], 3);
    }

    #[test]
    fn a_summary_that_cannot_be_written_becomes_a_full_archive() {
        let t = tempfile::tempdir().unwrap();
        let (state, disk) = (t.path().join("state"), t.path().join("disk"));
        let d = state.join("slot6");
        mk(&d, 8);
        fs::create_dir_all(&disk).unwrap();
        fs::write(disk.join("summary"), b"in the way").unwrap();
        let f = forget(&state, &disk, &d, "slot6-1", summ()).unwrap();
        assert!(f.archived && f.class == Class::Full && f.error.is_some(), "{f:?}");
        assert!(disk.join("full/slot6-1.tar.zst").exists());
    }

    #[test]
    fn summary_cap_prunes_oldest_summaries_and_never_full_records() {
        let t = tempfile::tempdir().unwrap();
        let disk = t.path();
        put_old(&disk.join("summary/s-old.summary.json"), 1000, 300);
        put_old(&disk.join("summary/s-mid.summary.json"), 1000, 200);
        put_old(&disk.join("summary/s-new.summary.json"), 1000, 100);
        put_old(&disk.join("full/f-ancient.tar.zst"), 5000, 900);
        put_old(&disk.join("f-legacy.tar.zst"), 5000, 800);
        let gone = prune(disk, Caps { summary: Some(1500), ..Caps::default() }, None);
        let names: Vec<_> = gone.iter().map(|g| g.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["s-old.summary.json", "s-mid.summary.json"]);
        assert!(gone.iter().all(|g| g.class == Class::Summary));
        assert!(disk.join("full/f-ancient.tar.zst").exists() && disk.join("f-legacy.tar.zst").exists());
        assert!(disk.join("summary/s-new.summary.json").exists());
        // the summary cap alone, even at zero, never reaches a full record
        prune(disk, Caps { summary: Some(0), ..Caps::default() }, None);
        assert_eq!(class_totals(disk, Class::Full), (10000, 2));
    }

    #[test]
    fn the_record_just_written_is_not_pruned() {
        let t = tempfile::tempdir().unwrap();
        let disk = t.path();
        put_old(&disk.join("summary/a.summary.json"), 1000, 100);
        put_old(&disk.join("summary/b.summary.json"), 1000, 200);
        let keep = disk.join("summary/a.summary.json");
        let gone = prune(disk, Caps { summary: Some(0), ..Caps::default() }, Some(&keep));
        assert_eq!(gone.len(), 1);
        assert!(keep.exists());
    }

    #[test]
    fn full_cap_prunes_full_records_oldest_first() {
        let t = tempfile::tempdir().unwrap();
        let disk = t.path();
        put_old(&disk.join("f-legacy.tar.zst"), 1000, 500);
        put_old(&disk.join("full/f-mid.tar.zst"), 1000, 400);
        put_old(&disk.join("full/f-new.tar.zst"), 1000, 100);
        put_old(&disk.join("summary/s.summary.json"), 1000, 900);
        let gone = prune(disk, Caps { full: Some(1500), ..Caps::default() }, None);
        assert_eq!(gone.len(), 2);
        assert!(gone.iter().all(|g| g.class == Class::Full));
        assert!(!disk.join("f-legacy.tar.zst").exists() && !disk.join("full/f-mid.tar.zst").exists());
        assert!(disk.join("full/f-new.tar.zst").exists() && disk.join("summary/s.summary.json").exists());
    }

    #[test]
    fn total_cap_prunes_oldest_of_everything() {
        let t = tempfile::tempdir().unwrap();
        let disk = t.path();
        put_old(&disk.join("summary/s.summary.json"), 1000, 900);
        put_old(&disk.join("full/f.tar.zst"), 1000, 100);
        let gone = prune(disk, Caps { total: Some(1500), ..Caps::default() }, None);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].class, Class::Summary);
        assert!(disk.join("full/f.tar.zst").exists());
    }

    #[test]
    fn legacy_archives_are_counted_as_full_and_survive_the_default() {
        let t = tempfile::tempdir().unwrap();
        let disk = t.path();
        put_old(&disk.join("slot1-1.tar.zst"), 2000, 100);
        put_old(&disk.join("slot2-1.tar.zst"), 3000, 200);
        let e = entries(disk);
        assert_eq!(e.len(), 2);
        assert!(e.iter().all(|x| x.class == Class::Full && x.legacy));
        assert_eq!(class_totals(disk, Class::Full), (5000, 2));
        assert_eq!(class_totals(disk, Class::Summary), (0, 0));
        assert!(prune(disk, Caps { summary: Some(0), ..Caps::default() }, None).is_empty());
        assert_eq!(entries(disk).len(), 2);
    }

    #[test]
    fn keep_all_is_a_mode_of_full_archives() {
        // the mode picks the Disposition in the manager; a valid env forgotten as Full is a full archive
        assert_eq!(Mode::default(), Mode::FailuresFull);
        assert_eq!(serde_json::from_str::<Mode>("\"keep_all\"").unwrap(), Mode::KeepAll);
        let t = tempfile::tempdir().unwrap();
        let (state, disk) = (t.path().join("state"), t.path().join("disk"));
        let d = state.join("slot7");
        mk(&d, 8);
        let f = forget(&state, &disk, &d, "slot7-1", Disposition::Full).unwrap();
        assert_eq!(f.class, Class::Full);
        assert!(prune(&disk, Caps::default(), None).is_empty());
    }
}
