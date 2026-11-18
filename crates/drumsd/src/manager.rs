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
use drumsd_core::lease::Lease;
use drumsd_core::metrics::{Registry, LATENCY_BUCKETS};
use drumsd_core::quiesce as q;
use drumsd_core::recovery::{self, Action, Attempt, CostRecord, FailureClass, Policy};
use drumsd_core::state::{Op, Outcome, SlotState};
use drumsd_core::statehash;
use drumsd_core::usage::{self, Ledger, Usage};
use drumsd_linux::{cgroup, checks, forget, guest::Guest, hook, procfs, proxy, quiesce, restore, slotfs, template};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore};

mod verifiers_api;

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
    /// When this episode's restore began (the crash watch judges the episode's tree from then on).
    #[serde(default)]
    pub episode_start_epoch: Option<f64>,
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
    /// This env's lease (None on records made before 0.4.2: the daemon's default applies).
    #[serde(default)]
    pub lease: Option<Lease>,
    /// The last API request that named this env (epoch seconds; the lease's idle time counts from it).
    #[serde(default)]
    pub last_used_epoch: Option<f64>,
    /// Incremental capture base: the images whose pages equal this env's memory except for the pages its soft-dirty
    /// bits mark (set by a restore that cleared them, or by a capture with --track-mem); None = next capture is full.
    #[serde(default)]
    pub dirty_base: Option<PathBuf>,
    /// Parent-chain depth of `dirty_base` (0 = a complete checkpoint).
    #[serde(default)]
    pub dirty_depth: u32,
    /// For a base that is the images the env was restored from: the uptime (us) when the restore cleared the soft-dirty
    /// bits, given to CRIU as --prev-images-uptime (tasks started later are pid reuse, dumped in full). None for a
    /// base made by a capture (its inventory holds the dump's uptime).
    #[serde(default)]
    pub dirty_base_uptime_us: Option<u64>,
    /// Why it was destroyed when it was not a client's DELETE (e.g. `lease: idle 1800 s`).
    #[serde(default)]
    pub closed_reason: Option<String>,
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
    /// What made the origin: the runtime sources' digest and the image inspection's digest at adopt time (see
    /// `provenance`). An installed image whose provenance is not the running one's is stale (`stale_reason`).
    #[serde(default)]
    pub provenance: Option<Value>,
    /// A saved state's lease (None: the daemon's default; never applied to installed images).
    #[serde(default)]
    pub lease: Option<Lease>,
    /// Delete this saved state as soon as no env made from it exists (fork with keep_state=false).
    #[serde(default)]
    pub delete_with_children: bool,
    /// When the last env made from it closed (its idle time counts from this or its creation).
    #[serde(default)]
    pub last_used_epoch: Option<f64>,
    /// Set when the env it was captured from had an app crash (or an unvouched stretch) in its episode before or
    /// during the dump: every env made from it starts with a failed `no_app_crash` gate naming this reason.
    #[serde(default)]
    pub crash_taint: Option<String>,
}

/// Why an installed image's checkpoint does not match this drumsd's runtime (None: it matches, or it is a saved state).
/// The origin's desktop was started by the runtime sources and the image inspection (its login facts) at provision
/// time; a checkpoint made by other ones would hand out envs with another session than the pinned runtime makes.
pub fn stale_reason(c: &CheckpointRecord, current: &Value) -> Option<String> {
    if c.captured_from.is_some() {
        return None;
    }
    match &c.provenance {
        None => Some("made before provenance was recorded (drumsd < 0.4): re-provision the origin".into()),
        Some(p) if p != current => {
            let diff: Vec<&str> =
                ["source_sha256", "inspection_sha256", "procview_dir", "harden"].into_iter().filter(|k| p.get(*k) != current.get(*k)).collect();
            Some(format!("made by other {} than this drumsd runs with: re-provision the origin", diff.join(" and ")))
        }
        _ => None,
    }
}

/// The provenance of an origin provisioned now: the digest of the runtime sources drumsd restores with and of the
/// inspection file named by the Python config (`inspection`), whose facts (user, session, login) the origin was made from.
pub fn provenance(source_sha: &str, py_config: &Path) -> Value {
    let cfg = std::fs::read(py_config).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let insp =
        cfg.as_ref().and_then(|v| v.get("inspection").and_then(|x| x.as_str()).map(PathBuf::from)).and_then(|p| std::fs::read(p).ok()).map(|b| hex_sha256(&b));
    let mut v = json!({"source_sha256": source_sha, "inspection_sha256": insp});
    // whether the origin's tree binds the per-env view: an origin made with the other setting is stale (re-provisioned)
    if let Some(pv) = cfg.as_ref().and_then(|c| c.get("procview_dir")).filter(|x| !x.is_null()) {
        v["procview_dir"] = pv.clone();
    }
    // the slot profile: an origin made unhardened (the trusted-only opt-out, "harden": false) and one made hardened are
    // not interchangeable; the hardened default keeps the earlier shape
    if cfg.as_ref().and_then(|c| c.get("harden")) == Some(&json!(false)) {
        v["harden"] = json!(false);
    }
    v
}

/// Whether `mp` is a mount point now (an entry of /proc/self/mountinfo).
pub fn is_mounted(mp: &Path) -> bool {
    let want = mp.to_string_lossy();
    let want = want.trim_end_matches('/');
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|m| m.lines().any(|l| l.split(' ').nth(4).map(|f| f.replace("\\040", " ").trim_end_matches('/') == want).unwrap_or(false)))
        .unwrap_or(false)
}

/// Why a checkpoint's state is gone (None: all of it is there). After a host reboot the state dir (a tmpfs) is empty and
/// the image mount is gone; the journal on disk still lists the checkpoint. `lower_mounted`: the lower layer is mounted.
pub fn state_lost(c: &CheckpointRecord, state_dir: &Path, lower: &Path, lower_mounted: bool) -> Option<String> {
    let sp = slotfs::paths(state_dir, c.slot);
    let mut gone: Vec<String> = vec![];
    if sp.dir.symlink_metadata().is_err() {
        gone.push(format!("slot directory {} is missing", sp.dir.display()));
    } else {
        let empty = std::fs::read_dir(&sp.images).map(|mut d| d.next().is_none()).unwrap_or(true);
        if empty {
            gone.push(format!("{} is missing or empty", sp.images.display()));
        }
        if sp.snap.symlink_metadata().is_err() {
            gone.push(format!("{} is missing", sp.snap.display()));
        }
    }
    if !lower_mounted {
        gone.push(format!("the image mount {} is not mounted", lower.display()));
    }
    (!gone.is_empty()).then(|| {
        format!("state_lost: {} (the host was restarted or its state dir emptied: run the installer or drums-boot.service to re-provision)", gone.join("; "))
    })
}

/// What the API returns for a checkpoint: its record plus `stale` (and why). `lost` is the reason its state is
/// lost (`state_lost`), which comes before a provenance mismatch.
pub fn ckpt_view_on(c: &CheckpointRecord, current: &Value, lost: Option<String>) -> Value {
    let mut v = serde_json::to_value(c).unwrap();
    let why = lost.or_else(|| stale_reason(c, current));
    v["stale"] = json!(why.is_some());
    v["stale_reason"] = json!(why);
    v
}

pub struct Slot {
    pub id: String,
    pub op: Mutex<()>,
    pub view: RwLock<SlotRecord>,
    pub targets: Vec<proxy::Target>,
    pub proxies: StdMutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Inputs the env took (steps, resets, task loads): a prefork is handed out only if none came after it started.
    pub inputs: std::sync::atomic::AtomicU64,
}

/// Resident capture verbs (`python -m sharedos_rootfs.capture serve`): one JSON request and one JSON reply per line.
/// A worker is taken for one capture and put back after; a worker that fails to answer is dropped.
#[derive(Default)]
pub struct VerbPool {
    idle: StdMutex<Vec<VerbWorker>>,
}

pub struct VerbWorker {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl Drop for VerbWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl VerbPool {
    /// Some((exit code, stdout, stderr)) of the verb with `argv`, or None if no worker could answer (caller runs a process).
    pub fn run(&self, argv: &[String], spawn: impl Fn() -> std::process::Command) -> Option<(Option<i32>, String, String)> {
        use std::io::{BufRead, Write};
        let w = self.idle.lock().unwrap().pop();
        let mut w = match w {
            Some(w) => w,
            None => {
                let mut c = spawn();
                c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
                let mut child = c.spawn().ok()?;
                let stdin = child.stdin.take()?;
                let stdout = std::io::BufReader::new(child.stdout.take()?);
                VerbWorker { child, stdin, stdout }
            }
        };
        let req = serde_json::to_string(&json!({"argv": argv})).ok()?;
        writeln!(w.stdin, "{req}").ok()?;
        w.stdin.flush().ok()?;
        let mut line = String::new();
        if w.stdout.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let v: Value = serde_json::from_str(&line).ok()?;
        let r = (v["rc"].as_i64().map(|x| x as i32), v["stdout"].as_str().unwrap_or("").to_string(), v["stderr"].as_str().unwrap_or("").to_string());
        self.idle.lock().unwrap().push(w);
        Some(r)
    }
}

/// A fork prepared ahead of the fork call (POST /v1/slots/:id/prefork): the same capture and children as a fork,
/// started right after an action so they are ready when the policy asks. Handed out by the next fork of the same
/// parent with the same n only when the parent took no input since it started, had no task start or exit since, and
/// its desktop state hash still equals the one the children were checked against; otherwise discarded and the fork
/// runs cold.
pub struct Prefork {
    pub n: usize,
    pub delete_state_with_children: bool,
    pub inputs_at_start: u64,
    pub started: Instant,
    pub started_epoch: f64,
    pub done: tokio::sync::watch::Receiver<Option<Result<Value, String>>>,
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
    alloc_cursor: std::sync::atomic::AtomicU32,
    usage_dirty: std::sync::atomic::AtomicBool,
    admit_in_flight: Arc<std::sync::atomic::AtomicU64>,
    admit_counts: StdMutex<BTreeMap<String, u64>>,
    dirty_slots: StdMutex<BTreeSet<String>>,
    source_sha: String,
    pub provenance: Value,
    limits: cgroup::Limits,
    natural: BTreeSet<String>,
    policy: Policy,
    pub usage: StdMutex<Usage>,
    pub token: String,
    pub started_epoch: f64,
    pub criu_sha256: String,
    /// Records pruned by retention since start, by class: (files, bytes).
    forget_pruned: StdMutex<BTreeMap<&'static str, (u64, u64)>>,
    /// Envs and saved states reaped by their lease since start, by "env:idle", "state:children_closed", ...
    lease_reaped: StdMutex<BTreeMap<String, u64>>,
    /// Saved states a fork is creating children from: the reaper leaves them alone until the children exist.
    pinned_states: StdMutex<BTreeSet<String>>,
    preforks: StdMutex<BTreeMap<String, Prefork>>,
    verbs: VerbPool,
    /// Held while a template server is being started (see ensure_server).
    server_start: StdMutex<()>,
    /// Exits by crash signal, attributed by cgroup (the `no_app_crash` gate). None only when `crash_watch` is off.
    pub crash: Option<drumsd_linux::crashwatch::CrashWatch>,
    /// Task starts and exits per cgroup (capture readiness from events, see `quiesce.events`). None when off.
    pub activity: Option<drumsd_linux::activity::ActivityWatch>,
    /// One-time provisioning capabilities: token -> the slot and (during a restore's task setup, before the slot record
    /// has them) its tree's init pid and cgroup; valid while one OSWorld helper call for that slot runs.
    provision_tokens: StdMutex<BTreeMap<String, ProvisionTarget>>,
    /// The image's desktop user (from the image inspection): provisioning steps run as this user, with sudo for the step.
    image_user: Option<ImageUser>,
    /// Uploaded lab verifiers by sha256 (None when the store directory could not be opened).
    pub vstore: Option<drumsd_core::verifiers::Store>,
}

#[derive(Clone, Debug)]
pub struct ProvisionTarget {
    pub num: u32,
    /// (init pid, the tree's cgroup) when the caller knows them better than the slot record (a restore's setup).
    pub tree: Option<(i32, PathBuf)>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ImageUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
}

/// The desktop user of the image, from the runtime config's inspection file.
pub fn read_image_user(py_config: &Path) -> Result<ImageUser, String> {
    let cfg: Value = serde_json::from_slice(&std::fs::read(py_config).map_err(|e| format!("{}: {e}", py_config.display()))?).map_err(|e| e.to_string())?;
    let insp = cfg["inspection"].as_str().ok_or("runtime config has no inspection path")?;
    let i: Value = serde_json::from_slice(&std::fs::read(insp).map_err(|e| format!("{insp}: {e}"))?).map_err(|e| e.to_string())?;
    serde_json::from_value(i["user"].clone()).map_err(|e| format!("inspection user: {e}"))
}

/// 32 random bytes, hex (a provisioning capability).
pub fn random_token() -> String {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).expect("/dev/urandom");
    hex::encode(b)
}

/// Lease and readiness options for a new env or saved state (from the request; None = the daemon's default).
#[derive(Clone, Debug, Default)]
pub struct LeaseOpts {
    pub idle_timeout_s: Option<f64>,
    pub ttl_s: Option<f64>,
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

/// Health warns when the filesystem holding `forgotten_dir` has less than this percentage free.
const FORGOTTEN_DISK_MIN_FREE_PCT: f64 = 10.0;
/// How much of a failed gate's detail the quarantine reason keeps (`state.reason`, what a client sees first). The full
/// detail is always in the record's `gates`. It was 300 characters: the cgroup gate's JSON was cut before the field
/// that said which check failed (T1-fixed host B s251, R2-3 host B s201-2: the reason ended inside `"limits":{`).
pub const GATE_DETAIL_CHARS: usize = 4000;

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
    /// When the CRIU restore was spawned: the episode's tree is judged by the crash watch from then on.
    spawn_epoch: f64,
    /// The pre-resume hook cleared the soft-dirty bits of every restored process (incremental capture base).
    softdirty_ok: bool,
    softdirty_uptime_us: Option<u64>,
    images: PathBuf,
}

impl Manager {
    pub fn new(cfg: Config) -> Result<Arc<Manager>, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        // drumsd's own cgroup (its gates, capture verbs and CRIU dumps run in it) ahead of the envs' episodes
        if cfg.daemon_cpu_weight > 0 {
            if let Some(cg) = std::fs::read_to_string("/proc/self/cgroup").ok().and_then(|c| c.lines().find_map(|l| l.strip_prefix("0::").map(String::from))) {
                let d = Path::new(cgroup::CG_ROOT).join(cg.trim_start_matches('/'));
                if let Err(e) = cgroup::set_cpu_weight(&d, cfg.daemon_cpu_weight) {
                    tracing::warn!("cpu.weight {} on drumsd's cgroup {}: {e}", cfg.daemon_cpu_weight, d.display());
                }
            }
        }
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
        metrics.gauge("drumsd_state_dir_free_bytes", "Free bytes on the state dir (tmpfs)");
        metrics.gauge("drumsd_procview_live", "1 when the per-env memory/CPU view mount answers");
        metrics.gauge("drumsd_forgotten_bytes", "Bytes of kept records of destroyed envs, by where they are (where) and by class (full, summary)");
        metrics.gauge("drumsd_forgotten_files", "Kept records of destroyed envs, by class (full, summary)");
        metrics.counter("drumsd_forget_total", "Slot directories moved off the state dir, by class (full, summary) and whether they were kept off the tmpfs");
        metrics.counter("drumsd_forget_pruned_total", "Kept records deleted by retention, by class");
        metrics.counter("drumsd_forget_pruned_bytes_total", "Bytes of kept records deleted by retention, by class");
        metrics.counter("drumsd_forget_tmpfs_bytes_freed_total", "Bytes of kept records moved off the state dir");
        metrics.gauge("drumsd_template_servers", "Template servers by liveness");
        metrics.counter("drumsd_lease_reaped_total", "Envs and saved states removed because their lease ran out, by kind and reason");
        metrics.histogram("drumsd_reset_seconds", "Reset latency, kill to all synchronous gates passed", LATENCY_BUCKETS);
        metrics.histogram("drumsd_create_seconds", "Create latency, branch dir to all synchronous gates passed", LATENCY_BUCKETS);
        metrics.histogram("drumsd_phase_seconds", "Restore pipeline phase latency", LATENCY_BUCKETS);
        metrics.histogram("drumsd_criu_restore_seconds", "CRIU's own restore clock (Restore finished successfully)", LATENCY_BUCKETS);
        let limits = cgroup::expected_limits(cfg.vcpus, cfg.memory_mib, cfg.swap_mib);
        let natural = cfg.natural_exits.iter().cloned().collect();
        let policy = Policy { max_attempts: cfg.max_attempts };
        let token = cfg.load_token()?;
        cfg.check_listen()?;
        let mut u: Usage = journal.get("meta", "usage").map_err(|e| format!("usage ledger: {e}"))?.unwrap_or_default();
        if u.window_start_epoch == 0.0 {
            u.window_start_epoch = drumsd_linux::now_epoch();
        }
        let provenance = provenance(&source_sha, &cfg.py_config);
        let criu_sha256 = std::fs::read(&cfg.criu).map(|b| hex_sha256(&b)).unwrap_or_else(|e| format!("unreadable: {e}"));
        // the crash watch starts before any env is adopted or made: an episode it did not see from its start is never
        // vouched for (judge() reports it incomplete)
        let crash =
            if cfg.crash_watch {
                Some(drumsd_linux::crashwatch::start(vec![cgroup::pool()]).map_err(|e| {
                    format!("crash watch (process events connector): {e}; set \"crash_watch\": false only on a host where no reward is trained on")
                })?)
            } else {
                tracing::warn!("crash_watch is off: an app that crashes in an env does not make its episode invalid");
                None
            };
        let activity = if cfg.quiesce.events {
            match drumsd_linux::activity::start() {
                Ok(w) => Some(w),
                Err(e) => {
                    tracing::warn!("activity watch (process events connector) not started: {e}; every capture samples its full quiescence window");
                    None
                }
            }
        } else {
            None
        };
        metrics.counter("drumsd_app_crashes_total", "Processes of envs that died of a crash signal, by signal");
        metrics.counter("drumsd_step_arg_over_limit_total", "Actions and tasks with a command string over the step-argument limit, by whether it was enforced");
        let image_user = match read_image_user(&cfg.py_config) {
            Ok(u) => Some(u),
            Err(e) => {
                tracing::warn!("no image user for provisioning steps ({e}): task setup steps that use sudo will fail");
                None
            }
        };
        let vstore = match drumsd_core::verifiers::Store::open(&cfg.verifiers.store_dir, cfg.verifiers.max_bytes) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("verifier store {}: {e}; uploads and tasks that name a verifier by hash are refused", cfg.verifiers.store_dir.display());
                None
            }
        };
        metrics.counter("drumsd_verifier_runs_total", "Sandboxed checker calls by error class");
        let m = Manager {
            vstore,
            crash,
            activity,
            provision_tokens: StdMutex::new(BTreeMap::new()),
            image_user,
            usage: StdMutex::new(u),
            token,
            started_epoch: drumsd_linux::now_epoch(),
            criu_sha256,
            forget_pruned: StdMutex::new(BTreeMap::new()),
            lease_reaped: StdMutex::new(BTreeMap::new()),
            pinned_states: StdMutex::new(BTreeSet::new()),
            preforks: StdMutex::new(BTreeMap::new()),
            verbs: VerbPool::default(),
            server_start: StdMutex::new(()),
            restore_sem: Arc::new(Semaphore::new(cfg.max_concurrent_restores)),
            cfg,
            exe,
            journal,
            metrics,
            slots: RwLock::new(BTreeMap::new()),
            ckpts: RwLock::new(BTreeMap::new()),
            alloc: StdMutex::new(BTreeSet::new()),
            alloc_cursor: std::sync::atomic::AtomicU32::new(0),
            usage_dirty: std::sync::atomic::AtomicBool::new(false),
            admit_in_flight: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            admit_counts: StdMutex::new(BTreeMap::new()),
            dirty_slots: StdMutex::new(BTreeSet::new()),
            source_sha,
            provenance,
            limits,
            natural,
            policy,
        };
        Ok(Arc::new(m))
    }

    // ------------------------------------------------------------------------------------------------ admission
    fn host_mem(&self) -> drumsd_core::admission::HostMem {
        let mut h = drumsd_core::admission::parse_host(
            &std::fs::read_to_string("/proc/meminfo").unwrap_or_default(),
            &std::fs::read_to_string("/proc/pressure/memory").unwrap_or_default(),
        );
        h.state_free_mib = statvfs_free(&self.cfg.state_dir).map(|b| b / (1024 * 1024));
        h
    }

    /// Admit an operation that starts `need` envs/captures, or refuse it (HTTP 429, code `overloaded`, with the
    /// reason). The returned guard holds the operation's expected memory as "in flight" until it is dropped.
    pub fn admit(&self, op: &str, need: drumsd_core::admission::Need) -> R<AdmitGuard> {
        use std::sync::atomic::Ordering::SeqCst;
        let c = &self.cfg.admission;
        let h = self.host_mem();
        let in_flight = self.admit_in_flight.load(SeqCst);
        match drumsd_core::admission::decide(c, &h, in_flight, need) {
            Ok(()) => {
                let mib = if c.enabled { need.mib(c) } else { 0 };
                self.admit_in_flight.fetch_add(mib, SeqCst);
                *self.admit_counts.lock().unwrap().entry(format!("admitted:{op}")).or_default() += 1;
                Ok(AdmitGuard { in_flight: self.admit_in_flight.clone(), mib })
            }
            Err(r) => {
                *self.admit_counts.lock().unwrap().entry(format!("refused:{op}:{}", r.reason)).or_default() += 1;
                self.metrics.inc("drumsd_admission_refused_total", &[("op", op), ("reason", &r.reason)], 1.0);
                Err(ApiError::new(429, format!("overloaded: {op} refused: {}", r.message))
                    .with(json!({"code": "overloaded", "reason": r.reason, "admission": r})))
            }
        }
    }

    pub fn admission_view(&self) -> Value {
        let h = self.host_mem();
        json!({"enabled": self.cfg.admission.enabled, "config": self.cfg.admission, "host": h,
               "floor_mib": drumsd_core::admission::floor_mib(&self.cfg.admission, &h),
               "in_flight_mib": self.admit_in_flight.load(std::sync::atomic::Ordering::SeqCst),
               "counts": *self.admit_counts.lock().unwrap()})
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
        let (ep0, v0) = (v.episode, verdict_str(&v.validity));
        f(&mut v);
        v.updated_epoch = drumsd_linux::now_epoch();
        // a quarantined env's episode is invalid, with the reason named (gate not_quarantined): every validity
        // signal (verdict, gates, verify's valid, the metrics) then agrees
        if mark_quarantined(&mut v) {
            self.metrics.inc("drumsd_gate_results_total", &[("gate", "not_quarantined"), ("status", "fail")], 1.0);
        }
        // the episode ledgers follow the verdict, under the same lock as the record (counted once; a valid episode
        // that becomes invalid is moved)
        if let Some(c) = usage::episode_count_change(ep0, v0, v.episode, verdict_str(&v.validity)) {
            v.usage.apply(c);
            self.usage.lock().unwrap().ledger.apply(c);
            self.usage_dirty.store(true, std::sync::atomic::Ordering::Release);
        }
        // a failure leaves a sticky mark in slot.json: the env's records are kept in full at the end, even after a
        // later reset succeeded and cleared last_error
        if let Some(why) = failure_seen(&v) {
            let sp = slotfs::paths(&self.cfg.state_dir, v.num);
            if sp.dir.is_dir() {
                let _ = slotfs::record(&sp, json!({FAILURE_SEEN: why}));
            }
        }
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
    /// Count an op in the host ledger and the slot's. Only memory is touched here: the ledgers (up to 4096 latency
    /// samples per op) are written by `flush_ledgers` about once a second. Before, every op (each step) re-wrote and
    /// fsynced the whole host ledger (~450 KB) and the slot record under one global lock, from async code; at 64
    /// envs that serialized drumsd (step p50 3 s while the guest answered in 0.1 s). A crash loses at most the last
    /// second of metrics; slot state transitions are still journaled synchronously by put_slot.
    fn account(&self, slot: Option<&Slot>, f: impl Fn(&mut Ledger)) {
        {
            let mut u = self.usage.lock().unwrap();
            f(&mut u.ledger);
        }
        self.usage_dirty.store(true, std::sync::atomic::Ordering::Release);
        if let Some(s) = slot {
            let mut v = s.view.write().unwrap();
            f(&mut v.usage);
            self.dirty_slots.lock().unwrap().insert(v.id.clone());
        }
    }

    /// Write the ledgers `account` changed since the last call: the host ledger, then each dirty slot record (under
    /// its view lock, so a newer record written by put_slot is never overwritten by an older one).
    pub fn flush_ledgers(&self) -> usize {
        let mut n = 0;
        if self.usage_dirty.swap(false, std::sync::atomic::Ordering::AcqRel) {
            let u = self.usage.lock().unwrap();
            if let Err(e) = self.journal.put("meta", "usage", &*u) {
                tracing::error!("journal put usage: {e}");
            }
            n += 1;
        }
        let ids: Vec<String> = std::mem::take(&mut *self.dirty_slots.lock().unwrap()).into_iter().collect();
        for id in ids {
            let Some(s) = self.slots.read().unwrap().get(&id).cloned() else { continue };
            let v = s.view.read().unwrap();
            if let Err(e) = self.journal.put("slots", &v.id, &*v) {
                tracing::error!("journal put {}: {e}", v.id);
            }
            n += 1;
        }
        n
    }

    /// Start the ledger flusher (one thread, every second).
    pub fn start_flusher(self: &Arc<Self>) {
        let w = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("ledger-flush".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(1));
                match w.upgrade() {
                    Some(m) => {
                        m.flush_ledgers();
                    }
                    None => return,
                }
            })
            .expect("ledger flusher thread");
    }

    pub fn slot(&self, id: &str) -> R<Arc<Slot>> {
        if let Some(s) = self.slots.read().unwrap().get(id).cloned() {
            return Ok(s);
        }
        // destroyed before this daemon started: its record is only in the journal
        if let Ok(Some(r)) = self.journal.get::<SlotRecord>("slots", id) {
            if r.state.is_terminal() {
                return Err(gone_error(&r, "this operation"));
            }
        }
        Err(ApiError::new(404, format!("no env {id} on this drumsd")).with(json!({"code": "not_found", "env": id})))
    }

    pub fn ckpt(&self, id: &str) -> R<CheckpointRecord> {
        self.ckpts.read().unwrap().get(id).cloned().ok_or_else(|| ApiError::new(404, format!("no checkpoint {id}")))
    }

    /// Why this checkpoint refuses envs: its state is gone (a few stats, checked on every call) or its origin is stale.
    pub fn stale(&self, c: &CheckpointRecord) -> Option<String> {
        state_lost(c, &self.cfg.state_dir, &self.cfg.lower, is_mounted(&self.cfg.lower)).or_else(|| stale_reason(c, &self.provenance))
    }

    pub fn ckpt_view(&self, c: &CheckpointRecord) -> Value {
        ckpt_view_on(c, &self.provenance, state_lost(c, &self.cfg.state_dir, &self.cfg.lower, is_mounted(&self.cfg.lower)))
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
    pub async fn adopt_checkpoint(
        self: &Arc<Self>,
        id: String,
        slot: u32,
        expected_hash: Option<String>,
        captured_from: Option<String>,
    ) -> R<CheckpointRecord> {
        self.adopt_checkpoint_from(id, slot, expected_hash, None, captured_from).await
    }

    /// adopt_checkpoint; `hash_port`: read the state-hash reference from the guest server on that port (the captured
    /// parent's) while the template server loads the images, instead of before it.
    pub async fn adopt_checkpoint_from(
        self: &Arc<Self>,
        id: String,
        slot: u32,
        expected_hash: Option<String>,
        hash_port: Option<u16>,
        captured_from: Option<String>,
    ) -> R<CheckpointRecord> {
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
            // the server's logs live in the checkpoint's own directory, so deleting the state takes them off the
            // state tmpfs with its other records (they used to pile up in the state dir: 318 pairs, 91 MB in 7 h)
            let (srv, port_hash) = std::thread::scope(|sc| {
                let hh = hash_port.map(|p| sc.spawn(move || state_hash(&Guest::new(p, Duration::from_secs(30))).ok()));
                let srv = template::start(&me.cfg.criu, &sock, &sp.images, &sp.dir, me.cfg.criu_log_level);
                (srv, hh.and_then(|h| h.join().ok().flatten()))
            });
            let srv = srv.map_err(e500)?;
            let expected_hash = expected_hash.or(port_hash);
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
                provenance: Some(me.provenance.clone()),
                lease: None,
                delete_with_children: false,
                last_used_epoch: None,
                crash_taint: None,
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
    /// `installed`: also delete an installed image's checkpoint (the installer does this to re-provision a stale origin).
    pub async fn delete_checkpoint(self: &Arc<Self>, id: &str, installed: bool) -> R<Value> {
        let c = self.ckpt(id)?;
        if c.captured_from.is_none() && !installed {
            return Err(ApiError::new(
                409,
                format!("checkpoint {id} is an installed image, not a saved state (installed=true deletes it; the installer re-provisions it)"),
            ));
        }
        // a checkpoint whose state is gone (host restart) can be deleted though envs made from it are still on record: the
        // ones that are down hold nothing of it any more
        let lost = state_lost(&c, &self.cfg.state_dir, &self.cfg.lower, is_mounted(&self.cfg.lower)).is_some();
        let users: Vec<String> = self
            .slots
            .read()
            .unwrap()
            .values()
            .filter_map(|s| {
                let v = s.view.read().unwrap();
                (v.checkpoint == id && blocks_checkpoint_delete(&v.state, lost)).then(|| v.id.clone())
            })
            .collect();
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
            for d in [
                "images",
                "upper",
                "upper.snap",
                "work",
                "upper.next",
                "work.next",
                "upper.next.tmp",
                "trash",
                "criu-root",
                "softdirty-base",
                "softdirty-base.new",
            ] {
                let p = sp.dir.join(d);
                if p.symlink_metadata().is_ok() {
                    freed += dir_bytes(&p);
                    std::fs::remove_dir_all(&p)?;
                }
            }
            if sp.dir.exists() {
                me.forget_dir(&sp.dir, &format!("slot{}-{}", c2.slot, (drumsd_linux::now_epoch() * 1000.0) as u64), ForgetBasis::Checkpoint(&c2))?;
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
        // One start at a time: concurrent creates of a checkpoint whose server is down (after a reinstall, every one)
        // all missed `adopt` while the first start was still building the template, then found its process in the
        // server cgroup and failed with "template server cgroup ... still holds processes" (dev host, 9 of 12 creates).
        // The winner starts it; the others wait here and adopt it.
        let _one = self.server_start.lock().unwrap();
        if let Some(pid) = template::adopt(&c.socket, &sp.images) {
            return Ok(pid);
        }
        let srv = template::start(&self.cfg.criu, &c.socket, &sp.images, &sp.dir, self.cfg.criu_log_level).map_err(|e| e.to_string())?;
        let mut c2 = c.clone();
        c2.server_pid = Some(srv.pid);
        self.put_ckpt(&c2);
        self.event(json!({"checkpoint": c.id, "server_restarted": srv.pid}));
        Ok(srv.pid)
    }

    // ------------------------------------------------------------------------------------------------ slots
    fn alloc_num(&self) -> R<u32> {
        // A number is held by a live slot (a destroyed one has moved its directory aside and no longer holds it), a
        // checkpoint, anything still on disk under it (its directory or its cgroup), or a reservation in flight: a
        // create between alloc and its slot record, a capture until its verb has made (or failed to make) the dir.
        let mut reserved = self.alloc.lock().unwrap();
        let mut held: BTreeSet<u32> = self
            .slots
            .read()
            .unwrap()
            .values()
            .filter_map(|s| {
                let v = s.view.read().unwrap();
                (!v.state.is_terminal()).then_some(v.num)
            })
            .collect();
        held.extend(self.ckpts.read().unwrap().values().map(|c| c.slot));
        let sd = self.cfg.state_dir.clone();
        let on_disk = |n: u32| slotfs::paths(&sd, n).dir.symlink_metadata().is_ok() || cgroup::slot_cg(&slotfs::marker_for(&sd, n)).exists();
        let from = self.alloc_cursor.load(std::sync::atomic::Ordering::Relaxed);
        match pick_num(self.cfg.slot_range, from, &held, &reserved, on_disk) {
            Some(n) => {
                self.alloc_cursor.store(n + 1, std::sync::atomic::Ordering::Relaxed);
                reserved.insert(n);
                Ok(n)
            }
            None => Err(ApiError::new(503, "no free slot number")),
        }
    }

    /// End the in-flight reservation of `n` (its slot record, checkpoint or directory now holds it, or nothing does).
    fn release_num(&self, n: u32) {
        self.alloc.lock().unwrap().remove(&n);
    }

    /// A slot id never used before on this host (journal included), so a stale handle can never reach a newer env
    /// that got the same number: `s<num>`, then `s<num>-2`, `s<num>-3`, ...
    fn fresh_id(&self, num: u32) -> String {
        slot_id_for(num, |id| self.slots.read().unwrap().contains_key(id) || self.journal.get::<Value>("slots", id).ok().flatten().is_some())
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
    /// `lo`: the env's lease (idle timeout, TTL; None = the daemon's default, 0 = none).
    pub async fn create_leased(self: &Arc<Self>, checkpoint: String, task: Option<Value>, setup: Option<bool>, lo: LeaseOpts) -> R<SlotRecord> {
        for x in [lo.idle_timeout_s, lo.ttl_s].into_iter().flatten() {
            if !x.is_finite() || x < 0.0 {
                return Err(ApiError::new(400, "idle_timeout_s and ttl_s must be >= 0 (0 = no limit)"));
            }
        }
        let lease = Lease::resolve(lo.idle_timeout_s, lo.ttl_s, self.cfg.lease.env());
        let c = self.ckpt(&checkpoint)?;
        if let Some(why) = self.stale(&c) {
            return Err(ApiError::new(409, format!("checkpoint {checkpoint} is stale: {why} (run the installer again)"))
                .with(json!({"code": "checkpoint_stale", "provenance": c.provenance, "current": self.provenance})));
        }
        if let Some(t) = &task {
            if (t.get("config").is_some() || t.get("evaluator").is_some()) && osworld_task_id(t).is_none() {
                return Err(ApiError::new(400, "task: not an OSWorld task (needs an id of [A-Za-z0-9_-], a config list and an evaluator object)"));
            }
            if osworld_task_id(t).is_some() && self.cfg.osw.is_none() {
                return Err(ApiError::new(400, "task: this drumsd has no OSWorld helper configured (osw)"));
            }
            if let Some((id, why)) = osworld_task_id(t).and_then(|id| unsupported_reason(&id).map(|w| (id, w))) {
                return Err(ApiError::new(422, format!("task {id} is unsupported in this execution profile: {why}"))
                    .with(json!({"code": "task_unsupported", "task_id": id, "reason": why})));
            }
            self.check_task_verifiers(t)?;
            self.step_arg_limit(drumsd_core::limits::check_task_steps(t), t["id"].as_str().unwrap_or("-"))?;
        }
        let (task, setup_on_restore) = match task {
            Some(t) => {
                let osw = osworld_task_id(&t).is_some();
                (Some(t), osw && setup.unwrap_or(true))
            }
            None => (c.task.clone(), false),
        };
        let num = self.alloc_num()?;
        let id = self.fresh_id(num);
        let (targets, handles) = match self.bind_proxies(num).await {
            Ok(x) => x,
            Err(e) => {
                self.release_num(num);
                return Err(e);
            }
        };
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
            episode_start_epoch: None,
            created_epoch: now,
            updated_epoch: now,
            task_setup_on_restore: setup_on_restore,
            usage: Ledger::default(),
            verified: None,
            closed_epoch: None,
            lease: Some(lease),
            last_used_epoch: Some(now),
            closed_reason: None,
            dirty_base: None,
            dirty_depth: 0,
            dirty_base_uptime_us: None,
        };
        let slot = Arc::new(Slot {
            id: id.clone(),
            op: Mutex::new(()),
            view: RwLock::new(rec),
            targets,
            proxies: StdMutex::new(handles),
            inputs: std::sync::atomic::AtomicU64::new(0),
        });
        self.slots.write().unwrap().insert(id.clone(), slot.clone());
        self.release_num(num); // the live slot record holds the number now
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
            drumsd_linux::idrange::assign(&me.cfg.state_dir, &sp.dir, &me.cfg.lower)?; // its own host uid range (hardened)
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
        self.took_input(&slot);
        let st = begin_op(&slot, Op::Reset)?;
        self.finalize_episode(&slot);
        self.put_slot(&slot, |r| r.state = st);
        self.restore_episode(&slot, Op::Reset).await
    }

    /// OOM kills of the slot's tasks since its episode became ready (slot cgroup memory.events against the baseline
    /// recorded at the end of the restore). None when there is no baseline (no episode yet, or an older record).
    fn episode_oom_kills(&self, num: u32) -> Option<u64> {
        let sp = slotfs::paths(&self.cfg.state_dir, num);
        let base = slotfs::read_json(&sp.slot_json).ok()?["oom_kill_baseline"].as_u64()?;
        let now = cgroup::oom_kills(&cgroup::slot_cg(&slotfs::marker_for(&self.cfg.state_dir, num)))?;
        Some(now.saturating_sub(base))
    }

    /// The episode is invalid if the OOM killer took one of its tasks: a failed `no_oom_kill` gate, once per episode.
    fn check_oom(&self, slot: &Slot) {
        let (num, episode, has) = {
            let v = slot.view.read().unwrap();
            (v.num, v.episode, v.gates.get(Gate::NoOomKill).is_some())
        };
        if episode == 0 || has {
            return;
        }
        let Some(n) = self.episode_oom_kills(num).filter(|n| *n > 0) else { return };
        let r = GateResult::fail(Gate::NoOomKill, 0.0, json!({"oom_kills": n}));
        self.metrics.inc("drumsd_gate_results_total", &[("gate", "no_oom_kill"), ("status", "fail")], 1.0);
        self.event(json!({"slot": slot.id, "episode": episode, "oom_kills": n}));
        self.put_slot(slot, |rec| {
            if rec.episode == episode {
                rec.gates.push(r.clone());
                rec.validity = rec.gates.validity(&required_gates());
            }
        });
    }

    /// Both after-the-fact checks of a running episode: OOM kills and app crashes.
    fn check_episode(&self, slot: &Slot) {
        self.check_oom(slot);
        self.check_crash(slot);
    }

    /// The cgroup that holds the slot's running tree (its leaf, or the slot cgroup itself without leaves).
    fn episode_cg(&self, num: u32, leaf: u8) -> PathBuf {
        let marker = slotfs::marker_for(&self.cfg.state_dir, num);
        if leaf == 0 {
            cgroup::slot_cg(&marker)
        } else {
            cgroup::slot_leaf(&marker, leaf)
        }
    }

    /// What the crash watch says about the slot's current episode. None when there is no episode, or no watch.
    fn crash_judgment(&self, slot: &Slot) -> Option<(u64, drumsd_linux::crashwatch::Judgment)> {
        let w = self.crash.as_ref()?;
        let (num, leaf, episode, start, has_pid) = {
            let v = slot.view.read().unwrap();
            (v.num, v.leaf, v.episode, v.episode_start_epoch, v.init_pid.is_some())
        };
        if episode == 0 || !has_pid {
            return None;
        }
        let cg = self.episode_cg(num, leaf);
        // an episode with no recorded start (adopted from a record written before the watch existed) cannot be vouched for
        let since = start.unwrap_or(0.0);
        Some((episode, w.judge(&cg.display().to_string(), since)))
    }

    /// The episode is invalid if any process of its tree died of a crash signal, or if the watch cannot vouch for it:
    /// a failed `no_app_crash` gate, once per episode, naming what crashed.
    pub fn check_crash(&self, slot: &Slot) {
        if slot.view.read().unwrap().gates.get(Gate::NoAppCrash).map(|g| g.status == GateStatus::Fail).unwrap_or(false) {
            return;
        }
        let Some((episode, j)) = self.crash_judgment(slot) else { return };
        if j.clean() {
            return;
        }
        for c in &j.crashes {
            self.metrics.inc("drumsd_app_crashes_total", &[("signal", c.signal_name.as_str())], 1.0);
        }
        let r = GateResult::fail(Gate::NoAppCrash, 0.0, json!({"crashes": j.crashes, "incomplete": j.incomplete}));
        self.metrics.inc("drumsd_gate_results_total", &[("gate", "no_app_crash"), ("status", "fail")], 1.0);
        self.event(json!({"slot": slot.id, "episode": episode, "no_app_crash": r.detail}));
        let mut applied = false;
        self.put_slot(slot, |rec| {
            if rec.episode == episode {
                rec.gates.push(r.clone());
                rec.validity = rec.gates.validity(&required_gates());
                applied = true;
            }
        });
        if applied {
            self.account(Some(slot), |l| l.failure("gate:no_app_crash"));
        }
    }

    /// Count the episode that is ending by its final validity.
    fn finalize_episode(&self, slot: &Slot) {
        self.check_episode(slot);
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
        // the restore's critical path (CRIU, gates, task setup) runs ahead of the busy envs: the slot cgroup's
        // cpu.weight is raised until this function returns (WeightBoost puts the default back on drop)
        let _boost = {
            let num = slot.view.read().unwrap().num;
            let marker = slotfs::marker_for(&self.cfg.state_dir, num);
            match (self.cfg.restore_cpu_weight, cgroup::ensure_slot_cg(&marker, &self.limits)) {
                (w, Ok(d)) if w > 0 => cgroup::WeightBoost::new(&d, w),
                _ => cgroup::WeightBoost::new(Path::new("/nonexistent"), 0),
            }
        };
        let mut queue_wait = 0.0f64;
        let outcome = loop {
            attempt += 1;
            let tq = Instant::now();
            let permit = self.restore_sem.clone().acquire_owned().await.map_err(e500)?;
            queue_wait += tq.elapsed().as_secs_f64();
            let me = self.clone();
            let s2 = slot.clone();
            let ta = Instant::now();
            let res = tokio::task::spawn_blocking(move || me.restore_once(&s2, private, permit)).await.map_err(e500)?;
            let mode = if private { "private" } else { "template" };
            match res {
                Ok(ok) => {
                    cost.attempts.push(Attempt {
                        n: attempt,
                        mode: mode.into(),
                        seconds: round3(ta.elapsed().as_secs_f64()),
                        ok: true,
                        class: None,
                        error: None,
                        action: None,
                    });
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
                        // the new tree is not in the slot record yet: provisioning steps of this setup run in it
                        let tree = Some((ok.pid, self.episode_cg(num, ok.leaf)));
                        let r = tokio::task::spawn_blocking(move || me.osw_on("setup", num, &t, None, tree)).await.map_err(e500)?;
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
                            timings.insert("queue_wait".into(), round3(queue_wait));
                            timings.insert("total".into(), round3(t0.elapsed().as_secs_f64()));
                            return Ok(self.put_slot(slot, |r| {
                                r.dirty_base = None;
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
                timings.insert("queue_wait".into(), round3(queue_wait));
                timings.insert("cpu_weight_boost".into(), if _boost.active() { self.cfg.restore_cpu_weight as f64 } else { 0.0 });
                timings.insert("attempts".into(), attempt as f64);
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
                    r.episode_start_epoch = Some(ok.spawn_epoch);
                    r.dirty_base = ok.softdirty_ok.then(|| ok.images.clone());
                    r.dirty_depth = if ok.softdirty_ok { slotfs::chain_depth(&ok.images) } else { 0 };
                    r.dirty_base_uptime_us = ok.softdirty_uptime_us;
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
                    Outcome::GateFailed { gate: g.into(), detail: detail.chars().take(GATE_DETAIL_CHARS).collect() }
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
                timings.insert("queue_wait".into(), round3(queue_wait));
                timings.insert("total".into(), round3(total));
                self.put_slot(slot, |r| {
                    r.state = SlotState::finish(op, &outcome);
                    r.episode += 1;
                    r.dirty_base = None;
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
    /// One restore attempt. `permit` (one of max_concurrent_restores) is held from the kill through CRIU's restore
    /// and released as soon as the restored init is known: the gates, the guest probe and the task's setup run
    /// without it. Holding it to the end made the permits the bottleneck at 128 envs (a create waited ~56 s for a
    /// permit while its own pipeline took ~5.5 s, most of it gates and setup under CPU contention).
    fn restore_once(&self, slot: &Slot, private: bool, permit: tokio::sync::OwnedSemaphorePermit) -> Result<RestoreOk, RestoreFailure> {
        let mut t: BTreeMap<String, f64> = BTreeMap::new();
        let mut gates = GateReport::default();
        let fail = |class: FailureClass, error: String, gates: &GateReport, t: &BTreeMap<String, f64>| RestoreFailure {
            class,
            error,
            gates: gates.clone(),
            timings: t.clone(),
        };
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
            let (_, l1, l2) =
                cgroup::ensure_slot_leaves(&marker, &self.limits).map_err(|e| fail(FailureClass::Infra, format!("slot cgroups: {e}"), &gates, &t))?;
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

        // 2. fresh upper/work (warm spare when ready), only with room for it on the state dir
        self.state_space_ok().map_err(|e| fail(FailureClass::StateDirFull, e, &gates, &t))?;
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
        if self.cfg.incremental_capture {
            std::fs::write(hook_dir.join("softdirty-clear"), b"").map_err(|e| fail(FailureClass::Infra, format!("hook: {e}"), &gates, &t))?;
        }
        if self.cfg.restore_cpu_boost {
            let rec = json!({"paths": [cg, slot_cg], "cpu_max": self.limits.cpu_max});
            std::fs::write(hook_dir.join("cpu-restore.json"), serde_json::to_vec(&rec).unwrap())
                .map_err(|e| fail(FailureClass::Infra, format!("hook: {e}"), &gates, &t))?;
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
        // the lower layer the checkpoint ran on: the idmapped view of the image for a hardened slot (own user namespace)
        let (lower, userxattr) =
            slotfs::restore_lower(&sp.dir, &self.cfg.lower, is_mounted).map_err(|e| fail(FailureClass::Infra, format!("restore lower: {e}"), &gates, &t))?;
        let ids = drumsd_linux::idrange::restore_args(&sp.dir, &slotfs::paths(&self.cfg.state_dir, c.slot).dir)
            .map_err(|e| fail(FailureClass::Infra, format!("uid range: {e}"), &gates, &t))?;
        let spec = restore::RestoreSpec {
            images: sp.images.clone(),
            croot: sp.croot.clone(),
            pidfile: pidfile.clone(),
            log: log.clone(),
            hook: Some(hook),
            file_locks,
            lower,
            upper: sp.upper.clone(),
            work: sp.work.clone(),
            cgroup: cg.clone(),
            socket,
            criu_bin: self.cfg.criu.clone(),
            out: sp.dir.join(format!("restore-drumsd-{gen}-{stamp}.out")),
            empty_net: self.cfg.netns_fast,
            nested_pidns: self.cfg.nested_pidns,
            externals: [slot_dev_externals(&sp.dir), view_externals(&sp.dir, &self.cfg.procview_dir), ids].concat(),
            userxattr,
            log_level: self.cfg.criu_log_level,
        };
        t.insert("prepare".into(), round3(ts.elapsed().as_secs_f64()));

        // 4. CRIU restore (template server fork, or private)
        let ts = Instant::now();
        let spawn_epoch = drumsd_linux::now_epoch();
        let run =
            restore::run(&self.exe, &spec, Duration::from_secs(300)).map_err(|e| fail(FailureClass::Infra, format!("restore client: {e}"), &gates, &t))?;
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
            let error = format!(
                "criu restore failed (exit {:?}): {} | log: {}",
                run.exit,
                run.out_tail.trim(),
                tail.lines().rev().take(6).collect::<Vec<_>>().join(" / ")
            );
            let class = recovery::classify(&error, &tail);
            // nothing of a failed restore may keep running
            let _ = cgroup::kill_and_wait(&cg, Duration::from_secs(20));
            let _ = slotfs::record(&sp, json!({"state": "down", "last_restore_error": error.chars().take(400).collect::<String>()}));
            return Err(fail(class, error, &gates, &t));
        }
        let pid = pid.unwrap();
        drop(permit); // CRIU is done with this restore: the next one may start while this one's gates run
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
        let gh = Guest::new(self.cfg.port_base + num as u16, STATE_HASH_TIMEOUT);
        let ((hash, sections_s), iso) = std::thread::scope(|s| {
            let h = s.spawn(|| state_hash_timed(&gh));
            let i = s.spawn(|| checks::isolation_gate(pid, &others, &cg));
            (h.join().unwrap(), i.join().unwrap())
        });
        let hs = ts.elapsed().as_secs_f64();
        match (&hash, &c.expected_state_hash) {
            (Ok(h), Some(want)) if h == want => gates.push(GateResult::pass(Gate::StateHash, hs, json!({"hash": h, "sections_s": sections_s}))),
            (Ok(h), Some(want)) => gates.push(GateResult::fail(Gate::StateHash, hs, json!({"expected": want, "got": h, "sections_s": sections_s}))),
            (Ok(h), None) => {
                // the first child defines the reference (recorded as such on the checkpoint)
                let mut c2 = c.clone();
                c2.expected_state_hash = Some(h.clone());
                c2.hash_reference = format!("first child {}", slot.id);
                self.put_ckpt(&c2);
                gates.push(GateResult::pass(Gate::StateHash, hs, json!({"hash": h, "reference": "this restore defined it", "sections_s": sections_s})));
            }
            (Err(e), _) => gates.push(GateResult::fail(
                Gate::StateHash,
                hs,
                json!({"error": e, "sections_s": sections_s,
                                                                                    "timeout_s": STATE_HASH_TIMEOUT.as_secs_f64()}),
            )),
        }
        // one line per state-hash gate with the host's load at that moment: the 60 s budget must not hide overload
        // (the gate's own seconds and each guest request's are what a report computes the 10/30/60 s tails from)
        if let Some(g) = gates.gates.iter().rev().find(|g| g.gate == Gate::StateHash) {
            self.event(json!({"state_hash_gate": {"slot": slot.id, "status": g.status, "seconds": round3(hs), "sections_s": sections_s,
                "error": g.detail.get("error"), "timeout_s": STATE_HASH_TIMEOUT.as_secs_f64(), "guest_ready_s": t.get("guest_ready"),
                "load": load_snapshot(), "admit_in_flight_mib": self.admit_in_flight.load(std::sync::atomic::Ordering::SeqCst)}}));
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

        // a state saved after an app crashed holds the broken app: every env made from it is invalid from the start.
        // Added after the restore gates' check: the restore itself is sound, so the env comes up (not quarantined, not
        // killed) and its rewards say why they are not usable
        if let Some(taint) = &c.crash_taint {
            gates.push(GateResult::fail(Gate::NoAppCrash, 0.0, json!({"inherited": taint, "checkpoint": c.id})));
        }

        // the episode's OOM baseline, taken BEFORE the task setup: an app the setup launches can be OOM-killed while it
        // starts, and that kill belongs to this episode (it used to be taken after the setup and missed it). Any growth
        // of the slot cgroup's oom_kill counter (hierarchical memory.events: every task of the tree) invalidates it.
        let _ = slotfs::record(&sp, json!({"oom_kill_baseline": cgroup::oom_kills(&slot_cg)}));

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
        let sd = if self.cfg.incremental_capture { slotfs::read_json(&hook_dir.join("softdirty-clear.done")).ok() } else { None };
        let softdirty_uptime_us = sd.as_ref().filter(|v| v["ok"].as_bool() == Some(true)).and_then(|v| v["uptime_us"].as_u64());
        let softdirty_ok = softdirty_uptime_us.is_some();
        Ok(RestoreOk {
            leaf: new_leaf,
            old_cg,
            pid,
            start_ticks,
            gates,
            timings: t,
            hook_dir,
            ready_at,
            ready_epoch,
            upper_warm: swap.warm,
            spawn_epoch,
            softdirty_ok,
            softdirty_uptime_us,
            images: sp.images.clone(),
        })
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
        let mut applied = false;
        self.put_slot(slot, |r| {
            // only while the episode runs: one that a reset or destroy already ended was counted then (ended_pending)
            if r.episode != episode || !matches!(r.state, SlotState::Ready | SlotState::Capturing | SlotState::Quarantined { .. }) {
                return;
            }
            applied = true;
            r.gates.push(result.clone());
            r.validity = r.gates.validity(&required_gates());
            if failed && matches!(r.state, SlotState::Ready) {
                r.state = SlotState::Quarantined { reason: format!("gate alive_after_resume failed: {}", result.detail["counted"]) };
            }
        });
        // the episode itself is counted by put_slot when its verdict resolves
        if applied && failed {
            self.account(Some(slot), |l| l.failure("gate:alive_after_resume"));
        }
        // a crash in the first seconds (the restore itself, the task's setup) shows in the verdict wait_valid() reads
        self.check_crash(slot);
    }

    pub async fn step(self: &Arc<Self>, id: &str, action: Value, screenshot: bool) -> R<Value> {
        self.step_arg_limit(drumsd_core::limits::check_action(&action, "action"), id)?;
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        self.took_input(&slot);
        let v = slot.view.read().unwrap().clone();
        v.state.begin(Op::Step).map_err(|e| refused(&v, e))?;
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
        self.check_episode(&slot);
        let cur = slot.view.read().unwrap().clone();
        Ok(json!({"result": out.0, "screenshot_png_b64": out.1.map(|p| b64(&p)), "episode": cur.episode, "validity": cur.validity,
                  "seconds": round3(t0.elapsed().as_secs_f64())}))
    }

    pub async fn screenshot(self: &Arc<Self>, id: &str) -> R<Vec<u8>> {
        let slot = self.slot(id)?;
        self.gone_check(&slot, "screenshot")?;
        let port = slot.view.read().unwrap().port;
        tokio::task::spawn_blocking(move || Guest::new(port, Duration::from_secs(30)).screenshot())
            .await
            .map_err(e500)?
            .map_err(|e| ApiError::new(502, e.to_string()))
    }

    /// 410 `env_gone` if the env is destroyed or being destroyed.
    fn gone_check(&self, slot: &Slot, op: &str) -> R<()> {
        let v = slot.view.read().unwrap();
        if matches!(v.state, SlotState::Destroyed | SlotState::Destroying) {
            return Err(gone_error(&v, op));
        }
        Ok(())
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
        ApiError::new(
            409,
            format!("env {id} is not quiescent, nothing was captured: {why}; unstable: {}", unstable.to_string().chars().take(600).collect::<String>()),
        )
        .with(json!({"code": "not_quiescent", "unstable": unstable, "quiescence": report}))
    }

    /// Wait until the slot's tree is quiescent (no task leaving, no process or thread starting or exiting for the
    /// window). Holds the slot's operation lock, so no action runs meanwhile. 409 `not_quiescent` on a timeout.
    pub async fn wait_quiescent(self: &Arc<Self>, id: &str, o: QuiesceOpts) -> R<Value> {
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        begin_op(&slot, Op::Step)?;
        let p = self.quiesce_params(&o);
        let cg = self.tree_cg(&slot);
        let aw = self.activity.clone();
        let (rep, s) = tokio::task::spawn_blocking(move || quiesce::wait_with(&cg, &p, aw.as_ref())).await.map_err(e500)?.map_err(e500)?;
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
    /// `lo`: the saved state's lease; `delete_with_children`: delete it when its last env closes.
    pub async fn capture_leased(
        self: &Arc<Self>,
        id: &str,
        ckpt_id: Option<String>,
        o: QuiesceOpts,
        lo: LeaseOpts,
        delete_with_children: bool,
    ) -> R<CheckpointRecord> {
        for x in [lo.idle_timeout_s, lo.ttl_s].into_iter().flatten() {
            if !x.is_finite() || x < 0.0 {
                return Err(ApiError::new(400, "state idle_timeout_s and ttl_s must be >= 0 (0 = no limit)"));
            }
        }
        let state_lease = Lease::resolve(lo.idle_timeout_s, lo.ttl_s, self.cfg.lease.state());
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        let st = begin_op(&slot, Op::Capture)?;
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
            let aw = self.activity.clone();
            let waited = tokio::task::spawn_blocking(move || quiesce::wait_with(&cg2, &p, aw.as_ref())).await.map_err(e500)?;
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
            // 2. dump (the tree keeps running), only with room for the images on the state dir
            if let Err(e) = self.state_space_ok() {
                back_to_ready(self);
                self.account(Some(&slot), |l| l.failure("capture:state_dir_full"));
                return Err(ApiError::new(507, e).with(json!({"code": "state_dir_full"})));
            }
            let into = match self.alloc_num() {
                Ok(x) => x,
                Err(e) => {
                    back_to_ready(self);
                    return Err(e);
                }
            };
            let me = self.clone();
            let td = Instant::now();
            // the parasite runs in the parent's own tasks: its slot cgroup goes ahead of the busy envs during the dump
            let boost = match self.cfg.restore_cpu_weight {
                0 => cgroup::WeightBoost::new(Path::new("/nonexistent"), 0),
                w => cgroup::WeightBoost::new(&cgroup::slot_cg(&slotfs::marker_for(&self.cfg.state_dir, num)), w),
            };
            // incremental capture: on top of the env's base, if it has one that still exists and is not too deep
            let (base, base_up) = if self.cfg.incremental_capture {
                let v = slot.view.read().unwrap();
                (v.dirty_base.clone().filter(|b| b.is_dir() && v.dirty_depth < self.cfg.incremental_max_depth), v.dirty_base_uptime_us)
            } else {
                (None, None)
            };
            // whatever happens next, the soft-dirty bits no longer match the old base until a dump succeeds
            self.put_slot(&slot, |r| r.dirty_base = None);
            let b2 = base.clone();
            let mut res = tokio::task::spawn_blocking(move || me.run_capture_verb(num, into, b2.as_deref(), base_up)).await;
            att["incremental"] = json!(base.as_ref().map(|b| b.display().to_string()));
            if base.is_some() && matches!(res, Ok(Err(_))) {
                // the incremental dump failed: its attempt is kept under forgotten/, then the same capture in full
                let why = match &res {
                    Ok(Err(e)) => e.chars().take(400).collect::<String>(),
                    _ => String::new(),
                };
                let me = self.clone();
                let kept = tokio::task::spawn_blocking(move || me.discard_dump(into)).await.unwrap_or_else(|e| format!("not moved: {e}"));
                self.event(json!({"slot": id, "incremental_capture_failed": why, "into": into, "records": kept, "retry": "full dump"}));
                self.metrics.inc("drumsd_incremental_capture_total", &[("outcome", "fell_back_full")], 1.0);
                att["incremental_failed"] = json!(why);
                let me = self.clone();
                res = tokio::task::spawn_blocking(move || me.run_capture_verb(num, into, None, None)).await;
            }
            drop(boost);
            self.release_num(into); // its directory (or nothing, if the verb made none) holds the number now
            let res = res.map_err(e500)?;
            let out = match res {
                Ok(out) => out,
                Err(e) => {
                    // the verb's directory (records and the failed dump's log) leaves the state dir like a rejected
                    // dump's: it held the tmpfs and the slot number until the next restart before
                    let me = self.clone();
                    let kept = tokio::task::spawn_blocking(move || me.discard_dump(into)).await.unwrap_or_else(|e| format!("not moved: {e}"));
                    self.event(json!({"slot": id, "capture_failed": e, "into": into, "records": kept}));
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
            // an incremental dump's parent chain becomes part of its images (hard links), so deleting the base later
            // never breaks this checkpoint
            let qimg = slotfs::paths(&self.cfg.state_dir, into).images;
            let q2 = qimg.clone();
            let tp = Instant::now();
            let depth = match tokio::task::spawn_blocking(move || slotfs::pin_parent(&q2)).await.map_err(e500)? {
                Ok(d) => d,
                Err(e) => {
                    att["rejected"] = json!({"why": format!("pinning the incremental dump's parent chain failed: {e}")});
                    self.event(json!({"slot": id, "capture_attempt_rejected": att}));
                    let me = self.clone();
                    att["discarded_to"] = json!(tokio::task::spawn_blocking(move || me.discard_dump(into)).await.map_err(e500)?);
                    attempts.push(att);
                    continue;
                }
            };
            att["chain_depth"] = json!(depth);
            att["pin_s"] = json!(round3(tp.elapsed().as_secs_f64()));
            if base.is_some() && depth > 0 {
                self.metrics.inc("drumsd_incremental_capture_total", &[("outcome", "incremental")], 1.0);
            }
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
            //    The parent's state hash is read while the template server loads the images (both after the dump).
            let port = slot.view.read().unwrap().port;
            let c = self.adopt_checkpoint_from(cid.clone(), into, None, Some(port), Some(id.to_string())).await;
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
                    let _ = self.delete_checkpoint(&cid, false).await;
                    att["discarded_to"] = json!("forgotten/ (delete_checkpoint)");
                } else {
                    let me = self.clone();
                    att["discarded_to"] = json!(tokio::task::spawn_blocking(move || me.discard_dump(into)).await.map_err(e500)?);
                }
                attempts.push(att);
                continue;
            }
            attempts.push(att);
            // the parent's next capture is taken on top of this one: its base is a hard-link copy of these images
            if self.cfg.incremental_capture && c.is_ok() {
                let pdir = slotfs::paths(&self.cfg.state_dir, num).dir;
                let q2 = qimg.clone();
                let nb = tokio::task::spawn_blocking(move || -> std::io::Result<PathBuf> {
                    let (new, cur) = (pdir.join("softdirty-base.new"), pdir.join("softdirty-base"));
                    if new.symlink_metadata().is_ok() {
                        std::fs::remove_dir_all(&new)?;
                    }
                    slotfs::link_images(&q2, &new)?;
                    slotfs::copy_access(&q2, &new)?;
                    if cur.symlink_metadata().is_ok() {
                        std::fs::remove_dir_all(&cur)?;
                    }
                    std::fs::rename(&new, &cur)?;
                    Ok(cur)
                })
                .await
                .map_err(e500)?;
                match nb {
                    Ok(b) => {
                        self.put_slot(&slot, |r| {
                            r.dirty_base = Some(b);
                            r.dirty_depth = depth;
                            r.dirty_base_uptime_us = None;
                        });
                    }
                    Err(e) => self.event(json!({"slot": id, "incremental_base": format!("not kept: {e}; the next capture is full")})),
                }
            }
            // an app that crashed in the source before or during the dump is in the saved state too
            self.check_episode(&slot);
            let taint = slot.view.read().unwrap().gates.get(Gate::NoAppCrash).filter(|g| g.status == GateStatus::Fail).map(|g| app_crash_words(&g.detail));
            let quiescence = json!({"attempts": attempts, "params": params, "seconds": round3(t0.elapsed().as_secs_f64()),
                                    "wait_s_total": round3(attempts.iter().filter_map(|a| a["wait_s"].as_f64()).sum::<f64>())});
            // a fork's state (delete_with_children) is pinned before it is marked so: the reaper deletes such a state
            // as soon as no env uses it, and its children do not exist yet (branch_leased unpins it after them)
            if delete_with_children && c.is_ok() {
                self.pinned_states.lock().unwrap().insert(cid.clone());
            }
            let c = c.map(|mut c| {
                // slots made from this checkpoint inherit the task, whose setup is already in the captured state
                c.task = slot.view.read().unwrap().task.clone().map(|mut t| {
                    if t.get("setup").is_some() {
                        t["setup"] = json!([]);
                    }
                    t
                });
                c.quiescence = Some(quiescence.clone());
                c.lease = Some(state_lease);
                c.delete_with_children = delete_with_children;
                c.crash_taint = taint.clone();
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
        for d in ["images", "upper.snap", "upper", "work", "softdirty-base", "softdirty-base.new"] {
            let p = sp.dir.join(d);
            if p.symlink_metadata().is_ok() {
                let _ = std::fs::remove_dir_all(&p);
            }
        }
        match self.forget_dir(&sp.dir, &format!("slot{into}-rejected-{}", (drumsd_linux::now_epoch() * 1000.0) as u64), ForgetBasis::Rejected) {
            Ok(p) => p.display().to_string(),
            Err(e) => format!("not moved: {e}"),
        }
    }

    /// Move a slot directory's kept records off the state tmpfs into `forgotten_dir` (a full archive, or a summary for
    /// an env that ended valid, see `forget_disposition`), apply retention, and account for it. Returns where the
    /// records are. Blocking.
    fn forget_dir(&self, dir: &Path, name: &str, basis: ForgetBasis) -> std::io::Result<PathBuf> {
        let fd = self.cfg.forgotten_dir();
        let seen = slotfs::read_json(&dir.join("slot.json")).ok();
        let disp = forget_disposition(self.cfg.retention.mode, &basis, seen.as_ref());
        let f = forget::forget(&self.cfg.state_dir, &fd, dir, name, disp)?;
        let archived = if f.archived { "true" } else { "false" };
        self.metrics.inc("drumsd_forget_total", &[("class", f.class.name()), ("archived", archived)], 1.0);
        self.metrics.inc("drumsd_forget_tmpfs_bytes_freed_total", &[], if f.archived { f.bytes_on_tmpfs as f64 } else { 0.0 });
        if !f.archived || f.error.is_some() {
            self.event(json!({"forget": name, "archived": f.archived, "class": f.class.name(), "kept_at": f.at, "error": f.error}));
            if !f.archived {
                tracing::error!("records of {name} could not be packed to {}: {:?}; kept on the state dir", fd.display(), f.error);
            }
        }
        let caps = self.cfg.forget_caps();
        let gone = forget::prune(&fd, caps, Some(&f.at));
        if !gone.is_empty() {
            let mut p = self.forget_pruned.lock().unwrap();
            for g in &gone {
                self.metrics.inc("drumsd_forget_pruned_total", &[("class", g.class.name())], 1.0);
                self.metrics.inc("drumsd_forget_pruned_bytes_total", &[("class", g.class.name())], g.bytes as f64);
                let e = p.entry(g.class.name()).or_insert((0, 0));
                e.0 += 1;
                e.1 += g.bytes;
            }
            drop(p);
            self.event(json!({"forgotten_retention": {"summary_max_bytes": caps.summary, "full_max_bytes": caps.full,
                "max_gib": self.cfg.forgotten_max_gib, "after": name, "deleted": gone}}));
        }
        self.refresh_forgotten_gauges();
        Ok(f.at)
    }

    fn refresh_forgotten_gauges(&self) {
        let fd = self.cfg.forgotten_dir();
        let mut total = 0u64;
        for c in [forget::Class::Full, forget::Class::Summary] {
            let (b, n) = forget::class_totals(&fd, c);
            total += b;
            self.metrics.set("drumsd_forgotten_bytes", &[("class", c.name())], b as f64);
            self.metrics.set("drumsd_forgotten_files", &[("class", c.name())], n as f64);
        }
        self.metrics.set("drumsd_forgotten_bytes", &[("where", "disk")], total as f64);
    }

    /// Free space on the state dir, refused below the reserve.
    fn state_space_ok(&self) -> Result<(), String> {
        let free = forget::free_bytes(&self.cfg.state_dir);
        if let Some(f) = free {
            self.metrics.set("drumsd_state_dir_free_bytes", &[], f as f64);
        }
        match free {
            Some(f) if f < self.cfg.state_min_free_mib << 20 => Err(format!(
                "state dir {} has {} MiB free, below the {} MiB reserve (state_min_free_mib): refused before writing anything; close envs or delete saved states",
                self.cfg.state_dir.display(), f >> 20, self.cfg.state_min_free_mib)),
            _ => Ok(()),
        }
    }

    /// `base`: dump on top of these images (`--prev-images-dir`, incremental); with incremental_capture every dump
    /// also keeps tracking (`--track-mem`).
    fn run_capture_verb(&self, slot: u32, into: u32, base: Option<&Path>, base_uptime_us: Option<u64>) -> Result<Value, String> {
        let py = self.cfg.python.clone().unwrap_or_else(|| PathBuf::from("/usr/bin/python3"));
        let criu_dir = self.cfg.criu.parent().map(|p| p.display().to_string()).unwrap_or_default();
        let mut extra: Vec<String> = vec![];
        if self.cfg.incremental_capture {
            extra.push("--track-mem".into());
        }
        if let Some(b) = base {
            // relative to the new images dir, state_dir/slotQ/images
            let rel = b.strip_prefix(&self.cfg.state_dir).map_err(|_| format!("incremental base {} is not under the state dir", b.display()))?;
            extra.push("--prev-images-dir".into());
            extra.push(format!("../../{}", rel.display()));
            if let Some(u) = base_uptime_us {
                extra.push("--prev-images-uptime".into());
                extra.push(u.to_string());
            }
        }
        let mut argv: Vec<String> = [
            "capture",
            "--slot",
            &slot.to_string(),
            "--into",
            &into.to_string(),
            "--leave-running",
            "--no-continue",
            "--dump-log-level",
            &self.cfg.criu_log_level.clamp(3, 4).to_string(),
        ]
        .iter()
        .map(|x| x.to_string())
        .collect();
        argv.extend(extra);
        let cmd = |args: &[&str]| {
            let mut c = std::process::Command::new(&py);
            c.args(["-m", "sharedos_rootfs.capture"])
                .args(args)
                .current_dir(&self.cfg.py_src)
                .env("SHAREDOS_ROOTFS_CONFIG", &self.cfg.py_config)
                .env("PYTHONPATH", &self.cfg.py_src)
                .env("PYTHONDONTWRITEBYTECODE", "1")
                .env("PATH", format!("{criu_dir}:/usr/sbin:/usr/bin:/sbin:/bin"));
            c
        };
        // a resident verb (no Python start per capture); any failure of the worker itself falls back to a process run
        let (code, out, err) = match self.cfg.resident_capture_verb.then(|| self.verbs.run(&argv, || cmd(&["serve"]))).flatten() {
            Some(r) => r,
            None => {
                let a: Vec<&str> = argv.iter().map(|x| x.as_str()).collect();
                let o = cmd(&a).output().map_err(|e| e.to_string())?;
                (o.status.code(), String::from_utf8_lossy(&o.stdout).to_string(), String::from_utf8_lossy(&o.stderr).to_string())
            }
        };
        if code != Some(0) {
            return Err(format!("capture verb rc {:?}: {}", code, err.chars().rev().take(800).collect::<String>().chars().rev().collect::<String>()));
        }
        // a saved state of the hardened profile: only root may enter it (the env it was captured from keeps its range)
        drumsd_linux::idrange::seal_checkpoint(&slotfs::paths(&self.cfg.state_dir, into).dir).map_err(|e| format!("seal the saved state: {e}"))?;
        Ok(out.lines().rev().find(|l| l.starts_with('{')).and_then(|l| serde_json::from_str(l).ok()).unwrap_or(json!({"stdout": out})))
    }

    /// Capture the slot's current state and start `n` children from it, concurrently. `child`: the children's lease;
    /// `state`: the saved state's lease; `delete_state_with_children`: the state the fork made is deleted as soon as
    /// its last env (child or other) is closed.
    pub async fn branch_leased(
        self: &Arc<Self>,
        id: &str,
        n: usize,
        o: QuiesceOpts,
        child: LeaseOpts,
        state: LeaseOpts,
        delete_state_with_children: bool,
    ) -> R<Value> {
        if n == 0 || n > 64 {
            return Err(ApiError::new(400, "n must be 1..64"));
        }
        for x in [child.idle_timeout_s, child.ttl_s].into_iter().flatten() {
            if !x.is_finite() || x < 0.0 {
                return Err(ApiError::new(400, "idle_timeout_s and ttl_s must be >= 0 (0 = no limit)"));
            }
        }
        let t0 = Instant::now();
        let parent = self.slot(id)?;
        let c = match self.capture_leased(id, None, o, state, delete_state_with_children).await {
            Ok(c) => c,
            Err(e) => {
                let secs = t0.elapsed().as_secs_f64();
                self.account(Some(&parent), |l| l.op("fork", secs, false));
                return Err(e);
            }
        };
        let t_cap = t0.elapsed().as_secs_f64();
        // the reaper must not delete the new state (delete_state_with_children) before its children exist
        self.pinned_states.lock().unwrap().insert(c.id.clone());
        let futs: Vec<_> = (0..n)
            .map(|_| {
                let me = self.clone();
                let cid = c.id.clone();
                let lo = child.clone();
                // children inherit the checkpoint's task (setup already in the state)
                tokio::spawn(async move { me.create_leased(cid, None, Some(false), lo).await })
            })
            .collect();
        let mut children = vec![];
        for f in futs {
            match f.await {
                Ok(Ok(r)) => children.push(serde_json::to_value(view(&r)).unwrap()),
                Ok(Err(e)) => children.push(json!({"error": e.message})),
                Err(e) => children.push(json!({"error": format!("child task: {e}")})),
            }
        }
        self.pinned_states.lock().unwrap().remove(&c.id);
        if delete_state_with_children && self.state_users(&c.id).is_empty() {
            // every child failed to get a record: nothing will ever release the state
            let me = self.clone();
            let cid = c.id.clone();
            tokio::spawn(async move { me.reap_state(&cid, "children_closed").await });
        }
        let secs = t0.elapsed().as_secs_f64();
        let all_up = children.iter().all(|c| c["state"] == "ready");
        self.account(Some(&parent), |l| l.op("fork", secs, all_up));
        Ok(
            json!({"checkpoint": c, "state": c.id, "delete_state_with_children": delete_state_with_children, "children": children, "capture_s": round3(t_cap), "children_ready_s": round3(secs - t_cap), "seconds": round3(secs)}),
        )
    }

    /// The env took an input (step, reset, task load, destroy): a prefork of it no longer matches its state and is
    /// discarded in the background.
    fn took_input(self: &Arc<Self>, slot: &Slot) {
        slot.inputs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let p = self.preforks.lock().unwrap().remove(&slot.id);
        if let Some(p) = p {
            let me = self.clone();
            let id = slot.id.clone();
            tokio::spawn(async move { me.discard_prefork(&id, p, "the parent took an input").await });
        }
    }

    /// Start a fork of the env now, in the background, for the next fork call (see `Prefork`). A prefork already
    /// prepared for this env is discarded first.
    pub async fn prefork(
        self: &Arc<Self>,
        id: &str,
        n: usize,
        o: QuiesceOpts,
        child: LeaseOpts,
        state: LeaseOpts,
        delete_state_with_children: bool,
    ) -> R<Value> {
        if n == 0 || n > 64 {
            return Err(ApiError::new(400, "n must be 1..64"));
        }
        let slot = self.slot(id)?;
        // The prefork's capture waits a shorter quiet window than a fork's (prefork_quiesce_window_s, default 0.1 s):
        // its children are handed out only if the tree then stays free of task starts and exits from the capture's end
        // to the fork call (claim_prefork), so the window here only saves work that would be discarded.
        let mut o = o;
        if o.window_s.is_none() {
            o.window_s = Some(self.cfg.prefork_quiesce_window_s.min(self.cfg.quiesce.window_s));
        }
        let old = self.preforks.lock().unwrap().remove(id);
        if let Some(p) = old {
            let me = self.clone();
            let id2 = id.to_string();
            tokio::spawn(async move { me.discard_prefork(&id2, p, "replaced by a new prefork").await });
        }
        let (tx, rx) = tokio::sync::watch::channel(None);
        let p = Prefork {
            n,
            delete_state_with_children,
            inputs_at_start: slot.inputs.load(std::sync::atomic::Ordering::SeqCst),
            started: Instant::now(),
            started_epoch: drumsd_linux::now_epoch(),
            done: rx,
        };
        let started_epoch = p.started_epoch;
        self.preforks.lock().unwrap().insert(id.to_string(), p);
        let me = self.clone();
        let id2 = id.to_string();
        tokio::spawn(async move {
            // admitted like a fork, for as long as it runs
            let r = match me.admit("fork", drumsd_core::admission::Need { envs: n as u64, captures: 1 }) {
                Ok(g) => {
                    let r = me.branch_leased(&id2, n, o, child, state, delete_state_with_children).await;
                    drop(g);
                    r
                }
                Err(e) => Err(e),
            };
            let _ = tx.send(Some(r.map_err(|e| e.message)));
        });
        self.metrics.inc("drumsd_prefork_total", &[("outcome", "started")], 1.0);
        Ok(json!({"prefork": "started", "env": id, "n": n, "started_epoch": started_epoch}))
    }

    /// The fork call: hand out the env's prefork if it matches (same n, same state lifetime) and still equals the
    /// parent's state; None means fork cold. Waits for a prefork still in flight.
    pub async fn claim_prefork(self: &Arc<Self>, id: &str, n: usize, delete_state_with_children: bool) -> Option<Value> {
        let t0 = Instant::now();
        let p = self.preforks.lock().unwrap().remove(id)?;
        let slot = self.slot(id).ok()?;
        let why: String = 'check: {
            if p.n != n || p.delete_state_with_children != delete_state_with_children {
                break 'check format!(
                    "asked fork({n}, delete_state_with_children={delete_state_with_children}), prepared fork({}, {})",
                    p.n, p.delete_state_with_children
                );
            }
            let mut rx = p.done.clone();
            let in_flight = rx.borrow().is_none();
            let res = match rx.wait_for(|v| v.is_some()).await {
                Ok(v) => v.clone().unwrap(),
                Err(_) => Err("the prefork task ended without a result".into()),
            };
            let waited_s = t0.elapsed().as_secs_f64();
            let v = match res {
                Ok(v) => v,
                Err(e) => {
                    self.metrics.inc("drumsd_prefork_total", &[("outcome", "failed")], 1.0);
                    break 'check format!("the prefork failed: {e}");
                }
            };
            // 1. no input reached the parent since the prefork started
            let inputs = slot.inputs.load(std::sync::atomic::Ordering::SeqCst);
            if inputs != p.inputs_at_start {
                break 'check "the parent took an input".into();
            }
            // 2. no task started or exited in the parent's tree since its capture ended (the capture's own checks cover
            //    the dump: the dumped tree must equal the quiescent sample, and every dumped process must still run after it)
            let now = drumsd_linux::now_epoch();
            let cap_end = p.started_epoch + v["capture_s"].as_f64().unwrap_or(f64::INFINITY);
            let need = (now - cap_end).max(0.0);
            let quiet = match &self.activity {
                Some(aw) => aw.quiet_for(&self.tree_cg(&slot).display().to_string(), now, need),
                None => Err("no activity watch".into()),
            };
            match quiet {
                Ok(q) if q + 1e-3 >= need => {}
                Ok(q) => {
                    break 'check format!("a task started or exited in the parent {q:.3} s ago, after its capture ended {need:.3} s ago");
                }
                Err(e) => {
                    break 'check format!("the activity watch can not vouch for the parent: {e}");
                }
            }
            // 3. every child is still up and not invalid
            let kids: Vec<String> = v["children"].as_array().cloned().unwrap_or_default().iter().filter_map(|c| c["id"].as_str().map(String::from)).collect();
            if kids.len() != n {
                break 'check format!("{} of {n} children came up", kids.len());
            }
            for k in &kids {
                let ok = self
                    .slot(k)
                    .map(|s| {
                        let r = s.view.read().unwrap();
                        r.state == SlotState::Ready && !matches!(r.validity, Validity::Invalid { .. })
                    })
                    .unwrap_or(false);
                if !ok {
                    break 'check format!("child {k} is no longer ready and valid");
                }
            }
            // 4. the parent's desktop state hash still equals the one the children were checked against
            let state_id = v["state"].as_str().unwrap_or("").to_string();
            let want = self.ckpt(&state_id).ok().and_then(|c| c.expected_state_hash);
            let port = slot.view.read().unwrap().port;
            let th = Instant::now();
            let now_hash = tokio::task::spawn_blocking(move || state_hash(&Guest::new(port, Duration::from_secs(10))).ok()).await.ok().flatten();
            let hash_s = th.elapsed().as_secs_f64();
            if want.is_none() || now_hash != want {
                break 'check format!("the parent's state hash is {now_hash:?}, the children's reference {want:?}");
            }
            let mut v = v;
            v["prefork"] = json!({"claimed": true, "age_s": round3(p.started.elapsed().as_secs_f64()), "in_flight_at_claim": in_flight,
                                   "waited_s": round3(waited_s), "hash_check_s": round3(hash_s), "prepare_seconds": v["seconds"].clone(),
                                   "claim_s": round3(t0.elapsed().as_secs_f64())});
            v["seconds"] = json!(round3(t0.elapsed().as_secs_f64()));
            self.metrics.inc("drumsd_prefork_total", &[("outcome", "claimed")], 1.0);
            self.event(json!({"prefork": id, "claimed": v["prefork"].clone()}));
            self.account(Some(&slot), |l| l.op("fork", t0.elapsed().as_secs_f64(), true));
            return Some(v);
        };
        // not handed out: discard what it made, then the caller forks cold
        self.discard_prefork(id, p, &why).await;
        None
    }

    /// Close a prefork's children and delete its state (after it finishes, if still in flight).
    async fn discard_prefork(self: &Arc<Self>, id: &str, p: Prefork, why: &str) {
        let mut rx = p.done.clone();
        let res = rx.wait_for(|v| v.is_some()).await.ok().and_then(|v| v.clone()).unwrap_or(Err("no result".into()));
        let mut closed = vec![];
        if let Ok(v) = &res {
            for c in v["children"].as_array().cloned().unwrap_or_default() {
                if let Some(k) = c["id"].as_str() {
                    let r = self.destroy_why(k, Some(format!("prefork discarded: {why}"))).await;
                    closed.push(json!({"env": k, "ok": r.is_ok()}));
                }
            }
            if let Some(st) = v["state"].as_str() {
                if !p.delete_state_with_children && self.ckpt(st).is_ok() {
                    let _ = self.delete_checkpoint(st, false).await;
                }
            }
        }
        self.metrics.inc("drumsd_prefork_total", &[("outcome", "discarded")], 1.0);
        self.event(json!({"prefork": id, "discarded": why, "closed": closed, "age_s": round3(p.started.elapsed().as_secs_f64())}));
    }

    /// Preforks nobody claimed within `max_age_s` are discarded (the reaper calls this).
    async fn expire_preforks(self: &Arc<Self>, max_age_s: f64) -> usize {
        let old: Vec<(String, Prefork)> = {
            let mut m = self.preforks.lock().unwrap();
            let ids: Vec<String> = m.iter().filter(|(_, p)| p.started.elapsed().as_secs_f64() > max_age_s).map(|(k, _)| k.clone()).collect();
            ids.into_iter().filter_map(|k| m.remove(&k).map(|p| (k, p))).collect()
        };
        let n = old.len();
        for (id, p) in old {
            self.discard_prefork(&id, p, "not claimed in time").await;
        }
        n
    }

    pub async fn quarantine(self: &Arc<Self>, id: &str, reason: String) -> R<SlotRecord> {
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        let st = begin_op(&slot, Op::Quarantine)?;
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
        let st = begin_op(&slot, Op::Release)?;
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
        self.destroy_why(id, None).await
    }

    /// Destroy an env. Idempotent: an env that is already destroyed returns its record. `why` is recorded as the
    /// record's `closed_reason` when it was not the client (a lease).
    pub async fn destroy_why(self: &Arc<Self>, id: &str, why: Option<String>) -> R<SlotRecord> {
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        if slot.view.read().unwrap().state.is_terminal() {
            return Ok(slot.view.read().unwrap().clone());
        }
        let st = begin_op(&slot, Op::Destroy)?;
        self.took_input(&slot);
        self.finalize_episode(&slot);
        // what the env was when it was closed: decides whether its records are kept in full or as a summary
        let last = slot.view.read().unwrap().clone();
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
            // records and logs are kept (packed on disk, off the state tmpfs); the number can be reused
            retire_slot_dir(&me.cfg.state_dir, num, (drumsd_linux::now_epoch() * 1000.0) as u64, |d, n| me.forget_dir(d, n, ForgetBasis::Env(&last)))
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
        let rec = self.put_slot(&slot, |r| {
            r.state = SlotState::finish(Op::Destroy, &outcome);
            r.init_pid = None;
            r.closed_epoch = Some(drumsd_linux::now_epoch());
            r.closed_reason = why.clone();
            if let Ok(k) = &res {
                r.timings.insert("destroyed_records_at_epoch".into(), drumsd_linux::now_epoch());
                r.last_error = Some(format!("records kept at {}", k.display()));
            }
        });
        self.state_released(&rec.checkpoint);
        Ok(rec)
    }

    /// Envs made from this state that are not destroyed.
    fn state_users(&self, ckpt: &str) -> Vec<String> {
        self.slots
            .read()
            .unwrap()
            .values()
            .filter_map(|s| {
                let v = s.view.read().unwrap();
                (v.checkpoint == ckpt && !v.state.is_terminal()).then(|| v.id.clone())
            })
            .collect()
    }

    /// An env made from `ckpt` closed: the saved state's idle time starts now, and a state made by a fork with
    /// keep_state=false is deleted once its last env is gone.
    fn state_released(self: &Arc<Self>, ckpt: &str) {
        let Some(mut c) = self.ckpts.read().unwrap().get(ckpt).cloned() else { return };
        if c.captured_from.is_none() {
            return;
        }
        c.last_used_epoch = Some(drumsd_linux::now_epoch());
        self.put_ckpt(&c);
        if c.delete_with_children && self.state_users(ckpt).is_empty() && !self.pinned_states.lock().unwrap().contains(ckpt) {
            let me = self.clone();
            let id = ckpt.to_string();
            tokio::spawn(async move { me.reap_state(&id, "children_closed").await });
        }
    }

    /// Delete a saved state because of its lease (or because its last child closed), counted and recorded.
    async fn reap_state(self: &Arc<Self>, id: &str, why: &str) {
        match self.delete_checkpoint(id, false).await {
            Ok(r) => {
                self.metrics.inc("drumsd_lease_reaped_total", &[("kind", "state"), ("reason", why)], 1.0);
                *self.lease_reaped.lock().unwrap().entry(format!("state:{why}")).or_default() += 1;
                self.event(json!({"lease_reaped": {"state": id, "reason": why, "freed_bytes": r["freed_bytes"]}}));
            }
            // a new env from it, or already deleted: nothing to do
            Err(e) => self.event(json!({"lease_reap_skipped": {"state": id, "reason": why, "error": e.message}})),
        }
    }

    /// Record a request naming this env (the lease's idle clock). Memory only; journaled with the next record write
    /// or by the ledger flusher within a second.
    pub fn touch(&self, id: &str) {
        let Some(s) = self.slots.read().unwrap().get(id).cloned() else { return };
        let mut v = s.view.write().unwrap();
        if v.state.is_terminal() {
            return;
        }
        v.last_used_epoch = Some(drumsd_linux::now_epoch());
        drop(v);
        self.dirty_slots.lock().unwrap().insert(id.to_string());
    }

    fn env_lease(&self, r: &SlotRecord) -> Lease {
        r.lease.unwrap_or_else(|| self.cfg.lease.env())
    }

    fn state_lease(&self, c: &CheckpointRecord) -> Lease {
        c.lease.unwrap_or_else(|| self.cfg.lease.state())
    }

    /// One pass of the reaper: destroy envs and delete saved states whose lease ran out. An env busy with an
    /// operation (its lock held) is in use and skipped; so is one in a transient state.
    pub async fn reap_once(self: &Arc<Self>) -> Value {
        let expired = self.expire_preforks(self.cfg.prefork_max_age_s).await;
        if expired > 0 {
            self.event(json!({"preforks_expired": expired}));
        }
        let now = drumsd_linux::now_epoch();
        let mut envs = vec![];
        for s in self.slots.read().unwrap().values() {
            let Ok(_busy) = s.op.try_lock() else { continue };
            let v = s.view.read().unwrap();
            if v.state.is_terminal() || v.state.is_transient() {
                continue;
            }
            let l = self.env_lease(&v);
            if let Some(why) = l.expired(v.created_epoch, v.last_used_epoch.unwrap_or(v.created_epoch), now) {
                let secs = if why == "ttl" { l.ttl_s } else { l.idle_timeout_s }.unwrap_or(0.0);
                envs.push((v.id.clone(), why, format!("lease: {why} {secs} s")));
            }
        }
        let mut done = vec![];
        let futs: Vec<_> = envs
            .into_iter()
            .map(|(id, why, text)| {
                let me = self.clone();
                tokio::spawn(async move {
                    let r = me.destroy_why(&id, Some(text)).await;
                    if r.is_ok() {
                        me.metrics.inc("drumsd_lease_reaped_total", &[("kind", "env"), ("reason", why)], 1.0);
                        *me.lease_reaped.lock().unwrap().entry(format!("env:{why}")).or_default() += 1;
                        me.event(json!({"lease_reaped": {"env": id, "reason": why}}));
                    }
                    json!({"env": id, "reason": why, "ok": r.is_ok(), "error": r.err().map(|e| e.message)})
                })
            })
            .collect();
        for f in futs {
            if let Ok(v) = f.await {
                done.push(v);
            }
        }
        // saved states (never installed images): only once no env made from them exists
        let pinned = self.pinned_states.lock().unwrap().clone();
        let cands: Vec<CheckpointRecord> =
            self.ckpts.read().unwrap().values().filter(|c| c.captured_from.is_some() && !pinned.contains(&c.id)).cloned().collect();
        let states: Vec<(String, &'static str)> = cands
            .iter()
            .filter_map(|c| {
                if !self.state_users(&c.id).is_empty() {
                    return None;
                }
                if c.delete_with_children {
                    return Some((c.id.clone(), "children_closed"));
                }
                self.state_lease(c).expired(c.created_epoch, c.last_used_epoch.unwrap_or(c.created_epoch), now).map(|w| (c.id.clone(), w))
            })
            .collect();
        for (id, why) in states {
            self.reap_state(&id, why).await;
            done.push(json!({"state": id, "reason": why}));
        }
        json!(done)
    }

    /// Start the reaper (every `lease.reap_interval_s`).
    pub fn start_reaper(self: &Arc<Self>) {
        let w = Arc::downgrade(self);
        let every = Duration::from_secs_f64(self.cfg.lease.reap_interval_s.clamp(1.0, 3600.0));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                match w.upgrade() {
                    Some(m) => {
                        m.reap_once().await;
                    }
                    None => return,
                }
            }
        });
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
        self.osw_on(verb, num, task, extra, None)
    }

    /// `osw` for a tree the slot record does not name yet (`tree`: init pid and cgroup), e.g. a restore's task setup.
    pub fn osw_on(&self, verb: &str, num: u32, task: &Value, extra: Option<Value>, tree: Option<(i32, PathBuf)>) -> Result<Value, String> {
        use std::io::Write as _;
        use std::os::unix::process::CommandExt;
        let o = self.cfg.osw.as_ref().ok_or("no OSWorld helper configured (osw)")?;
        let (server, chromium, vlc) = self.osw_ports(num);
        // a one-time capability for this call: setup steps that run sudo go to drumsd (provision_step), not to the desktop
        let token = random_token();
        self.provision_tokens.lock().unwrap().insert(token.clone(), ProvisionTarget { num, tree });
        struct Revoke<'a>(&'a StdMutex<BTreeMap<String, ProvisionTarget>>, String);
        impl Drop for Revoke<'_> {
            fn drop(&mut self) {
                self.0.lock().unwrap().remove(&self.1);
            }
        }
        let _revoke = Revoke(&self.provision_tokens, token.clone());
        let port = self.cfg.listen.rsplit(':').next().unwrap_or("7070");
        let req = json!({"task": task, "host": "127.0.0.1", "server_port": server, "chromium_port": chromium, "vlc_port": vlc,
                         "cache_dir": o.cache_dir, "osworld_dir": o.osworld_dir, "client_password": o.client_password, "extra": extra,
                         "verifiers_dir": o.verifiers_dir, "assets_dir": o.assets_dir(),
                         "provision_url": format!("http://127.0.0.1:{port}/provision/{token}"),
                         // lab verifiers named by hash (and by file name with verifiers.sandbox_named) run in drumsd's sandbox
                         "sandbox_url": format!("http://127.0.0.1:{port}/provision/{token}/verifier"),
                         "sandbox_named": self.cfg.verifiers.sandbox_named});
        let timeout = Duration::from_secs(if verb == "evaluate" { o.eval_timeout_s } else { o.setup_timeout_s });
        let mut cmd = std::process::Command::new(&o.python);
        cmd.arg("-I")
            .arg(&o.helper)
            .arg(verb)
            .current_dir(&o.osworld_dir)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &o.cache_dir)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("OSWORLD_DIR", &o.osworld_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .uid(o.uid)
            .gid(o.gid);
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
            None => Err(format!(
                "osw {verb} exit {:?}, no reply; stderr: {}",
                out.status.code(),
                stderr.chars().rev().take(1200).collect::<String>().chars().rev().collect::<String>()
            )),
        }
    }

    /// A task setup step that needs root (its command runs sudo), sent by the OSWorld helper of a running setup or
    /// checker call through that call's capability URL. Runs in the slot as the desktop user with sudo for this step
    /// only (drumsd_linux::provision). Reply in the guest server's /setup/execute form.
    pub async fn provision_step(self: &Arc<Self>, token: &str, body: Value) -> R<Value> {
        let target = self.provision_tokens.lock().unwrap().get(token).cloned().ok_or_else(|| ApiError::new(403, "unknown or expired provisioning token"))?;
        let num = target.num;
        let slot = {
            let map = self.slots.read().unwrap();
            map.values().find(|s| s.view.read().unwrap().num == num && !s.view.read().unwrap().state.is_terminal()).cloned()
        }
        .ok_or_else(|| ApiError::new(409, format!("no live env in slot {num}")))?;
        let (init_pid, cgroup, id) = {
            let v = slot.view.read().unwrap();
            match &target.tree {
                Some((p, cg)) => (Some(*p), cg.clone(), v.id.clone()),
                None => (v.init_pid, self.episode_cg(num, v.leaf), v.id.clone()),
            }
        };
        let init_pid = init_pid.ok_or_else(|| ApiError::new(409, format!("env {id} has no running tree")))?;
        let user = self.image_user.clone().ok_or_else(|| ApiError::new(503, "no image user known (inspection unreadable): provisioning is unavailable"))?;
        let shell = body["shell"].as_bool().unwrap_or(false);
        // only what needs root comes here: everything else runs as the user through the guest server
        if !drumsd_linux::provision::needs_root(&body["command"]) {
            return Err(ApiError::new(400, "provisioning runs only setup steps whose command runs sudo"));
        }
        let argv = drumsd_linux::provision::step_argv(&body["command"], shell).map_err(|e| ApiError::new(400, e))?;
        let spec = drumsd_linux::provision::ProvisionSpec {
            init_pid,
            cgroup,
            user: user.name,
            uid: user.uid,
            gid: user.gid,
            home: user.home,
            argv,
            timeout: Duration::from_secs(120),
            userns: drumsd_linux::provision::own_userns(init_pid),
        };
        let out = tokio::task::spawn_blocking(move || drumsd_linux::provision::run(&spec))
            .await
            .map_err(e500)?
            .map_err(|e| ApiError::new(500, format!("provisioning: {e}")))?;
        // the command itself is not logged (OSWorld commands carry {CLIENT_PASSWORD}); its exit and timing are
        self.event(json!({"slot": id, "provision_step": {"returncode": out.returncode, "timed_out": out.timed_out, "seconds": round3(out.seconds)}}));
        self.account(Some(&slot), |l| l.op("provision_step", out.seconds, out.returncode == Some(0)));
        if out.timed_out {
            return Ok(
                json!({"status": "error", "message": "provisioning step timed out after 120 s", "output": out.output, "error": out.error, "returncode": null}),
            );
        }
        Ok(json!({"status": "success", "output": out.output, "error": out.error, "returncode": out.returncode}))
    }

    /// Load an OSWorld task into a ready slot: its setup runs now, and again after every reset of this slot.
    pub async fn load_task(self: &Arc<Self>, id: &str, task: Value) -> R<SlotRecord> {
        if osworld_task_id(&task).is_none() {
            return Err(ApiError::new(400, "task: not an OSWorld task (needs an id of [A-Za-z0-9_-], a config list and an evaluator object)"));
        }
        self.check_task_verifiers(&task)?;
        self.step_arg_limit(drumsd_core::limits::check_task_steps(&task), id)?;
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        begin_op(&slot, Op::Step)?;
        self.took_input(&slot);
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

    /// The step-argument limit (drumsd_core::limits): a 400 naming the field, the limit and the length when
    /// `enforce_step_arg_limit` is on; otherwise the over-limit request is recorded and goes ahead (today's behaviour).
    fn step_arg_limit(&self, r: Result<(), drumsd_core::limits::LimitError>, subject: &str) -> R<()> {
        let Err(e) = r else { return Ok(()) };
        self.metrics.inc("drumsd_step_arg_over_limit_total", &[("enforced", if self.cfg.enforce_step_arg_limit { "true" } else { "false" })], 1.0);
        self.event(json!({"step_arg_over_limit": {"subject": subject, "field": e.field, "limit": e.limit, "length": e.length, "enforced": self.cfg.enforce_step_arg_limit}}));
        if self.cfg.enforce_step_arg_limit {
            return Err(ApiError::new(400, e.message()).with(e.body()));
        }
        Ok(())
    }

    /// Run the task's checker on the slot's current state. The reward always carries the episode's gate verdict; a
    /// reward from an episode that is not valid is returned with `valid: false`, never as a plain score.
    pub async fn verify(self: &Arc<Self>, id: &str) -> R<Value> {
        let slot = self.slot(id)?;
        let _g = slot.op.lock().await;
        self.gone_check(&slot, "verify")?;
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
        // a quarantined env's state is not trusted: its checker is not run (no score; the reward is invalid and says why)
        let quarantined = match &slot.view.read().unwrap().state {
            SlotState::Quarantined { reason } => Some(reason.clone()),
            _ => None,
        };
        let me = self.clone();
        let t = task.clone();
        let r = match &quarantined {
            None => tokio::task::spawn_blocking(move || me.osw("evaluate", num, &t, None)).await.map_err(e500)?,
            Some(why) => Ok(json!({"skipped": format!("quarantined: {why}")})),
        };
        // an OOM kill since the last step (GIMP was killed while it built the filter the last click opened, then the
        // checker scored 0.0 and the reward came back valid) or during the checker's own postconfig actions: the
        // validity the reward carries is read only after this check
        self.check_episode(&slot);
        let v = slot.view.read().unwrap().clone();
        let valid = v.validity == Validity::Valid && matches!(v.state, SlotState::Ready);
        let (score, err, detail) = match &r {
            Ok(x) => (x["score"].as_f64(), x["checker_error"].as_str().map(|s| s.to_string()), x.clone()),
            Err(e) => (None, Some(e.clone()), Value::Null),
        };
        let secs = t0.elapsed().as_secs_f64();
        let skipped = quarantined.is_some();
        let audit = reward_audit(&detail, score, err.as_deref(), valid, skipped);
        for f in audit["flags"].as_array().cloned().unwrap_or_default() {
            if let Some(f) = f.as_str() {
                self.metrics.inc("drumsd_audit_flag_total", &[("flag", f)], 1.0);
            }
        }
        self.metrics.inc("drumsd_audit_total", &[("trusted", if audit["trusted"] == true { "true" } else { "false" })], 1.0);
        self.account(Some(&slot), |l| {
            l.op("verify", secs, skipped || score.is_some());
            if skipped {
                l.verify_skipped();
            } else {
                l.verified(score, valid);
                if score.is_none() {
                    l.failure("checker_error");
                }
            }
        });
        // unattributed crashes the watch did not count against this episode (its tree ran no Chrome then): reported, so a
        // valid verdict next to a crash elsewhere on the host is never silent
        let crash_not_counted = self.crash_judgment(&slot).map(|(_, j)| j.excused).unwrap_or_default();
        let out = json!({
            "slot": v.id, "episode": v.episode, "task_id": tid, "score": score, "valid": valid,
            "validity": v.validity, "invalid_reasons": invalid_reasons(&v), "crash_not_counted": crash_not_counted, "checker_skipped": detail.get("skipped"),
            "state": v.state.name(), "gates": v.gates, "checker_error": err,
            "checker": {"func": task["evaluator"]["func"], "osworld_commit": detail["osworld_commit"], "seconds": detail["seconds"],
                        "postconfig_ran": detail["postconfig_ran"], "verifier": detail.get("verifier"), "info": detail.get("info"),
                        "verifier_sha256": detail.get("verifier_sha256"), "sandbox": detail.get("sandbox")},
            "audit": audit,
            "seconds": round3(secs),
        });
        self.event(json!({"slot": v.id, "verify": {"episode": v.episode, "task": tid, "score": score, "valid": valid, "error": err}}));
        self.metrics.inc(
            "drumsd_verify_total",
            &[
                ("valid", if valid { "true" } else { "false" }),
                (
                    "scored",
                    if score.is_some() {
                        "true"
                    } else if skipped {
                        "skipped"
                    } else {
                        "false"
                    },
                ),
            ],
            1.0,
        );
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
                        let why = t["id"].as_str().and_then(unsupported_reason);
                        out.push(json!({"id": t["id"], "domain": dom, "instruction": t["instruction"], "supported": why.is_none(), "unsupported_reason": why}));
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
        self.slots
            .read()
            .unwrap()
            .values()
            .map(|s| {
                let v = s.view.read().unwrap();
                if v.closed_epoch.is_some() || v.state.is_terminal() {
                    0.0
                } else {
                    (now - v.created_epoch.max(window_start)).max(0.0)
                }
            })
            .sum()
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
               "leases_reaped": *self.lease_reaped.lock().unwrap(),
               "cost_note": "cost.host_usd is the host price times the WHOLE window's wall time, idle time included (the window starts when drumsd first started or at the last POST /v1/metrics/reset). Call client.reset_metrics() at the start of a run to measure that run.",
               "definitions": "seconds = wall time inside drumsd per op, failed attempts included; episodes counted once when the verdict resolves (a valid episode that is later quarantined or OOM-killed is moved to invalid); cost = host_usd_per_hour x wall hours of the window, divided by valid episodes / verified rewards from valid episodes; price is the operator's configured number"})
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
        let ck: Vec<Value> = self
            .ckpts
            .read()
            .unwrap()
            .values()
            .map(|c| {
                let sp = slotfs::paths(&self.cfg.state_dir, c.slot);
                json!({"id": c.id, "template_server": template::adopt(&c.socket, &sp.images), "task": c.task.as_ref().map(|t| t["id"].clone()),
                   "stale_reason": self.stale(c)})
            })
            .collect();
        let mut states: BTreeMap<&'static str, u64> = BTreeMap::new();
        for s in self.slots.read().unwrap().values() {
            *states.entry(s.view.read().unwrap().state.name()).or_default() += 1;
        }
        let free = |p: &Path| -> Option<u64> {
            let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
            let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
            (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
        };
        let ok = ck.iter().all(|c| !c["template_server"].is_null() && c["stale_reason"].as_str().is_none_or(|w| !w.starts_with("state_lost")));
        let mut warnings: Vec<String> = vec![];
        for c in &ck {
            if let Some(w) = c["stale_reason"].as_str() {
                warnings.push(format!("checkpoint {} is stale and refuses new envs: {w}", c["id"].as_str().unwrap_or("?")));
            }
        }
        let (sf, st) = (forget::free_bytes(&self.cfg.state_dir), forget::total_bytes(&self.cfg.state_dir));
        if let (Some(f), Some(t)) = (sf, st) {
            self.metrics.set("drumsd_state_dir_free_bytes", &[], f as f64);
            let pct = 100.0 * f as f64 / t.max(1) as f64;
            if pct < self.cfg.state_warn_free_pct {
                warnings.push(format!(
                    "state dir {} is {:.1}% free ({} MiB; restores are refused below {} MiB)",
                    self.cfg.state_dir.display(),
                    pct,
                    f >> 20,
                    self.cfg.state_min_free_mib
                ));
            }
        }
        let fd = self.cfg.forgotten_dir();
        let (full_b, full_n) = forget::class_totals(&fd, forget::Class::Full);
        let (sum_b, sum_n) = forget::class_totals(&fd, forget::Class::Summary);
        let fbytes = full_b + sum_b;
        let legacy_n = forget::entries(&fd).iter().filter(|e| e.legacy).count();
        let left_on_tmpfs = forget::dir_bytes(&self.cfg.state_dir.join("forgotten"));
        self.refresh_forgotten_gauges();
        self.metrics.set("drumsd_forgotten_bytes", &[("where", "state_dir")], left_on_tmpfs as f64);
        let gibf = |b: u64| b as f64 / (1u64 << 30) as f64;
        if full_b as f64 > self.cfg.forgotten_warn_gib * (1u64 << 30) as f64 {
            warnings.push(format!("{} holds {:.1} GiB of full failure records (forgotten_warn_gib {}); full records are never pruned by default: set retention.full_max_gib (or forgotten_max_gib) to cap them", fd.display(), gibf(full_b), self.cfg.forgotten_warn_gib));
        }
        let pruned = self.forget_pruned.lock().unwrap().clone();
        let pruned_of = |c: &str| pruned.get(c).copied().unwrap_or((0, 0));
        {
            let mut probe = fd.clone();
            while !probe.exists() && probe.pop() {}
            if let (Some(f), Some(t)) = (forget::free_bytes(&probe), forget::total_bytes(&probe)) {
                let pct = 100.0 * f as f64 / t.max(1) as f64;
                if pct < FORGOTTEN_DISK_MIN_FREE_PCT {
                    warnings.push(format!(
                        "the filesystem of forgotten_dir {} is {:.1}% free ({} MiB): kept records may stop being written",
                        fd.display(),
                        pct,
                        f >> 20
                    ));
                }
            }
        }
        match &self.crash {
            None => warnings.push("crash_watch is off: an env whose app crashed can return a valid reward".into()),
            Some(w) if !w.summary()["running"].as_bool().unwrap_or(false) => {
                warnings.push("the crash watch stopped: every episode since then is invalid (no_app_crash)".into())
            }
            _ => {}
        }
        if left_on_tmpfs > 0 {
            warnings.push(format!(
                "{} MiB of env records could not be packed and are on the state dir ({}/forgotten)",
                left_on_tmpfs >> 20,
                self.cfg.state_dir.display()
            ));
        }
        {
            use std::os::unix::fs::MetadataExt;
            let dev = |p: &Path| std::fs::metadata(p).ok().map(|m| m.dev());
            if dev(&fd).is_some() && dev(&fd) == dev(&self.cfg.state_dir) {
                warnings.push(format!("forgotten_dir {} is on the same filesystem as the state dir", fd.display()));
            }
        }
        let pv_live = drumsd_linux::procview::fuse::is_live(&self.cfg.procview_dir);
        let pv_needed =
            self.ckpts.read().unwrap().values().any(|c| !view_externals(&slotfs::paths(&self.cfg.state_dir, c.slot).dir, &self.cfg.procview_dir).is_empty());
        self.metrics.set("drumsd_procview_live", &[], if pv_live { 1.0 } else { 0.0 });
        if pv_needed && !pv_live {
            warnings.push(format!(
                "the per-env memory/CPU view at {} is not live (drums-procview.service): restores of checkpoints that bind it fail closed",
                self.cfg.procview_dir.display()
            ));
        }
        let pv_status: Value = std::fs::read(drumsd_linux::procview::STATUS).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        if pv_needed && pv_live && pv_status["sysinfo"].as_str() != Some("attached") {
            warnings
                .push(format!("the per-env view serves files but not sysinfo(2): {} (sysconf(_SC_PHYS_PAGES) in envs reports the host)", pv_status["sysinfo"]));
        }
        json!({"ok": ok, "version": env!("CARGO_PKG_VERSION"),
               "procview": {"dir": self.cfg.procview_dir, "live": pv_live, "used_by_checkpoints": pv_needed, "status": pv_status}, "uptime_s": round3(drumsd_linux::now_epoch() - self.started_epoch),
               "criu": {"path": self.cfg.criu, "sha256": self.criu_sha256, "nested_pidns": self.cfg.nested_pidns},
               "crash_watch": self.crash.as_ref().map(|w| w.summary()),
               "activity_watch": self.activity.as_ref().map(|w| w.summary()),
               "image_sha256": self.cfg.image_sha256, "provenance": self.provenance, "checkpoints": ck, "slots_by_state": states,
               "osworld": self.cfg.osw.as_ref().map(|o| json!({"dir": o.osworld_dir})),
               "state_dir_free_bytes": free(&self.cfg.state_dir), "warnings": warnings,
               "forgotten": {"dir": fd, "bytes": fbytes, "on_state_dir_bytes": left_on_tmpfs, "max_gib": self.cfg.forgotten_max_gib,
                             "mode": self.cfg.retention.mode.name(),
                             "caps_gib": {"summary": self.cfg.retention.summary_max_gib, "full": self.cfg.retention.full_max_gib, "all": self.cfg.forgotten_max_gib},
                             "warn_gib": self.cfg.forgotten_warn_gib,
                             "full": {"bytes": full_b, "files": full_n, "legacy_files": legacy_n, "pruned_files": pruned_of("full").0, "pruned_bytes": pruned_of("full").1},
                             "summary": {"bytes": sum_b, "files": sum_n, "pruned_files": pruned_of("summary").0, "pruned_bytes": pruned_of("summary").1},
                             "summaries_pruned": pruned_of("summary").0 > 0},
               "host_mem_available_bytes": std::fs::read_to_string("/proc/meminfo").ok().and_then(|m| m.lines().find(|l| l.starts_with("MemAvailable:"))
                   .and_then(|l| l.split_whitespace().nth(1)).and_then(|k| k.parse::<u64>().ok())).map(|k| k * 1024),
               "loadavg": std::fs::read_to_string("/proc/loadavg").ok().map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" ")), "host_usd_per_hour": self.cfg.host_usd_per_hour,
               "limits": {"vcpus": self.cfg.vcpus, "memory_mib": self.cfg.memory_mib, "swap_mib": self.cfg.swap_mib, "max_concurrent_restores": self.cfg.max_concurrent_restores},
               "leases": {"defaults": self.cfg.lease, "reaped": *self.lease_reaped.lock().unwrap()},
               "admission": self.admission_view()})
    }

    // ------------------------------------------------------------------------------------------------ restart
    /// Reconcile the journal with the kernel after a (re)start. Transient states mean the daemon died mid-operation.
    pub async fn recover(self: &Arc<Self>) -> Result<Value, String> {
        let mut report = vec![];
        // records an older daemon left on the state tmpfs (or a forget cut short) are packed to disk, in the background
        let me = self.clone();
        tokio::task::spawn_blocking(move || {
            let swept = forget::sweep(&me.cfg.state_dir, &me.cfg.forgotten_dir());
            if !swept.is_empty() {
                let bytes: u64 = swept.iter().filter(|f| f.archived).map(|f| f.bytes_on_tmpfs).sum();
                me.event(json!({"forgotten_swept": swept.len(), "archived": swept.iter().filter(|f| f.archived).count(), "tmpfs_bytes_freed": bytes}));
            }
            // template-server logs drumsd < 0.4.2 wrote to the state dir itself; any no process holds open are packed
            // to disk like other records (one archive), so an upgraded host gets that tmpfs back
            match template::collect_loose_logs(&me.cfg.state_dir, (drumsd_linux::now_epoch() * 1000.0) as u64) {
                Ok(Some((dir, n, bytes))) => {
                    let name = dir.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
                    let at = me.forget_dir(&dir, &name, ForgetBasis::Rejected);
                    me.event(json!({"template_logs_moved": {"files": n, "bytes": bytes, "to": at.as_ref().map(|p| p.display().to_string()).map_err(|e| e.to_string())}}));
                }
                Ok(None) => {}
                Err(e) => me.event(json!({"template_logs_moved": {"error": e.to_string()}})),
            }
        });
        let ckpts: Vec<(String, CheckpointRecord)> = self.journal.all("checkpoints").map_err(|e| e.to_string())?;
        for (_, c) in ckpts {
            let sp = slotfs::paths(&self.cfg.state_dir, c.slot);
            let mut c2 = c.clone();
            let lost = state_lost(&c, &self.cfg.state_dir, &self.cfg.lower, is_mounted(&self.cfg.lower));
            let adopted = if lost.is_some() { None } else { template::adopt(&c.socket, &sp.images) };
            c2.server_pid = adopted;
            self.put_ckpt(&c2);
            match lost {
                Some(why) => {
                    tracing::error!("checkpoint {} LOST, refusing envs: {why}", c.id);
                    report.push(json!({"checkpoint": c.id, "server": null, "lost": why}));
                }
                None => report.push(json!({"checkpoint": c.id, "server": adopted})),
            }
        }
        // template servers no checkpoint owns hold memory for nothing: stop them (recorded)
        let owned: BTreeSet<i32> = self.ckpts.read().unwrap().values().filter_map(|c| c.server_pid).collect();
        for (pid, cmd) in template::orphans(&self.cfg.state_dir, &owned) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            report.push(json!({"orphan_template_server_killed": pid, "cmdline": cmd}));
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
                    && procfs::cgroup_path(p)
                        .map(|c| c.contains(&format!("/drumsbench-{}-pool.scope", slotfs::marker_for(&self.cfg.state_dir, num))))
                        .unwrap_or(false)
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
                let gone = !slotfs::paths(&self.cfg.state_dir, num).dir.exists();
                rec2.state = SlotState::Down {
                    reason: if gone {
                        format!(
                            "state_lost: slot directory {} is gone and the tree is not running (the host was restarted)",
                            slotfs::paths(&self.cfg.state_dir, num).dir.display()
                        )
                    } else {
                        "tree not running after daemon restart".into()
                    },
                };
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
            let slot = Arc::new(Slot {
                id: id.clone(),
                op: Mutex::new(()),
                view: RwLock::new(rec2.clone()),
                targets,
                proxies: StdMutex::new(handles),
                inputs: std::sync::atomic::AtomicU64::new(0),
            });
            self.slots.write().unwrap().insert(id.clone(), slot.clone());
            self.put_slot(&slot, |_| {});
            // a pending alive gate of a still-running episode is judged now
            if matches!(rec2.state, SlotState::Ready) && rec2.gates.get(Gate::AliveAfterResume).map(|g| g.status == GateStatus::Pending).unwrap_or(false) {
                let gen =
                    slotfs::read_json(&slotfs::paths(&self.cfg.state_dir, num).slot_json).ok().and_then(|v| v["restore_generation"].as_u64()).unwrap_or(0);
                let hook_dir = slotfs::paths(&self.cfg.state_dir, num).dir.join(format!("lock-hook-{gen}"));
                let me = self.clone();
                let s2 = slot.clone();
                let ep = rec2.episode;
                tokio::task::spawn_blocking(move || {
                    me.judge_alive(&s2, ep, &hook_dir, Instant::now().checked_sub(Duration::from_secs(60)).unwrap_or_else(Instant::now))
                })
                .await
                .map_err(|e| e.to_string())?;
            }
            report.push(json!({"slot": id, "was": rec.state.name(), "now": slot.view.read().unwrap().state.name(), "action": action}));
        }
        self.event(json!({"recovered": report}));
        self.refresh_slot_gauges();
        Ok(json!(report))
    }
}

/// Begin `op` on the slot, or refuse it: 410 `env_gone` when the env is destroyed, 409 `wrong_state` otherwise.
fn begin_op(slot: &Slot, op: Op) -> Result<SlotState, ApiError> {
    let v = slot.view.read().unwrap();
    v.state.begin(op).map_err(|e| refused(&v, e))
}

/// The error for an operation the env's state does not allow.
///
/// For a quarantined env the body also carries `reason` (why it was quarantined) and `next` (what a client can do):
/// a quarantined env is never restored into again by itself, so a client that keeps sending the same `reset` gets
/// the same 409 every time (R2-3 host B: 10,325 of them in 50 s from a harness that did not read the state). The
/// SDK and the gateway turn `next` into release-then-reset or a new env.
pub fn refused(r: &SlotRecord, e: drumsd_core::state::TransitionError) -> ApiError {
    if matches!(r.state, SlotState::Destroyed | SlotState::Destroying) {
        let drumsd_core::state::TransitionError::NotAllowed { op, .. } = &e;
        return gone_error(r, op);
    }
    let mut body = json!({"code": "wrong_state", "state": r.state.name()});
    if let SlotState::Quarantined { reason } = &r.state {
        body["reason"] = json!(reason);
        body["next"] =
            json!("release (POST /v1/slots/{id}/release) then reset, or destroy it and create a new env; the env is not restored into again by itself");
    }
    ApiError::new(409, e.to_string()).with(body)
}

/// 410 `env_gone`: the env was destroyed (by a client's DELETE or by its lease); nothing can be done with it but read its record.
pub fn gone_error(r: &SlotRecord, op: &str) -> ApiError {
    let why = r.closed_reason.as_ref().map(|w| format!(" ({w})")).unwrap_or_default();
    ApiError::new(410, format!("env {} is destroyed{why}: {op} is not possible; create a new env", r.id))
        .with(json!({"code": "env_gone", "env": r.id, "closed_reason": r.closed_reason}))
}

/// When the env is quarantined, its current episode gets a failed `not_quarantined` gate that names the reason (once
/// per episode), and its validity is recomputed: a quarantined env is never valid. Returns true when it added one.
pub fn mark_quarantined(r: &mut SlotRecord) -> bool {
    let SlotState::Quarantined { reason } = &r.state else { return false };
    if r.episode == 0 || r.gates.get(Gate::NotQuarantined).is_some() {
        return false;
    }
    let reason = reason.clone();
    r.gates.push(GateResult::fail(Gate::NotQuarantined, 0.0, json!({"reason": reason})));
    r.validity = r.gates.validity(&required_gates());
    true
}

/// Why an episode is not valid, in words: every failed gate (a quarantine names its reason, an OOM kill its count),
/// every pending gate, and, when no gate explains it, the env's state. Empty for a valid episode.
pub fn invalid_reasons(r: &SlotRecord) -> Vec<String> {
    let mut out = vec![];
    let (failed, pending) = match &r.validity {
        Validity::Valid => return out,
        Validity::Invalid { gates } => (gates.clone(), vec![]),
        Validity::Pending { gates } => (vec![], gates.clone()),
    };
    for g in &failed {
        let d = r.gates.get(*g).map(|x| &x.detail);
        out.push(match g {
            Gate::NotQuarantined => format!("quarantined: {}", d.and_then(|d| d["reason"].as_str()).unwrap_or("?")),
            Gate::NoOomKill => {
                format!("no_oom_kill: the OOM killer took {} task(s) of the env", d.map(|d| d["oom_kills"].to_string()).unwrap_or_else(|| "?".into()))
            }
            Gate::NoAppCrash => format!("no_app_crash: {}", d.map(app_crash_words).unwrap_or_else(|| "?".into())),
            _ if r.gates.get(*g).is_none() => format!("gate {} missing", g.as_str()),
            _ => format!("gate {} failed", g.as_str()),
        });
    }
    for g in &pending {
        out.push(format!("gate {} still pending", g.as_str()));
    }
    if out.is_empty() {
        out.push(match &r.state {
            SlotState::Down { reason } => format!("env is down: {reason}"),
            SlotState::Quarantined { reason } => format!("quarantined: {reason}"),
            s => format!("episode ended in state {}{}", s.name(), r.last_error.as_ref().map(|e| format!(": {e}")).unwrap_or_default()),
        });
    } else if !matches!(r.state, SlotState::Ready | SlotState::Quarantined { .. }) {
        out.push(format!("env is {}", r.state.name()));
    }
    out
}

/// The `no_app_crash` detail in words: what died of what, or why the watch cannot vouch for the episode.
pub fn app_crash_words(d: &Value) -> String {
    let mut parts: Vec<String> = vec![];
    if let Some(t) = d["inherited"].as_str() {
        parts.push(format!("made from saved state {} captured after an app crash ({t})", d["checkpoint"].as_str().unwrap_or("?")));
    }
    for c in d["crashes"].as_array().into_iter().flatten() {
        let who = c["comm"].as_str().map(|s| s.to_string()).unwrap_or_else(|| format!("a child of {}", c["parent_comm"].as_str().unwrap_or("?")));
        parts.push(format!("{who} (pid {}) died of {}", c["tgid"], c["signal_name"].as_str().unwrap_or("?")));
    }
    for s in d["incomplete"].as_array().into_iter().flatten() {
        parts.push(s.as_str().unwrap_or("?").to_string());
    }
    if parts.is_empty() {
        "?".into()
    } else {
        parts.join("; ")
    }
}

/// Key in slot.json that marks an env that failed at some point (value: the first reason seen).
pub const FAILURE_SEEN: &str = "drumsd_failure_seen";

/// Why this slot record shows a failure (a failed or quarantined state, an invalid verdict, a failed gate, an error).
fn failure_seen(r: &SlotRecord) -> Option<String> {
    match &r.state {
        SlotState::Quarantined { reason } => return Some(format!("quarantined: {reason}")),
        SlotState::Down { reason } => return Some(format!("down: {reason}")),
        SlotState::Destroyed | SlotState::Destroying => return None,
        _ => {}
    }
    if let Some(g) = r.gates.first_failure() {
        return Some(format!("gate {:?} failed", g.gate));
    }
    if matches!(r.validity, Validity::Invalid { .. }) {
        return Some("validity invalid".into());
    }
    r.last_error.as_ref().map(|e| format!("error: {e}"))
}

/// What the manager knows about a directory it forgets.
pub enum ForgetBasis<'a> {
    /// A rejected or failed dump: always kept in full.
    Rejected,
    /// An env being destroyed: its record as it was when closed.
    Env(&'a SlotRecord),
    /// A deleted saved state (or installed image).
    Checkpoint(&'a CheckpointRecord),
}

/// Full archive or summary. A summary only when the manager knows the env ended valid: closed from `ready` with a
/// valid verdict, no failed gate, no error, and no failure mark in slot.json. Everything else, and everything that
/// cannot be classified with certainty, is kept in full.
pub fn forget_disposition(mode: forget::Mode, basis: &ForgetBasis, slot_json: Option<&Value>) -> forget::Disposition {
    use forget::Disposition::{Full, Summary};
    if mode == forget::Mode::KeepAll {
        return Full;
    }
    if slot_json.is_some_and(|j| j.get(FAILURE_SEEN).is_some()) {
        return Full;
    }
    match basis {
        ForgetBasis::Rejected => Full,
        ForgetBasis::Env(r) => {
            let clean = matches!(r.state, SlotState::Ready) && r.validity == Validity::Valid && r.gates.first_failure().is_none() && r.last_error.is_none();
            if !clean {
                return Full;
            }
            Summary {
                record: serde_json::to_value(r).unwrap_or(Value::Null),
                reason: "env ended valid (closed from ready, valid verdict, no failed gate, never quarantined)".into(),
            }
        }
        ForgetBasis::Checkpoint(c) => {
            let rejected = c.quiescence.as_ref().and_then(|q| q["attempts"].as_array()).is_some_and(|a| a.iter().any(|x| !x["rejected"].is_null()));
            if c.captured_from.is_none() || rejected {
                return Full; // installed images and saved states that needed a rejected dump keep their logs
            }
            Summary { record: serde_json::to_value(c).unwrap_or(Value::Null), reason: "saved state deleted; no rejected dump in its capture".into() }
        }
    }
}

/// Whether an env in `state` keeps its checkpoint from being deleted. Once the checkpoint's state is lost (a host
/// restart), an env that is down or quarantined holds nothing of it: no tree runs (drums-scale, 2026-10-30: 115 down
/// and quarantined envs from before a reboot blocked the boot unit's re-provisioning with 409 "used by envs").
pub fn blocks_checkpoint_delete(state: &SlotState, lost: bool) -> bool {
    !state.is_terminal() && (!lost || !matches!(state, SlotState::Down { .. } | SlotState::Quarantined { .. }))
}

/// The host device binds of a hardened slot's checkpoint (its `mounts-pre-dump.json`), as CRIU restore arguments
/// re-binding them from the host's own nodes. A checkpoint of an unhardened slot gets none.
pub fn slot_dev_externals(slot_dir: &Path) -> Vec<String> {
    let rows: Vec<[String; 3]> = std::fs::read(slot_dir.join("mounts-pre-dump.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    slotfs::dev_restore_externals(&rows)
}

/// The per-env view binds of a checkpoint (its `mounts-pre-dump.json`), as CRIU restore arguments re-binding them
/// from the live view mount. A checkpoint made without the view gets none and restores as before.
pub fn view_externals(slot_dir: &Path, dir: &Path) -> Vec<String> {
    let rows: Vec<[String; 3]> = std::fs::read(slot_dir.join("mounts-pre-dump.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    drumsd_linux::procview::restore_externals(&rows, dir)
}

/// The filesystem half of a destroy: remove the slot's disk state, keep its records and logs under forgotten/ (the
/// directory moves aside so the number can be reused). Idempotent: a destroy interrupted by a daemon kill after the
/// move left no directory, and the retry used to fail with ENOENT, leaving an env that could never be closed (and
/// its saved state never deleted).
pub fn retire_slot_dir(state_dir: &Path, num: u32, now_ms: u64, forget: impl FnOnce(&Path, &str) -> std::io::Result<PathBuf>) -> std::io::Result<PathBuf> {
    let sp = slotfs::paths(state_dir, num);
    if sp.dir.symlink_metadata().is_err() {
        return Ok(PathBuf::from(format!("slot{num}-* (moved by an earlier, interrupted destroy)")));
    }
    for d in
        ["upper", "upper.snap", "work", "images", "upper.next", "work.next", "upper.next.tmp", "trash", "criu-root", "softdirty-base", "softdirty-base.new"]
    {
        let p = sp.dir.join(d);
        if p.symlink_metadata().is_ok() {
            std::fs::remove_dir_all(&p)?;
        }
    }
    forget(&sp.dir, &format!("slot{num}-{now_ms}"))
}

/// Expected memory of an admitted operation, counted as in flight until the operation ends.
pub struct AdmitGuard {
    in_flight: Arc<std::sync::atomic::AtomicU64>,
    mib: u64,
}

impl Drop for AdmitGuard {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(self.mib, std::sync::atomic::Ordering::SeqCst);
    }
}

fn statvfs_free(p: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

/// The first number at or after `from` (wrapping around `range`) not held, not reserved and with nothing on disk.
/// Searching from just after the last number handed out spreads reuse over the whole range, so a freed number (its
/// ports, its cgroup) is taken again only after every other free number was.
pub fn pick_num(range: (u32, u32), from: u32, held: &BTreeSet<u32>, reserved: &BTreeSet<u32>, on_disk: impl Fn(u32) -> bool) -> Option<u32> {
    let from = from.clamp(range.0, range.1);
    (from..range.1).chain(range.0..from).find(|n| !held.contains(n) && !reserved.contains(n) && !on_disk(*n))
}

/// `s<num>` if no slot ever had that id, else the first free `s<num>-<k>` (k >= 2).
pub fn slot_id_for(num: u32, taken: impl Fn(&str) -> bool) -> String {
    let base = format!("s{num}");
    if !taken(&base) {
        return base;
    }
    (2u32..).map(|k| format!("s{num}-{k}")).find(|c| !taken(c)).unwrap()
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
/// OSWorld tasks this execution profile cannot run: the agent gets no privilege escalation, and these need the agent
/// itself to act as root (a task whose *setup* needs root is fine: drumsd runs that step as trusted provisioning).
pub const UNSUPPORTED_AGENT_ROOT: [(&str, &str); 3] = [
    ("5812b315-e7bd-4265-b51f-863c02174c28", "the agent must create a system user with SSH access (useradd, /etc/ssh): agent root is not available"),
    ("94d95f96-9699-4208-98ba-3c3119edf9c2", "the agent must install a system package (Spotify: apt or snap): agent root is not available"),
    (
        "b6781586-6346-41cd-935a-a6b1487918fc",
        "the agent must change the system time zone (timedatectl): agent root is not available (its setup also needs systemd, which a slot does not run)",
    ),
];

pub fn unsupported_reason(task_id: &str) -> Option<&'static str> {
    UNSUPPORTED_AGENT_ROOT.iter().find(|(id, _)| *id == task_id).map(|(_, w)| *w)
}

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
/// How long the state-hash gate waits for the guest's answer to each of its three read-only commands (run at once).
/// It was the 10 s of the guest probe: at 256 envs on one host, 479 freshly restored trees answered later than that
/// and were quarantined with "Resource temporarily unavailable" (the socket read timeout), not with a hash mismatch.
/// The gate itself is unchanged: the hash must equal the checkpoint's; no answer within this time still fails it.
pub const STATE_HASH_TIMEOUT: Duration = Duration::from_secs(60);

/// drumsd's thread count, memory and CPU pressure (some avg10) and the 1-minute load, read now (cheap: four small files).
pub fn load_snapshot() -> Value {
    let rd = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let threads = rd("/proc/self/status").lines().find_map(|l| l.strip_prefix("Threads:").and_then(|v| v.trim().parse::<u64>().ok()));
    let psi = |p: &str| {
        rd(p)
            .lines()
            .find(|l| l.starts_with("some"))
            .and_then(|l| l.split_whitespace().find_map(|f| f.strip_prefix("avg10=").and_then(|v| v.parse::<f64>().ok())))
    };
    let la = rd("/proc/loadavg").split_whitespace().next().and_then(|v| v.parse::<f64>().ok());
    json!({"drumsd_threads": threads, "mem_psi_some_avg10": psi("/proc/pressure/memory"), "cpu_psi_some_avg10": psi("/proc/pressure/cpu"), "loadavg1": la})
}

pub fn state_hash(g: &Guest) -> Result<String, String> {
    state_hash_timed(g).0
}

/// The state hash and each section's seconds (how long the guest took to answer it).
pub fn state_hash_timed(g: &Guest) -> (Result<String, String>, Vec<f64>) {
    let outs: Vec<(Result<String, String>, f64)> = std::thread::scope(|s| {
        let hs: Vec<_> = statehash::STATE_SECTIONS
            .iter()
            .map(|c| {
                s.spawn(move || {
                    let t = Instant::now();
                    let r = g.sh(c).map_err(|e| e.to_string());
                    (r, round3(t.elapsed().as_secs_f64()))
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let secs: Vec<f64> = outs.iter().map(|x| x.1).collect();
    let mut o: [String; 3] = Default::default();
    for (i, (r, _)) in outs.into_iter().enumerate() {
        match r {
            Ok(v) => o[i] = v,
            Err(e) => return (Err(format!("section {i}: {e}")), secs),
        }
    }
    (Ok(statehash::state_hash(&o)), secs)
}

/// Actions: {"type": "execute", "command": [..] | "..", "shell": bool}, {"type": "pyautogui", "code": "..."},
/// {"type": "launch", "command": .., "shell": bool} (detached, OSWorld /setup/launch), {"type": "sleep", "seconds": s}.
pub fn run_action(g: &Guest, a: &Value) -> std::io::Result<Value> {
    match a["type"].as_str().unwrap_or("execute") {
        "execute" => g.execute(a["command"].clone(), a["shell"].as_bool().unwrap_or(false)),
        "pyautogui" => g.execute(json!(["python3", "-c", pyautogui_command(a["code"].as_str().unwrap_or(""))]), false),
        "launch" => {
            let r = g.request("POST", "/setup/launch", Some(&json!({"command": a["command"], "shell": a["shell"].as_bool().unwrap_or(false)})))?;
            if r.status != 200 {
                return Err(std::io::Error::other(format!(
                    "/setup/launch HTTP {}: {}",
                    r.status,
                    String::from_utf8_lossy(&r.body).chars().take(300).collect::<String>()
                )));
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

/// OSWorld's PYAUTOGUI_PKGS_PREFIX (desktop_env/controllers/python.py at the pinned 044bc339), so an action runs as
/// it runs under OSWorld: `time` imported (agents' actions use time.sleep), FAILSAFE off, and OSWorld's Linux table of
/// shifted characters. drumsd's own prefix lacked `import time`: an action stopped with NameError at its first
/// time.sleep (SDK < 0.2.2 returned that exit status without raising).
pub const PYAUTOGUI_PREFIX: &str = concat!(
    "import pyautogui; import time; import platform; ",
    "pyautogui.FAILSAFE = False; ",
    "_osworld_shift_chars = '~!@#$%^&*()_+' + chr(123) + chr(125) + '|:\"<>?'; ",
    "_osworld_linux_shift_chars = '~!@#$%^&*()_+' + chr(123) + chr(125) + '|:\">?'; ",
    "pyautogui.isShiftCharacter = lambda character: character.isupper() or ",
    "character in (_osworld_linux_shift_chars if platform.system() == 'Linux' else _osworld_shift_chars); ",
);

pub fn pyautogui_command(code: &str) -> String {
    format!("{PYAUTOGUI_PREFIX}{code}")
}

/// What the API returns for a slot.
pub fn view(r: &SlotRecord) -> Value {
    let mut v = serde_json::to_value(r).unwrap();
    v["url"] = json!(format!("http://127.0.0.1:{}", r.port));
    v["lease_expires_epoch"] = match (&r.lease, r.state.is_terminal()) {
        (Some(l), false) => json!(l.expires_at(r.created_epoch, r.last_used_epoch.unwrap_or(r.created_epoch))),
        _ => Value::Null,
    };
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

/// The reward's audit block: the helper's checks (recheck, redundant checker, scope) plus what only drumsd sees (the
/// helper killed at its timeout, a helper failure, the episode's validity). `trusted` is true only for a valid
/// episode whose checker returned a score with no audit flag. `flags` is the union.
pub fn reward_audit(detail: &Value, score: Option<f64>, err: Option<&str>, valid: bool, skipped: bool) -> Value {
    let mut a = match detail.get("audit") {
        Some(Value::Object(m)) => Value::Object(m.clone()),
        _ => json!({"version": 1, "recheck": {"ran": false}, "redundant": {"ran": false}, "scope": {"ran": false}}),
    };
    let mut flags: Vec<String> = a["flags"].as_array().map(|v| v.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let helper_reply = !detail.is_null();
    if skipped {
        flags.push("checker_skipped".into());
    } else if !helper_reply {
        // the helper itself failed: killed at drumsd's timeout, or no reply
        let e = err.unwrap_or("");
        flags.push(if e.contains("timed out after") { "checker_timeout".into() } else { "checker_crash".into() });
        a["checker"] = json!({"error_class": if e.contains("timed out after") { "timeout" } else { "helper_failed" }});
    }
    if !valid {
        flags.push("invalid_episode".into());
    }
    flags.sort();
    flags.dedup();
    let trusted = valid && score.is_some() && err.is_none() && flags.is_empty();
    a["flags"] = json!(flags);
    a["trusted"] = json!(trusted);
    a
}

#[cfg(test)]
mod tests {

    #[test]
    fn reward_audit_marks_only_clean_valid_scores_trusted() {
        use super::reward_audit;
        use serde_json::json;
        let ok = json!({"score": 1.0, "audit": {"version": 1, "flags": [], "recheck": {"ran": true, "agree": true}}});
        let a = reward_audit(&ok, Some(1.0), None, true, false);
        assert_eq!(a["trusted"], true);
        assert_eq!(a["recheck"]["agree"], true);
        let a = reward_audit(&ok, Some(1.0), None, false, false);
        assert_eq!(a["trusted"], false);
        assert_eq!(a["flags"], json!(["invalid_episode"]));
        let flagged = json!({"score": 1.0, "audit": {"version": 1, "flags": ["scope_violation"]}});
        let a = reward_audit(&flagged, Some(1.0), None, true, false);
        assert_eq!((a["trusted"].clone(), a["flags"].clone()), (json!(false), json!(["scope_violation"])));
        let a = reward_audit(&serde_json::Value::Null, None, Some("osw evaluate timed out after 600s"), true, false);
        assert_eq!(a["flags"], json!(["checker_timeout"]));
        assert_eq!(a["checker"]["error_class"], "timeout");
        let a = reward_audit(&serde_json::Value::Null, None, Some("osw evaluate exit Some(1), no reply"), true, false);
        assert_eq!(a["flags"], json!(["checker_crash"]));
        let a = reward_audit(&json!({"skipped": "quarantined: gate"}), None, None, false, true);
        assert_eq!(a["flags"], json!(["checker_skipped", "invalid_episode"]));
    }

    #[test]
    fn a_lost_checkpoint_is_not_held_by_down_or_quarantined_envs() {
        let down = SlotState::Down { reason: "state_lost".into() };
        let quar = SlotState::Quarantined { reason: "gate".into() };
        assert!(!blocks_checkpoint_delete(&down, true));
        assert!(!blocks_checkpoint_delete(&quar, true));
        assert!(blocks_checkpoint_delete(&SlotState::Ready, true));
        assert!(blocks_checkpoint_delete(&SlotState::Creating, true));
        // not lost: every live env holds it, as before
        assert!(blocks_checkpoint_delete(&down, false));
        assert!(blocks_checkpoint_delete(&quar, false));
        assert!(!blocks_checkpoint_delete(&SlotState::Destroyed, false));
    }
    use super::*;

    fn qrec(state: SlotState) -> SlotRecord {
        let mut gates = GateReport::default();
        for g in Gate::ALL {
            gates.push(GateResult::pass(g, 0.0, json!({})));
        }
        SlotRecord {
            id: "s147".into(),
            num: 147,
            checkpoint: "osworld-ubuntu".into(),
            state,
            episode: 1,
            init_pid: Some(1),
            init_start_ticks: None,
            restore_generation: 1,
            leaf: 1,
            mode: "template".into(),
            port: 25147,
            extra_ports: vec![],
            validity: gates.validity(&required_gates()),
            gates,
            cost: CostRecord::default(),
            timings: BTreeMap::new(),
            last_error: None,
            task: None,
            ready_epoch: None,
            episode_start_epoch: None,
            created_epoch: 0.0,
            updated_epoch: 0.0,
            task_setup_on_restore: false,
            usage: Ledger::default(),
            verified: None,
            closed_epoch: None,
            lease: None,
            last_used_epoch: None,
            closed_reason: None,
            dirty_base: None,
            dirty_depth: 0,
            dirty_base_uptime_us: None,
        }
    }

    #[test]
    fn a_quarantined_env_is_invalid_with_the_reason_named() {
        // the blind user's case: a valid episode, then env.quarantine("client")
        let mut r = qrec(SlotState::Ready);
        assert_eq!(r.validity, Validity::Valid);
        assert!(!mark_quarantined(&mut r), "a ready env is not marked");
        assert!(invalid_reasons(&r).is_empty());
        r.state = SlotState::Quarantined { reason: "client".into() };
        assert!(mark_quarantined(&mut r));
        assert_eq!(r.validity, Validity::Invalid { gates: vec![Gate::NotQuarantined] });
        assert_eq!(r.gates.get(Gate::NotQuarantined).unwrap().detail["reason"], "client");
        assert_eq!(invalid_reasons(&r), vec!["quarantined: client".to_string()]);
        assert!(!mark_quarantined(&mut r), "once per episode");
        assert_eq!(r.gates.gates.iter().filter(|g| g.gate == Gate::NotQuarantined).count(), 1);
        // the API's verdict object says so
        let v = serde_json::to_value(&r.validity).unwrap();
        assert_eq!(v, json!({"verdict": "invalid", "gates": ["not_quarantined"]}));
        // a new episode (reset after release) starts from its own gates
        let mut n = qrec(SlotState::Ready);
        n.episode = 2;
        assert_eq!(n.validity, Validity::Valid);
    }

    #[test]
    fn invalid_reasons_never_come_back_empty_for_an_invalid_episode() {
        let mut r = qrec(SlotState::Down { reason: "task setup failed: download".into() });
        r.validity = Validity::Invalid { gates: vec![] };
        assert_eq!(invalid_reasons(&r), vec!["env is down: task setup failed: download".to_string()]);
        let mut o = qrec(SlotState::Ready);
        o.gates.push(GateResult::fail(Gate::NoOomKill, 0.0, json!({"oom_kills": 2})));
        o.validity = o.gates.validity(&required_gates());
        assert_eq!(invalid_reasons(&o), vec!["no_oom_kill: the OOM killer took 2 task(s) of the env".to_string()]);
        let mut p = qrec(SlotState::Ready);
        p.gates.push(GateResult::pending(Gate::AliveAfterResume));
        p.validity = p.gates.validity(&required_gates());
        assert_eq!(invalid_reasons(&p), vec!["gate alive_after_resume still pending".to_string()]);
    }

    #[test]
    fn an_app_crash_makes_the_episode_invalid_with_a_named_reason() {
        let mut o = qrec(SlotState::Ready);
        assert_eq!(o.validity, Validity::Valid);
        let crash = json!({"crashes": [{"tgid": 15003, "pid": 15003, "comm": "chrome", "parent_comm": "chrome", "signal_name": "SIGILL"},
                                       {"tgid": 15982, "pid": 15982, "comm": null, "parent_comm": "chrome", "signal_name": "SIGILL"}],
                           "incomplete": []});
        o.gates.push(GateResult::fail(Gate::NoAppCrash, 0.0, crash));
        o.validity = o.gates.validity(&required_gates());
        assert_eq!(o.validity, Validity::Invalid { gates: vec![Gate::NoAppCrash] });
        assert_eq!(invalid_reasons(&o), vec!["no_app_crash: chrome (pid 15003) died of SIGILL; a child of chrome (pid 15982) died of SIGILL".to_string()]);
        // an episode the watch cannot vouch for, and one made from a state saved after a crash
        let w = app_crash_words(&json!({"crashes": [], "incomplete": ["the kernel dropped process events at 1.000 (socket overflow)"]}));
        assert_eq!(w, "the kernel dropped process events at 1.000 (socket overflow)");
        let w = app_crash_words(&json!({"inherited": "chrome (pid 9) died of SIGSEGV", "checkpoint": "s101-c102"}));
        assert_eq!(w, "made from saved state s101-c102 captured after an app crash (chrome (pid 9) died of SIGSEGV)");
    }

    #[test]
    fn tasks_that_need_agent_root_are_unsupported() {
        assert!(unsupported_reason("5812b315-e7bd-4265-b51f-863c02174c28").unwrap().contains("agent root"));
        assert!(unsupported_reason("b6781586-6346-41cd-935a-a6b1487918fc").is_some());
        // a task whose setup alone uses sudo is supported (its setup runs as provisioning)
        assert!(unsupported_reason("e0df059f-28a6-4169-924f-b9623e7184cc").is_none());
        assert!(unsupported_reason("acme-expense-01").is_none());
    }

    #[test]
    fn operations_on_a_destroyed_env_are_410_env_gone() {
        let mut r = qrec(SlotState::Destroyed);
        r.closed_reason = Some("lease: idle 1800 s".into());
        let e = refused(&r, SlotState::Destroyed.begin(Op::Step).unwrap_err());
        assert_eq!(e.status, 410);
        assert_eq!(e.detail.as_ref().unwrap()["code"], "env_gone");
        assert!(e.message.contains("lease: idle 1800 s") && e.message.contains("step"), "{}", e.message);
        let e = refused(&qrec(SlotState::Destroying), SlotState::Destroying.begin(Op::Reset).unwrap_err());
        assert_eq!(e.status, 410);
        // a live env in the wrong state is still 409, with a code
        let q = qrec(SlotState::Quarantined { reason: "x".into() });
        let e = refused(&q, q.state.begin(Op::Step).unwrap_err());
        assert_eq!((e.status, e.detail.unwrap()["code"].clone()), (409, json!("wrong_state")));
    }

    /// R2-3 Firecracker host B, N=256: env s201-2 quarantined ("gate cgroup failed"), then 10,325 resets refused with
    /// 409 in 50 s. The refusal now says why the env is quarantined and what to do, so a client need not GET the record
    /// to find out, and never mistakes the 409 for a transient failure.
    #[test]
    fn a_reset_refused_on_a_quarantined_env_names_the_reason_and_the_way_out() {
        let q = qrec(SlotState::Quarantined { reason: "gate cgroup failed: {\"failed\":[\"outside_cgroup\"]}".into() });
        let e = refused(&q, q.state.begin(Op::Reset).unwrap_err());
        assert_eq!(e.status, 409);
        let d = e.detail.unwrap();
        assert_eq!(d["code"], "wrong_state");
        assert_eq!(d["state"], "quarantined");
        assert_eq!(d["reason"], "gate cgroup failed: {\"failed\":[\"outside_cgroup\"]}");
        assert!(d["next"].as_str().unwrap().contains("release") && d["next"].as_str().unwrap().contains("create a new env"), "{d}");
        assert!(e.message.contains("quarantined") && e.message.contains("reset is not allowed"), "{}", e.message);
        // a down env (a failed restore) is retried in place: no reason, no next
        let dn = qrec(SlotState::Down { reason: "criu restore failed".into() });
        let e = refused(&dn, dn.state.begin(Op::Step).unwrap_err());
        assert!(e.detail.unwrap().get("next").is_none());
    }

    /// The quarantine reason keeps the whole gate detail (bounded at GATE_DETAIL_CHARS), not its first 300
    /// characters: the cgroup gate's JSON was cut before the field that said which check failed.
    #[test]
    fn the_quarantine_reason_keeps_the_full_gate_detail() {
        let detail = json!({"failed": ["outside_cgroup"], "cgroup": "/system.slice/drumsbench-pool.scope/drumsbench-rootfs-76bafd47-201-pool.scope/drumsbench-rootfs-76bafd47-201-r2.scope",
            "init_cgroup": "/system.slice/drumsbench-pool.scope/drumsbench-rootfs-76bafd47-201-pool.scope/drumsbench-rootfs-76bafd47-201-r2.scope",
            "init_oom_score_adj": 300, "limits": {"memory_max": 4294967296u64, "pids_max": 4096, "cpu_weight": 100}, "want": {"memory_max": 4294967296u64, "pids_max": 4096, "cpu_weight": 100},
            "members": 61, "outside_cgroup": [412233], "outside_cgroup_procs": [[412233, "sh", "/system.slice/other.scope"]], "want_oom_score_adj": 300, "negative_oom_score_adj": []}).to_string();
        assert!(detail.len() > 300 && detail.len() < GATE_DETAIL_CHARS, "{}", detail.len());
        let st = SlotState::finish(Op::Reset, &Outcome::GateFailed { gate: "cgroup".into(), detail: detail.chars().take(GATE_DETAIL_CHARS).collect() });
        let SlotState::Quarantined { reason } = st else { panic!("{st:?}") };
        assert_eq!(reason, format!("gate cgroup failed: {detail}"));
        assert!(reason.contains("outside_cgroup_procs"), "the part that says what failed survives");
        // the bound still holds for a detail that is longer
        let long = "x".repeat(GATE_DETAIL_CHARS + 10);
        assert_eq!(long.chars().take(GATE_DETAIL_CHARS).count(), GATE_DETAIL_CHARS);
    }

    #[test]
    fn the_view_shows_when_the_lease_runs_out() {
        let mut r = qrec(SlotState::Ready);
        r.created_epoch = 1000.0;
        r.last_used_epoch = Some(1500.0);
        r.lease = Some(Lease { idle_timeout_s: Some(60.0), ttl_s: None });
        assert_eq!(view(&r)["lease_expires_epoch"], json!(1560.0));
        r.state = SlotState::Destroyed;
        assert!(view(&r)["lease_expires_epoch"].is_null());
    }

    fn ck(captured_from: Option<&str>, provenance: Option<Value>) -> CheckpointRecord {
        CheckpointRecord {
            id: "osworld-ubuntu".into(),
            slot: 0,
            socket: "/s".into(),
            server_pid: None,
            argv_marker: "m".into(),
            expected_state_hash: None,
            hash_reference: "r".into(),
            captured_from: captured_from.map(|s| s.into()),
            created_epoch: 0.0,
            server_ready_s: 0.0,
            task: None,
            quiescence: None,
            provenance,
            lease: None,
            delete_with_children: false,
            last_used_epoch: None,
            crash_taint: None,
        }
    }

    #[test]
    fn an_installed_origin_from_other_sources_or_inspection_is_stale() {
        let cur = json!({"source_sha256": "a", "inspection_sha256": "i"});
        assert_eq!(stale_reason(&ck(None, Some(cur.clone())), &cur), None);
        // v0.3 records have no provenance: the origin was made by the old login (re-provision)
        assert!(stale_reason(&ck(None, None), &cur).unwrap().contains("before provenance"));
        let r = stale_reason(&ck(None, Some(json!({"source_sha256": "a", "inspection_sha256": "old"}))), &cur).unwrap();
        assert!(r.contains("inspection_sha256") && !r.contains("source_sha256"), "{r}");
        let r = stale_reason(&ck(None, Some(json!({"source_sha256": "b", "inspection_sha256": "old"}))), &cur).unwrap();
        assert!(r.contains("source_sha256 and inspection_sha256"), "{r}");
        // a saved state is the user's data: never stale, whatever made it
        assert_eq!(stale_reason(&ck(Some("s101"), None), &cur), None);
        let v = ckpt_view_on(&ck(None, None), &cur, None);
        assert_eq!(v["stale"], json!(true));
        assert!(v["stale_reason"].is_string());
        // a v0.3 journal record (no provenance field) still loads
        let mut old = serde_json::to_value(ck(None, None)).unwrap();
        old.as_object_mut().unwrap().remove("provenance");
        let back: CheckpointRecord = serde_json::from_value(old).unwrap();
        assert!(back.provenance.is_none());
    }

    #[test]
    fn a_checkpoint_whose_state_is_gone_is_state_lost() {
        let d = std::env::temp_dir().join(format!("drumsd-lost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let lower = d.join("lower");
        let c = ck(None, None);
        // state dir emptied (tmpfs after a reboot): the slot directory is missing
        let r = state_lost(&c, &d, &lower, true).unwrap();
        assert!(r.starts_with("state_lost:") && r.contains("slot directory") && r.contains("missing"), "{r}");
        // slot directory present but images empty and no upper.snap
        let sp = slotfs::paths(&d, 0);
        std::fs::create_dir_all(&sp.images).unwrap();
        let r = state_lost(&c, &d, &lower, true).unwrap();
        assert!(r.contains("images") && r.contains("upper.snap"), "{r}");
        // everything there, image mount gone
        std::fs::write(sp.images.join("core-1.img"), b"x").unwrap();
        std::fs::create_dir_all(&sp.snap).unwrap();
        let r = state_lost(&c, &d, &lower, false).unwrap();
        assert!(r.contains("not mounted") && !r.contains("images"), "{r}");
        // all present: not lost, and the view is not stale
        assert_eq!(state_lost(&c, &d, &lower, true), None);
        let cur = json!({"source_sha256": "a", "inspection_sha256": "i"});
        let ok = ckpt_view_on(&ck(None, Some(cur.clone())), &cur, None);
        assert_eq!(ok["stale"], json!(false));
        let bad = ckpt_view_on(&ck(None, Some(cur.clone())), &cur, state_lost(&c, &d, &lower, false));
        assert_eq!(bad["stale"], json!(true));
        assert!(bad["stale_reason"].as_str().unwrap().starts_with("state_lost:"));
        // the state vanishing later (checked on every create) is seen at once
        std::fs::remove_dir_all(&sp.dir).unwrap();
        assert!(state_lost(&c, &d, &lower, true).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn is_mounted_sees_root_and_not_a_plain_directory() {
        assert!(is_mounted(Path::new("/")) || std::fs::read_to_string("/proc/self/mountinfo").is_err());
        assert!(!is_mounted(&std::env::temp_dir().join("drumsd-not-a-mount-xyz")));
    }

    /// A guest that answers each command after `delay`, with fixed outputs.
    fn slow_guest(delay: Duration) -> u16 {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let mut c = c.unwrap();
                std::thread::spawn(move || {
                    let mut got = vec![];
                    let mut buf = [0u8; 4096];
                    while !got.windows(4).any(|w| w == b"\r\n\r\n") || !got.ends_with(b"}") {
                        let n = c.read(&mut buf).unwrap();
                        if n == 0 {
                            return;
                        }
                        got.extend_from_slice(&buf[..n]);
                    }
                    std::thread::sleep(delay);
                    let _ = c.write_all(b"HTTP/1.0 200 OK\r\n\r\n{\"status\": \"success\", \"output\": \"a\\n\", \"error\": \"\", \"returncode\": 0}");
                });
            }
        });
        port
    }

    #[test]
    fn a_slow_guest_is_hashed_within_the_gate_timeout_and_a_silent_one_fails_with_a_timeout() {
        // the gate waits STATE_HASH_TIMEOUT, not the 10 s guest probe: 479 trees at 256 envs answered later than 10 s
        assert!(STATE_HASH_TIMEOUT >= Duration::from_secs(60));
        let src = include_str!("manager.rs");
        assert!(src.contains("let gh = Guest::new(self.cfg.port_base + num as u16, STATE_HASH_TIMEOUT);"));
        let p = slow_guest(Duration::from_millis(700));
        let (h, secs) = state_hash_timed(&Guest::new(p, Duration::from_secs(5)));
        assert_eq!(h.unwrap(), statehash::state_hash(&["a\n".into(), "a\n".into(), "a\n".into()]));
        assert_eq!(secs.len(), 3);
        assert!(secs.iter().all(|s| *s >= 0.7), "{secs:?}");
        // the same guest with too short a wait: a named timeout, never a hash (the gate fails closed)
        let (h, _) = state_hash_timed(&Guest::new(p, Duration::from_millis(200)));
        let e = h.unwrap_err();
        assert!(e.contains("did not answer POST /execute within 0.2 s") && !e.starts_with("Resource"), "{e}");
    }

    #[test]
    fn pyautogui_actions_run_with_osworlds_prefix() {
        // the text of OSWorld 044bc339 PYAUTOGUI_PKGS_PREFIX with {command} at its end
        let osw = concat!(
            "import pyautogui; import time; import platform; pyautogui.FAILSAFE = False; ",
            "_osworld_shift_chars = '~!@#$%^&*()_+' + chr(123) + chr(125) + '|:\"<>?'; ",
            "_osworld_linux_shift_chars = '~!@#$%^&*()_+' + chr(123) + chr(125) + '|:\">?'; ",
            "pyautogui.isShiftCharacter = lambda character: character.isupper() or ",
            "character in (_osworld_linux_shift_chars if platform.system() == 'Linux' else _osworld_shift_chars); {command}"
        );
        assert_eq!(pyautogui_command("{command}"), osw);
        let c = pyautogui_command("pyautogui.hotkey('ctrl', 't'); time.sleep(1)");
        assert!(c.contains("import time;") && c.ends_with("time.sleep(1)"));
        // the action is code appended verbatim: python must parse the whole line (checked when python3 is present)
        if let Ok(o) = std::process::Command::new("python3").args(["-c", &format!("import ast; ast.parse({:?})", c)]).output() {
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        }
    }

    #[test]
    fn a_failed_capture_verb_moves_its_directory_off_the_state_dir() {
        let src = include_str!("manager.rs");
        let f = src.find("let res = tokio::task::spawn_blocking(move || me.run_capture_verb(num, into)).await;").unwrap();
        let body = &src[f..f + 1500];
        let err = body.find("Err(e) => {").unwrap();
        assert!(body[err..].find("me.discard_dump(into)").unwrap() < body[err..].find("return Err(ApiError::new(500, e));").unwrap());
    }

    #[test]
    fn provenance_hashes_the_inspection_named_by_the_python_config() {
        let t = tempfile::tempdir().unwrap();
        let insp = t.path().join("inspection.json");
        std::fs::write(&insp, b"{\"login\": {}}").unwrap();
        let cfgp = t.path().join("rootfs-config.json");
        std::fs::write(&cfgp, serde_json::to_vec(&json!({"inspection": insp})).unwrap()).unwrap();
        let p = provenance("src", &cfgp);
        assert_eq!(p["source_sha256"], json!("src"));
        assert_eq!(p["inspection_sha256"], json!(hex_sha256(b"{\"login\": {}}")));
        std::fs::write(&insp, b"{\"login\": {\"locale\": {}}}").unwrap();
        assert_ne!(provenance("src", &cfgp), p);
        assert_eq!(provenance("src", &t.path().join("missing.json"))["inspection_sha256"], Value::Null);
        // the view setting is part of it; a config without one keeps the 0.4 shape
        assert!(p.get("procview_dir").is_none());
        std::fs::write(&cfgp, serde_json::to_vec(&json!({"inspection": insp, "procview_dir": "/run/drums/procview"})).unwrap()).unwrap();
        let q = provenance("src", &cfgp);
        assert_eq!(q["procview_dir"], json!("/run/drums/procview"));
        std::fs::write(&cfgp, serde_json::to_vec(&json!({"inspection": insp, "procview_dir": null})).unwrap()).unwrap();
        assert_ne!(provenance("src", &cfgp), q);
        // the slot profile: hardened (absent or true) keeps the shape, the trusted-only opt-out is recorded
        std::fs::write(&cfgp, serde_json::to_vec(&json!({"inspection": insp})).unwrap()).unwrap();
        let plain = provenance("src", &cfgp);
        std::fs::write(&cfgp, serde_json::to_vec(&json!({"inspection": insp, "harden": true})).unwrap()).unwrap();
        assert_eq!(provenance("src", &cfgp), plain);
        std::fs::write(&cfgp, serde_json::to_vec(&json!({"inspection": insp, "harden": false})).unwrap()).unwrap();
        assert_eq!(provenance("src", &cfgp)["harden"], json!(false));
    }
    #[test]
    fn numbers_of_destroyed_slots_are_reused_and_reservations_end() {
        // the leak this replaces: every number ever handed out stayed reserved, so a long run hit "no free slot
        // number" after (range size) creates + captures
        let range = (100, 103);
        let mut held = BTreeSet::new();
        let mut reserved = BTreeSet::new();
        let disk: BTreeSet<u32> = BTreeSet::new();
        for _ in 0..1000 {
            // create: reserve, the record takes over, the reservation ends; destroy: the record goes terminal
            let n = pick_num(range, 0, &held, &reserved, |n| disk.contains(&n)).expect("a number must be free after every destroy");
            reserved.insert(n);
            held.insert(n);
            reserved.remove(&n);
            held.remove(&n);
        }
        // in-flight reservations, live slots and leftovers on disk are never handed out
        let held: BTreeSet<u32> = [100].into();
        let reserved: BTreeSet<u32> = [101].into();
        assert_eq!(pick_num(range, 0, &held, &reserved, |n| n == 102), None);
        assert_eq!(pick_num(range, 0, &held, &reserved, |_| false), Some(102));
        // the search starts after the last number handed out and wraps
        let none = BTreeSet::new();
        assert_eq!(pick_num((100, 110), 105, &none, &none, |_| false), Some(105));
        assert_eq!(pick_num((100, 110), 110, &none, &none, |_| false), Some(100));
        assert_eq!(pick_num((100, 110), 108, &none, &none, |n| n >= 108), Some(100));
    }

    #[test]
    fn destroy_retried_after_an_interrupted_one_succeeds() {
        // L32b: drumsd SIGKILLed 1 s into a destroy; after the restart every DELETE of that env failed with ENOENT
        let d = std::env::temp_dir().join(format!("drumsd-retire-{}", std::process::id()));
        let sp = slotfs::paths(&d, 887);
        std::fs::create_dir_all(sp.dir.join("upper")).unwrap();
        std::fs::write(sp.dir.join("slot.json"), "{}").unwrap();
        let fd = d.join("disk-forgotten");
        let k = retire_slot_dir(&d, 887, 1, |dir, n| forget::forget(&d, &fd, dir, n, forget::Disposition::Full).map(|f| f.at)).unwrap();
        assert!(k.ends_with("full/slot887-1.tar.zst") && k.exists() && !sp.dir.exists(), "{k:?}");
        assert!(
            retire_slot_dir(&d, 887, 2, |dir, n| forget::forget(&d, &fd, dir, n, forget::Disposition::Full).map(|f| f.at)).is_ok(),
            "the retry must not fail"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn slot_ids_are_never_reused() {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for _ in 0..5 {
            let id = slot_id_for(105, |c| seen.contains(c));
            assert!(seen.insert(id));
        }
        assert_eq!(seen.iter().cloned().collect::<Vec<_>>(), vec!["s105", "s105-2", "s105-3", "s105-4", "s105-5"]);
        assert_eq!(slot_id_for(7, |_| false), "s7");
    }

    #[test]
    fn restore_permit_is_released_before_the_gates() {
        // L128a: the permit was held through gates, guest probe and task setup, so creates queued ~56 s for one.
        // restore_once must drop it once CRIU's restore is done (the pid is read) and before the kernel gates.
        let src = include_str!("manager.rs");
        let f =
            src.find("fn restore_once(&self, slot: &Slot, private: bool, permit: tokio::sync::OwnedSemaphorePermit)").expect("restore_once takes the permit");
        let body = &src[f..];
        let drop_at = body.find("drop(permit);").expect("restore_once drops the permit");
        let restore_at = body.find("restore::run(").unwrap();
        let gates_at = body.find("// 5. kernel-side gates").unwrap();
        assert!(restore_at < drop_at && drop_at < gates_at, "permit must cover the CRIU restore and end before the gates");
        // and the caller no longer holds it after restore_once returns
        assert!(src.contains("spawn_blocking(move || me.restore_once(&s2, private, permit))"));
    }

    #[tokio::test]
    async fn a_dropped_permit_lets_the_next_restore_start_at_once() {
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let p = sem.clone().acquire_owned().await.unwrap();
        let held = std::thread::spawn(move || {
            drop(p); // what restore_once does after CRIU
            std::thread::sleep(Duration::from_millis(300)); // gates and setup, without the permit
        });
        let t = Instant::now();
        let _p2 =
            tokio::time::timeout(Duration::from_millis(200), sem.clone().acquire_owned()).await.expect("next restore must not wait for the gates").unwrap();
        assert!(t.elapsed() < Duration::from_millis(200));
        held.join().unwrap();
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
    }

    fn rec(state: SlotState) -> SlotRecord {
        serde_json::from_value(json!({"id": "s-1", "num": 900, "checkpoint": "c", "state": state.name(), "reason": "x", "episode": 2, "init_pid": null,
            "init_start_ticks": null, "restore_generation": 1, "mode": "template", "port": 1, "extra_ports": [],
            "gates": {"gates": []}, "validity": {"verdict": "valid"}, "cost": CostRecord::default(), "timings": {"restore_s": 1.0},
            "last_error": null, "task": null, "ready_epoch": null, "created_epoch": 1.0, "updated_epoch": 2.0}))
        .unwrap()
    }

    fn is_summary(d: &forget::Disposition) -> bool {
        matches!(d, forget::Disposition::Summary { .. })
    }

    #[test]
    fn valid_envs_get_a_summary_and_failed_ones_a_full_archive() {
        use forget::Mode::*;
        let ok = rec(SlotState::Ready);
        assert!(is_summary(&forget_disposition(FailuresFull, &ForgetBasis::Env(&ok), None)));
        // keep_all keeps everything in full
        assert!(!is_summary(&forget_disposition(KeepAll, &ForgetBasis::Env(&ok), None)));
        // quarantined, down, invalid, pending, a failed gate, an error, a failure mark
        let q = rec(SlotState::Quarantined { reason: "gate alive failed".into() });
        let d = rec(SlotState::Down { reason: "restore refused".into() });
        let mut inv = rec(SlotState::Ready);
        inv.validity = Validity::Invalid { gates: vec![] };
        let mut pend = rec(SlotState::Ready);
        pend.validity = Validity::Pending { gates: vec![] };
        let mut err = rec(SlotState::Ready);
        err.last_error = Some("boom".into());
        for r in [&q, &d, &inv, &pend, &err] {
            assert!(!is_summary(&forget_disposition(FailuresFull, &ForgetBasis::Env(r), None)), "{:?}", r.state);
        }
        let mark = json!({FAILURE_SEEN: "quarantined: earlier"});
        assert!(!is_summary(&forget_disposition(FailuresFull, &ForgetBasis::Env(&ok), Some(&mark))), "a past failure keeps the full records");
        assert!(is_summary(&forget_disposition(FailuresFull, &ForgetBasis::Env(&ok), Some(&json!({"restore_generation": 1})))));
        // rejected dumps are always full
        assert!(!is_summary(&forget_disposition(FailuresFull, &ForgetBasis::Rejected, None)));
    }

    #[test]
    fn failure_marks_follow_the_record() {
        assert!(failure_seen(&rec(SlotState::Ready)).is_none());
        assert!(failure_seen(&rec(SlotState::Quarantined { reason: "r".into() })).unwrap().contains("quarantined"));
        assert!(failure_seen(&rec(SlotState::Destroyed)).is_none());
        let mut r = rec(SlotState::Resetting);
        r.last_error = Some("e".into());
        assert!(failure_seen(&r).is_some());
    }

    #[test]
    fn retire_keeps_full_for_a_quarantined_env_and_a_summary_for_a_valid_one() {
        let d = std::env::temp_dir().join(format!("drumsd-retire-class-{}", std::process::id()));
        let fd = d.join("disk");
        for (num, state, want) in
            [(881u32, SlotState::Ready, forget::Class::Summary), (882, SlotState::Quarantined { reason: "gate".into() }, forget::Class::Full)]
        {
            let sp = slotfs::paths(&d, num);
            std::fs::create_dir_all(sp.dir.join("upper")).unwrap();
            std::fs::write(sp.dir.join("slot.json"), "{}").unwrap();
            std::fs::write(sp.dir.join("restore.log"), vec![b'x'; 1 << 20]).unwrap();
            let r = rec(state);
            let mut class = None;
            let at = retire_slot_dir(&d, num, 5, |dir, n| {
                let disp = forget_disposition(forget::Mode::FailuresFull, &ForgetBasis::Env(&r), slotfs::read_json(&dir.join("slot.json")).ok().as_ref());
                let f = forget::forget(&d, &fd, dir, n, disp)?;
                class = Some(f.class);
                Ok(f.at)
            })
            .unwrap();
            assert_eq!(class, Some(want));
            assert!(at.exists());
        }
        assert_eq!(forget::class_totals(&fd, forget::Class::Summary).1, 1);
        assert_eq!(forget::class_totals(&fd, forget::Class::Full).1, 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
