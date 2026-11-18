//! The verifier store: lab verifiers (a client's checker code) kept by the sha256 of their bytes
//! (docs/design/verifier-sandbox.md, section 5).
//!
//! `<dir>/<sha256>.py` holds the source (mode 0644, written to a temporary name, fsynced and renamed) and
//! `<dir>/<sha256>.json` its record. A task names a verifier by hash, so a verifier is immutable: a changed checker is a
//! new hash. Every read re-hashes the file and refuses one whose bytes no longer match its name.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const DEFAULT_STORE_DIR: &str = "/var/lib/drums/verifiers";
/// The default upload cap: a checker is source code, 1 MiB is generous (drums_t6.py is 31 KB).
pub const DEFAULT_MAX_BYTES: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub sha256: String,
    pub bytes: usize,
    /// A display name (`[A-Za-z_][A-Za-z0-9_]{0,63}`); never used to find the file.
    pub name: Option<String>,
    /// Who added it: "upload" (the API), "bundled" (the release), "named" (imported from the named verifiers dir).
    pub source: String,
    /// The owner label the gateway sets (its workspace id); drumsd itself has one tenant per host.
    #[serde(default)]
    pub owner: Option<String>,
    pub created_epoch: f64,
}

#[derive(Debug, PartialEq)]
pub enum StoreError {
    /// Bad input: (status, message). 400 or 413.
    Invalid(u16, String),
    NotFound(String),
    /// The file is there but its bytes do not match its hash.
    Corrupt(String),
    Io(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Invalid(_, m) | StoreError::NotFound(m) | StoreError::Corrupt(m) | StoreError::Io(m) => f.write_str(m),
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e.to_string())
    }
}

pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn valid_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b.len() <= 64 && (b[0].is_ascii_alphabetic() || b[0] == b'_') && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

pub fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

/// What an upload must be: non-empty, at most `max` bytes, UTF-8 text with no NUL byte.
pub fn check_source(src: &[u8], max: usize) -> Result<(), StoreError> {
    if src.is_empty() {
        return Err(StoreError::Invalid(400, "verifier source is empty".into()));
    }
    if src.len() > max {
        return Err(StoreError::Invalid(413, format!("verifier source is {} bytes; the limit is {max} bytes", src.len())));
    }
    if std::str::from_utf8(src).is_err() {
        return Err(StoreError::Invalid(400, "verifier source is not UTF-8 text".into()));
    }
    if src.contains(&0) {
        return Err(StoreError::Invalid(400, "verifier source contains a NUL byte".into()));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Store {
    pub dir: PathBuf,
    pub max_bytes: usize,
}

impl Store {
    /// The store at `dir` (created 0755 if missing).
    pub fn open(dir: &Path, max_bytes: usize) -> io::Result<Store> {
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
        }
        Ok(Store { dir: dir.to_path_buf(), max_bytes })
    }

    fn src_path(&self, sha: &str) -> PathBuf {
        self.dir.join(format!("{sha}.py"))
    }
    fn rec_path(&self, sha: &str) -> PathBuf {
        self.dir.join(format!("{sha}.json"))
    }

    /// Store `src`. Returns the record and whether it was new (the same bytes again change nothing).
    pub fn put(&self, src: &[u8], name: Option<&str>, source: &str, owner: Option<&str>, now_epoch: f64) -> Result<(Record, bool), StoreError> {
        check_source(src, self.max_bytes)?;
        if let Some(n) = name {
            if !valid_name(n) {
                return Err(StoreError::Invalid(400, format!("verifier name {n:?}: letters, digits and _ only, at most 64, not starting with a digit")));
            }
        }
        let sha = sha256_hex(src);
        if let Ok(r) = self.get(&sha) {
            return Ok((r, false));
        }
        let rec = Record {
            sha256: sha.clone(),
            bytes: src.len(),
            name: name.map(|s| s.to_string()),
            source: source.into(),
            owner: owner.map(|s| s.to_string()),
            created_epoch: now_epoch,
        };
        write_atomic(&self.src_path(&sha), src)?;
        write_atomic(&self.rec_path(&sha), &serde_json::to_vec_pretty(&rec).unwrap())?;
        Ok((rec, true))
    }

    /// The record, after checking the source file still hashes to its name.
    pub fn get(&self, sha: &str) -> Result<Record, StoreError> {
        self.read(sha).map(|(r, _)| r)
    }

    /// The record and the source bytes (checked against the hash).
    pub fn read(&self, sha: &str) -> Result<(Record, Vec<u8>), StoreError> {
        if !is_sha256(sha) {
            return Err(StoreError::Invalid(400, format!("verifier_sha256 {sha:?}: 64 lowercase hex characters")));
        }
        let src = match fs::read(self.src_path(sha)) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(format!("no verifier {sha} in the store (upload it: POST /v1/verifiers)")))
            }
            Err(e) => return Err(e.into()),
        };
        if sha256_hex(&src) != sha {
            return Err(StoreError::Corrupt(format!("verifier {sha}: the stored bytes no longer match the hash; refusing to run it")));
        }
        let rec = fs::read(self.rec_path(sha)).ok().and_then(|b| serde_json::from_slice::<Record>(&b).ok()).unwrap_or(Record {
            sha256: sha.into(),
            bytes: src.len(),
            name: None,
            source: "unknown".into(),
            owner: None,
            created_epoch: 0.0,
        });
        Ok((rec, src))
    }

    /// The source file's path, after checking its hash.
    pub fn path(&self, sha: &str) -> Result<PathBuf, StoreError> {
        self.read(sha)?;
        Ok(self.src_path(sha))
    }

    pub fn list(&self) -> Vec<Record> {
        let mut out = vec![];
        for e in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if let Some(sha) = n.strip_suffix(".py") {
                if is_sha256(sha) {
                    if let Ok(r) = self.get(sha) {
                        out.push(r);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.sha256.cmp(&b.sha256));
        out
    }

    /// Remove one verifier. True when it existed.
    pub fn delete(&self, sha: &str) -> Result<bool, StoreError> {
        if !is_sha256(sha) {
            return Err(StoreError::Invalid(400, format!("verifier_sha256 {sha:?}: 64 lowercase hex characters")));
        }
        let existed = self.src_path(sha).exists();
        for p in [self.src_path(sha), self.rec_path(sha)] {
            match fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(existed)
    }
}

fn write_atomic(p: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = p.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))?;
    }
    fs::rename(&tmp, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("v"), 64).unwrap();
        (d, s)
    }

    #[test]
    fn put_is_content_addressed_and_idempotent() {
        let (_d, s) = store();
        let (r, new) = s.put(b"def check(ctx):\n    return 1\n", Some("acme"), "upload", None, 1.0).unwrap();
        assert!(new && is_sha256(&r.sha256) && r.bytes == 29 && r.name.as_deref() == Some("acme"));
        assert_eq!(r.sha256, sha256_hex(b"def check(ctx):\n    return 1\n"));
        let (r2, new2) = s.put(b"def check(ctx):\n    return 1\n", None, "upload", None, 2.0).unwrap();
        assert!(!new2 && r2 == r, "the same bytes again change nothing");
        assert_eq!(s.list(), vec![r.clone()]);
        assert_eq!(s.read(&r.sha256).unwrap().1, b"def check(ctx):\n    return 1\n");
    }

    #[test]
    fn uploads_are_checked() {
        let (_d, s) = store();
        assert!(matches!(s.put(b"", None, "upload", None, 0.0), Err(StoreError::Invalid(400, _))));
        assert!(matches!(s.put(&[b'x'; 65], None, "upload", None, 0.0), Err(StoreError::Invalid(413, m)) if m.contains("limit is 64")));
        assert!(matches!(s.put(&[0xff, 0xfe], None, "upload", None, 0.0), Err(StoreError::Invalid(400, m)) if m.contains("UTF-8")));
        assert!(matches!(s.put(b"a\0b", None, "upload", None, 0.0), Err(StoreError::Invalid(400, m)) if m.contains("NUL")));
        assert!(matches!(s.put(b"x=1", Some("../etc"), "upload", None, 0.0), Err(StoreError::Invalid(400, _))));
        assert!(s.list().is_empty());
    }

    #[test]
    fn a_tampered_file_is_refused_and_bad_hashes_are_400() {
        let (_d, s) = store();
        let (r, _) = s.put(b"x = 1\n", None, "upload", None, 0.0).unwrap();
        fs::write(s.dir.join(format!("{}.py", r.sha256)), b"x = 2\n").unwrap();
        assert!(matches!(s.get(&r.sha256), Err(StoreError::Corrupt(_))));
        assert!(matches!(s.path(&r.sha256), Err(StoreError::Corrupt(_))));
        assert!(s.list().is_empty(), "a corrupt entry is not listed");
        assert!(matches!(s.get("../../etc/passwd"), Err(StoreError::Invalid(400, _))));
        assert!(matches!(s.get(&"A".repeat(64)), Err(StoreError::Invalid(400, _))), "upper case is not a store key");
        assert!(matches!(s.get(&"0".repeat(64)), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn delete_removes_both_files() {
        let (_d, s) = store();
        let (r, _) = s.put(b"x = 1\n", None, "upload", None, 0.0).unwrap();
        assert!(s.delete(&r.sha256).unwrap());
        assert!(!s.delete(&r.sha256).unwrap());
        assert_eq!(fs::read_dir(&s.dir).unwrap().count(), 0);
    }
}
