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
use drumsd_core::gates::{Gate, GateReport, GateResult, GateStatus, Validity};
use drumsd_core::journal::Journal;
use drumsd_core::metrics::{Registry, LATENCY_BUCKETS};
use drumsd_core::recovery::{self, Action, Attempt, CostRecord, FailureClass, Policy};
use drumsd_core::state::{Op, Outcome, SlotState};
use drumsd_core::statehash;
use drumsd_linux::{cgroup, checks, guest::Guest, hook, procfs, proxy, restore, slotfs, template};
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
}

impl ApiError {
    pub fn new(status: u16, m: impl Into<String>) -> Self {
        ApiError { status, message: m.into() }
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
        let m = Manager {
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
            };
            me.put_ckpt(&c);
            me.event(json!({"checkpoint": c.id, "adopted_slot": slot, "server_pid": srv.pid, "ready_s": c.server_ready_s}));
            me.metrics.set("drumsd_template_servers", &[("state", "up")], me.ckpts.read().unwrap().len() as f64);
            Ok(c)
        })
        .await
        .map_err(e500)?
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

    pub async fn create(self: &Arc<Self>, checkpoint: String, task: Option<Value>) -> R<SlotRecord> {
        let c = self.ckpt(&checkpoint)?;
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
        let total = t0.elapsed().as_secs_f64();
        let rec = match outcome {
            Ok(ok) => {
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
                timings.insert("total".into(), round3(total));
                let r = self.put_slot(slot, |r| {
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
                let outcome = if f.class == FailureClass::GateFailed {
                    let g = f.gates.first_failure().map(|g| g.gate.as_str()).unwrap_or("?");
                    Outcome::GateFailed { gate: g.into(), detail: f.error.chars().take(300).collect() }
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
        let reference: BTreeMap<String, String> = std::fs::read(hook_dir.join("procs-pre-resume.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| serde_json::from_value(v["procs"].clone()).ok())
            .unwrap_or_default();
        let result = if reference.is_empty() {
            GateResult::fail(Gate::AliveAfterResume, 0.0, json!({"error": "no pre-resume process reference"}))
        } else {
            let now = pid.map(procfs::ns_procs).unwrap_or_default();
            let v = alive::judge(&reference, &now, &self.natural, ready_at.elapsed().as_secs_f64());
            let d = serde_json::to_value(&v).unwrap();
            if v.ok {
                GateResult::pass(Gate::AliveAfterResume, t0.elapsed().as_secs_f64(), d)
            } else {
                GateResult::fail(Gate::AliveAfterResume, t0.elapsed().as_secs_f64(), d)
            }
        };
        let failed = result.status == GateStatus::Fail;
        self.metrics.inc("drumsd_gate_results_total", &[("gate", "alive_after_resume"), ("status", status_str(&result.status))], 1.0);
        self.put_slot(slot, |r| {
            if r.episode != episode {
                return;
            }
            r.gates.push(result.clone());
            r.validity = r.gates.validity(&required_gates());
            if failed && matches!(r.state, SlotState::Ready) {
                r.state = SlotState::Quarantined { reason: format!("gate alive_after_resume failed: {}", result.detail["counted"]) };
            }
        });
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
        .map_err(e500)?
        .map_err(|e| ApiError::new(502, e.to_string()))?;
        let cur = slot.view.read().unwrap().clone();
        Ok(json!({"result": out.0, "screenshot_png_b64": out.1.map(|p| b64(&p)), "episode": cur.episode, "validity": cur.validity,
                  "seconds": round3(t0.elapsed().as_secs_f64())}))
    }

    pub async fn screenshot(self: &Arc<Self>, id: &str) -> R<Vec<u8>> {
        let slot = self.slot(id)?;
        let port = slot.view.read().unwrap().port;
        tokio::task::spawn_blocking(move || Guest::new(port, Duration::from_secs(30)).screenshot()).await.map_err(e500)?.map_err(|e| ApiError::new(502, e.to_string()))
    }

    /// Checkpoint the running slot with the existing capture verb (`--leave-running --no-continue`: CRIU dumps the
    /// frozen tree, its post-dump action copies the upper layer, the tree continues), then adopt the result.
    pub async fn capture(self: &Arc<Self>, id: &str, ckpt_id: Option<String>) -> R<CheckpointRecord> {
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        let st = slot.view.read().unwrap().state.begin(Op::Capture).map_err(|e| ApiError::new(409, e.to_string()))?;
        self.put_slot(&slot, |r| r.state = st);
        let into = self.alloc_num()?;
        // reserve the number while the verb runs (its directory appears only inside the verb)
        let num = slot.view.read().unwrap().num;
        let me = self.clone();
        let t0 = Instant::now();
        let res = tokio::task::spawn_blocking(move || me.run_capture_verb(num, into)).await.map_err(e500)?;
        match res {
            Ok(out) => {
                let cid = ckpt_id.unwrap_or_else(|| format!("{}-c{}", id, into));
                // the parent's desktop right now is the checkpoint's desktop (the tree was frozen for the dump and
                // no action ran since): its hash is the children's reference
                let port = slot.view.read().unwrap().port;
                let h = tokio::task::spawn_blocking(move || state_hash(&Guest::new(port, Duration::from_secs(30)))).await.map_err(e500)?.ok();
                let c = self.adopt_checkpoint(cid, into, h, Some(id.to_string())).await;
                let ok = c.is_ok();
                let outcome = if ok { Outcome::Ok } else { Outcome::Failed { error: "adopt after capture failed".into() } };
                self.put_slot(&slot, |r| {
                    r.state = SlotState::finish(Op::Capture, &outcome);
                    r.timings.insert("capture".into(), round3(t0.elapsed().as_secs_f64()));
                });
                self.event(json!({"slot": id, "captured_into": into, "verb": out}));
                self.metrics.inc("drumsd_ops_total", &[("op", "capture"), ("outcome", if ok { "ok" } else { "failed" })], 1.0);
                c
            }
            Err(e) => {
                self.put_slot(&slot, |r| {
                    r.state = SlotState::finish(Op::Capture, &Outcome::Failed { error: e.clone() });
                    r.last_error = Some(e.clone());
                });
                self.metrics.inc("drumsd_ops_total", &[("op", "capture"), ("outcome", "failed")], 1.0);
                Err(ApiError::new(500, e))
            }
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
    pub async fn branch(self: &Arc<Self>, id: &str, n: usize) -> R<Value> {
        if n == 0 || n > 64 {
            return Err(ApiError::new(400, "n must be 1..64"));
        }
        let t0 = Instant::now();
        let c = self.capture(id, None).await?;
        let t_cap = t0.elapsed().as_secs_f64();
        let task = self.slot(id)?.view.read().unwrap().task.clone();
        let futs: Vec<_> = (0..n)
            .map(|_| {
                let me = self.clone();
                let cid = c.id.clone();
                let task = task.clone();
                tokio::spawn(async move { me.create(cid, task.map(|mut t| { t["setup"] = json!([]); t })).await })
            })
            .collect();
        let mut children = vec![];
        for f in futs {
            match f.await.map_err(e500)? {
                Ok(r) => children.push(serde_json::to_value(view(&r)).unwrap()),
                Err(e) => children.push(json!({"error": e.message})),
            }
        }
        Ok(json!({"checkpoint": c, "children": children, "capture_s": round3(t_cap), "children_ready_s": round3(t0.elapsed().as_secs_f64() - t_cap)}))
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
        Ok(self.put_slot(&slot, |r| {
            r.state = SlotState::finish(Op::Destroy, &outcome);
            r.init_pid = None;
            if let Ok(k) = &res {
                r.timings.insert("destroyed_records_at_epoch".into(), drumsd_linux::now_epoch());
                r.last_error = Some(format!("records kept at {}", k.display()));
            }
        }))
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

/// Actions: {"type": "execute", "command": [..] | "..", "shell": bool} or {"type": "pyautogui", "code": "..."}.
pub fn run_action(g: &Guest, a: &Value) -> std::io::Result<Value> {
    match a["type"].as_str().unwrap_or("execute") {
        "execute" => g.execute(a["command"].clone(), a["shell"].as_bool().unwrap_or(false)),
        "pyautogui" => {
            let code = a["code"].as_str().unwrap_or("");
            g.execute(json!(["python3", "-c", format!("import pyautogui; pyautogui.FAILSAFE = False; {code}")]), false)
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
