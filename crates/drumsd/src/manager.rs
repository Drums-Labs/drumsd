//! The slot manager: owns every slot's lifecycle.
//!
//! * One `tokio::Mutex` per slot serialises operations on it; reads go to a separate snapshot (`view`) so `GET`
//!   never waits for a reset.
//! * Every transition is journaled (record + event) before the work it starts.
//! * The restore pipeline is blocking code run on the blocking pool; a semaphore caps concurrent CRIU restores.
//! * Warm resources per slot: its cgroup (created once), its forwarder listeners (bound once), a spare upper/work pair
//!   (prepared in the background after every restore), and the checkpoint's template server (one per checkpoint).

use crate::config::Config;
use drumsd_core::alive;
use drumsd_core::quiesce as q;
use drumsd_core::gates::{Gate, GateReport, GateResult, GateStatus, Validity};
use drumsd_core::journal::Journal;
use drumsd_core::metrics::{Registry, LATENCY_BUCKETS};
use drumsd_core::recovery::{self, Action, Attempt, CostRecord, FailureClass, Policy};
use drumsd_core::state::{Op, Outcome, SlotState};
use drumsd_core::statehash;
use drumsd_core::usage::{self, Ledger, Usage};
use drumsd_linux::{cgroup, checks, guest::Guest, hook, procfs, proxy, quiesce, restore, slotfs, template};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore};

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    /// Extra fields merged into the error body (e.g. `code: "not_quiescent"` and the unstable processes).
    pub detail: Option<Value>,
}

impl ApiError {
    pub fn new(status: u16, m: impl Into<String>) -> Self {
        ApiError { status, message: m.into(), detail: None }
    }
    pub fn with(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }
}

type R<T> = Result<T, ApiError>;

fn e500(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(500, e.to_string())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlotRecord {
    pub id: String,
    pub num: u32,
    pub checkpoint: String,
    #[serde(flatten)]
    pub state: SlotState,
    pub episode: u64,
    pub init_pid: Option<i32>,
    pub init_start_ticks: Option<String>,
    pub restore_generation: u64,
    /// Leaf cgroup (1 or 2) that holds the running tree; 0 = the slot cgroup itself.
    #[serde(default)]
    pub leaf: u8,
    pub mode: String,
    pub port: u16,
    pub extra_ports: Vec<(u16, u16)>,
    pub gates: GateReport,
    pub validity: Validity,
    pub cost: CostRecord,
    pub timings: BTreeMap<String, f64>,
    pub last_error: Option<String>,
    pub task: Option<Value>,
    pub ready_epoch: Option<f64>,
    pub created_epoch: f64,
    pub updated_epoch: f64,
    /// OSWorld task setup runs after every restore of this slot (a slot made from a clean image with a task). False for
    /// fork children and from_state slots: their checkpoint already holds the task's start state.
    #[serde(default)]
    pub task_setup_on_restore: bool,
    /// This slot's own ledger (op latency, episodes, failures/retries by cause, rewards).
    #[serde(default)]
    pub usage: Ledger,
    /// The last verify result of the current episode.
    #[serde(default)]
    pub verified: Option<Value>,
    #[serde(default)]
    pub closed_epoch: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub id: String,
    /// The slot directory that holds the checkpoint (images + upper.snap + records).
    pub slot: u32,
    pub socket: PathBuf,
    pub server_pid: Option<i32>,
    pub argv_marker: String,
    pub expected_state_hash: Option<String>,
    pub hash_reference: String,
    pub captured_from: Option<String>,
    pub created_epoch: f64,
    pub server_ready_s: f64,
    /// The task of the slot this checkpoint was captured from (its setup already ran): slots made from it inherit it.
    #[serde(default)]
    pub task: Option<Value>,
    /// Capture readiness record: the quiescence wait, every dump attempt and why a rejected one was rejected.
    #[serde(default)]
    pub quiescence: Option<Value>,
}

pub struct Slot {
    pub id: String,
    pub op: Mutex<()>,
    pub view: RwLock<SlotRecord>,
    pub targets: Vec<proxy::Target>,
    pub proxies: StdMutex<Vec<tokio::task::JoinHandle<()>>>,
}

pub struct Manager {
    pub cfg: Config,
    pub exe: PathBuf,
    pub journal: Journal,
    pub metrics: Registry,
    pub slots: RwLock<BTreeMap<String, Arc<Slot>>>,
    pub ckpts: RwLock<BTreeMap<String, CheckpointRecord>>,
    restore_sem: Arc<Semaphore>,
    alloc: StdMutex<BTreeSet<u32>>,
    source_sha: String,
    limits: cgroup::Limits,
    natural: BTreeSet<String>,
    policy: Policy,
    pub usage: StdMutex<Usage>,
    pub token: String,
    pub started_epoch: f64,
    pub criu_sha256: String,
}

/// Per-request overrides of the capture-readiness wait (the daemon's `quiesce` config gives the defaults).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QuiesceOpts {
    pub timeout_s: Option<f64>,
    pub window_s: Option<f64>,
}

/// Write `<dir>/dumped-reference.json`: the alive gate's reference set taken from what CRIU dumped (its log).
pub fn write_dumped_reference(dir: &Path) -> Result<usize, String> {
    let b = std::fs::read(dir.join("dump.log")).map_err(|e| format!("dump.log: {e}"))?;
    let d = q::parse_dump_log(&String::from_utf8_lossy(&b))?;
    let r = q::dumped_reference(&d)?;
    let zombies: Vec<_> = d.values().filter(|t| t.state == q::TASK_DEAD).map(|t| json!([t.levels.last(), t.comm])).collect();
    let rec = json!({"procs": r, "source": "dump.log (CRIU's seized tasks)", "dumped_tasks": d.len(), "dumped_zombies": zombies,
                     "written_epoch": drumsd_linux::now_epoch()});
    std::fs::write(dir.join("dumped-reference.json"), serde_json::to_vec_pretty(&rec).unwrap()).map_err(|e| e.to_string())?;
    Ok(r.len())
}

pub fn required_gates() -> Vec<Gate> {
    Gate::ALL.to_vec()
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[derive(Debug)]
struct RestoreFailure {
    class: FailureClass,
    error: String,
    gates: GateReport,
    timings: BTreeMap<String, f64>,
}

struct RestoreOk {
    leaf: u8,
    old_cg: Option<PathBuf>,
    pid: i32,
    start_ticks: String,
    gates: GateReport,
    timings: BTreeMap<String, f64>,
    hook_dir: PathBuf,
    ready_at: Instant,
    ready_epoch: f64,
    upper_warm: bool,
}

impl Manager {
    pub fn new(cfg: Config) -> Result<Arc<Manager>, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let journal = Journal::open(&cfg.journal_dir).map_err(|e| format!("journal {}: {e}", cfg.journal_dir.display()))?;
        let source_sha = slotfs::source_sha256(&cfg.py_src.join("sharedos_rootfs")).map_err(|e| format!("python sources: {e}"))?;
        let metrics = Registry::default();
        metrics.counter("drumsd_ops_total", "Slot operations by op and outcome");
        metrics.counter("drumsd_restore_attempts_total", "Restore attempts by mode and result class");
        metrics.counter("drumsd_gate_results_total", "Gate results by gate and status");
        metrics.counter("drumsd_episodes_total", "Episodes by final validity");
        metrics.counter("drumsd_upper_spare_total", "Fresh upper layers by source (warm spare or cold copy)");
        metrics.counter("drumsd_recoveries_total", "Restores that succeeded after a retry");
        metrics.gauge("drumsd_slots", "Slots by state");
        metrics.gauge("drumsd_template_servers", "Template servers by liveness");
        metrics.histogram("drumsd_reset_seconds", "Reset latency, kill to all synchronous gates passed", LATENCY_BUCKETS);
        metrics.histogram("drumsd_create_seconds", "Create latency, branch dir to all synchronous gates passed", LATENCY_BUCKETS);
        metrics.histogram("drumsd_phase_seconds", "Restore pipeline phase latency", LATENCY_BUCKETS);
        metrics.histogram("drumsd_criu_restore_seconds", "CRIU's own restore clock (Restore finished successfully)", LATENCY_BUCKETS);
        let limits = cgroup::expected_limits(cfg.vcpus, cfg.memory_mib);
        let natural = cfg.natural_exits.iter().cloned().collect();
        let policy = Policy { max_attempts: cfg.max_attempts };
        let token = cfg.load_token()?;
        cfg.check_listen()?;
        let mut u: Usage = journal.get("meta", "usage").map_err(|e| format!("usage ledger: {e}"))?.unwrap_or_default();
        if u.window_start_epoch == 0.0 {
            u.window_start_epoch = drumsd_linux::now_epoch();
        }
        let criu_sha256 = std::fs::read(&cfg.criu).map(|b| hex_sha256(&b)).unwrap_or_else(|e| format!("unreadable: {e}"));
        let m = Manager {
            usage: StdMutex::new(u),
            token,
            started_epoch: drumsd_linux::now_epoch(),
            criu_sha256,
            restore_sem: Arc::new(Semaphore::new(cfg.max_concurrent_restores)),
            cfg,
            exe,
            journal,
            metrics,
            slots: RwLock::new(BTreeMap::new()),
            ckpts: RwLock::new(BTreeMap::new()),
            alloc: StdMutex::new(BTreeSet::new()),
            source_sha,
            limits,
            natural,
            policy,
        };
        Ok(Arc::new(m))
    }

    // ------------------------------------------------------------------------------------------------ records
    fn event(&self, ev: Value) {
        let mut ev = ev;
        ev["t"] = json!(drumsd_linux::now_epoch());
        if let Err(e) = self.journal.event(&ev) {
            tracing::error!("event log: {e}");
        }
    }

    fn put_slot(&self, s: &Slot, f: impl FnOnce(&mut SlotRecord)) -> SlotRecord {
        let mut v = s.view.write().unwrap();
        let from = v.state.name();
        f(&mut v);
        v.updated_epoch = drumsd_linux::now_epoch();
        if let Err(e) = self.journal.put("slots", &v.id, &*v) {
            tracing::error!("journal put {}: {e}", v.id);
        }
        if from != v.state.name() {
            self.event(json!({"slot": v.id, "from": from, "to": v.state.name(), "episode": v.episode, "state": v.state}));
        }
        let out = v.clone();
        drop(v);
        self.refresh_slot_gauges();
        out
    }

    fn refresh_slot_gauges(&self) {
        let mut counts: BTreeMap<&'static str, f64> = BTreeMap::new();
        for n in ["creating", "ready", "resetting", "capturing", "down", "quarantined", "destroying", "destroyed"] {
            counts.insert(n, 0.0);
        }
        for s in self.slots.read().unwrap().values() {
            *counts.entry(s.view.read().unwrap().state.name()).or_insert(0.0) += 1.0;
        }
        for (k, v) in counts {
            self.metrics.set("drumsd_slots", &[("state", k)], v);
        }
    }

    fn put_ckpt(&self, c: &CheckpointRecord) {
        if let Err(e) = self.journal.put("checkpoints", &c.id, c) {
            tracing::error!("journal put checkpoint {}: {e}", c.id);
        }
        self.ckpts.write().unwrap().insert(c.id.clone(), c.clone());
    }

    /// Apply `f` to the daemon ledger and (if given) the slot's ledger; both are journaled.
    fn account(&self, slot: Option<&Slot>, f: impl Fn(&mut Ledger)) {
        {
            let mut u = self.usage.lock().unwrap();
            f(&mut u.ledger);
            if let Err(e) = self.journal.put("meta", "usage", &*u) {
                tracing::error!("journal put usage: {e}");
            }
        }
        if let Some(s) = slot {
            let mut v = s.view.write().unwrap();
            f(&mut v.usage);
            if let Err(e) = self.journal.put("slots", &v.id, &*v) {
                tracing::error!("journal put {}: {e}", v.id);
            }
        }
    }

    pub fn slot(&self, id: &str) -> R<Arc<Slot>> {
        self.slots.read().unwrap().get(id).cloned().ok_or_else(|| ApiError::new(404, format!("no slot {id}")))
    }

    pub fn ckpt(&self, id: &str) -> R<CheckpointRecord> {
        self.ckpts.read().unwrap().get(id).cloned().ok_or_else(|| ApiError::new(404, format!("no checkpoint {id}")))
    }

    fn live_inits(&self, except: &str) -> Vec<i32> {
        self.slots
            .read()
            .unwrap()
            .values()
            .filter(|s| s.id != except)
            .filter_map(|s| {
                let v = s.view.read().unwrap();
                if matches!(v.state, SlotState::Ready | SlotState::Capturing) {
                    v.init_pid
                } else {
                    None
                }
            })
            .collect()
    }

    // ------------------------------------------------------------------------------------------------ checkpoints
    /// Adopt a checkpoint slot made by the existing desktop setup (`candidate snapshot`, `capture`), and keep a
    /// template server warm for it.
    pub async fn adopt_checkpoint(self: &Arc<Self>, id: String, slot: u32, expected_hash: Option<String>, captured_from: Option<String>) -> R<CheckpointRecord> {
        if self.ckpts.read().unwrap().contains_key(&id) {
            return Err(ApiError::new(409, format!("checkpoint {id} exists")));
        }
        let me = self.clone();
        let id2 = id.clone();
        tokio::task::spawn_blocking(move || {
            let sp = slotfs::paths(&me.cfg.state_dir, slot);
            for n in slotfs::REQUIRED_RECORDS {
                if !sp.dir.join(n).is_file() {
                    return Err(ApiError::new(400, format!("slot {slot} lacks {n}")));
                }
            }
            if !sp.snap.is_dir() || std::fs::read_dir(&sp.images).map(|mut d| d.next().is_none()).unwrap_or(true) {
                return Err(ApiError::new(400, format!("slot {slot} has no upper.snap + images")));
            }
            let sock = me.cfg.state_dir.join(format!("ps-{slot}.sock"));
            let srv = template::start(&me.cfg.criu, &sock, &sp.images, &me.cfg.state_dir).map_err(e500)?;
            let (expected, reference) = match expected_hash {
                Some(h) => (Some(h), "given".to_string()),
                None => {
                    // the origin slot, if it still runs: its guest server is the reference desktop
                    let g = Guest::new(me.cfg.port_base + slot as u16, Duration::from_secs(10));
                    match state_hash(&g) {
                        Ok(h) => (Some(h), format!("origin slot {slot} guest at adopt time")),
                        Err(_) => (None, "first child".to_string()),
                    }
                }
            };
            let c = CheckpointRecord {
                id: id2,
                slot,
                socket: sock,
                server_pid: Some(srv.pid),
                argv_marker: slotfs::argv_marker(&me.cfg.state_dir, slot),
                expected_state_hash: expected,
                hash_reference: reference,
                captured_from,
                created_epoch: drumsd_linux::now_epoch(),
                server_ready_s: round3(srv.ready_s),
                task: None,
                quiescence: None,
            };
            // the alive gate's reference set comes from the dump itself (CRIU's log of what it seized)
            if let Err(e) = write_dumped_reference(&sp.dir) {
                me.event(json!({"checkpoint": c.id, "dumped_reference": format!("not written: {e}")}));
            }
            me.put_ckpt(&c);
            me.event(json!({"checkpoint": c.id, "adopted_slot": slot, "server_pid": srv.pid, "ready_s": c.server_ready_s}));
            me.metrics.set("drumsd_template_servers", &[("state", "up")], me.ckpts.read().unwrap().len() as f64);
            Ok(c)
        })
        .await
        .map_err(e500)?
    }

    /// Delete a saved state: stop its template server, remove its images (records and logs are kept under
    /// state_dir/forgotten). Refused while any env made from it still exists, and for installed images.
    pub async fn delete_checkpoint(self: &Arc<Self>, id: &str) -> R<Value> {
        let c = self.ckpt(id)?;
        if c.captured_from.is_none() {
            return Err(ApiError::new(409, format!("checkpoint {id} is an installed image, not a saved state")));
        }
        let users: Vec<String> = self.slots.read().unwrap().values().filter_map(|s| {
            let v = s.view.read().unwrap();
            (v.checkpoint == id && !v.state.is_terminal()).then(|| v.id.clone())
        }).collect();
        if !users.is_empty() {
            return Err(ApiError::new(409, format!("checkpoint {id} is used by envs {users:?}: close them first")));
        }
        self.ckpts.write().unwrap().remove(id);
        let me = self.clone();
        let c2 = c.clone();
        let res = tokio::task::spawn_blocking(move || {
            let sp = slotfs::paths(&me.cfg.state_dir, c2.slot);
            if let Some(pid) = template::adopt(&c2.socket, &sp.images) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            let _ = std::fs::remove_file(&c2.socket);
            let _ = std::fs::remove_file(template::pidfile_for(&c2.socket));
            let _ = kill_slot(&slotfs::marker_for(&me.cfg.state_dir, c2.slot));
            let mut freed = 0u64;
            for d in ["images", "upper", "upper.snap", "work", "upper.next", "work.next", "upper.next.tmp", "trash", "criu-root"] {
                let p = sp.dir.join(d);
                if p.symlink_metadata().is_ok() {
                    freed += dir_bytes(&p);
                    std::fs::remove_dir_all(&p)?;
                }
            }
            let keep = me.cfg.state_dir.join("forgotten").join(format!("slot{}-{}", c2.slot, (drumsd_linux::now_epoch() * 1000.0) as u64));
            std::fs::create_dir_all(keep.parent().unwrap())?;
            if sp.dir.exists() {
                std::fs::rename(&sp.dir, &keep)?;
            }
            Ok::<_, std::io::Error>(freed)
        })
        .await
        .map_err(e500)?
        .map_err(e500)?;
        let _ = self.journal.remove("checkpoints", id);
        self.event(json!({"checkpoint": id, "deleted": true, "freed_bytes": res}));
        Ok(json!({"id": id, "deleted": true, "freed_bytes": res}))
    }

    fn ensure_server(&self, c: &CheckpointRecord) -> Result<i32, String> {
        let sp = slotfs::paths(&self.cfg.state_dir, c.slot);
        if let Some(pid) = template::adopt(&c.socket, &sp.images) {
            return Ok(pid);
        }
        let srv = template::start(&self.cfg.criu, &c.socket, &sp.images, &self.cfg.state_dir).map_err(|e| e.to_string())?;
        let mut c2 = c.clone();
        c2.server_pid = Some(srv.pid);
        self.put_ckpt(&c2);
        self.event(json!({"checkpoint": c.id, "server_restarted": srv.pid}));
        Ok(srv.pid)
    }

    // ------------------------------------------------------------------------------------------------ slots
    fn alloc_num(&self) -> R<u32> {
        // numbers handed out are reserved for the daemon's life (a capture's target dir appears only inside the verb)
        let mut reserved = self.alloc.lock().unwrap();
        let used: BTreeSet<u32> = self.slots.read().unwrap().values().map(|s| s.view.read().unwrap().num).collect();
        let ck: BTreeSet<u32> = self.ckpts.read().unwrap().values().map(|c| c.slot).collect();
        for n in self.cfg.slot_range.0..self.cfg.slot_range.1 {
            if !used.contains(&n) && !ck.contains(&n) && !reserved.contains(&n) && slotfs::paths(&self.cfg.state_dir, n).dir.symlink_metadata().is_err() {
                reserved.insert(n);
                return Ok(n);
            }
        }
        Err(ApiError::new(503, "no free slot number"))
    }

    async fn bind_proxies(&self, num: u32) -> R<(Vec<proxy::Target>, Vec<tokio::task::JoinHandle<()>>)> {
        let mut targets = vec![];
        let mut handles = vec![];
        let mut ports = vec![(self.cfg.port_base, self.cfg.guest_port)];
        ports.extend(self.cfg.extra_ports.iter().copied());
        for (base, gp) in ports {
            let t = proxy::Target::default();
            let h = proxy::spawn(base + num as u16, gp, t.clone()).await.map_err(|e| ApiError::new(500, format!("bind {}: {e}", base + num as u16)))?;
            targets.push(t);
            handles.push(h);
        }
        Ok((targets, handles))
    }

    /// `setup`: run the task's OSWorld setup after every restore. Default: yes for a task given here, no for a task
    /// inherited from the checkpoint (it was captured after the setup ran).
    pub async fn create_with(self: &Arc<Self>, checkpoint: String, task: Option<Value>, setup: Option<bool>) -> R<SlotRecord> {
        let c = self.ckpt(&checkpoint)?;
        if let Some(t) = &task {
            if (t.get("config").is_some() || t.get("evaluator").is_some()) && osworld_task_id(t).is_none() {
                return Err(ApiError::new(400, "task: not an OSWorld task (needs an id of [A-Za-z0-9_-], a config list and an evaluator object)"));
            }
            if osworld_task_id(t).is_some() && self.cfg.osw.is_none() {
                return Err(ApiError::new(400, "task: this drumsd has no OSWorld helper configured (osw)"));
            }
        }
        let (task, setup_on_restore) = match task {
            Some(t) => {
                let osw = osworld_task_id(&t).is_some();
                (Some(t), osw && setup.unwrap_or(true))
            }
            None => (c.task.clone(), false),
        };
        let num = self.alloc_num()?;
        let id = format!("s{num}");
        let (targets, handles) = self.bind_proxies(num).await?;
        let now = drumsd_linux::now_epoch();
        let rec = SlotRecord {
            id: id.clone(),
            num,
            checkpoint: checkpoint.clone(),
            state: SlotState::Down { reason: "new".into() },
            episode: 0,
            init_pid: None,
            init_start_ticks: None,
            restore_generation: 0,
            leaf: 0,
            mode: "template".into(),
            port: self.cfg.port_base + num as u16,
            extra_ports: self.cfg.extra_ports.iter().map(|(b, g)| (b + num as u16, *g)).collect(),
            gates: GateReport::default(),
            validity: Validity::Pending { gates: vec![] },
            cost: CostRecord::default(),
            timings: BTreeMap::new(),
            last_error: None,
            task,
            ready_epoch: None,
            created_epoch: now,
            updated_epoch: now,
            task_setup_on_restore: setup_on_restore,
            usage: Ledger::default(),
            verified: None,
            closed_epoch: None,
        };
        let slot = Arc::new(Slot { id: id.clone(), op: Mutex::new(()), view: RwLock::new(rec), targets, proxies: StdMutex::new(handles) });
        self.slots.write().unwrap().insert(id.clone(), slot.clone());
        let _g = slot.op.lock().await;
        let st = slot.view.read().unwrap().state.begin(Op::Create).map_err(|e| ApiError::new(409, e.to_string()))?;
        self.put_slot(&slot, |r| r.state = st);
        let me = self.clone();
        let s2 = slot.clone();
        let t0 = Instant::now();
        let made = tokio::task::spawn_blocking(move || {
            let sp = slotfs::create_branch_dir(&slotfs::BranchSpec {
                state_dir: &me.cfg.state_dir,
                origin: c.slot,
                slot: num,
                lower: &me.cfg.lower,
                image_sha256: &me.cfg.image_sha256,
                source_sha256: &me.source_sha,
                socket: &c.socket,
                port: me.cfg.port_base as u32 + num,
            })?;
            cgroup::ensure_slot_cg(&slotfs::marker_for(&me.cfg.state_dir, num), &me.limits)?;
            if me.cfg.warm_upper {
                let _ = slotfs::prepare_spare(&sp);
            }
            Ok::<_, std::io::Error>(())
        })
        .await
        .map_err(e500)?;
        if let Err(e) = made {
            let r = self.put_slot(&slot, |r| {
                r.state = SlotState::finish(Op::Create, &Outcome::Failed { error: e.to_string() });
                r.last_error = Some(e.to_string());
            });
            self.metrics.inc("drumsd_ops_total", &[("op", "create"), ("outcome", "failed")], 1.0);
            return Ok(r);
        }
        let r = self.restore_episode(&s2, Op::Create).await;
        self.metrics.observe("drumsd_create_seconds", &[], t0.elapsed().as_secs_f64());
        r
    }

    pub async fn reset(self: &Arc<Self>, id: &str) -> R<SlotRecord> {
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        let st = slot.view.read().unwrap().state.begin(Op::Reset).map_err(|e| ApiError::new(409, e.to_string()))?;
        self.finalize_episode(&slot);
        self.put_slot(&slot, |r| r.state = st);
        self.restore_episode(&slot, Op::Reset).await
    }

    /// Count the episode that is ending by its final validity.
    fn finalize_episode(&self, slot: &Slot) {
        let v = slot.view.read().unwrap();
        if v.episode == 0 {
            return;
        }
        let name = match &v.validity {
            Validity::Valid => "valid",
            Validity::Pending { .. } => "pending_at_end",
            Validity::Invalid { .. } => "invalid",
        };
        self.metrics.inc("drumsd_episodes_total", &[("validity", name)], 1.0);
        let pending = matches!(v.validity, Validity::Pending { .. });
        drop(v);
        // valid/invalid episodes are counted when the verdict resolves; one ended before that is counted here
        if pending {
            self.account(Some(slot), |l| l.episode("ended_pending"));
        }
    }

    /// Restore (with the recovery policy) into the slot, which is already in its transient state.
    async fn restore_episode(self: &Arc<Self>, slot: &Arc<Slot>, op: Op) -> R<SlotRecord> {
        let t0 = Instant::now();
        let mut cost = CostRecord::default();
        let mut private = slot.view.read().unwrap().mode == "private";
        let mut attempt = 0;
        let outcome = loop {
            attempt += 1;
            let permit = self.restore_sem.clone().acquire_owned().await.map_err(e500)?;
            let me = self.clone();
            let s2 = slot.clone();
            let ta = Instant::now();
            let res = tokio::task::spawn_blocking(move || {
                let r = me.restore_once(&s2, private);
                drop(permit);
                r
            })
            .await
            .map_err(e500)?;
            let mode = if private { "private" } else { "template" };
            match res {
                Ok(ok) => {
                    cost.attempts.push(Attempt { n: attempt, mode: mode.into(), seconds: round3(ta.elapsed().as_secs_f64()), ok: true, class: None, error: None, action: None });
                    self.metrics.inc("drumsd_restore_attempts_total", &[("mode", mode), ("class", "ok")], 1.0);
                    break Ok(ok);
                }
                Err(f) => {
                    let action = if f.class == FailureClass::GateFailed { Action::GiveUp } else { self.policy.next(attempt, &f.class, private) };
                    let cls = serde_json::to_value(&f.class).unwrap().as_str().unwrap_or("?").to_string();
                    self.metrics.inc("drumsd_restore_attempts_total", &[("mode", mode), ("class", &cls)], 1.0);
                    cost.attempts.push(Attempt {
                        n: attempt,
                        mode: mode.into(),
                        seconds: round3(ta.elapsed().as_secs_f64()),
                        ok: false,
                        class: Some(f.class.clone()),
                        error: Some(f.error.chars().take(600).collect()),
                        action: Some(action.clone()),
                    });
                    self.event(json!({"slot": slot.id, "attempt": attempt, "failure": f.class, "error": f.error.chars().take(400).collect::<String>(), "action": action}));
                    let cause = usage::cause_of(&f.class, f.gates.first_failure().map(|g| g.gate.as_str()));
                    if action == Action::GiveUp {
                        self.account(Some(slot), |l| l.failure(&cause));
                    } else {
                        self.account(Some(slot), |l| l.retry(&cause));
                    }
                    match action {
                        Action::Retry => continue,
                        Action::RetryPrivate => {
                            private = true;
                            continue;
                        }
                        Action::RestartServerAndRetry => {
                            if let Ok(c) = self.ckpt(&slot.view.read().unwrap().checkpoint) {
                                let _ = self.ensure_server(&c);
                            }
                            continue;
                        }
                        Action::GiveUp => break Err(f),
                    }
                }
            }
        };
        let rec = match outcome {
            Ok(ok) => {
                // OSWorld task setup: after the restore permit is released, before the slot is handed out
                let (task, on_restore, num) = {
                    let v = slot.view.read().unwrap();
                    (v.task.clone(), v.task_setup_on_restore, v.num)
                };
                let mut setup_s = None;
                if let (Some(t), true) = (task, on_restore) {
                    if osworld_task_id(&t).is_some() {
                        let ts = Instant::now();
                        let me = self.clone();
                        let r = tokio::task::spawn_blocking(move || me.osw("setup", num, &t, None)).await.map_err(e500)?;
                        let secs = ts.elapsed().as_secs_f64();
                        setup_s = Some(round3(secs));
                        self.account(Some(slot), |l| l.op("task_setup", secs, r.is_ok()));
                        if let Err(e) = r {
                            let marker = slotfs::marker_for(&self.cfg.state_dir, num);
                            for tg in &slot.targets {
                                tg.clear();
                            }
                            let _ = tokio::task::spawn_blocking(move || kill_slot(&marker)).await;
                            self.account(Some(slot), |l| {
                                l.failure("task_setup");
                                l.episode("invalid");
                                l.op(op.as_str(), t0.elapsed().as_secs_f64(), false);
                            });
                            self.metrics.inc("drumsd_ops_total", &[("op", op.as_str()), ("outcome", "task_setup_failed")], 1.0);
                            let err = format!("task setup failed: {e}");
                            let mut timings = ok.timings.clone();
                            timings.insert("task_setup".into(), round3(secs));
                            timings.insert("total".into(), round3(t0.elapsed().as_secs_f64()));
                            return Ok(self.put_slot(slot, |r| {
                                r.state = SlotState::Down { reason: err.chars().take(300).collect() };
                                r.episode += 1;
                                r.gates = ok.gates.clone();
                                r.validity = Validity::Invalid { gates: vec![] };
                                r.cost = cost.clone();
                                r.timings = timings.clone();
                                r.last_error = Some(err.clone());
                                r.init_pid = None;
                                r.verified = None;
                            }));
                        }
                    }
                }
                let total = t0.elapsed().as_secs_f64();
                if cost.recovered() {
                    self.metrics.inc("drumsd_recoveries_total", &[], 1.0);
                }
                self.metrics.observe("drumsd_reset_seconds", &[("op", op.as_str())], total);
                self.metrics.inc("drumsd_upper_spare_total", &[("source", if ok.upper_warm { "warm" } else { "cold" })], 1.0);
                for (k, v) in &ok.timings {
                    self.metrics.observe("drumsd_phase_seconds", &[("phase", k)], *v);
                }
                if let Some(c) = ok.timings.get("criu_clock") {
                    self.metrics.observe("drumsd_criu_restore_seconds", &[], *c);
                }
                for g in &ok.gates.gates {
                    self.metrics.inc("drumsd_gate_results_total", &[("gate", g.gate.as_str()), ("status", status_str(&g.status))], 1.0);
                }
                let validity = ok.gates.validity(&required_gates());
                let mut timings = ok.timings.clone();
                if let Some(x) = setup_s {
                    timings.insert("task_setup".into(), x);
                }
                timings.insert("total".into(), round3(total));
                self.account(Some(slot), |l| l.op(op.as_str(), total, true));
                let r = self.put_slot(slot, |r| {
                    r.verified = None;
                    r.state = SlotState::finish(op, &Outcome::Ok);
                    r.episode += 1;
                    r.init_pid = Some(ok.pid);
                    r.leaf = ok.leaf;
                    r.init_start_ticks = Some(ok.start_ticks.clone());
                    r.gates = ok.gates.clone();
                    r.validity = validity;
                    r.cost = cost.clone();
                    r.timings = timings;
                    r.last_error = None;
                    r.ready_epoch = Some(ok.ready_epoch);
                    r.mode = if private { "private".into() } else { "template".into() };
                });
                self.metrics.inc("drumsd_ops_total", &[("op", op.as_str()), ("outcome", "ok")], 1.0);
                self.spawn_after_ready(slot.clone(), r.episode, ok.hook_dir, ok.ready_at, ok.old_cg);
                r
            }
            Err(f) => {
                let total = t0.elapsed().as_secs_f64();
                self.account(Some(slot), |l| {
                    l.op(op.as_str(), total, false);
                    l.episode("invalid");
                });
                let outcome = if f.class == FailureClass::GateFailed {
                    let ff = f.gates.first_failure();
                    let g = ff.map(|g| g.gate.as_str()).unwrap_or("?");
                    let detail = ff.map(|g| g.detail.to_string()).unwrap_or_else(|| f.error.clone());
                    Outcome::GateFailed { gate: g.into(), detail: detail.chars().take(300).collect() }
                } else {
                    Outcome::Failed { error: f.error.chars().take(300).collect() }
                };
                for g in &f.gates.gates {
                    self.metrics.inc("drumsd_gate_results_total", &[("gate", g.gate.as_str()), ("status", status_str(&g.status))], 1.0);
                }
                let oname = if f.class == FailureClass::GateFailed { "gate_failed" } else { "failed" };
                self.metrics.inc("drumsd_ops_total", &[("op", op.as_str()), ("outcome", oname)], 1.0);
                let validity = f.gates.validity(&required_gates());
                let mut timings = f.timings.clone();
                timings.insert("total".into(), round3(total));
                self.put_slot(slot, |r| {
                    r.state = SlotState::finish(op, &outcome);
                    r.episode += 1;
                    r.gates = f.gates.clone();
                    r.validity = match validity {
                        Validity::Valid => Validity::Invalid { gates: vec![] },
                        v => v,
                    };
                    r.cost = cost.clone();
                    r.timings = timings;
                    r.last_error = Some(f.error.clone());
                    r.init_pid = None;
                    r.verified = None;
                })
            }
        };
        Ok(rec)
    }

    /// One complete restore of the slot: kill, fresh upper, restore, gates. Blocking.
    fn restore_once(&self, slot: &Slot, private: bool) -> Result<RestoreOk, RestoreFailure> {
        let mut t: BTreeMap<String, f64> = BTreeMap::new();
        let mut gates = GateReport::default();
        let fail = |class: FailureClass, error: String, gates: &GateReport, t: &BTreeMap<String, f64>| RestoreFailure { class, error, gates: gates.clone(), timings: t.clone() };
        let (num, ckpt_id, task) = {
            let v = slot.view.read().unwrap();
            (v.num, v.checkpoint.clone(), v.task.clone())
        };
        let c = self.ckpt(&ckpt_id).map_err(|e| fail(FailureClass::Infra, e.message, &gates, &t))?;
        let sp = slotfs::paths(&self.cfg.state_dir, num);
        let marker = slotfs::marker_for(&self.cfg.state_dir, num);
        let slot_cg = cgroup::slot_cg(&marker);
        let t_all = Instant::now();
        let cur_leaf = slot.view.read().unwrap().leaf;

        // 1. stop forwarding and kill the old tree. async_kill: SIGKILL the old leaf (cgroup.kill) and restore into
        //    the other leaf at once; the old one drains in the background. Otherwise wait for populated 0.
        for tg in &slot.targets {
            tg.clear();
        }
        let ts = Instant::now();
        let (cg, new_leaf, old_cg) = if self.cfg.async_kill {
            let (_, l1, l2) = cgroup::ensure_slot_leaves(&marker, &self.limits).map_err(|e| fail(FailureClass::Infra, format!("slot cgroups: {e}"), &gates, &t))?;
            let (old, new, nl) = if cur_leaf == 1 { (l1, l2, 2u8) } else { (l2, l1, 1u8) };
            cgroup::kill_nowait(&old).map_err(|e| fail(FailureClass::Infra, e.to_string(), &gates, &t))?;
            let w = cgroup::wait_empty(&new, Duration::from_secs(20)).map_err(|e| fail(FailureClass::Infra, e.to_string(), &gates, &t))?;
            t.insert("wait_target_leaf".into(), round3(w));
            (new, nl, Some(old))
        } else {
            cgroup::ensure_slot_cg(&marker, &self.limits).map_err(|e| fail(FailureClass::Infra, format!("slot cgroup: {e}"), &gates, &t))?;
            cgroup::kill_and_wait(&slot_cg, Duration::from_secs(20)).map_err(|e| fail(FailureClass::Infra, e.to_string(), &gates, &t))?;
            (slot_cg.clone(), 0u8, None)
        };
        let cg_rel = format!("/{}", cg.strip_prefix(cgroup::CG_ROOT).unwrap().display());
        t.insert("kill".into(), round3(ts.elapsed().as_secs_f64()));

        // 2. fresh upper/work (warm spare when ready)
        let ts = Instant::now();
        let swap = slotfs::swap_in_fresh_upper(&sp).map_err(|e| fail(FailureClass::Infra, format!("fresh upper: {e}"), &gates, &t))?;
        t.insert("upper".into(), round3(ts.elapsed().as_secs_f64()));

        // 3. restore generation, hook, pidfile (slot.json kept in the Python format)
        let ts = Instant::now();
        let sj = slotfs::read_json(&sp.slot_json).unwrap_or_else(|_| json!({}));
        let gen = sj["restore_generation"].as_u64().unwrap_or(0) + 1;
        let file_locks = sj["file_locks"].as_bool().unwrap_or(false);
        let _ = slotfs::record(&sp, json!({"restore_generation": gen, "state": "restoring"}));
        let hook_dir = sp.dir.join(format!("lock-hook-{gen}"));
        let _ = std::fs::remove_dir_all(&hook_dir);
        let hook = hook::write_script(&hook_dir, &self.exe).map_err(|e| fail(FailureClass::Infra, format!("hook: {e}"), &gates, &t))?;
        if self.cfg.netns_fast {
            std::fs::write(hook_dir.join("netns-lo-up"), b"").map_err(|e| fail(FailureClass::Infra, format!("hook: {e}"), &gates, &t))?;
        }
        if self.cfg.restore_cpu_boost {
            let rec = json!({"paths": [cg, slot_cg], "cpu_max": self.limits.cpu_max});
            std::fs::write(hook_dir.join("cpu-restore.json"), serde_json::to_vec(&rec).unwrap()).map_err(|e| fail(FailureClass::Infra, format!("hook: {e}"), &gates, &t))?;
            for d in [&slot_cg, &cg] {
                cgroup::set_cpu_max(d, "max 100000").map_err(|e| fail(FailureClass::Infra, format!("boost: {e}"), &gates, &t))?;
            }
        }
        let pidfile = sp.dir.join("restored.pid");
        let _ = std::fs::remove_file(&pidfile);
        let stamp = (drumsd_linux::now_epoch() * 1000.0) as u64;
        let log = sp.dir.join(format!("restore-drumsd-{gen}-{stamp}.log"));
        let socket = if private {
            None
        } else {
            match self.ensure_server(&c) {
                Ok(_) => Some(c.socket.clone()),
                Err(e) => return Err(fail(FailureClass::TemplateServerDown, e, &gates, &t)),
            }
        };
        let spec = restore::RestoreSpec {
            images: sp.images.clone(),
            croot: sp.croot.clone(),
            pidfile: pidfile.clone(),
            log: log.clone(),
            hook: Some(hook),
            file_locks,
            lower: self.cfg.lower.clone(),
            upper: sp.upper.clone(),
            work: sp.work.clone(),
            cgroup: cg.clone(),
            socket,
            criu_bin: self.cfg.criu.clone(),
            out: sp.dir.join(format!("restore-drumsd-{gen}-{stamp}.out")),
            empty_net: self.cfg.netns_fast,
            nested_pidns: self.cfg.nested_pidns,
        };
        t.insert("prepare".into(), round3(ts.elapsed().as_secs_f64()));

        // 4. CRIU restore (template server fork, or private)
        let ts = Instant::now();
        let spawn_epoch = drumsd_linux::now_epoch();
        let run = restore::run(&self.exe, &spec, Duration::from_secs(300)).map_err(|e| fail(FailureClass::Infra, format!("restore client: {e}"), &gates, &t))?;
        t.insert("restore".into(), round3(ts.elapsed().as_secs_f64()));
        // client stamps (relative to the spawn): start, mounted, sent, result
        for l in run.out_tail.lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() == 3 && f[0] == "drumsd-client" {
                if let Ok(e) = f[2].parse::<f64>() {
                    t.insert(format!("client_{}", f[1]), round3(e - spawn_epoch));
                }
            }
        }
        let log_text = match String::from_utf8(std::fs::read(&log).unwrap_or_default()) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        };
        if let Some(cc) = restore::criu_clock(&log_text) {
            t.insert("criu_clock".into(), cc);
        }
        let pid = std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse::<i32>().ok());
        if self.cfg.restore_cpu_boost {
            t.insert("boost_restored_by_hook".into(), if hook_dir.join("cpu-restore.done").exists() { 1.0 } else { 0.0 });
            for d in [&cg, &slot_cg] {
                if cgroup::read_limits(d).map(|l| l.cpu_max != self.limits.cpu_max).unwrap_or(true) {
                    let _ = cgroup::set_cpu_max(d, &self.limits.cpu_max);
                }
            }
        }
        if !run.ok || pid.is_none() {
            let tail = restore::log_tail(&log, 3000);
            let error = format!("criu restore failed (exit {:?}): {} | log: {}", run.exit, run.out_tail.trim(), tail.lines().rev().take(6).collect::<Vec<_>>().join(" / "));
            let class = recovery::classify(&error, &tail);
            // nothing of a failed restore may keep running
            let _ = cgroup::kill_and_wait(&cg, Duration::from_secs(20));
            let _ = slotfs::record(&sp, json!({"state": "down", "last_restore_error": error.chars().take(400).collect::<String>()}));
            return Err(fail(class, error, &gates, &t));
        }
        let pid = pid.unwrap();
        let start_ticks = procfs::start_ticks(pid).unwrap_or_default();

        // 5. kernel-side gates, in parallel
        let ts = Instant::now();
        let argv_marker = c.argv_marker.clone();
        let (la, lp, ident, cgg, mnt, netg) = std::thread::scope(|s| {
            let l = s.spawn(|| checks::lock_gates(&sp.dir, &sp.images, &hook_dir, &log_text));
            let i = s.spawn(|| checks::init_identity(pid, &argv_marker, &cg_rel));
            let g = s.spawn(|| checks::cgroup_gate(pid, &cg, &slot_cg, &self.limits));
            let m = s.spawn(|| checks::mount_gate(pid, &sp.dir));
            let n = s.spawn(|| checks::netns_gate(pid, &sp.dir));
            let (la, lp) = l.join().unwrap();
            (la, lp, i.join().unwrap(), g.join().unwrap(), m.join().unwrap(), n.join().unwrap())
        });
        for g in [la, lp, ident, cgg, mnt, netg] {
            gates.push(g);
        }
        t.insert("gates_kernel".into(), round3(ts.elapsed().as_secs_f64()));
        if let Some(f) = gates.first_failure() {
            let msg = format!("gate {} failed: {}", f.gate.as_str(), f.detail);
            let _ = cgroup::kill_and_wait(&cg, Duration::from_secs(20));
            return Err(fail(FailureClass::GateFailed, msg, &gates, &t));
        }

        // 6. forwarders point at the new namespace; the guest server must answer
        let ts = Instant::now();
        for tg in &slot.targets {
            if let Err(e) = tg.set_pid(pid) {
                return Err(fail(FailureClass::Infra, format!("netns of {pid}: {e}"), &gates, &t));
            }
        }
        let g = Guest::new(self.cfg.port_base + num as u16, Duration::from_secs(10));
        let mut last = String::new();
        let mut tries = 0;
        let ready = loop {
            tries += 1;
            match g.platform() {
                Ok(p) if p == "Linux" => break true,
                Ok(p) => last = format!("unexpected /platform reply {p:?}"),
                Err(e) => last = e.to_string(),
            }
            if ts.elapsed() > Duration::from_secs(60) {
                break false;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let ready_at = Instant::now();
        let ready_epoch = drumsd_linux::now_epoch();
        t.insert("guest_ready".into(), round3(ts.elapsed().as_secs_f64()));
        if !ready {
            gates.push(GateResult::fail(Gate::GuestReady, ts.elapsed().as_secs_f64(), json!({"error": last, "tries": tries})));
            let _ = cgroup::kill_and_wait(&cg, Duration::from_secs(20));
            return Err(fail(FailureClass::GateFailed, format!("guest server not answering: {last}"), &gates, &t));
        }
        gates.push(GateResult::pass(Gate::GuestReady, ts.elapsed().as_secs_f64(), json!({"tries": tries})));
        t.insert("ready".into(), round3(t_all.elapsed().as_secs_f64()));

        // 7. state hash and isolation, in parallel
        let ts = Instant::now();
        let others = self.live_inits(&slot.id);
        let (hash, iso) = std::thread::scope(|s| {
            let h = s.spawn(|| state_hash(&g));
            let i = s.spawn(|| checks::isolation_gate(pid, &others, &cg));
            (h.join().unwrap(), i.join().unwrap())
        });
        let hs = ts.elapsed().as_secs_f64();
        match (&hash, &c.expected_state_hash) {
            (Ok(h), Some(want)) if h == want => gates.push(GateResult::pass(Gate::StateHash, hs, json!({"hash": h}))),
            (Ok(h), Some(want)) => gates.push(GateResult::fail(Gate::StateHash, hs, json!({"expected": want, "got": h}))),
            (Ok(h), None) => {
                // the first child defines the reference (recorded as such on the checkpoint)
                let mut c2 = c.clone();
                c2.expected_state_hash = Some(h.clone());
                c2.hash_reference = format!("first child {}", slot.id);
                self.put_ckpt(&c2);
                gates.push(GateResult::pass(Gate::StateHash, hs, json!({"hash": h, "reference": "this restore defined it"})));
            }
            (Err(e), _) => gates.push(GateResult::fail(Gate::StateHash, hs, json!({"error": e}))),
        }
        gates.push(iso);
        t.insert("hash_isolation".into(), round3(ts.elapsed().as_secs_f64()));
        gates.push(GateResult::pending(Gate::AliveAfterResume));
        if let Some(f) = gates.first_failure() {
            let msg = format!("gate {} failed: {}", f.gate.as_str(), f.detail);
            let _ = cgroup::kill_and_wait(&cg, Duration::from_secs(20));
            for tg in &slot.targets {
                tg.clear();
            }
            return Err(fail(FailureClass::GateFailed, msg, &gates, &t));
        }

        // 8. task setup (after the hash: the hash is of the checkpoint's state)
        if let Some(task) = task {
            let ts = Instant::now();
            for a in task["setup"].as_array().cloned().unwrap_or_default() {
                if let Err(e) = run_action(&g, &a) {
                    let _ = cgroup::kill_and_wait(&cg, Duration::from_secs(20));
                    return Err(fail(FailureClass::Infra, format!("task setup: {e}"), &gates, &t));
                }
            }
            t.insert("task_setup".into(), round3(ts.elapsed().as_secs_f64()));
        }

        let _ = slotfs::record(
            &sp,
            json!({"init_pid": pid, "init_start_ticks": start_ticks, "state": "running", "restore_generation": gen,
                   "pageshare": {"mode": if private {"pool"} else {"template"}, "socket": c.socket, "criu": if private {Some(self.cfg.criu.clone())} else {None}},
                   "last_restore": {"by": "drumsd", "log": log.file_name().map(|x| x.to_string_lossy().to_string()), "epoch": drumsd_linux::now_epoch()}}),
        );
        t.insert("sync_total".into(), round3(t_all.elapsed().as_secs_f64()));
        Ok(RestoreOk { leaf: new_leaf, old_cg, pid, start_ticks, gates, timings: t, hook_dir, ready_at, ready_epoch, upper_warm: swap.warm })
    }

    /// After a restore: the alive gate at ready + alive_after_s, the next warm spare, trash removal.
    fn spawn_after_ready(self: &Arc<Self>, slot: Arc<Slot>, episode: u64, hook_dir: PathBuf, ready_at: Instant, old_cg: Option<PathBuf>) {
        let me = self.clone();
        let warm = self.cfg.warm_upper;
        tokio::spawn(async move {
            // housekeeping first (it overlaps with the wait)
            let sp = slotfs::paths(&me.cfg.state_dir, slot.view.read().unwrap().num);
            let sp2 = sp.clone();
            let _ = tokio::task::spawn_blocking(move || {
                // the killed leaf drains while the new episode runs, so the next reset finds it empty
                if let Some(old) = old_cg {
                    let _ = cgroup::wait_empty(&old, Duration::from_secs(30));
                }
                let _ = slotfs::empty_trash(&sp2);
                if warm {
                    let _ = slotfs::prepare_spare(&sp2);
                }
            })
            .await;
            let wait = Duration::from_secs_f64(me.cfg.alive_after_s).saturating_sub(ready_at.elapsed());
            tokio::time::sleep(wait).await;
            let me2 = me.clone();
            let s2 = slot.clone();
            let _ = tokio::task::spawn_blocking(move || me2.judge_alive(&s2, episode, &hook_dir, ready_at)).await;
        });
    }

    fn judge_alive(&self, slot: &Slot, episode: u64, hook_dir: &Path, ready_at: Instant) {
        let (pid, cur_episode) = {
            let v = slot.view.read().unwrap();
            (v.init_pid, v.episode)
        };
        if cur_episode != episode {
            return; // a newer episode started; this one's verdict no longer matters to anyone but the metrics
        }
        let t0 = Instant::now();
        let read_procs = |p: PathBuf| -> Option<BTreeMap<String, String>> {
            std::fs::read(p).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| serde_json::from_value(v["procs"].clone()).ok())
        };
        let pre = read_procs(hook_dir.join("procs-pre-resume.json")).unwrap_or_default();
        // the reference is what the checkpoint holds (CRIU's dump), so a process that exited before the capture is
        // not in it, and one the restore failed to bring back is missing from the restored tree
        let ck_dir = self.ckpt(&slot.view.read().unwrap().checkpoint).ok().map(|c| slotfs::paths(&self.cfg.state_dir, c.slot).dir);
        let dumped = ck_dir.and_then(|d| read_procs(d.join("dumped-reference.json"))).filter(|d| !d.is_empty());
        let (reference, source) = match dumped {
            Some(d) => (d, "dump"),
            None => (pre.clone(), "pre_resume_census"),
        };
        let result = if reference.is_empty() {
            GateResult::fail(Gate::AliveAfterResume, 0.0, json!({"error": "no process reference (neither the dump's nor the pre-resume census)"}))
        } else {
            let now = pid.map(procfs::ns_procs).unwrap_or_default();
            let v = alive::judge(&reference, &now, &self.natural, ready_at.elapsed().as_secs_f64());
            let lost: Vec<(String, String)> = reference.iter().filter(|(p, c)| pre.get(*p) != Some(*c)).map(|(p, c)| (p.clone(), c.clone())).collect();
            let mut d = serde_json::to_value(&v).unwrap();
            d["reference_source"] = json!(source);
            d["pre_resume_n"] = json!(pre.len());
            d["not_restored"] = json!(lost);
            if v.ok {
                GateResult::pass(Gate::AliveAfterResume, t0.elapsed().as_secs_f64(), d)
            } else {
                GateResult::fail(Gate::AliveAfterResume, t0.elapsed().as_secs_f64(), d)
            }
        };
        let failed = result.status == GateStatus::Fail;
        self.metrics.inc("drumsd_gate_results_total", &[("gate", "alive_after_resume"), ("status", status_str(&result.status))], 1.0);
        let rec = self.put_slot(slot, |r| {
            if r.episode != episode {
                return;
            }
            r.gates.push(result.clone());
            r.validity = r.gates.validity(&required_gates());
            if failed && matches!(r.state, SlotState::Ready) {
                r.state = SlotState::Quarantined { reason: format!("gate alive_after_resume failed: {}", result.detail["counted"]) };
            }
        });
        if rec.episode == episode && !matches!(rec.validity, Validity::Pending { .. }) {
            let v = verdict_str(&rec.validity);
            self.account(Some(slot), |l| {
                l.episode(v);
                if failed {
                    l.failure("gate:alive_after_resume");
                }
            });
        }
    }

    pub async fn step(self: &Arc<Self>, id: &str, action: Value, screenshot: bool) -> R<Value> {
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        let v = slot.view.read().unwrap().clone();
        v.state.begin(Op::Step).map_err(|e| ApiError::new(409, e.to_string()))?;
        let port = v.port;
        let t0 = Instant::now();
        let out = tokio::task::spawn_blocking(move || {
            let g = Guest::new(port, Duration::from_secs(130));
            let r = run_action(&g, &action)?;
            let png = if screenshot { Some(g.screenshot()?) } else { None };
            Ok::<_, std::io::Error>((r, png))
        })
        .await
        .map_err(e500)?;
        let secs = t0.elapsed().as_secs_f64();
        self.account(Some(&slot), |l| l.op("step", secs, out.is_ok()));
        let out = out.map_err(|e| ApiError::new(502, e.to_string()))?;
        let cur = slot.view.read().unwrap().clone();
        Ok(json!({"result": out.0, "screenshot_png_b64": out.1.map(|p| b64(&p)), "episode": cur.episode, "validity": cur.validity,
                  "seconds": round3(t0.elapsed().as_secs_f64())}))
    }

    pub async fn screenshot(self: &Arc<Self>, id: &str) -> R<Vec<u8>> {
        let slot = self.slot(id)?;
        let port = slot.view.read().unwrap().port;
        tokio::task::spawn_blocking(move || Guest::new(port, Duration::from_secs(30)).screenshot()).await.map_err(e500)?.map_err(|e| ApiError::new(502, e.to_string()))
    }

    /// The cgroup that holds the slot's running tree (its current leaf, or the slot cgroup itself).
    fn tree_cg(&self, slot: &Slot) -> PathBuf {
        let (num, leaf) = {
            let v = slot.view.read().unwrap();
            (v.num, v.leaf)
        };
        let marker = slotfs::marker_for(&self.cfg.state_dir, num);
        if leaf == 0 {
            cgroup::slot_cg(&marker)
        } else {
            cgroup::slot_leaf(&marker, leaf)
        }
    }

    fn quiesce_params(&self, o: &QuiesceOpts) -> q::Params {
        q::Params {
            window_s: o.window_s.unwrap_or(self.cfg.quiesce.window_s).clamp(0.0, 60.0),
            timeout_s: o.timeout_s.unwrap_or(self.cfg.quiesce.timeout_s).clamp(0.0, 600.0),
            poll_ms: self.cfg.quiesce.poll_ms,
        }
    }

    fn not_quiescent(&self, slot: &Slot, id: &str, report: &Value, why: &str) -> ApiError {
        self.account(Some(slot), |l| l.failure("capture:not_quiescent"));
        self.metrics.inc("drumsd_ops_total", &[("op", "capture"), ("outcome", "not_quiescent")], 1.0);
        self.event(json!({"slot": id, "not_quiescent": report}));
        let unstable = report["unstable"].clone();
        ApiError::new(409, format!("env {id} is not quiescent, nothing was captured: {why}; unstable: {}", unstable.to_string().chars().take(600).collect::<String>()))
            .with(json!({"code": "not_quiescent", "unstable": unstable, "quiescence": report}))
    }

    /// Wait until the slot's tree is quiescent (no task leaving, no process or thread starting or exiting for the
    /// window). Holds the slot's operation lock, so no action runs meanwhile. 409 `not_quiescent` on a timeout.
    pub async fn wait_quiescent(self: &Arc<Self>, id: &str, o: QuiesceOpts) -> R<Value> {
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        slot.view.read().unwrap().state.begin(Op::Step).map_err(|e| ApiError::new(409, e.to_string()))?;
        let p = self.quiesce_params(&o);
        let cg = self.tree_cg(&slot);
        let (rep, s) = tokio::task::spawn_blocking(move || quiesce::wait(&cg, &p)).await.map_err(e500)?.map_err(e500)?;
        let rv = serde_json::to_value(&rep).unwrap();
        self.account(Some(&slot), |l| l.op("wait_quiescent", rep.waited_s, s.is_some()));
        if s.is_none() {
            return Err(self.not_quiescent(&slot, id, &rv, &format!("no quiescent window of {} s within {} s", rep.window_s, rep.timeout_s)));
        }
        Ok(rv)
    }

    /// Checkpoint the running slot with the existing capture verb (`--leave-running --no-continue`: CRIU dumps the
    /// frozen tree, its post-dump action copies the upper layer, the tree continues), then adopt the result.
    /// Safe to call right after an action: the dump happens only once the tree is quiescent, the dump
    /// must be the tree that was judged quiescent (CRIU's seized tasks and threads), and every dumped process must
    /// still run in the source right after the dump. A dump that fails either check is discarded (its records kept)
    /// and the capture waits and dumps again; when no good dump is possible in time it fails with `not_quiescent`
    /// and the env stays ready. Nothing is captured silently: every attempt is in the checkpoint's `quiescence`.
    pub async fn capture_with(self: &Arc<Self>, id: &str, ckpt_id: Option<String>, o: QuiesceOpts) -> R<CheckpointRecord> {
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        let st = slot.view.read().unwrap().state.begin(Op::Capture).map_err(|e| ApiError::new(409, e.to_string()))?;
        self.put_slot(&slot, |r| r.state = st);
        let num = slot.view.read().unwrap().num;
        let t0 = Instant::now();
        let params = self.quiesce_params(&o);
        let deadline = t0 + Duration::from_secs_f64(params.timeout_s);
        let cg = self.tree_cg(&slot);
        let mut attempts: Vec<Value> = vec![];
        let max_attempts = self.cfg.quiesce.max_attempts.max(1);
        let back_to_ready = |me: &Self| {
            me.put_slot(&slot, |r| r.state = SlotState::finish(Op::Capture, &Outcome::Ok));
        };
        for n in 1..=max_attempts {
            // 1. wait for a quiescent window
            let mut p = params.clone();
            p.timeout_s = deadline.saturating_duration_since(Instant::now()).as_secs_f64();
            let cg2 = cg.clone();
            let tq = Instant::now();
            let waited = tokio::task::spawn_blocking(move || quiesce::wait(&cg2, &p)).await.map_err(e500)?;
            let (rep, sample) = match waited {
                Ok(x) => x,
                Err(e) => {
                    back_to_ready(self);
                    return Err(ApiError::new(500, format!("reading the tree of {id}: {e}")));
                }
            };
            let wait_s = tq.elapsed().as_secs_f64();
            let mut att = json!({"n": n, "wait": rep, "wait_s": round3(wait_s)});
            let Some(sample) = sample else {
                attempts.push(att);
                back_to_ready(self);
                let rep = json!({"attempts": attempts, "unstable": rep.unstable, "seconds": round3(t0.elapsed().as_secs_f64())});
                return Err(self.not_quiescent(&slot, id, &rep, &format!("no quiescent window of {} s within {} s", params.window_s, params.timeout_s)));
            };
            // 2. dump (the tree keeps running)
            let into = match self.alloc_num() {
                Ok(x) => x,
                Err(e) => {
                    back_to_ready(self);
                    return Err(e);
                }
            };
            let me = self.clone();
            let td = Instant::now();
            let res = tokio::task::spawn_blocking(move || me.run_capture_verb(num, into)).await.map_err(e500)?;
            let out = match res {
                Ok(out) => out,
                Err(e) => {
                    let secs = t0.elapsed().as_secs_f64();
                    self.account(Some(&slot), |l| {
                        l.op("capture", secs, false);
                        l.failure("capture");
                    });
                    self.put_slot(&slot, |r| {
                        r.state = SlotState::finish(Op::Capture, &Outcome::Failed { error: e.clone() });
                        r.last_error = Some(e.clone());
                    });
                    self.metrics.inc("drumsd_ops_total", &[("op", "capture"), ("outcome", "failed")], 1.0);
                    return Err(ApiError::new(500, e));
                }
            };
            att["dump_s"] = json!(round3(td.elapsed().as_secs_f64()));
            att["into"] = json!(into);
            // 3. the dump must be the quiescent tree
            let qdir = slotfs::paths(&self.cfg.state_dir, into).dir;
            let tc = Instant::now();
            let dumped = std::fs::read(qdir.join("dump.log")).map_err(|e| e.to_string()).and_then(|b| q::parse_dump_log(&String::from_utf8_lossy(&b)));
            let mismatch = match &dumped {
                Ok(d) => q::compare_dump(&sample, d),
                Err(e) => vec![q::Unstable { kind: "dump_log_unreadable".into(), pid: 0, tid: None, comm: e.clone() }],
            };
            att["dump_check_s"] = json!(round3(tc.elapsed().as_secs_f64()));
            att["dumped_processes"] = json!(dumped.as_ref().map(|d| d.len()).unwrap_or(0));
            if !mismatch.is_empty() {
                att["rejected"] = json!({"why": "the dump differs from the quiescent sample", "unstable": mismatch});
                self.event(json!({"slot": id, "capture_attempt_rejected": att}));
                self.account(Some(&slot), |l| l.retry("capture:dump_mismatch"));
                let me = self.clone();
                att["discarded_to"] = json!(tokio::task::spawn_blocking(move || me.discard_dump(into)).await.map_err(e500)?);
                attempts.push(att);
                continue;
            }
            let dumped = dumped.unwrap();
            // 4. adopt (template server, the children's state-hash reference from the parent's desktop now)
            let cid = ckpt_id.clone().unwrap_or_else(|| format!("{}-c{}", id, into));
            let port = slot.view.read().unwrap().port;
            let h = tokio::task::spawn_blocking(move || state_hash(&Guest::new(port, Duration::from_secs(30)))).await.map_err(e500)?.ok();
            let c = self.adopt_checkpoint(cid.clone(), into, h, Some(id.to_string())).await;
            // 5. every dumped process still runs in the source (no action was applied to it since the dump)
            let hold = Duration::from_secs_f64(self.cfg.quiesce.confirm_s.clamp(0.0, 60.0)).saturating_sub(td.elapsed());
            if !hold.is_zero() {
                tokio::time::sleep(hold).await;
            }
            let cg2 = cg.clone();
            let after = tokio::task::spawn_blocking(move || quiesce::sample(&cg2)).await.map_err(e500)?.map_err(e500)?;
            let gone = q::source_after_dump(&sample, &dumped, &after);
            att["source_check_after_s"] = json!(round3(td.elapsed().as_secs_f64()));
            if !gone.is_empty() {
                att["rejected"] = json!({"why": "a dumped process exited in the source right after the dump", "unstable": gone});
                self.event(json!({"slot": id, "capture_attempt_rejected": att}));
                self.account(Some(&slot), |l| l.retry("capture:exited_after_dump"));
                if c.is_ok() {
                    let _ = self.delete_checkpoint(&cid).await;
                    att["discarded_to"] = json!("forgotten/ (delete_checkpoint)");
                } else {
                    let me = self.clone();
                    att["discarded_to"] = json!(tokio::task::spawn_blocking(move || me.discard_dump(into)).await.map_err(e500)?);
                }
                attempts.push(att);
                continue;
            }
            attempts.push(att);
            let quiescence = json!({"attempts": attempts, "params": params, "seconds": round3(t0.elapsed().as_secs_f64()),
                                    "wait_s_total": round3(attempts.iter().filter_map(|a| a["wait_s"].as_f64()).sum::<f64>())});
            let c = c.map(|mut c| {
                // slots made from this checkpoint inherit the task, whose setup is already in the captured state
                c.task = slot.view.read().unwrap().task.clone().map(|mut t| {
                    if t.get("setup").is_some() {
                        t["setup"] = json!([]);
                    }
                    t
                });
                c.quiescence = Some(quiescence.clone());
                self.put_ckpt(&c);
                c
            });
            let ok = c.is_ok();
            let secs = t0.elapsed().as_secs_f64();
            self.account(Some(&slot), |l| l.op("capture", secs, ok));
            let outcome = if ok { Outcome::Ok } else { Outcome::Failed { error: "adopt after capture failed".into() } };
            self.put_slot(&slot, |r| {
                r.state = SlotState::finish(Op::Capture, &outcome);
                r.timings.insert("capture".into(), round3(secs));
                r.timings.insert("capture_quiesce_wait".into(), quiescence["wait_s_total"].as_f64().unwrap_or(0.0));
            });
            self.event(json!({"slot": id, "captured_into": into, "verb": out, "quiescence": quiescence}));
            self.metrics.inc("drumsd_ops_total", &[("op", "capture"), ("outcome", if ok { "ok" } else { "failed" })], 1.0);
            return c;
        }
        back_to_ready(self);
        let rep = json!({"attempts": attempts, "unstable": attempts.last().map(|a| a["rejected"]["unstable"].clone()).unwrap_or(Value::Null),
                         "seconds": round3(t0.elapsed().as_secs_f64())});
        Err(self.not_quiescent(&slot, id, &rep, &format!("{max_attempts} dumps rejected")))
    }

    /// A rejected dump that was never adopted: its images and disk copy are removed, its records and logs move to
    /// state_dir/forgotten (kept). Returns where.
    fn discard_dump(&self, into: u32) -> String {
        let sp = slotfs::paths(&self.cfg.state_dir, into);
        for d in ["images", "upper.snap", "upper", "work"] {
            let p = sp.dir.join(d);
            if p.symlink_metadata().is_ok() {
                let _ = std::fs::remove_dir_all(&p);
            }
        }
        let keep = self.cfg.state_dir.join("forgotten").join(format!("slot{into}-rejected-{}", (drumsd_linux::now_epoch() * 1000.0) as u64));
        let _ = std::fs::create_dir_all(keep.parent().unwrap());
        match std::fs::rename(&sp.dir, &keep) {
            Ok(()) => keep.display().to_string(),
            Err(e) => format!("not moved: {e}"),
        }
    }

    fn run_capture_verb(&self, slot: u32, into: u32) -> Result<Value, String> {
        let py = self.cfg.python.clone().unwrap_or_else(|| PathBuf::from("/usr/bin/python3"));
        let criu_dir = self.cfg.criu.parent().map(|p| p.display().to_string()).unwrap_or_default();
        let o = std::process::Command::new(py)
            .args(["-m", "sharedos_rootfs.capture", "capture", "--slot", &slot.to_string(), "--into", &into.to_string(), "--leave-running", "--no-continue"])
            .current_dir(&self.cfg.py_src)
            .env("SHAREDOS_ROOTFS_CONFIG", &self.cfg.py_config)
            .env("PYTHONPATH", &self.cfg.py_src)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("PATH", format!("{criu_dir}:/usr/sbin:/usr/bin:/sbin:/bin"))
            .output()
            .map_err(|e| e.to_string())?;
        let out = String::from_utf8_lossy(&o.stdout).to_string();
        if !o.status.success() {
            return Err(format!("capture verb rc {:?}: {}", o.status.code(), String::from_utf8_lossy(&o.stderr).chars().rev().take(800).collect::<String>().chars().rev().collect::<String>()));
        }
        Ok(out.lines().rev().find(|l| l.starts_with('{')).and_then(|l| serde_json::from_str(l).ok()).unwrap_or(json!({"stdout": out})))
    }

    /// Capture the slot's current state and start `n` children from it, concurrently.
    pub async fn branch_with(self: &Arc<Self>, id: &str, n: usize, o: QuiesceOpts) -> R<Value> {
        if n == 0 || n > 64 {
            return Err(ApiError::new(400, "n must be 1..64"));
        }
        let t0 = Instant::now();
        let parent = self.slot(id)?;
        let c = match self.capture_with(id, None, o).await {
            Ok(c) => c,
            Err(e) => {
                let secs = t0.elapsed().as_secs_f64();
                self.account(Some(&parent), |l| l.op("fork", secs, false));
                return Err(e);
            }
        };
        let t_cap = t0.elapsed().as_secs_f64();
        let futs: Vec<_> = (0..n)
            .map(|_| {
                let me = self.clone();
                let cid = c.id.clone();
                // children inherit the checkpoint's task (setup already in the state)
                tokio::spawn(async move { me.create_with(cid, None, Some(false)).await })
            })
            .collect();
        let mut children = vec![];
        for f in futs {
            match f.await.map_err(e500)? {
                Ok(r) => children.push(serde_json::to_value(view(&r)).unwrap()),
                Err(e) => children.push(json!({"error": e.message})),
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        let all_up = children.iter().all(|c| c["state"] == "ready");
        self.account(Some(&parent), |l| l.op("fork", secs, all_up));
        Ok(json!({"checkpoint": c, "children": children, "capture_s": round3(t_cap), "children_ready_s": round3(secs - t_cap), "seconds": round3(secs)}))
    }

    pub async fn quarantine(self: &Arc<Self>, id: &str, reason: String) -> R<SlotRecord> {
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        let st = slot.view.read().unwrap().state.begin(Op::Quarantine).map_err(|e| ApiError::new(409, e.to_string()))?;
        Ok(self.put_slot(&slot, |r| {
            r.state = match st {
                SlotState::Quarantined { .. } => SlotState::Quarantined { reason: reason.clone() },
                s => s,
            }
        }))
    }

    pub async fn release(self: &Arc<Self>, id: &str) -> R<SlotRecord> {
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        let st = slot.view.read().unwrap().state.begin(Op::Release).map_err(|e| ApiError::new(409, e.to_string()))?;
        // a released slot never keeps the tree that was quarantined
        let num = slot.view.read().unwrap().num;
        let marker = slotfs::marker_for(&self.cfg.state_dir, num);
        for tg in &slot.targets {
            tg.clear();
        }
        tokio::task::spawn_blocking(move || kill_slot(&marker)).await.map_err(e500)?.map_err(e500)?;
        Ok(self.put_slot(&slot, |r| {
            r.state = st;
            r.init_pid = None;
        }))
    }

    pub async fn destroy(self: &Arc<Self>, id: &str) -> R<SlotRecord> {
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        let st = slot.view.read().unwrap().state.begin(Op::Destroy).map_err(|e| ApiError::new(409, e.to_string()))?;
        self.finalize_episode(&slot);
        self.put_slot(&slot, |r| r.state = st);
        for tg in &slot.targets {
            tg.clear();
        }
        for h in slot.proxies.lock().unwrap().drain(..) {
            h.abort();
        }
        let num = slot.view.read().unwrap().num;
        let me = self.clone();
        let res = tokio::task::spawn_blocking(move || {
            let marker = slotfs::marker_for(&me.cfg.state_dir, num);
            kill_slot(&marker)?;
            for l in [1, 2] {
                let _ = std::fs::remove_dir(cgroup::slot_leaf(&marker, l));
            }
            let _ = std::fs::remove_dir(cgroup::slot_cg(&marker));
            let sp = slotfs::paths(&me.cfg.state_dir, num);
            for d in ["upper", "upper.snap", "work", "images", "upper.next", "work.next", "upper.next.tmp", "trash", "criu-root"] {
                let p = sp.dir.join(d);
                if p.symlink_metadata().is_ok() {
                    std::fs::remove_dir_all(&p)?;
                }
            }
            // records and logs are kept; the directory moves aside so the number can be reused
            let keep = me.cfg.state_dir.join("forgotten").join(format!("slot{num}-{}", (drumsd_linux::now_epoch() * 1000.0) as u64));
            std::fs::create_dir_all(keep.parent().unwrap())?;
            std::fs::rename(&sp.dir, &keep)?;
            Ok::<_, std::io::Error>(keep)
        })
        .await
        .map_err(e500)?;
        let outcome = match &res {
            Ok(_) => Outcome::Ok,
            Err(e) => Outcome::Failed { error: e.to_string() },
        };
        self.metrics.inc("drumsd_ops_total", &[("op", "destroy"), ("outcome", if res.is_ok() { "ok" } else { "failed" })], 1.0);
        {
            let now = drumsd_linux::now_epoch();
            let created = slot.view.read().unwrap().created_epoch;
            let mut u = self.usage.lock().unwrap();
            u.closed_slot_seconds += (now - created.max(u.window_start_epoch)).max(0.0);
            let _ = self.journal.put("meta", "usage", &*u);
        }
        Ok(self.put_slot(&slot, |r| {
            r.state = SlotState::finish(Op::Destroy, &outcome);
            r.init_pid = None;
            r.closed_epoch = Some(drumsd_linux::now_epoch());
            if let Ok(k) = &res {
                r.timings.insert("destroyed_records_at_epoch".into(), drumsd_linux::now_epoch());
                r.last_error = Some(format!("records kept at {}", k.display()));
            }
        }))
    }

    // ------------------------------------------------------------------------------------------------ OSWorld tasks
    /// Ports of slot `num` as OSWorld sees them: guest server, Chrome DevTools (guest 9222), VLC (guest 8080).
    fn osw_ports(&self, num: u32) -> (u16, Option<u16>, Option<u16>) {
        let find = |g: u16| self.cfg.extra_ports.iter().find(|(_, gp)| *gp == g).map(|(b, _)| b + num as u16);
        (self.cfg.port_base + num as u16, find(9222), find(8080))
    }

    /// Run the OSWorld helper (`drums_osw.py VERB`) as the unprivileged user, request on stdin, JSON reply on the last
    /// stdout line. Blocking.
    pub fn osw(&self, verb: &str, num: u32, task: &Value, extra: Option<Value>) -> Result<Value, String> {
        use std::io::Write as _;
        use std::os::unix::process::CommandExt;
        let o = self.cfg.osw.as_ref().ok_or("no OSWorld helper configured (osw)")?;
        let (server, chromium, vlc) = self.osw_ports(num);
        let req = json!({"task": task, "host": "127.0.0.1", "server_port": server, "chromium_port": chromium, "vlc_port": vlc,
                         "cache_dir": o.cache_dir, "osworld_dir": o.osworld_dir, "client_password": o.client_password, "extra": extra});
        let timeout = Duration::from_secs(if verb == "evaluate" { o.eval_timeout_s } else { o.setup_timeout_s });
        let mut cmd = std::process::Command::new(&o.python);
        cmd.arg("-I").arg(&o.helper).arg(verb).current_dir(&o.osworld_dir).env_clear()
            .env("PATH", "/usr/bin:/bin").env("HOME", &o.cache_dir).env("PYTHONDONTWRITEBYTECODE", "1").env("OSWORLD_DIR", &o.osworld_dir)
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
            .uid(o.uid).gid(o.gid);
        unsafe {
            // its own process group, so a timeout kills the helper and anything it started
            // (std drops supplementary groups itself when a uid is set; this runs after the uid change)
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| format!("osw helper: {e}"))?;
        child.stdin.take().unwrap().write_all(&serde_json::to_vec(&req).unwrap()).map_err(|e| e.to_string())?;
        let pid = child.id() as i32;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let out = match rx.recv_timeout(timeout) {
            Ok(r) => r.map_err(|e| e.to_string())?,
            Err(_) => {
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                return Err(format!("osw {verb} timed out after {}s", timeout.as_secs()));
            }
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reply: Option<Value> = stdout.lines().rev().find(|l| l.starts_with('{')).and_then(|l| serde_json::from_str(l).ok());
        match reply {
            Some(v) if v["ok"] == true => Ok(v),
            Some(v) => Err(v["error"].as_str().unwrap_or("helper reported failure").chars().take(1500).collect()),
            None => Err(format!("osw {verb} exit {:?}, no reply; stderr: {}", out.status.code(), stderr.chars().rev().take(1200).collect::<String>().chars().rev().collect::<String>())),
        }
    }

    /// Load an OSWorld task into a ready slot: its setup runs now, and again after every reset of this slot.
    pub async fn load_task(self: &Arc<Self>, id: &str, task: Value) -> R<SlotRecord> {
        if osworld_task_id(&task).is_none() {
            return Err(ApiError::new(400, "task: not an OSWorld task (needs an id of [A-Za-z0-9_-], a config list and an evaluator object)"));
        }
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        slot.view.read().unwrap().state.begin(Op::Step).map_err(|e| ApiError::new(409, e.to_string()))?;
        let num = slot.view.read().unwrap().num;
        let me = self.clone();
        let t = task.clone();
        let ts = Instant::now();
        let r = tokio::task::spawn_blocking(move || me.osw("setup", num, &t, None)).await.map_err(e500)?;
        let secs = ts.elapsed().as_secs_f64();
        self.account(Some(&slot), |l| {
            l.op("task_setup", secs, r.is_ok());
            if r.is_err() {
                l.failure("task_setup");
            }
        });
        match r {
            Ok(_) => Ok(self.put_slot(&slot, |r| {
                r.task = Some(task.clone());
                r.task_setup_on_restore = true;
                r.verified = None;
                r.timings.insert("task_setup".into(), round3(secs));
            })),
            Err(e) => {
                // the desktop is in an unknown part-set-up state: the episode is not the task's start state
                let r = self.put_slot(&slot, |r| {
                    r.state = SlotState::Quarantined { reason: format!("task setup failed: {}", e.chars().take(300).collect::<String>()) };
                    r.last_error = Some(e.clone());
                });
                Err(ApiError::new(422, format!("task setup failed on {}: {e}; slot quarantined ({})", r.id, r.state.name())))
            }
        }
    }

    /// Run the task's checker on the slot's current state. The reward always carries the episode's gate verdict; a
    /// reward from an episode that is not valid is returned with `valid: false`, never as a plain score.
    pub async fn verify(self: &Arc<Self>, id: &str) -> R<Value> {
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        let t0 = Instant::now();
        let (task, state, pid) = {
            let v = slot.view.read().unwrap();
            (v.task.clone(), v.state.clone(), v.init_pid)
        };
        let task = task.ok_or_else(|| ApiError::new(400, format!("slot {id} has no task: create it with a task, or POST /v1/slots/{id}/task")))?;
        let tid = osworld_task_id(&task).ok_or_else(|| ApiError::new(400, "slot's task is not an OSWorld task (no evaluator)"))?;
        if !matches!(state, SlotState::Ready | SlotState::Quarantined { .. }) || pid.is_none() {
            return Err(ApiError::new(409, format!("slot {id} is {}: nothing running to verify", state.name())));
        }
        // the alive gate resolves alive_after_s after the slot answered: wait for it, the reward needs the verdict
        let deadline = Instant::now() + Duration::from_secs_f64(self.cfg.alive_after_s + 30.0);
        while matches!(slot.view.read().unwrap().validity, Validity::Pending { .. }) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let num = slot.view.read().unwrap().num;
        let me = self.clone();
        let t = task.clone();
        let r = tokio::task::spawn_blocking(move || me.osw("evaluate", num, &t, None)).await.map_err(e500)?;
        let v = slot.view.read().unwrap().clone();
        let valid = v.validity == Validity::Valid && matches!(v.state, SlotState::Ready);
        let (score, err, detail) = match &r {
            Ok(x) => (x["score"].as_f64(), x["checker_error"].as_str().map(|s| s.to_string()), x.clone()),
            Err(e) => (None, Some(e.clone()), Value::Null),
        };
        let secs = t0.elapsed().as_secs_f64();
        self.account(Some(&slot), |l| {
            l.op("verify", secs, score.is_some());
            l.verified(score, valid);
            if score.is_none() {
                l.failure("checker_error");
            }
        });
        let out = json!({
            "slot": v.id, "episode": v.episode, "task_id": tid, "score": score, "valid": valid,
            "validity": v.validity, "state": v.state.name(), "gates": v.gates, "checker_error": err,
            "checker": {"func": task["evaluator"]["func"], "osworld_commit": detail["osworld_commit"], "seconds": detail["seconds"],
                        "postconfig_ran": detail["postconfig_ran"]},
            "seconds": round3(secs),
        });
        self.event(json!({"slot": v.id, "verify": {"episode": v.episode, "task": tid, "score": score, "valid": valid, "error": err}}));
        self.metrics.inc("drumsd_verify_total", &[("valid", if valid { "true" } else { "false" }), ("scored", if score.is_some() { "true" } else { "false" })], 1.0);
        self.put_slot(&slot, |r| r.verified = Some(out.clone()));
        Ok(out)
    }

    /// The OSWorld tasks in the pinned checkout: id, domain, instruction.
    pub fn tasks(&self, domain: Option<&str>) -> R<Vec<Value>> {
        let o = self.cfg.osw.as_ref().ok_or_else(|| ApiError::new(404, "no OSWorld checkout configured"))?;
        let ex = o.osworld_dir.join("evaluation_examples").join("examples");
        let mut out = vec![];
        for d in std::fs::read_dir(&ex).map_err(e500)? {
            let d = d.map_err(e500)?.path();
            let dom = d.file_name().unwrap().to_string_lossy().to_string();
            if domain.map(|x| x != dom).unwrap_or(false) || !d.is_dir() {
                continue;
            }
            for f in std::fs::read_dir(&d).map_err(e500)? {
                let f = f.map_err(e500)?.path();
                if f.extension().map(|x| x == "json").unwrap_or(false) {
                    if let Ok(t) = serde_json::from_slice::<Value>(&std::fs::read(&f).unwrap_or_default()) {
                        out.push(json!({"id": t["id"], "domain": dom, "instruction": t["instruction"]}));
                    }
                }
            }
        }
        out.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        Ok(out)
    }

    /// One task by id or unique id prefix (>= 8 characters).
    pub fn task(&self, id: &str) -> R<Value> {
        if id.len() < 8 || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Err(ApiError::new(400, "task id: at least 8 characters of the OSWorld id"));
        }
        let o = self.cfg.osw.as_ref().ok_or_else(|| ApiError::new(404, "no OSWorld checkout configured"))?;
        let ex = o.osworld_dir.join("evaluation_examples").join("examples");
        let mut hits = vec![];
        for d in std::fs::read_dir(&ex).map_err(e500)? {
            let d = d.map_err(e500)?.path();
            for f in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let n = f.file_name().to_string_lossy().to_string();
                if n.starts_with(id) && n.ends_with(".json") {
                    hits.push((d.file_name().unwrap().to_string_lossy().to_string(), f.path()));
                }
            }
        }
        match hits.as_slice() {
            [(dom, p)] => {
                let raw = std::fs::read(p).map_err(e500)?;
                let mut t: Value = serde_json::from_slice(&raw).map_err(e500)?;
                t["_domain"] = json!(dom);
                t["_sha256"] = json!(hex_sha256(&raw));
                Ok(t)
            }
            [] => Err(ApiError::new(404, format!("no OSWorld task {id}"))),
            _ => Err(ApiError::new(400, format!("task id {id} is ambiguous ({} matches)", hits.len()))),
        }
    }

    // ------------------------------------------------------------------------------------------------ metrics
    fn slot_seconds_now(&self, window_start: f64) -> f64 {
        let now = drumsd_linux::now_epoch();
        self.slots.read().unwrap().values().map(|s| {
            let v = s.view.read().unwrap();
            if v.closed_epoch.is_some() || v.state.is_terminal() { 0.0 } else { (now - v.created_epoch.max(window_start)).max(0.0) }
        }).sum()
    }

    /// Aggregate metrics for the current window (see drumsd_core::usage for every definition).
    pub fn metrics_summary(&self) -> Value {
        let u = self.usage.lock().unwrap().clone();
        let now = drumsd_linux::now_epoch();
        let wall = now - u.window_start_epoch;
        let slot_s = u.closed_slot_seconds + self.slot_seconds_now(u.window_start_epoch);
        let c = usage::cost(self.cfg.host_usd_per_hour, wall, slot_s, &u.ledger);
        let mut states: BTreeMap<&'static str, u64> = BTreeMap::new();
        for s in self.slots.read().unwrap().values() {
            *states.entry(s.view.read().unwrap().state.name()).or_default() += 1;
        }
        json!({"window": {"start_epoch": u.window_start_epoch, "now_epoch": now, "wall_s": round3(wall)},
               "slots_by_state": states, "cost": c, "usage": u.ledger.summary(),
               "definitions": "seconds = wall time inside drumsd per op, failed attempts included; episodes counted once when the verdict resolves; cost = host_usd_per_hour x wall hours of the window, divided by valid episodes / verified rewards from valid episodes; price is the operator's configured number"})
    }

    pub fn slot_metrics(&self, id: &str) -> R<Value> {
        let s = self.slot(id)?;
        let v = s.view.read().unwrap().clone();
        let u = self.usage.lock().unwrap().clone();
        let now = drumsd_linux::now_epoch();
        let end = v.closed_epoch.unwrap_or(now);
        let life = (end - v.created_epoch).max(0.0);
        let in_window = (end - v.created_epoch.max(u.window_start_epoch)).max(0.0);
        let all = u.closed_slot_seconds + self.slot_seconds_now(u.window_start_epoch);
        let host_usd = self.cfg.host_usd_per_hour * (now - u.window_start_epoch) / 3600.0;
        Ok(json!({"slot": v.id, "episode": v.episode, "state": v.state.name(), "validity": v.validity, "lifetime_s": round3(life),
                  "last_op_timings": v.timings, "last_cost": {"attempts": v.cost.attempts, "wasted_s": round3(v.cost.wasted_seconds())},
                  "usage": v.usage.summary(),
                  "cost": {"host_usd_per_hour": self.cfg.host_usd_per_hour, "slot_seconds_in_window": round3(in_window),
                           "usd_share_by_slot_seconds": usage::share(host_usd, in_window, all),
                           "usd_if_alone_on_host": round3(self.cfg.host_usd_per_hour * life / 3600.0 * 1000.0) / 1000.0}}))
    }

    /// Start a new metrics window (the old ledger goes to the event log).
    pub fn reset_metrics(&self) -> Value {
        let mut u = self.usage.lock().unwrap();
        let old = u.clone();
        *u = Usage { window_start_epoch: drumsd_linux::now_epoch(), ..Default::default() };
        let _ = self.journal.put("meta", "usage", &*u);
        drop(u);
        self.event(json!({"metrics_window_closed": old}));
        json!({"closed": old.ledger.summary(), "new_window_start_epoch": self.usage.lock().unwrap().window_start_epoch})
    }

    pub fn health(&self) -> Value {
        let ck: Vec<Value> = self.ckpts.read().unwrap().values().map(|c| {
            let sp = slotfs::paths(&self.cfg.state_dir, c.slot);
            json!({"id": c.id, "template_server": template::adopt(&c.socket, &sp.images), "task": c.task.as_ref().map(|t| t["id"].clone())})
        }).collect();
        let mut states: BTreeMap<&'static str, u64> = BTreeMap::new();
        for s in self.slots.read().unwrap().values() {
            *states.entry(s.view.read().unwrap().state.name()).or_default() += 1;
        }
        let free = |p: &Path| -> Option<u64> {
            let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
            let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
            (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
        };
        let ok = ck.iter().all(|c| !c["template_server"].is_null());
        json!({"ok": ok, "version": env!("CARGO_PKG_VERSION"), "uptime_s": round3(drumsd_linux::now_epoch() - self.started_epoch),
               "criu": {"path": self.cfg.criu, "sha256": self.criu_sha256, "nested_pidns": self.cfg.nested_pidns},
               "image_sha256": self.cfg.image_sha256, "checkpoints": ck, "slots_by_state": states,
               "osworld": self.cfg.osw.as_ref().map(|o| json!({"dir": o.osworld_dir})),
               "state_dir_free_bytes": free(&self.cfg.state_dir),
               "host_mem_available_bytes": std::fs::read_to_string("/proc/meminfo").ok().and_then(|m| m.lines().find(|l| l.starts_with("MemAvailable:"))
                   .and_then(|l| l.split_whitespace().nth(1)).and_then(|k| k.parse::<u64>().ok())).map(|k| k * 1024),
               "loadavg": std::fs::read_to_string("/proc/loadavg").ok().map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" ")), "host_usd_per_hour": self.cfg.host_usd_per_hour,
               "limits": {"vcpus": self.cfg.vcpus, "memory_mib": self.cfg.memory_mib, "max_concurrent_restores": self.cfg.max_concurrent_restores}})
    }

    // ------------------------------------------------------------------------------------------------ restart
    /// Reconcile the journal with the kernel after a (re)start. Transient states mean the daemon died mid-operation.
    pub async fn recover(self: &Arc<Self>) -> Result<Value, String> {
        let mut report = vec![];
        let ckpts: Vec<(String, CheckpointRecord)> = self.journal.all("checkpoints").map_err(|e| e.to_string())?;
        for (_, c) in ckpts {
            let sp = slotfs::paths(&self.cfg.state_dir, c.slot);
            let mut c2 = c.clone();
            let adopted = template::adopt(&c.socket, &sp.images);
            c2.server_pid = adopted;
            self.put_ckpt(&c2);
            report.push(json!({"checkpoint": c.id, "server": adopted}));
        }
        let recs: Vec<(String, SlotRecord)> = self.journal.all("slots").map_err(|e| e.to_string())?;
        for (_, rec) in recs {
            if rec.state.is_terminal() {
                continue;
            }
            let (targets, handles) = match self.bind_proxies(rec.num).await {
                Ok(x) => x,
                Err(e) => {
                    report.push(json!({"slot": rec.id, "error": e.message}));
                    continue;
                }
            };
            let id = rec.id.clone();
            let num = rec.num;
            let alive = rec.init_pid.map(|p| {
                procfs::start_ticks(p) == rec.init_start_ticks
                    && procfs::cgroup_path(p).map(|c| c.contains(&format!("/drumsbench-{}-pool.scope", slotfs::marker_for(&self.cfg.state_dir, num)))).unwrap_or(false)
            });
            let alive = alive.unwrap_or(false);
            let mut rec2 = rec.clone();
            let action;
            if rec.state.is_transient() {
                let _ = kill_slot(&slotfs::marker_for(&self.cfg.state_dir, num));
                rec2.state = if matches!(rec.state, SlotState::Capturing) {
                    SlotState::Quarantined { reason: "daemon restarted during capture".into() }
                } else {
                    SlotState::Down { reason: format!("daemon restarted during {}", rec.state.name()) }
                };
                rec2.init_pid = None;
                rec2.validity = Validity::Invalid { gates: vec![] };
                action = "transient -> cleaned";
            } else if matches!(rec.state, SlotState::Ready) && !alive {
                rec2.state = SlotState::Down { reason: "tree not running after daemon restart".into() };
                rec2.init_pid = None;
                action = "ready but dead -> down";
            } else {
                action = if alive { "adopted running tree" } else { "kept" };
            }
            if alive && matches!(rec2.state, SlotState::Ready | SlotState::Quarantined { .. }) {
                for t in &targets {
                    let _ = t.set_pid(rec.init_pid.unwrap());
                }
            }
            let slot = Arc::new(Slot { id: id.clone(), op: Mutex::new(()), view: RwLock::new(rec2.clone()), targets, proxies: StdMutex::new(handles) });
            self.slots.write().unwrap().insert(id.clone(), slot.clone());
            self.put_slot(&slot, |_| {});
            // a pending alive gate of a still-running episode is judged now
            if matches!(rec2.state, SlotState::Ready) && rec2.gates.get(Gate::AliveAfterResume).map(|g| g.status == GateStatus::Pending).unwrap_or(false) {
                let gen = slotfs::read_json(&slotfs::paths(&self.cfg.state_dir, num).slot_json).ok().and_then(|v| v["restore_generation"].as_u64()).unwrap_or(0);
                let hook_dir = slotfs::paths(&self.cfg.state_dir, num).dir.join(format!("lock-hook-{gen}"));
                let me = self.clone();
                let s2 = slot.clone();
                let ep = rec2.episode;
                tokio::task::spawn_blocking(move || me.judge_alive(&s2, ep, &hook_dir, Instant::now().checked_sub(Duration::from_secs(60)).unwrap_or_else(Instant::now))).await.map_err(|e| e.to_string())?;
            }
            report.push(json!({"slot": id, "was": rec.state.name(), "now": slot.view.read().unwrap().state.name(), "action": action}));
        }
        self.event(json!({"recovered": report}));
        self.refresh_slot_gauges();
        Ok(json!(report))
    }
}

/// Kill every process of a slot: both leaves and the slot cgroup itself.
pub fn kill_slot(marker: &str) -> std::io::Result<()> {
    for l in [1, 2] {
        cgroup::kill_and_wait(&cgroup::slot_leaf(marker, l), Duration::from_secs(20))?;
    }
    cgroup::kill_and_wait(&cgroup::slot_cg(marker), Duration::from_secs(20)).map(|_| ())
}

fn dir_bytes(p: &Path) -> u64 {
    let Ok(m) = p.symlink_metadata() else { return 0 };
    if m.is_dir() {
        std::fs::read_dir(p).map(|d| d.flatten().map(|e| dir_bytes(&e.path())).sum()).unwrap_or(0)
    } else {
        m.len()
    }
}

pub fn hex_sha256(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(b))
}

/// An OSWorld task: an `id` usable as a directory name, a `config` list and an `evaluator` object.
pub fn osworld_task_id(t: &Value) -> Option<String> {
    let id = t["id"].as_str()?;
    if id.is_empty() || id.len() > 128 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return None;
    }
    if !t["evaluator"].is_object() || !(t["config"].is_array() || t["config"].is_null()) {
        return None;
    }
    Some(id.to_string())
}

fn verdict_str(v: &Validity) -> &'static str {
    match v {
        Validity::Valid => "valid",
        Validity::Pending { .. } => "pending",
        Validity::Invalid { .. } => "invalid",
    }
}

pub fn status_str(s: &GateStatus) -> &'static str {
    match s {
        GateStatus::Pass => "pass",
        GateStatus::Fail => "fail",
        GateStatus::Pending => "pending",
    }
}

/// The three STATE_SECTIONS through the guest server, concurrently, hashed exactly as the Python path does.
pub fn state_hash(g: &Guest) -> Result<String, String> {
    let outs: Vec<Result<String, String>> = std::thread::scope(|s| {
        let hs: Vec<_> = statehash::STATE_SECTIONS.iter().map(|c| s.spawn(move || g.sh(c).map_err(|e| e.to_string()))).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut o: [String; 3] = Default::default();
    for (i, r) in outs.into_iter().enumerate() {
        o[i] = r?;
    }
    Ok(statehash::state_hash(&o))
}

/// Actions: {"type": "execute", "command": [..] | "..", "shell": bool}, {"type": "pyautogui", "code": "..."},
/// {"type": "launch", "command": .., "shell": bool} (detached, OSWorld /setup/launch), {"type": "sleep", "seconds": s}.
pub fn run_action(g: &Guest, a: &Value) -> std::io::Result<Value> {
    match a["type"].as_str().unwrap_or("execute") {
        "execute" => g.execute(a["command"].clone(), a["shell"].as_bool().unwrap_or(false)),
        "pyautogui" => {
            let code = a["code"].as_str().unwrap_or("");
            g.execute(json!(["python3", "-c", format!("import pyautogui; pyautogui.FAILSAFE = False; {code}")]), false)
        }
        "launch" => {
            let r = g.request("POST", "/setup/launch", Some(&json!({"command": a["command"], "shell": a["shell"].as_bool().unwrap_or(false)})))?;
            if r.status != 200 {
                return Err(std::io::Error::other(format!("/setup/launch HTTP {}: {}", r.status, String::from_utf8_lossy(&r.body).chars().take(300).collect::<String>())));
            }
            Ok(json!({"status": "success"}))
        }
        "sleep" => {
            std::thread::sleep(Duration::from_secs_f64(a["seconds"].as_f64().unwrap_or(0.0).min(60.0)));
            Ok(json!({"status": "success"}))
        }
        t => Err(std::io::Error::other(format!("unknown action type {t:?}"))),
    }
}

/// What the API returns for a slot.
pub fn view(r: &SlotRecord) -> Value {
    let mut v = serde_json::to_value(r).unwrap();
    v["url"] = json!(format!("http://127.0.0.1:{}", r.port));
    v
}

pub fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
    }
}
