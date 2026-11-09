//! Tasks with checkers for the Android and Windows backends.
//!
//! A task is JSON: an instruction, setup commands run after every reset, a checker, and an oracle (the actions of a
//! known passing run). Checkers are commands run in the desktop (Android: `sh` through adb; Windows: PowerShell
//! through the guest server) whose output is compared with an expectation. The same engine is implemented in the
//! study's Python harness (osvm/harness/checker.py) so the incumbent arms are scored by identical rules.

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Check {
    /// Command whose stdout is judged.
    pub cmd: String,
    /// stdout (trimmed) equals this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<String>,
    /// stdout matches this regex (search, multi-line).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
    /// stdout contains this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    /// stdout does not contain this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_contains: Option<String>,
    /// stdout parsed as a number lies in [min, max].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// "android" or "windows"
    pub os: String,
    pub instruction: String,
    /// The incumbent task this ports (e.g. "AndroidWorld SystemBrightnessMax, task_evals/single/system.py").
    #[serde(default)]
    pub source: String,
    /// Commands run after every reset, before the episode starts.
    #[serde(default)]
    pub setup: Vec<String>,
    /// All checks must pass for reward 1.
    pub check: Vec<Check>,
    /// Actions (step bodies) of a known passing run.
    #[serde(default)]
    pub oracle: Vec<Value>,
    /// "gui" (screen actions only) or "shell" (uses shell/execute actions).
    #[serde(default)]
    pub oracle_kind: String,
    /// Packages/apps the task needs (an env without them refuses the task).
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub max_steps: u32,
}

/// Judge one check's stdout. Returns (pass, detail).
pub fn judge(c: &Check, stdout: &str, rc: i32) -> (bool, String) {
    let s = stdout.trim();
    let short: String = s.chars().take(200).collect();
    if rc != 0
        && c.equals.is_none()
        && c.regex.is_none()
        && c.contains.is_none()
        && c.not_contains.is_none()
        && c.min.is_none()
        && c.max.is_none()
    {
        return (false, format!("exit {rc}"));
    }
    if let Some(e) = &c.equals {
        if s != e.trim() {
            return (false, format!("got {short:?}, want {e:?}"));
        }
    }
    if let Some(r) = &c.regex {
        match Regex::new(&format!("(?m){r}")) {
            Ok(re) if re.is_match(s) => {}
            Ok(_) => return (false, format!("{short:?} does not match /{r}/")),
            Err(e) => return (false, format!("bad regex {r}: {e}")),
        }
    }
    if let Some(x) = &c.contains {
        if !s.contains(x.as_str()) {
            return (false, format!("{short:?} lacks {x:?}"));
        }
    }
    if let Some(x) = &c.not_contains {
        if s.contains(x.as_str()) {
            return (false, format!("{short:?} contains {x:?}"));
        }
    }
    if c.min.is_some() || c.max.is_some() {
        let v: f64 = match s.parse() {
            Ok(v) => v,
            Err(_) => return (false, format!("{short:?} is not a number")),
        };
        if c.min.is_some_and(|m| v < m) || c.max.is_some_and(|m| v > m) {
            return (false, format!("{v} outside [{:?}, {:?}]", c.min, c.max));
        }
    }
    (true, format!("ok: {short:?}"))
}

/// Load every `*.json` task file under `dir` (one task per file, or an array).
pub fn load_dir(dir: &Path) -> Result<BTreeMap<String, Task>, String> {
    let mut out = BTreeMap::new();
    let rd = std::fs::read_dir(dir).map_err(|e| format!("tasks dir {}: {e}", dir.display()))?;
    let mut files: Vec<_> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    for f in files {
        let b = std::fs::read(&f).map_err(|e| format!("{}: {e}", f.display()))?;
        let v: Value = serde_json::from_slice(&b).map_err(|e| format!("{}: {e}", f.display()))?;
        let list = if v.is_array() {
            v.as_array().unwrap().clone()
        } else {
            vec![v]
        };
        for t in list {
            let t: Task = serde_json::from_value(t).map_err(|e| format!("{}: {e}", f.display()))?;
            if out.insert(t.id.clone(), t.clone()).is_some() {
                return Err(format!("duplicate task id {}", t.id));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn c() -> Check {
        Check {
            cmd: "x".into(),
            equals: None,
            regex: None,
            contains: None,
            not_contains: None,
            min: None,
            max: None,
        }
    }
    #[test]
    fn judges() {
        assert!(
            judge(
                &Check {
                    equals: Some("255".into()),
                    ..c()
                },
                "255\n",
                0
            )
            .0
        );
        assert!(
            !judge(
                &Check {
                    equals: Some("255".into()),
                    ..c()
                },
                "254",
                0
            )
            .0
        );
        assert!(
            judge(
                &Check {
                    regex: Some("^name=Ada$".into()),
                    ..c()
                },
                "x\nname=Ada\ny",
                0
            )
            .0
        );
        assert!(
            !judge(
                &Check {
                    contains: Some("a".into()),
                    ..c()
                },
                "b",
                0
            )
            .0
        );
        assert!(
            !judge(
                &Check {
                    not_contains: Some("a".into()),
                    ..c()
                },
                "a",
                0
            )
            .0
        );
        assert!(
            judge(
                &Check {
                    min: Some(1.3),
                    ..c()
                },
                "1.5",
                0
            )
            .0
        );
        assert!(
            !judge(
                &Check {
                    min: Some(1.3),
                    ..c()
                },
                "1.0",
                0
            )
            .0
        );
        assert!(
            !judge(
                &Check {
                    min: Some(1.3),
                    ..c()
                },
                "abc",
                0
            )
            .0
        );
        // bare check: exit status
        assert!(judge(&c(), "", 0).0);
        assert!(!judge(&c(), "", 1).0);
    }
    #[test]
    fn loads_dir() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("a.json"),
            r#"{"id":"a/1","os":"android","instruction":"i","check":[{"cmd":"true"}]}"#,
        )
        .unwrap();
        std::fs::write(
            d.path().join("b.json"),
            r#"[{"id":"a/2","os":"android","instruction":"i","check":[{"cmd":"true"}]}]"#,
        )
        .unwrap();
        let m = load_dir(d.path()).unwrap();
        assert_eq!(m.len(), 2);
        std::fs::write(
            d.path().join("c.json"),
            r#"{"id":"a/2","os":"android","instruction":"i","check":[{"cmd":"true"}]}"#,
        )
        .unwrap();
        assert!(load_dir(d.path()).is_err());
    }
}
