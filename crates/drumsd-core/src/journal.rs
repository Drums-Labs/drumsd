//! Crash-safe records.
//!
//! * One JSON record per slot (`<dir>/slots/<id>.json`), replaced atomically: write `<id>.json.tmp`, fsync it,
//!   rename over the old record, fsync the directory. A reader sees either the old or the new record, never a torn one.
//! * One append-only event log (`<dir>/events.jsonl`): one line per transition, fsynced before the call returns,
//!   so the history survives a crash even when the record is replaced.
//!
//! The record is written with the operation's transient state *before* the operation does any work; on restart
//! the daemon reads every record and reconciles the transient ones against the kernel.

use serde::{de::DeserializeOwned, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct Journal {
    dir: PathBuf,
    events: Mutex<File>,
}

fn fsync_dir(d: &Path) -> io::Result<()> {
    File::open(d)?.sync_all()
}

impl Journal {
    pub fn open(dir: impl AsRef<Path>) -> io::Result<Journal> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(dir.join("slots"))?;
        fs::create_dir_all(dir.join("checkpoints"))?;
        fs::create_dir_all(dir.join("meta"))?;
        // a leftover tmp file is a write that never completed; the previous record is authoritative
        for sub in ["slots", "checkpoints", "meta"] {
            for e in fs::read_dir(dir.join(sub))? {
                let p = e?.path();
                if p.extension().map(|x| x == "tmp").unwrap_or(false) {
                    fs::remove_file(&p)?;
                }
            }
        }
        let events = OpenOptions::new().create(true).append(true).open(dir.join("events.jsonl"))?;
        Ok(Journal { dir, events: Mutex::new(events) })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, kind: &str, id: &str) -> io::Result<PathBuf> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("bad record id {id:?}")));
        }
        Ok(self.dir.join(kind).join(format!("{id}.json")))
    }

    pub fn put<T: Serialize>(&self, kind: &str, id: &str, rec: &T) -> io::Result<()> {
        let p = self.path(kind, id)?;
        let tmp = p.with_extension("json.tmp");
        {
            let mut f = File::create(&tmp)?;
            serde_json::to_writer_pretty(&mut f, rec)?;
            f.write_all(b"\n")?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &p)?;
        fsync_dir(p.parent().unwrap())
    }

    pub fn get<T: DeserializeOwned>(&self, kind: &str, id: &str) -> io::Result<Option<T>> {
        let p = self.path(kind, id)?;
        match fs::read(&p) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn all<T: DeserializeOwned>(&self, kind: &str) -> io::Result<Vec<(String, T)>> {
        let mut out = vec![];
        for e in fs::read_dir(self.dir.join(kind))? {
            let p = e?.path();
            if p.extension().map(|x| x == "json").unwrap_or(false) {
                let id = p.file_stem().unwrap().to_string_lossy().to_string();
                out.push((id, serde_json::from_slice(&fs::read(&p)?)?));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    pub fn event(&self, ev: &serde_json::Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(ev)?;
        line.push(b'\n');
        let mut f = self.events.lock().unwrap();
        f.write_all(&line)?;
        f.sync_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn put_get_all_and_events() {
        let d = tempfile::tempdir().unwrap();
        let j = Journal::open(d.path()).unwrap();
        j.put("slots", "s1", &json!({"state": "ready"})).unwrap();
        j.put("slots", "s1", &json!({"state": "resetting"})).unwrap();
        j.put("slots", "s2", &json!({"state": "down"})).unwrap();
        let got: Option<serde_json::Value> = j.get("slots", "s1").unwrap();
        assert_eq!(got.unwrap()["state"], "resetting");
        let all: Vec<(String, serde_json::Value)> = j.all("slots").unwrap();
        assert_eq!(all.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["s1", "s2"]);
        j.event(&json!({"slot": "s1", "to": "ready"})).unwrap();
        j.event(&json!({"slot": "s1", "to": "resetting"})).unwrap();
        let ev = fs::read_to_string(d.path().join("events.jsonl")).unwrap();
        assert_eq!(ev.lines().count(), 2);
    }

    #[test]
    fn a_torn_write_leaves_the_old_record() {
        let d = tempfile::tempdir().unwrap();
        {
            let j = Journal::open(d.path()).unwrap();
            j.put("slots", "s1", &json!({"state": "ready"})).unwrap();
        }
        // simulate a crash after the tmp file was written but before the rename
        fs::write(d.path().join("slots/s1.json.tmp"), b"{\"state\": \"resett").unwrap();
        let j = Journal::open(d.path()).unwrap();
        let got: serde_json::Value = j.get("slots", "s1").unwrap().unwrap();
        assert_eq!(got["state"], "ready");
        assert!(!d.path().join("slots/s1.json.tmp").exists());
    }

    #[test]
    fn ids_cannot_escape_the_directory() {
        let d = tempfile::tempdir().unwrap();
        let j = Journal::open(d.path()).unwrap();
        assert!(j.put("slots", "../x", &json!({})).is_err());
        assert!(j.put("slots", "", &json!({})).is_err());
    }
}
