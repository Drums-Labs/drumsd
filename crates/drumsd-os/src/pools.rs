//! Pools of Android and Windows desktops behind the drumsd slot API.
//!
//! Each pool names one backend and one clean image; `POST /v1/slots {"checkpoint": "<pool name>"}` makes a desktop
//! from it. A desktop's lifecycle and validity rules follow the Linux slots (docs/validity.md): every create and
//! reset starts an episode with gates, an episode is valid only when every gate passed, and a failed gate
//! quarantines the desktop.
//!
//! Gates: `process` (the container/VM/emulator runs), `guest_ready` (adb boot completed / guest server answers),
//! `state_hash` (the probe of user-visible state equals the pool's clean hash), `isolation` (no other episode's
//! canary is present; this episode's canary is written), `task_setup` (the task's setup commands exited 0),
//! `alive_after_resume` (still running and answering `alive_after_s` after ready; judged in the background and again
//! at verify).

use crate::android::{AndroidCfg, AndroidDesk};
use crate::tasks::{judge, Task};
use crate::windows::{WinDesk, WinImage, WindowsCfg};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

fn d_max() -> usize {
    64
}
fn d_alive() -> f64 {
    5.0
}
fn d_conc() -> usize {
    8
}
fn d_excl() -> String {
    "^$".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoolCfg {
    /// Also the image id clients create from.
    pub name: String,
    pub port_base: u16,
    #[serde(default = "d_max")]
    pub max_desktops: usize,
    #[serde(default = "d_conc")]
    pub max_concurrent_starts: usize,
    #[serde(default)]
    pub android: Option<AndroidCfg>,
    #[serde(default)]
    pub windows: Option<WindowsCfg>,
    /// State-hash probe override (Android: sh; Windows: PowerShell). `{EXCLUDE}` = probe_exclude.
    #[serde(default)]
    pub probe: Option<String>,
    /// Windows: run the probe in the guest's Python (`windows.python`) instead of PowerShell ("python" or "powershell").
    #[serde(default)]
    pub probe_kind: Option<String>,
    /// Regex of settings/registry names that change on their own between clean starts (measured, see the study).
    #[serde(default = "d_excl")]
    pub probe_exclude: String,
    /// Expected clean state hash. Unset: the first clean desktop's hash is adopted (event `clean_hash_adopted`).
    #[serde(default)]
    pub clean_hash: Option<String>,
    #[serde(default = "d_alive")]
    pub alive_after_s: f64,
    /// Task JSON files for this pool (tasks.rs).
    #[serde(default)]
    pub tasks_dir: Option<PathBuf>,
}

fn d_listen() -> String {
    "127.0.0.1:7070".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OsConfig {
    #[serde(default = "d_listen")]
    pub listen: String,
    pub token_file: Option<PathBuf>,
    #[serde(default)]
    pub allow_remote: bool,
    pub journal_dir: PathBuf,
    /// For cost per desktop-hour in /v1/health (host price, USD per hour; dated in the study's MANIFEST).
    #[serde(default)]
    pub host_usd_per_hour: f64,
    pub pools: Vec<PoolCfg>,
}

impl OsConfig {
    pub fn load(path: &str) -> Result<OsConfig, String> {
        let b = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        let c: OsConfig = serde_json::from_slice(&b).map_err(|e| format!("{path}: {e}"))?;
        c.validate()?;
        Ok(c)
    }
    pub fn validate(&self) -> Result<(), String> {
        let mut names = std::collections::BTreeSet::new();
        for p in &self.pools {
            if !names.insert(p.name.clone()) {
                return Err(format!("pool {} twice", p.name));
            }
            if p.android.is_some() == p.windows.is_some() {
                return Err(format!("pool {}: exactly one of android, windows", p.name));
            }
        }
        let mut ranges: Vec<(u32, u32, &str)> =
            self.pools.iter().map(|p| (p.port_base as u32, p.port_base as u32 + 2 * p.max_desktops as u32, p.name.as_str())).collect();
        ranges.sort();
        for w in ranges.windows(2) {
            if w[0].1 > w[1].0 {
                return Err(format!("pools {} and {} have overlapping port ranges", w[0].2, w[1].2));
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct OsError {
    pub status: u16,
    pub message: String,
}
impl OsError {
    pub fn new(status: u16, m: impl Into<String>) -> OsError {
        OsError { status, message: m.into() }
    }
}
type R<T> = Result<T, OsError>;
fn e500(m: impl Into<String>) -> OsError {
    OsError::new(500, m)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Gate {
    pub name: String,
    /// null while pending
    pub ok: Option<bool>,
    pub detail: String,
    pub seconds: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SlotView {
    pub id: String,
    pub pool: String,
    pub os: String,
    pub backend: String,
    pub image: String,
    pub state: String,
    pub reason: Option<String>,
    pub episode: u64,
    pub task_id: Option<String>,
    pub instruction: Option<String>,
    pub verdict: String,
    pub gates: Vec<Gate>,
    pub cost: Value,
    pub steps: u64,
    pub port: u16,
    pub created_epoch: f64,
}

pub enum Desk {
    A(AndroidDesk),
    W(WinDesk),
}

pub struct Slot {
    pub id: String,
    pub pool: Arc<PoolRt>,
    pub desk: tokio::sync::Mutex<Desk>,
    pub view: RwLock<SlotView>,
    pub task: RwLock<Option<Task>>,
    pub index: usize,
}

pub struct PoolRt {
    pub cfg: PoolCfg,
    pub clean_hash: RwLock<Option<String>>,
    pub starts: tokio::sync::Semaphore,
    pub tasks: BTreeMap<String, Task>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SavedState {
    pub id: String,
    pub pool: String,
    pub from_slot: String,
    pub image: WinImage,
    pub created_epoch: f64,
}

pub struct OsPools {
    pub cfg: OsConfig,
    pub pools: BTreeMap<String, Arc<PoolRt>>,
    pub slots: RwLock<BTreeMap<String, Arc<Slot>>>,
    pub states: RwLock<BTreeMap<String, SavedState>>,
    events: Mutex<std::fs::File>,
    next: Mutex<u64>,
}

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()
}
fn sha(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

impl OsPools {
    pub fn new(cfg: OsConfig) -> Result<Arc<OsPools>, String> {
        std::fs::create_dir_all(&cfg.journal_dir).map_err(|e| e.to_string())?;
        let ev = std::fs::OpenOptions::new().create(true).append(true).open(cfg.journal_dir.join("events.jsonl")).map_err(|e| e.to_string())?;
        let mut pools = BTreeMap::new();
        for p in &cfg.pools {
            let tasks = match &p.tasks_dir {
                Some(d) => crate::tasks::load_dir(d)?,
                None => BTreeMap::new(),
            };
            pools.insert(
                p.name.clone(),
                Arc::new(PoolRt {
                    cfg: p.clone(),
                    clean_hash: RwLock::new(p.clean_hash.clone()),
                    starts: tokio::sync::Semaphore::new(p.max_concurrent_starts.max(1)),
                    tasks,
                }),
            );
        }
        Ok(Arc::new(OsPools {
            cfg,
            pools,
            slots: RwLock::new(BTreeMap::new()),
            states: RwLock::new(BTreeMap::new()),
            events: Mutex::new(ev),
            next: Mutex::new(0),
        }))
    }

    pub fn event(&self, kind: &str, mut v: Value) {
        v["event"] = json!(kind);
        v["t"] = json!(now());
        let mut f = self.events.lock().unwrap();
        let _ = writeln!(f, "{}", v);
        let _ = f.flush();
    }

    /// Remove leftovers of a previous daemon (no adoption: a restart starts from a clean slate).
    pub async fn cleanup_stale(&self) {
        for p in self.pools.values() {
            if let Some(a) = &p.cfg.android {
                let o = crate::exec::run(&a.docker, &["ps", "-aq", "--filter", "name=^drums-os-"], None, crate::exec::secs(30.0)).await;
                for id in o.text().split_whitespace() {
                    let _ = crate::exec::run(&a.docker, &["rm", "-f", id], None, crate::exec::secs(30.0)).await;
                }
                let _ = crate::exec::run(
                    "sh",
                    &["-c", &format!("grep ' {}/' /proc/mounts | cut -d' ' -f2 | xargs -r -n1 umount -l", a.work_dir.display())],
                    None,
                    crate::exec::secs(30.0),
                )
                .await;
            }
            // per-desktop state of an earlier daemon (overlay uppers, emulator logs, VM overlays) is never reused
            let wd = p.cfg.android.as_ref().map(|a| a.work_dir.clone()).or(p.cfg.windows.as_ref().map(|w| w.work_dir.clone()));
            if let Some(wd) = wd {
                // its processes (VMs, emulators) are killed by their recorded process group
                let n = crate::exec::kill_recorded_groups(&wd);
                if n > 0 {
                    self.event("stale_killed", json!({"pool": p.cfg.name, "groups": n}));
                }
                if let Ok(rd) = std::fs::read_dir(&wd) {
                    for e in rd.flatten() {
                        if e.file_name().to_string_lossy().starts_with("drums-os-") {
                            let _ = crate::exec::run("rm", &["-rf", &e.path().to_string_lossy()], None, crate::exec::secs(600.0)).await;
                        }
                    }
                }
            }
        }
    }

    pub fn is_os_slot(&self, id: &str) -> bool {
        self.slots.read().unwrap().contains_key(id)
    }
    pub fn owns_image(&self, image: &str) -> bool {
        self.pools.contains_key(image) || self.states.read().unwrap().contains_key(image)
    }

    pub fn slot(&self, id: &str) -> R<Arc<Slot>> {
        self.slots.read().unwrap().get(id).cloned().ok_or_else(|| OsError::new(404, format!("no env {id}")))
    }

    fn resolve_task(&self, pool: &PoolRt, task: Option<Value>) -> R<Option<Task>> {
        match task {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(id)) => {
                let t = pool.tasks.get(&id).cloned().or_else(|| {
                    let m: Vec<_> = pool.tasks.values().filter(|t| id.len() >= 8 && t.id.starts_with(&id)).collect();
                    (m.len() == 1).then(|| m[0].clone())
                });
                t.map(Some).ok_or_else(|| OsError::new(404, format!("no task {id} in pool {}", pool.cfg.name)))
            }
            Some(v) => serde_json::from_value(v).map(Some).map_err(|e| OsError::new(400, format!("task: {e}"))),
        }
    }

    pub async fn create(self: &Arc<Self>, image: &str, task: Option<Value>) -> R<SlotView> {
        let (pool, win_image) = if let Some(p) = self.pools.get(image) {
            (p.clone(), None)
        } else if let Some(s) = self.states.read().unwrap().get(image) {
            (self.pools[&s.pool].clone(), Some(s.image.clone()))
        } else {
            return Err(OsError::new(404, format!("no image or saved state {image}")));
        };
        let task = self.resolve_task(&pool, task)?;
        if let Some(t) = &task {
            let os = if pool.cfg.android.is_some() { "android" } else { "windows" };
            if t.os != os {
                return Err(OsError::new(400, format!("task {} is for {}, pool {} is {os}", t.id, t.os, pool.cfg.name)));
            }
        }
        let n = {
            let mut g = self.next.lock().unwrap();
            *g += 1;
            *g
        };
        let (prefix, os) = if pool.cfg.android.is_some() { ("a", "android") } else { ("w", "windows") };
        let id = format!("{prefix}{n:05}");
        let name = format!("drums-os-{id}");
        // choose a free index (port) and insert the slot under one write lock: concurrent creates never share a port
        let slot = {
            let mut slots = self.slots.write().unwrap();
            let used: std::collections::BTreeSet<usize> =
                slots.values().filter(|s| s.pool.cfg.name == pool.cfg.name && s.view.read().unwrap().state != "destroyed").map(|s| s.index).collect();
            if used.len() >= pool.cfg.max_desktops {
                return Err(OsError::new(429, format!("pool {} is full ({} desktops)", pool.cfg.name, pool.cfg.max_desktops)));
            }
            let index = (0..pool.cfg.max_desktops).find(|i| !used.contains(i)).unwrap();
            let (desk, backend, port) = if let Some(a) = &pool.cfg.android {
                let port = match a.mode {
                    // emulator console ports must be even; adb uses console + 1
                    crate::android::AndroidMode::Emulator => pool.cfg.port_base + 2 * index as u16,
                    crate::android::AndroidMode::Redroid => pool.cfg.port_base + index as u16,
                };
                let backend = match a.mode {
                    crate::android::AndroidMode::Emulator => "android-emulator",
                    crate::android::AndroidMode::Redroid => "android-redroid",
                };
                (Desk::A(AndroidDesk::new(a.clone(), name, port)), backend, port)
            } else {
                let w = pool.cfg.windows.as_ref().unwrap();
                let port = pool.cfg.port_base + index as u16;
                let img = win_image.clone().unwrap_or_else(|| WinDesk::clean_image(w));
                (Desk::W(WinDesk::new(w.clone(), name, port, img)), "windows-qemu", port)
            };
            let view = SlotView {
                id: id.clone(),
                pool: pool.cfg.name.clone(),
                os: os.into(),
                backend: backend.into(),
                image: image.into(),
                state: "creating".into(),
                reason: None,
                episode: 0,
                task_id: task.as_ref().map(|t| t.id.clone()),
                instruction: task.as_ref().map(|t| t.instruction.clone()),
                verdict: "pending".into(),
                gates: vec![],
                cost: json!({}),
                steps: 0,
                port,
                created_epoch: now(),
            };
            let slot = Arc::new(Slot {
                id: id.clone(),
                pool: pool.clone(),
                desk: tokio::sync::Mutex::new(desk),
                view: RwLock::new(view),
                task: RwLock::new(task),
                index,
            });
            slots.insert(id.clone(), slot.clone());
            slot
        };
        let op_t0 = std::time::Instant::now();
        let mut desk = slot.desk.lock().await;
        let r = {
            let _permit = pool.starts.acquire().await.unwrap();
            match &mut *desk {
                Desk::A(d) => d.start().await,
                Desk::W(d) => d.start().await,
            }
        };
        self.event("create", json!({"slot": id, "pool": pool.cfg.name, "image": image, "ok": r.is_ok(), "timing": r.as_ref().ok(), "error": r.as_ref().err()}));
        self.begin_episode(&slot, &mut desk, r, "create", op_t0).await
    }

    /// After a start or reset: run the gates and the task setup; returns the view.
    async fn begin_episode(
        self: &Arc<Self>,
        slot: &Arc<Slot>,
        desk: &mut Desk,
        started: Result<Value, String>,
        op: &str,
        op_t0: std::time::Instant,
    ) -> R<SlotView> {
        let mut gates = vec![];
        let timing = match started {
            Ok(t) => t,
            Err(e) => {
                let mut v = slot.view.write().unwrap();
                v.state = "down".into();
                v.reason = Some(format!("{op} failed: {e}"));
                v.verdict = "invalid".into();
                return Err(e500(format!("{op} failed: {e}")));
            }
        };
        let episode = {
            let mut v = slot.view.write().unwrap();
            v.episode += 1;
            v.steps = 0;
            v.episode
        };
        let t = std::time::Instant::now();
        let alive = match desk {
            Desk::A(d) => d.alive().await,
            Desk::W(d) => d.alive(),
        };
        gates.push(Gate {
            name: "process".into(),
            ok: Some(alive),
            detail: if alive { "running".into() } else { "not running".into() },
            seconds: t.elapsed().as_secs_f64(),
        });
        gates.push(Gate {
            name: "guest_ready".into(),
            ok: Some(true),
            detail: "answered during start".into(),
            seconds: timing["total_s"].as_f64().unwrap_or(0.0),
        });
        // state hash and canary in one desktop command (the probe's output is taken before the canary is written)
        let t = std::time::Instant::now();
        let canary = format!("drums_canary_{}_{}", slot.id, episode);
        let probe = self.probe(slot, desk, Some(&canary)).await;
        let probe_s = t.elapsed().as_secs_f64();
        let (probe, canary_ok) = match probe {
            Ok(text) => match text.trim_end().strip_suffix(CANARY_OK) {
                Some(rest) => (Ok(rest.to_string()), true),
                None => (Ok(text), false),
            },
            Err(e) => (Err(e), false),
        };
        let g = match probe {
            Ok(text) => {
                let h = sha(&text);
                let want = slot.pool.clean_hash.read().unwrap().clone();
                match want {
                    Some(w) if w == h => Gate { name: "state_hash".into(), ok: Some(true), detail: h, seconds: probe_s },
                    Some(w) => {
                        let p = self.cfg.journal_dir.join(format!("probe-{}-{}.txt", slot.id, episode));
                        let _ = std::fs::write(&p, &text);
                        Gate {
                            name: "state_hash".into(),
                            ok: Some(false),
                            detail: format!("{h} != clean {w} (probe saved to {})", p.display()),
                            seconds: probe_s,
                        }
                    }
                    None => {
                        *slot.pool.clean_hash.write().unwrap() = Some(h.clone());
                        let p = self.cfg.journal_dir.join(format!("clean-probe-{}.txt", slot.pool.cfg.name));
                        let _ = std::fs::write(&p, &text);
                        self.event("clean_hash_adopted", json!({"pool": slot.pool.cfg.name, "slot": slot.id, "hash": h, "probe": p}));
                        Gate { name: "state_hash".into(), ok: Some(true), detail: format!("{h} (adopted as the pool's clean hash)"), seconds: probe_s }
                    }
                }
            }
            Err(e) => Gate { name: "state_hash".into(), ok: Some(false), detail: format!("probe failed: {e}"), seconds: probe_s },
        };
        gates.push(g);
        // isolation: the clean hash proved no other canary is present; this episode's canary was written after the probe
        gates.push(Gate {
            name: "isolation".into(),
            ok: Some(canary_ok),
            detail: if canary_ok { format!("canary {canary} written") } else { format!("canary {canary} not confirmed") },
            seconds: 0.0,
        });
        // the desktop is back and proven clean: everything up to here is the reset (or create) latency
        let ready_s = op_t0.elapsed().as_secs_f64();
        // task setup
        let t_setup = std::time::Instant::now();
        let task = slot.task.read().unwrap().clone();
        if let Some(task) = &task {
            let t = std::time::Instant::now();
            let mut ok = true;
            let mut detail = String::new();
            for c in &task.setup {
                match self.run_cmd(desk, c, 120.0).await {
                    Ok((0, _)) => {}
                    Ok((rc, o)) => {
                        ok = false;
                        detail = format!("{c:?} exit {rc}: {}", o.chars().take(300).collect::<String>());
                        break;
                    }
                    Err(e) => {
                        ok = false;
                        detail = format!("{c:?}: {e}");
                        break;
                    }
                }
            }
            gates.push(Gate {
                name: "task_setup".into(),
                ok: Some(ok),
                detail: if ok { format!("{} commands", task.setup.len()) } else { detail },
                seconds: t.elapsed().as_secs_f64(),
            });
        }
        gates.push(Gate {
            name: "alive_after_resume".into(),
            ok: None,
            detail: format!("judged {:.1} s after ready", slot.pool.cfg.alive_after_s),
            seconds: 0.0,
        });
        let failed: Vec<String> = gates.iter().filter(|g| g.ok == Some(false)).map(|g| format!("{}: {}", g.name, g.detail)).collect();
        {
            let mut v = slot.view.write().unwrap();
            v.gates = gates;
            v.cost =
                json!({"op": op, "timing": timing, "ready_s": ready_s, "setup_s": t_setup.elapsed().as_secs_f64(), "total_s": op_t0.elapsed().as_secs_f64()});
            if failed.is_empty() {
                v.state = "ready".into();
                v.reason = None;
                v.verdict = "pending".into();
            } else {
                v.state = "quarantined".into();
                v.reason = Some(failed.join("; "));
                v.verdict = "invalid".into();
            }
        }
        self.event("episode", json!({"slot": slot.id, "op": op, "episode": episode, "view": *slot.view.read().unwrap()}));
        // alive gate in the background
        let me = self.clone();
        let s2 = slot.clone();
        let after = slot.pool.cfg.alive_after_s;
        tokio::spawn(async move {
            tokio::time::sleep(crate::exec::secs(after)).await;
            me.judge_alive(&s2, episode).await;
        });
        Ok(slot.view.read().unwrap().clone())
    }

    async fn judge_alive(&self, slot: &Arc<Slot>, episode: u64) {
        let t = std::time::Instant::now();
        let ok = {
            let mut desk = slot.desk.lock().await;
            if slot.view.read().unwrap().episode != episode {
                return;
            }
            let alive = match &mut *desk {
                Desk::A(d) => d.alive().await && d.adb.booted().await,
                Desk::W(d) => d.alive() && d.wait_guest(5.0).await.is_ok(),
            };
            alive
        };
        let mut v = slot.view.write().unwrap();
        if v.episode != episode {
            return;
        }
        if let Some(g) = v.gates.iter_mut().find(|g| g.name == "alive_after_resume") {
            g.ok = Some(ok);
            g.seconds = t.elapsed().as_secs_f64();
            g.detail = if ok { "running and answering".into() } else { "not running or not answering".into() };
        }
        let all = v.gates.iter().all(|g| g.ok == Some(true));
        if !ok {
            v.state = "quarantined".into();
            v.reason = Some("gate alive_after_resume failed".into());
            v.verdict = "invalid".into();
        } else if all && v.state == "ready" {
            v.verdict = "valid".into();
        }
    }

    async fn probe(&self, slot: &Slot, desk: &Desk, canary: Option<&str>) -> Result<String, String> {
        let pc = &slot.pool.cfg;
        let p = pc.probe.clone().unwrap_or_else(|| match desk {
            Desk::A(_) => AndroidDesk::PROBE.to_string(),
            Desk::W(_) => WinDesk::PROBE.to_string(),
        });
        if let (Desk::W(d), Some("python")) = (desk, pc.probe_kind.as_deref()) {
            let code = pc
                .probe
                .clone()
                .unwrap_or_else(|| WinDesk::PROBE_PY.to_string())
                .replace("{EXCLUDE}", &pc.probe_exclude)
                .replace("{CANARY}", canary.unwrap_or(""));
            let v = d.execute(json!([d.cfg.python, "-c", code]), false, 60.0).await?;
            let rc = v["returncode"].as_i64().unwrap_or(-1);
            if rc != 0 {
                return Err(format!("probe exit {rc}: {}", v["error"].as_str().unwrap_or("").chars().take(300).collect::<String>()));
            }
            return Ok(v["output"].as_str().unwrap_or("").replace("\r\n", "\n"));
        }
        let mut cmd = p.replace("{EXCLUDE}", &pc.probe_exclude);
        if let Some(c) = canary {
            cmd = match desk {
                Desk::A(_) => format!("{cmd}; {} && echo {CANARY_OK}", canary_write(desk, c)),
                Desk::W(_) => format!("{cmd}; {}; if ($?) {{ '{CANARY_OK}' }}", canary_write(desk, c)),
            };
        }
        let (rc, out) = self.run_cmd(desk, &cmd, 60.0).await?;
        if rc != 0 && out.trim().is_empty() {
            return Err(format!("probe exit {rc}"));
        }
        Ok(out)
    }

    /// Run a command in the desktop (Android sh / Windows PowerShell). Returns (rc, stdout).
    pub async fn run_cmd(&self, desk: &Desk, cmd: &str, timeout_s: f64) -> Result<(i32, String), String> {
        match desk {
            Desk::A(d) => {
                let o = d.shell(cmd, timeout_s).await;
                if o.code == -1 || o.code == -2 {
                    return Err(o.stderr);
                }
                Ok((o.code, o.text()))
            }
            Desk::W(d) => d.powershell(cmd, timeout_s).await.map(|(rc, o, _)| (rc, o)),
        }
    }

    pub async fn reset(self: &Arc<Self>, id: &str) -> R<SlotView> {
        let slot = self.slot(id)?;
        let op_t0 = std::time::Instant::now();
        let mut desk = slot.desk.lock().await;
        {
            let mut v = slot.view.write().unwrap();
            match v.state.as_str() {
                "ready" | "down" => v.state = "resetting".into(),
                s => return Err(OsError::new(409, format!("env is {s}: reset is allowed from ready, down"))),
            }
        }
        let r = match &mut *desk {
            Desk::A(d) => d.reset().await,
            Desk::W(d) => d.reset().await,
        };
        self.event("reset", json!({"slot": id, "ok": r.is_ok(), "timing": r.as_ref().ok(), "error": r.as_ref().err()}));
        self.begin_episode(&slot, &mut desk, r, "reset", op_t0).await
    }

    pub async fn step(self: &Arc<Self>, id: &str, action: Value, screenshot: bool) -> R<Value> {
        let slot = self.slot(id)?;
        let desk = slot.desk.lock().await;
        let st = slot.view.read().unwrap().state.clone();
        if st != "ready" {
            return Err(OsError::new(409, format!("env is {st}: step needs ready")));
        }
        let t = std::time::Instant::now();
        let r = match &*desk {
            Desk::A(d) => d.act(&action).await,
            Desk::W(d) => d.act(&action).await,
        };
        let act_s = t.elapsed().as_secs_f64();
        let res = r.map_err(|e| OsError::new(502, format!("action failed: {e}")))?;
        let mut out = json!({"result": res, "seconds": act_s});
        if screenshot {
            let t2 = std::time::Instant::now();
            let png = match &*desk {
                Desk::A(d) => d.screenshot().await,
                Desk::W(d) => d.screenshot().await,
            }
            .map_err(|e| OsError::new(502, e))?;
            out["observation"] = json!({"png_b64": b64(&png), "bytes": png.len(), "seconds": t2.elapsed().as_secs_f64()});
        }
        let v = {
            let mut v = slot.view.write().unwrap();
            v.steps += 1;
            v.clone()
        };
        out["episode"] = json!(v.episode);
        out["validity"] = json!({"verdict": v.verdict, "gates": v.gates});
        out["total_seconds"] = json!(t.elapsed().as_secs_f64());
        Ok(out)
    }

    pub async fn screenshot(&self, id: &str) -> R<Vec<u8>> {
        let slot = self.slot(id)?;
        let desk = slot.desk.lock().await;
        match &*desk {
            Desk::A(d) => d.screenshot().await,
            Desk::W(d) => d.screenshot().await,
        }
        .map_err(|e| OsError::new(502, e))
    }

    pub async fn load_task(self: &Arc<Self>, id: &str, task: Value) -> R<SlotView> {
        let slot = self.slot(id)?;
        let t = self.resolve_task(&slot.pool, Some(task))?;
        {
            let mut v = slot.view.write().unwrap();
            v.task_id = t.as_ref().map(|t| t.id.clone());
            v.instruction = t.as_ref().map(|t| t.instruction.clone());
        }
        *slot.task.write().unwrap() = t;
        // the task's setup belongs to an episode: start one
        self.reset(id).await
    }

    /// Run the task's checker; reward 1 only when every check passed. `valid` = every gate passed (incl. the
    /// alive gate, judged now if still pending, and the canary check).
    pub async fn verify(self: &Arc<Self>, id: &str) -> R<Value> {
        let slot = self.slot(id)?;
        let task = slot.task.read().unwrap().clone().ok_or_else(|| OsError::new(400, "env has no task"))?;
        let t0 = std::time::Instant::now();
        let mut desk = slot.desk.lock().await;
        let episode = slot.view.read().unwrap().episode;
        let mut checks = vec![];
        let mut pass = true;
        for c in &task.check {
            let (ok, detail) = match self.run_cmd(&desk, &c.cmd, 120.0).await {
                Ok((rc, out)) => judge(c, &out, rc),
                Err(e) => (false, format!("checker did not run: {e}")),
            };
            pass &= ok;
            checks.push(json!({"cmd": c.cmd, "pass": ok, "detail": detail}));
        }
        // canary: exactly this episode's
        let mine = format!("drums_canary_{}_{}", slot.id, episode);
        let list = self.run_cmd(&desk, &canary_list(&desk), 30.0).await;
        let iso_ok = match &list {
            Ok((_, out)) => {
                out.split_whitespace().map(|s| s.rsplit(['/', '\\']).next().unwrap_or(s)).filter(|s| s.starts_with("drums_canary_")).collect::<Vec<_>>()
                    == vec![mine.as_str()]
            }
            Err(_) => false,
        };
        let alive = match &mut *desk {
            Desk::A(d) => d.alive().await,
            Desk::W(d) => d.alive(),
        };
        drop(desk);
        let v = {
            let mut v = slot.view.write().unwrap();
            if !iso_ok {
                v.gates.push(Gate {
                    name: "isolation_end".into(),
                    ok: Some(false),
                    detail: format!("canaries at verify: {:?}", list.as_ref().map(|x| x.1.trim().to_string())),
                    seconds: 0.0,
                });
                v.verdict = "invalid".into();
            }
            if !alive {
                v.verdict = "invalid".into();
            }
            if let Some(g) = v.gates.iter_mut().find(|g| g.name == "alive_after_resume" && g.ok.is_none()) {
                g.ok = Some(alive);
                g.detail = "judged at verify".into();
            }
            if v.verdict == "pending" && v.gates.iter().all(|g| g.ok == Some(true)) {
                v.verdict = "valid".into();
            }
            v.clone()
        };
        let reward = if pass { 1.0 } else { 0.0 };
        let out = json!({"env": id, "episode": episode, "task_id": task.id, "reward": reward, "valid": v.verdict == "valid", "verdict": v.verdict, "checks": checks, "gates": v.gates, "seconds": t0.elapsed().as_secs_f64()});
        self.event("verify", out.clone());
        Ok(out)
    }

    pub async fn capture(self: &Arc<Self>, id: &str, name: Option<String>) -> R<Value> {
        let slot = self.slot(id)?;
        let mut desk = slot.desk.lock().await;
        let Desk::W(d) = &mut *desk else {
            return Err(OsError::new(
                501,
                "capture is not supported by the android backends (redroid has no memory snapshot; read-only emulators cannot save snapshots)",
            ));
        };
        if slot.view.read().unwrap().state != "ready" {
            return Err(OsError::new(409, "capture needs ready"));
        }
        let sid = name.unwrap_or_else(|| format!("{}-s{}", id, (now() * 1000.0) as u64));
        if self.owns_image(&sid) {
            return Err(OsError::new(409, format!("{sid} exists")));
        }
        let dir = d.cfg.work_dir.join("states").join(&sid);
        let (img, timing) = d.capture(&dir).await.map_err(e500)?;
        let st = SavedState { id: sid.clone(), pool: slot.pool.cfg.name.clone(), from_slot: id.into(), image: img, created_epoch: now() };
        self.states.write().unwrap().insert(sid.clone(), st.clone());
        let out = json!({"id": sid, "state": st, "timing": timing});
        self.event("capture", out.clone());
        Ok(out)
    }

    pub async fn branch(self: &Arc<Self>, id: &str, n: usize) -> R<Value> {
        let t0 = std::time::Instant::now();
        let cap = self.capture(id, None).await?;
        let sid = cap["id"].as_str().unwrap().to_string();
        let task = self.slot(id)?.task.read().unwrap().clone().map(|t| serde_json::to_value(t).unwrap());
        let mut futs = vec![];
        for _ in 0..n {
            let me = self.clone();
            let s = sid.clone();
            let t = task.clone();
            futs.push(tokio::spawn(async move { me.create_from_state(&s, t).await }));
        }
        let mut children = vec![];
        for f in futs {
            match f.await {
                Ok(Ok(v)) => children.push(json!(v)),
                Ok(Err(e)) => children.push(json!({"error": e.message})),
                Err(e) => children.push(json!({"error": e.to_string()})),
            }
        }
        let out = json!({"state": sid, "capture": cap, "children": children, "seconds": t0.elapsed().as_secs_f64()});
        self.event("branch", out.clone());
        Ok(out)
    }

    /// A child of a saved state: the task's setup is NOT re-run (the child continues the parent's episode state).
    async fn create_from_state(self: &Arc<Self>, sid: &str, task: Option<Value>) -> R<SlotView> {
        let v = self.create(sid, None).await?;
        if let Some(t) = task {
            let slot = self.slot(&v.id)?;
            let t: Task = serde_json::from_value(t).map_err(|e| e500(e.to_string()))?;
            let mut vw = slot.view.write().unwrap();
            vw.task_id = Some(t.id.clone());
            vw.instruction = Some(t.instruction.clone());
            *slot.task.write().unwrap() = Some(t);
        }
        Ok(self.slot(&v.id)?.view.read().unwrap().clone())
    }

    pub async fn quarantine(&self, id: &str, reason: String) -> R<SlotView> {
        let slot = self.slot(id)?;
        let mut v = slot.view.write().unwrap();
        if v.state == "destroyed" {
            return Err(OsError::new(409, "env is destroyed"));
        }
        v.state = "quarantined".into();
        v.reason = Some(reason);
        v.verdict = "invalid".into();
        Ok(v.clone())
    }
    pub async fn release(&self, id: &str) -> R<SlotView> {
        let slot = self.slot(id)?;
        let mut v = slot.view.write().unwrap();
        if v.state != "quarantined" {
            return Err(OsError::new(409, format!("env is {}: release needs quarantined", v.state)));
        }
        v.state = "down".into();
        Ok(v.clone())
    }

    pub async fn destroy(&self, id: &str) -> R<SlotView> {
        let slot = self.slot(id)?;
        let mut desk = slot.desk.lock().await;
        let r = match &mut *desk {
            Desk::A(d) => d.stop().await,
            Desk::W(d) => d.stop().await,
        };
        let mut v = slot.view.write().unwrap();
        v.state = "destroyed".into();
        if let Err(e) = &r {
            v.reason = Some(format!("stop: {e}"));
        }
        self.event("destroy", json!({"slot": id, "ok": r.is_ok(), "error": r.err()}));
        Ok(v.clone())
    }

    pub fn list(&self, include_destroyed: bool) -> Vec<SlotView> {
        self.slots.read().unwrap().values().map(|s| s.view.read().unwrap().clone()).filter(|v| include_destroyed || v.state != "destroyed").collect()
    }

    pub fn images(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self.pools.values().map(|p| json!({"id": p.cfg.name, "kind": "image", "os": if p.cfg.android.is_some() {"android"} else {"windows"}, "clean_hash": *p.clean_hash.read().unwrap(), "tasks": p.tasks.len()})).collect();
        out.extend(self.states.read().unwrap().values().map(|s| json!({"id": s.id, "kind": "saved_state", "pool": s.pool, "from_slot": s.from_slot})));
        out
    }

    pub fn tasks(&self, os: Option<&str>) -> Vec<Value> {
        self.pools
            .values()
            .flat_map(|p| p.tasks.values().map(|t| json!({"id": t.id, "os": t.os, "pool": p.cfg.name, "instruction": t.instruction, "source": t.source})))
            .filter(|t| os.is_none_or(|o| t["os"] == o))
            .collect()
    }
    pub fn task(&self, id: &str) -> Option<Value> {
        self.pools.values().find_map(|p| p.tasks.get(id)).map(|t| serde_json::to_value(t).unwrap())
    }

    pub fn health(&self) -> Value {
        let slots = self.slots.read().unwrap();
        let mut per: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        for s in slots.values() {
            let v = s.view.read().unwrap();
            *per.entry(v.pool.clone()).or_default().entry(v.state.clone()).or_default() += 1;
        }
        let mem = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let kv =
            |k: &str| mem.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) / 1024;
        let live: usize = per.values().map(|m| m.iter().filter(|(k, _)| *k != "destroyed").map(|(_, n)| n).sum::<usize>()).sum();
        json!({"ok": true, "mode": "os", "pools": per, "images": self.images(), "host": {"mem_total_mib": kv("MemTotal:"), "mem_available_mib": kv("MemAvailable:"), "ksm_pages_sharing": std::fs::read_to_string("/sys/kernel/mm/ksm/pages_sharing").ok().map(|s| s.trim().to_string())},
               "live_desktops": live, "host_usd_per_hour": self.cfg.host_usd_per_hour, "usd_per_desktop_hour": if live > 0 { self.cfg.host_usd_per_hour / live as f64 } else { 0.0 }})
    }
}

const CANARY_OK: &str = "DRUMS_CANARY_OK";

fn canary_write(desk: &Desk, name: &str) -> String {
    match desk {
        Desk::A(_) => format!("touch /data/local/tmp/{name}"),
        Desk::W(_) => {
            format!("New-Item -ItemType File -Force -Path C:\\Users\\Public\\{name} | Out-Null")
        }
    }
}
fn canary_list(desk: &Desk) -> String {
    match desk {
        Desk::A(_) => "ls /data/local/tmp/ | grep drums_canary_ || true".into(),
        Desk::W(_) => "Get-ChildItem -Force -Name C:\\Users\\Public\\drums_canary_*".into(),
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
pub fn b64(d: &[u8]) -> String {
    let mut o = String::with_capacity(d.len().div_ceil(3) * 4);
    for c in d.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                o.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                o.push('=');
            }
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn base64() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
    }
    #[test]
    fn config_validation() {
        let a = r#"{"mode":"redroid","image":"x","golden_data":"/g","work_dir":"/w"}"#;
        let mk = |pools: &str| serde_json::from_str::<OsConfig>(&format!(r#"{{"journal_dir":"/j","pools":[{pools}]}}"#)).unwrap();
        assert!(mk(&format!(r#"{{"name":"a","port_base":6000,"max_desktops":10,"android":{a}}}"#)).validate().is_ok());
        assert!(mk(&format!(r#"{{"name":"a","port_base":6000,"android":{a}}},{{"name":"a","port_base":9000,"android":{a}}}"#)).validate().is_err());
        assert!(mk(&format!(r#"{{"name":"a","port_base":6000,"max_desktops":10,"android":{a}}},{{"name":"b","port_base":6010,"android":{a}}}"#))
            .validate()
            .is_err());
        assert!(mk(r#"{"name":"a","port_base":6000}"#).validate().is_err());
    }
}
