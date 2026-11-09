//! Pool state: hosts and their health, placement by headroom, warm slots, the create queue (backpressure), leases,
//! and failover of envs whose host died.
//!
//! Ids. Clients see pool ids only: envs are `pe-<boot>-<n>`, saved states are `ps-<boot>-<n>`. Images (installed
//! checkpoints such as `osworld-ubuntu`) keep their names: every host has them. A pool id maps to (host, host id).

use crate::config::{Config, HostSpec};
use crate::metrics::Metrics;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{watch, Notify};

pub fn now_epoch() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostState {
    /// Registered; no good health poll yet.
    Joining,
    Healthy,
    /// Recent polls failed: no new envs; existing envs are still served.
    Suspect,
    /// No good poll for dead_after_s: its envs failed over. A host that answers again is swept of orphans first.
    Dead,
    /// No new envs; removed once it holds none.
    Draining,
}

impl HostState {
    pub fn name(&self) -> &'static str {
        match self {
            HostState::Joining => "joining",
            HostState::Healthy => "healthy",
            HostState::Suspect => "suspect",
            HostState::Dead => "dead",
            HostState::Draining => "draining",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Snap {
    pub mem_total_mib: f64,
    pub mem_avail_mib: f64,
    pub psi_mem: f64,
    pub load1: f64,
    pub live_slots: u32,
    pub health_ok: bool,
    pub rtt_s: f64,
    pub admission_refusals: f64,
    /// The host's last /v1/health reply (fingerprint fields are aggregated from it).
    pub raw: Value,
}

pub struct Host {
    pub spec: HostSpec,
    pub token: String,
    pub state: HostState,
    pub fails: u32,
    pub last_ok: Option<Instant>,
    pub registered: Instant,
    pub snap: Option<Snap>,
    /// Creates placed here and not yet answered.
    pub inflight: u32,
    pub warm_inflight: u32,
    /// Envs the pool owns here (live, or waiting for their failover reset).
    pub owned: u32,
    pub warm: Vec<String>,
    pub young: VecDeque<Instant>,
    pub penalty_until: Option<Instant>,
    /// Host memory in use with no env (learned at the first poll with no live slot).
    pub base_used_mib: Option<f64>,
    pub env_mib_est: f64,
    /// Bumped when the host is declared dead: in-flight requests to it watch this and give up at once.
    pub dead_tx: watch::Sender<u64>,
    pub dead_gen: u64,
    pub needs_sweep: bool,
    pub sweeping: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EnvStatus {
    Live,
    /// Its host died; a replacement is being made.
    Replacing { from_host: String, since: f64 },
    /// Replaced on a new host: every request except reset/close/get answers 409 episode_lost until a reset.
    NeedsReset { from_host: String, at: f64 },
    /// Its host died and no replacement could be made.
    Lost { reason: String },
}

#[derive(Clone, Debug)]
pub struct PoolEnv {
    pub id: String,
    pub host: String,
    pub slot: String,
    /// What the client created it from: an image name or a pool saved-state id.
    pub checkpoint: String,
    pub task: Option<Value>,
    pub setup: Option<bool>,
    pub idle_timeout_s: f64,
    pub ttl_s: f64,
    pub created_epoch: f64,
    pub created: Instant,
    pub last_used: Instant,
    pub status: EnvStatus,
    pub record: Value,
    pub failovers: u32,
}

#[derive(Clone, Debug)]
pub struct SavedState {
    pub id: String,
    pub host: String,
    pub host_id: String,
    pub lost: bool,
}

pub struct Queue {
    pub tickets: Mutex<VecDeque<u64>>,
    pub next: AtomicU64,
    pub notify: Notify,
}

pub struct Pool {
    pub cfg: Config,
    pub token: String,
    pub admin_token: String,
    pub boot: String,
    pub hosts: Mutex<HashMap<String, Host>>,
    pub envs: Mutex<HashMap<String, PoolEnv>>,
    pub states: Mutex<HashMap<String, SavedState>>,
    pub tombstones: Mutex<(HashMap<String, String>, VecDeque<String>)>,
    pub queue: Queue,
    pub http: reqwest::Client,
    pub metrics: Metrics,
    pub seq: AtomicU64,
    pub started: Instant,
    events: Mutex<Option<std::fs::File>>,
}

pub type P = Arc<Pool>;

/// What a placement attempt found.
#[derive(Debug, Clone, PartialEq)]
pub enum Placement {
    Host(String),
    Warm(String, String),
}

/// Why a host could not take one more env right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Blocked {
    NotHealthy,
    Penalized,
    Slots,
    Memory,
    Cpu,
    MemoryPressure,
    Inflight,
}

impl Blocked {
    pub fn name(&self) -> &'static str {
        match self {
            Blocked::NotHealthy => "not_healthy",
            Blocked::Penalized => "refused_recently",
            Blocked::Slots => "slots",
            Blocked::Memory => "memory",
            Blocked::Cpu => "cpu",
            Blocked::MemoryPressure => "memory_pressure",
            Blocked::Inflight => "inflight",
        }
    }
}

/// Headroom of one host for one more env: Ok(score in 0..1, higher = emptier) or the binding constraint.
pub fn headroom(cfg: &Config, h: &Host, now: Instant) -> Result<f64, Blocked> {
    if h.state != HostState::Healthy {
        return Err(Blocked::NotHealthy);
    }
    if h.penalty_until.is_some_and(|t| t > now) {
        return Err(Blocked::Penalized);
    }
    let s = match &h.snap {
        Some(s) => s,
        None => return Err(Blocked::NotHealthy),
    };
    let young = h.young.iter().filter(|t| now.duration_since(**t).as_secs_f64() < cfg.young_env_s).count() as f64;
    if h.inflight + h.warm_inflight >= cfg.max_inflight_per_host {
        return Err(Blocked::Inflight);
    }
    let pending = (h.inflight + h.warm_inflight) as f64;
    let live = ((h.owned + h.warm.len() as u32) as f64).max(s.live_slots as f64) + pending;
    let max = h.spec.max_envs as f64;
    if live + 1.0 > max {
        return Err(Blocked::Slots);
    }
    if s.psi_mem > 10.0 {
        return Err(Blocked::MemoryPressure);
    }
    let est = h.env_mib_est.max(64.0);
    let floor = s.mem_total_mib * cfg.mem_floor_pct / 100.0;
    let free = s.mem_avail_mib - (pending + young) * est - floor;
    if free < est {
        return Err(Blocked::Memory);
    }
    let cpus = h.spec.cpus.max(1) as f64;
    let cpu_frac = s.load1 / (cpus * cfg.max_load_per_cpu);
    if cpu_frac >= 1.0 {
        return Err(Blocked::Cpu);
    }
    let usable = (s.mem_total_mib - floor).max(1.0);
    let mem_frac = 1.0 - (free / usable).clamp(0.0, 1.0);
    let slot_frac = live / max;
    Ok(1.0 - mem_frac.max(slot_frac).max(cpu_frac))
}

impl Pool {
    pub fn new(cfg: Config, token: String, admin_token: String) -> P {
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(4096)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_nodelay(true)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("http client");
        let boot = format!("{:04x}", (now_epoch() * 1000.0) as u64 & 0xffff);
        let events = cfg.event_log.as_ref().and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok());
        let p = Arc::new(Pool {
            cfg,
            token,
            admin_token,
            boot,
            hosts: Mutex::new(HashMap::new()),
            envs: Mutex::new(HashMap::new()),
            states: Mutex::new(HashMap::new()),
            tombstones: Mutex::new((HashMap::new(), VecDeque::new())),
            queue: Queue { tickets: Mutex::new(VecDeque::new()), next: AtomicU64::new(1), notify: Notify::new() },
            http,
            metrics: Metrics::default(),
            seq: AtomicU64::new(1),
            started: Instant::now(),
            events: Mutex::new(events),
        });
        describe(&p.metrics);
        p
    }

    pub fn event(&self, mut ev: Value) {
        ev["t"] = json!(now_epoch());
        if let Some(f) = self.events.lock().unwrap().as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "{ev}");
        }
        tracing::info!("{ev}");
    }

    /// A host slot record as clients see it: pool ids, no host-local url, the host and region named.
    pub fn pool_view(&self, v: &mut Value, pool_id: &str, host: &str, slot: &str) {
        if !v.is_object() {
            return;
        }
        if v["id"].as_str() == Some(slot) {
            v["id"] = json!(pool_id);
        }
        if let Some(ck) = v["checkpoint"].as_str() {
            if let Some(pid) = self.states.lock().unwrap().values().find(|s| s.host == host && s.host_id == ck).map(|s| s.id.clone()) {
                v["checkpoint"] = json!(pid);
            }
        }
        if let Some(o) = v.as_object_mut() {
            o.remove("url");
        }
        let region = self.hosts.lock().unwrap().get(host).map(|h| h.spec.region.clone()).unwrap_or_default();
        v["pool"] = json!({"host": host, "region": region});
    }

    pub fn new_env_id(&self) -> String {
        format!("pe-{}-{:07}", self.boot, self.seq.fetch_add(1, Ordering::SeqCst))
    }
    pub fn new_state_id(&self) -> String {
        format!("ps-{}-{:07}", self.boot, self.seq.fetch_add(1, Ordering::SeqCst))
    }

    // ---------------------------------------------------------------------------------------------- hosts
    pub fn add_host(&self, spec: HostSpec) -> Result<(), String> {
        let token = spec.token()?;
        if spec.name.is_empty() || !spec.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err("host name: [A-Za-z0-9_-]+".into());
        }
        let mut hs = self.hosts.lock().unwrap();
        if let Some(h) = hs.get(&spec.name) {
            if h.state != HostState::Dead && h.state != HostState::Draining {
                return Err(format!("host {} is already registered ({})", spec.name, h.state.name()));
            }
        }
        let (tx, _) = watch::channel(0u64);
        let sweep = spec.reclaim_orphans;
        let name = spec.name.clone();
        hs.insert(
            name.clone(),
            Host {
                env_mib_est: self.cfg.env_mib_default,
                spec,
                token,
                state: HostState::Joining,
                fails: 0,
                last_ok: None,
                registered: Instant::now(),
                snap: None,
                inflight: 0,
                warm_inflight: 0,
                owned: 0,
                warm: vec![],
                young: VecDeque::new(),
                penalty_until: None,
                base_used_mib: None,
                dead_tx: tx,
                dead_gen: 0,
                needs_sweep: sweep,
                sweeping: false,
            },
        );
        drop(hs);
        self.event(json!({"ev": "host_registered", "host": name}));
        Ok(())
    }

    pub fn host_conn(&self, name: &str) -> Option<(String, String, watch::Receiver<u64>, u64)> {
        let hs = self.hosts.lock().unwrap();
        hs.get(name).map(|h| (h.spec.url.clone(), h.token.clone(), h.dead_tx.subscribe(), h.dead_gen))
    }

    /// Placement for one new env made from `image` (None: a saved state, placed on its own host by the caller).
    /// Prefers a warm slot of that image on the emptiest host that has one; otherwise the host with the most headroom.
    pub fn place(&self, image: Option<&str>) -> Option<Placement> {
        let now = Instant::now();
        let mut hs = self.hosts.lock().unwrap();
        let mut best: Option<(f64, String)> = None;
        let mut best_warm: Option<(f64, String)> = None;
        let mut blocked: HashMap<&'static str, u32> = HashMap::new();
        for (n, h) in hs.iter() {
            let hr = headroom(&self.cfg, h, now);
            let warm_ok = image == Some(self.cfg.warm_image.as_str()) && !h.warm.is_empty() && h.state == HostState::Healthy;
            if warm_ok {
                // a warm slot is already counted against the host: taking it needs no new headroom
                let score = hr.unwrap_or(0.0);
                if best_warm.as_ref().is_none_or(|(s, _)| score > *s) {
                    best_warm = Some((score, n.clone()));
                }
            }
            match hr {
                Ok(score) => {
                    if best.as_ref().is_none_or(|(s, _)| score > *s) {
                        best = Some((score, n.clone()));
                    }
                }
                Err(b) => *blocked.entry(b.name()).or_default() += 1,
            }
        }
        if let Some((_, n)) = best_warm {
            let h = hs.get_mut(&n).unwrap();
            let slot = h.warm.pop().unwrap();
            h.inflight += 1;
            return Some(Placement::Warm(n, slot));
        }
        if let Some((_, n)) = best {
            hs.get_mut(&n).unwrap().inflight += 1;
            return Some(Placement::Host(n));
        }
        drop(hs);
        for (r, c) in blocked {
            self.metrics.add("pool_place_blocked_total", &[("reason", r)], c as f64);
        }
        None
    }

    /// A saved state's own host, if it can take one more env.
    pub fn place_on(&self, host: &str) -> Result<(), Option<Blocked>> {
        let now = Instant::now();
        let mut hs = self.hosts.lock().unwrap();
        let h = hs.get_mut(host).ok_or(None)?;
        if h.state == HostState::Dead {
            return Err(None);
        }
        match headroom(&self.cfg, h, now) {
            Ok(_) => {
                h.inflight += 1;
                Ok(())
            }
            Err(b) => Err(Some(b)),
        }
    }

    /// A placed create ended (`ok`: an env now exists there and the pool owns it).
    pub fn placed_done(&self, host: &str, ok: bool) {
        let mut hs = self.hosts.lock().unwrap();
        if let Some(h) = hs.get_mut(host) {
            h.inflight = h.inflight.saturating_sub(1);
            if ok {
                h.owned += 1;
                h.young.push_back(Instant::now());
                while h.young.len() > 512 {
                    h.young.pop_front();
                }
            }
        }
        drop(hs);
        self.queue.notify.notify_waiters();
    }

    pub fn penalize(&self, host: &str) {
        if let Some(h) = self.hosts.lock().unwrap().get_mut(host) {
            h.penalty_until = Some(Instant::now() + Duration::from_secs_f64(self.cfg.refuse_penalty_s));
        }
    }

    pub fn disown(&self, host: &str) {
        if let Some(h) = self.hosts.lock().unwrap().get_mut(host) {
            h.owned = h.owned.saturating_sub(1);
        }
        self.queue.notify.notify_waiters();
    }

    // ---------------------------------------------------------------------------------------------- tombstones
    pub fn bury(&self, id: &str, reason: &str) {
        let mut t = self.tombstones.lock().unwrap();
        if t.0.insert(id.to_string(), reason.to_string()).is_none() {
            t.1.push_back(id.to_string());
        }
        while t.1.len() > 200_000 {
            if let Some(o) = t.1.pop_front() {
                t.0.remove(&o);
            }
        }
    }
    pub fn grave(&self, id: &str) -> Option<String> {
        self.tombstones.lock().unwrap().0.get(id).cloned()
    }

    // ---------------------------------------------------------------------------------------------- upstream
    /// One request to a host. Gives up at once if the host is declared dead meanwhile (Err "host_dead").
    pub async fn upstream(&self, host: &str, method: reqwest::Method, path: &str, body: Option<&Value>, timeout_s: f64) -> Result<(u16, Vec<u8>, String), String> {
        let (url, token, mut dead, gen) = self.host_conn(host).ok_or_else(|| "host_unknown".to_string())?;
        if *dead.borrow() != gen {
            return Err("host_dead".into());
        }
        let mut rb = self.http.request(method, format!("{url}{path}")).bearer_auth(token).timeout(Duration::from_secs_f64(timeout_s));
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let fut = async {
            let r = rb.send().await.map_err(|e| format!("upstream: {e}"))?;
            let st = r.status().as_u16();
            let ct = r.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            let b = r.bytes().await.map_err(|e| format!("upstream body: {e}"))?;
            Ok::<_, String>((st, b.to_vec(), ct))
        };
        tokio::select! {
            r = fut => r,
            _ = async { loop { if dead.changed().await.is_err() { std::future::pending::<()>().await; } if *dead.borrow() != gen { break; } } } => Err("host_dead".into()),
        }
    }

    // ---------------------------------------------------------------------------------------------- health loop
    pub async fn poll_host(self: &P, name: String) {
        let t0 = Instant::now();
        let r = match self.host_conn(&name) {
            Some((url, token, _, _)) => {
                let fut = self.http.get(format!("{url}/v1/health")).bearer_auth(token).timeout(Duration::from_secs_f64(self.cfg.health_timeout_s)).send();
                match fut.await {
                    Ok(resp) if resp.status().is_success() => resp.json::<Value>().await.map_err(|e| e.to_string()),
                    Ok(resp) => Err(format!("status {}", resp.status())),
                    Err(e) => Err(e.to_string()),
                }
            }
            None => return,
        };
        let rtt = t0.elapsed().as_secs_f64();
        self.metrics.observe("pool_health_poll_seconds", &[("host", &name)], rtt);
        let mut became_dead = false;
        let mut became_healthy_after_dead = false;
        {
            let mut hs = self.hosts.lock().unwrap();
            let Some(h) = hs.get_mut(&name) else { return };
            match r {
                Ok(v) => {
                    let ad = &v["admission"]["host"];
                    let total = ad["mem_total_mib"].as_f64().unwrap_or(0.0);
                    let avail = ad["mem_available_mib"].as_f64().or_else(|| v["host_mem_available_bytes"].as_f64().map(|b| b / 1048576.0)).unwrap_or(0.0);
                    let load1 = v["loadavg"].as_str().and_then(|s| s.split_whitespace().next()).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);
                    let live = v["slots_by_state"].as_object().map(|m| m.iter().filter(|(k, _)| k.as_str() != "destroyed").map(|(_, n)| n.as_u64().unwrap_or(0)).sum::<u64>()).unwrap_or(0) as u32;
                    let refusals = v["admission"]["counts"].as_object().map(|m| m.values().filter_map(|x| x.as_f64()).sum::<f64>()).unwrap_or(0.0);
                    let snap = Snap { mem_total_mib: total, mem_avail_mib: avail, psi_mem: ad["memory_psi_some_avg10"].as_f64().unwrap_or(0.0), load1, live_slots: live, health_ok: v["ok"].as_bool().unwrap_or(false), rtt_s: rtt, admission_refusals: refusals, raw: v.clone() };
                    if live == 0 && total > 0.0 {
                        h.base_used_mib = Some(total - avail);
                    }
                    if let Some(base) = h.base_used_mib {
                        if live >= 8 {
                            let per = ((total - avail - base) / live as f64).clamp(128.0, 8192.0);
                            h.env_mib_est = 0.8 * h.env_mib_est + 0.2 * per;
                        }
                    }
                    h.snap = Some(snap);
                    h.fails = 0;
                    h.last_ok = Some(Instant::now());
                    match h.state {
                        HostState::Joining | HostState::Dead if h.needs_sweep => {
                            if !h.sweeping {
                                h.sweeping = true;
                                h.state = HostState::Joining;
                                became_healthy_after_dead = true;
                            }
                        }
                        HostState::Joining | HostState::Suspect | HostState::Dead => h.state = HostState::Healthy,
                        _ => {}
                    }
                }
                Err(e) => {
                    h.fails += 1;
                    let since = h.last_ok.unwrap_or(h.registered);
                    let down_s = since.elapsed().as_secs_f64();
                    if h.state == HostState::Healthy && h.fails >= self.cfg.suspect_after {
                        h.state = HostState::Suspect;
                        drop(hs);
                        self.event(json!({"ev": "host_suspect", "host": name, "fails": self.cfg.suspect_after, "error": e}));
                        hs = self.hosts.lock().unwrap();
                    }
                    let h = hs.get_mut(&name).unwrap();
                    if matches!(h.state, HostState::Suspect | HostState::Healthy | HostState::Draining | HostState::Joining) && down_s >= self.cfg.dead_after_s && h.last_ok.is_some() {
                        h.state = HostState::Dead;
                        h.dead_gen += 1;
                        let g = h.dead_gen;
                        h.dead_tx.send_replace(g);
                        h.inflight = 0;
                        h.warm_inflight = 0;
                        h.owned = 0;
                        h.warm.clear();
                        h.needs_sweep = true;
                        became_dead = true;
                    }
                }
            }
        }
        if became_dead {
            self.metrics.inc("pool_host_dead_total", &[("host", &name)]);
            self.event(json!({"ev": "host_dead", "host": name, "dead_after_s": self.cfg.dead_after_s}));
            let me = self.clone();
            tokio::spawn(async move { me.fail_over(name).await });
        } else if became_healthy_after_dead {
            let me = self.clone();
            tokio::spawn(async move { me.rejoin(name).await });
        }
        self.queue.notify.notify_waiters();
    }

    /// A host that was dead (or asked for it at join) answers: delete every live slot on it the pool does not own,
    /// then let it take envs again.
    pub async fn rejoin(self: P, name: String) {
        // envs still bound to this host and live (a Replacing env's old slot here is an orphan)
        let owned: std::collections::HashSet<String> = self.envs.lock().unwrap().values().filter(|e| e.host == name && matches!(e.status, EnvStatus::Live | EnvStatus::NeedsReset { .. })).map(|e| e.slot.clone()).collect();
        let mut deleted = 0;
        let mut failed = 0;
        let listed = self.upstream(&name, reqwest::Method::GET, "/v1/slots", None, 30.0).await;
        let list: Vec<Value> = match listed {
            Ok((200, b, _)) => match serde_json::from_slice::<Value>(&b) {
                Ok(Value::Array(a)) => a,
                Ok(v) => v["items"].as_array().cloned().unwrap_or_default(),
                _ => vec![],
            },
            other => {
                // could not list: stay out of placement and try again at a later good poll
                if let Some(h) = self.hosts.lock().unwrap().get_mut(&name) {
                    h.sweeping = false;
                    h.state = HostState::Dead;
                }
                self.event(json!({"ev": "host_sweep_failed", "host": name, "error": format!("{:?}", other.map(|x| x.0))}));
                return;
            }
        };
        for r in list {
            let id = r["id"].as_str().unwrap_or("").to_string();
            if id.is_empty() || owned.contains(&id) || r["state"] == "destroyed" {
                continue;
            }
            match self.upstream(&name, reqwest::Method::DELETE, &format!("/v1/slots/{id}"), None, 60.0).await {
                Ok((s, _, _)) if s < 300 || s == 404 || s == 410 => deleted += 1,
                _ => failed += 1,
            }
        }
        self.metrics.add("pool_orphans_reclaimed_total", &[("host", &name)], deleted as f64);
        if let Some(h) = self.hosts.lock().unwrap().get_mut(&name) {
            h.needs_sweep = false;
            h.sweeping = false;
            h.state = HostState::Healthy;
            h.fails = 0;
        }
        self.event(json!({"ev": "host_rejoined", "host": name, "orphans_deleted": deleted, "orphans_failed": failed}));
        self.queue.notify.notify_waiters();
    }

    /// Every env of a dead host: replaced on another host (failover "replace", envs made from an image), or lost.
    pub async fn fail_over(self: P, dead: String) {
        let t0 = Instant::now();
        let victims: Vec<PoolEnv> = {
            let mut envs = self.envs.lock().unwrap();
            let mut v = vec![];
            for e in envs.values_mut().filter(|e| e.host == dead) {
                let replace = self.cfg.failover == "replace" && !e.checkpoint.starts_with("ps-");
                e.status = if replace { EnvStatus::Replacing { from_host: dead.clone(), since: now_epoch() } } else { EnvStatus::Lost { reason: format!("host {dead} died") } };
                if replace {
                    v.push(e.clone());
                } else {
                    self.metrics.inc("pool_episodes_lost_total", &[("host", &dead), ("why", "not_replaceable")]);
                }
            }
            v
        };
        {
            let mut st = self.states.lock().unwrap();
            for s in st.values_mut().filter(|s| s.host == dead) {
                s.lost = true;
            }
        }
        self.metrics.add("pool_episodes_lost_total", &[("host", &dead), ("why", "host_dead")], victims.len() as f64);
        self.event(json!({"ev": "failover_start", "host": dead, "envs": victims.len()}));
        let mut tasks = tokio::task::JoinSet::new();
        for e in victims {
            let me = self.clone();
            let dead = dead.clone();
            tasks.spawn(async move {
                let t = Instant::now();
                let r = me.make_env(&e.checkpoint, None, e.task.clone(), e.setup, e.idle_timeout_s, e.ttl_s, true, me.cfg.queue_timeout_s).await.map(|(h, s, mut rec)| {
                    me.pool_view(&mut rec, &e.id, &h, &s);
                    (h, s, rec)
                });
                // decide under the lock, act (metrics, deletes) after it
                enum Act { Replaced, Surplus(String, String), Failed, Nothing }
                let act = {
                    let mut envs = me.envs.lock().unwrap();
                    match (r, envs.get_mut(&e.id)) {
                        (Ok((host, slot, rec)), Some(pe)) if matches!(pe.status, EnvStatus::Replacing { .. }) => {
                            pe.host = host;
                            pe.slot = slot;
                            pe.record = rec;
                            pe.status = EnvStatus::NeedsReset { from_host: dead.clone(), at: now_epoch() };
                            pe.failovers += 1;
                            pe.last_used = Instant::now();
                            Act::Replaced
                        }
                        // the client closed it meanwhile: the replacement is not needed
                        (Ok((host, slot, _)), _) => Act::Surplus(host, slot),
                        (Err(err), Some(pe)) => {
                            pe.status = EnvStatus::Lost { reason: format!("host {dead} died; replacement failed: {err:?}") };
                            Act::Failed
                        }
                        (Err(_), None) => Act::Nothing,
                    }
                };
                match act {
                    Act::Replaced => {
                        me.metrics.observe("pool_failover_replace_seconds", &[], t.elapsed().as_secs_f64());
                        me.metrics.inc("pool_failover_total", &[("outcome", "replaced")]);
                        true
                    }
                    Act::Surplus(host, slot) => {
                        let _ = me.upstream(&host, reqwest::Method::DELETE, &format!("/v1/slots/{slot}"), None, 60.0).await;
                        me.disown(&host);
                        false
                    }
                    Act::Failed => {
                        me.metrics.inc("pool_failover_total", &[("outcome", "failed")]);
                        false
                    }
                    Act::Nothing => false,
                }
            });
        }
        let mut ok = 0;
        let mut n = 0;
        while let Some(r) = tasks.join_next().await {
            n += 1;
            if r.unwrap_or(false) {
                ok += 1;
            }
        }
        self.event(json!({"ev": "failover_done", "host": dead, "envs": n, "replaced": ok, "seconds": t0.elapsed().as_secs_f64()}));
    }

    // ---------------------------------------------------------------------------------------------- create
    /// Make one env (wait for capacity in the queue first). Returns (host, slot id, host record).
    #[allow(clippy::too_many_arguments)]
    pub async fn make_env(self: &P, checkpoint: &str, host_ckpt: Option<(String, String)>, task: Option<Value>, setup: Option<bool>, idle: f64, ttl: f64, priority: bool, queue_timeout_s: f64) -> Result<(String, String, Value), MakeErr> {
        let deadline = Instant::now() + Duration::from_secs_f64(queue_timeout_s.max(0.0));
        let host_lease = |x: f64| if x > 0.0 { x + self.cfg.host_lease_margin_s } else { 0.0 };
        let mut attempts = 0;
        loop {
            attempts += 1;
            let q0 = Instant::now();
            let placement = match &host_ckpt {
                Some((h, _)) => self.wait_on_host(h, deadline).await.map(|_| Placement::Host(h.clone()))?,
                None => self.wait_in_queue(checkpoint, priority, deadline).await?,
            };
            self.metrics.observe("pool_queue_wait_seconds", &[], q0.elapsed().as_secs_f64());
            let (host, warm) = match placement {
                Placement::Host(h) => (h, None),
                Placement::Warm(h, s) => (h, Some(s)),
            };
            let t = Instant::now();
            let res = match &warm {
                Some(slot) => match &task {
                    Some(tk) if setup != Some(false) => self.upstream(&host, reqwest::Method::POST, &format!("/v1/slots/{slot}/task"), Some(&json!({"task": tk})), self.cfg.create_timeout_s).await,
                    _ => self.upstream(&host, reqwest::Method::GET, &format!("/v1/slots/{slot}"), None, 30.0).await,
                },
                None => {
                    let hc = host_ckpt.as_ref().map(|(_, c)| c.clone()).unwrap_or_else(|| checkpoint.to_string());
                    let mut body = json!({"checkpoint": hc, "task": task, "idle_timeout_s": host_lease(idle), "ttl_s": host_lease(ttl)});
                    if let Some(s) = setup {
                        body["setup"] = json!(s);
                    }
                    self.upstream(&host, reqwest::Method::POST, "/v1/slots", Some(&body), self.cfg.create_timeout_s).await
                }
            };
            let kind = if warm.is_some() { "warm" } else { "cold" };
            self.metrics.observe("pool_upstream_seconds", &[("op", if warm.is_some() { "create_warm" } else { "create" })], t.elapsed().as_secs_f64());
            match res {
                Ok((s, b, _)) if s < 300 => {
                    let rec: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
                    let slot = rec["id"].as_str().unwrap_or("").to_string();
                    self.placed_done(&host, true);
                    self.metrics.inc("pool_create_total", &[("outcome", "ok"), ("kind", kind), ("host", &host)]);
                    return Ok((host, slot, rec));
                }
                Ok((s, b, ct)) if (s == 429 || s == 503) && Instant::now() < deadline => {
                    // the host refused (admission control, restarting): back off that host and place again
                    self.placed_done(&host, false);
                    if let Some(slot) = &warm {
                        self.return_warm(&host, slot);
                    }
                    self.penalize(&host);
                    self.metrics.inc("pool_host_refused_total", &[("host", &host), ("status", &s.to_string())]);
                    self.event(json!({"ev": "host_refused", "host": host, "status": s, "body": String::from_utf8_lossy(&b[..b.len().min(300)]), "ct": ct}));
                    continue;
                }
                Ok((s, _, _)) if s == 429 || s == 503 => {
                    // refused until the deadline: that is a queue timeout, not the host's refusal
                    self.placed_done(&host, false);
                    if let Some(slot) = &warm {
                        self.return_warm(&host, slot);
                    }
                    self.metrics.inc("pool_queue_timeout_total", &[]);
                    return Err(MakeErr::QueueTimeout);
                }
                Ok((s, b, ct)) => {
                    // a real answer that is not a success (task setup failed, gate failed...): the client sees it.
                    // For a cold create, drumsd keeps the failed env (down/quarantined) only for 4xx/5xx with a record;
                    // a warm slot whose task setup failed is quarantined: close it.
                    self.placed_done(&host, false);
                    if let Some(slot) = &warm {
                        let _ = self.upstream(&host, reqwest::Method::DELETE, &format!("/v1/slots/{slot}"), None, 60.0).await;
                    }
                    self.metrics.inc("pool_create_total", &[("outcome", "error"), ("kind", kind), ("host", &host)]);
                    return Err(MakeErr::Upstream(s, b, ct));
                }
                Err(e) => {
                    self.placed_done(&host, false);
                    self.metrics.inc("pool_create_total", &[("outcome", "unreachable"), ("kind", kind), ("host", &host)]);
                    if attempts < 6 && host_ckpt.is_none() {
                        self.penalize(&host);
                        continue;
                    }
                    return Err(MakeErr::Unavailable(format!("host {host}: {e}")));
                }
            }
        }
    }

    fn return_warm(&self, host: &str, slot: &str) {
        if let Some(h) = self.hosts.lock().unwrap().get_mut(host) {
            if h.state != HostState::Dead {
                h.warm.push(slot.to_string());
            }
        }
    }

    async fn wait_in_queue(&self, checkpoint: &str, priority: bool, deadline: Instant) -> Result<Placement, MakeErr> {
        let ticket = self.queue.next.fetch_add(1, Ordering::SeqCst);
        {
            let mut q = self.queue.tickets.lock().unwrap();
            if q.len() >= self.cfg.max_queue {
                return Err(MakeErr::QueueFull);
            }
            if priority {
                q.push_front(ticket);
            } else {
                q.push_back(ticket);
            }
            self.metrics.set("pool_queue_depth", &[], q.len() as f64);
        }
        let image = Some(checkpoint);
        loop {
            let notified = self.queue.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let head = self.queue.tickets.lock().unwrap().front().copied() == Some(ticket);
            if head {
                if let Some(p) = self.place(image) {
                    let mut q = self.queue.tickets.lock().unwrap();
                    q.retain(|t| *t != ticket);
                    self.metrics.set("pool_queue_depth", &[], q.len() as f64);
                    drop(q);
                    self.queue.notify.notify_waiters();
                    return Ok(p);
                }
            }
            if Instant::now() >= deadline {
                let mut q = self.queue.tickets.lock().unwrap();
                q.retain(|t| *t != ticket);
                self.metrics.set("pool_queue_depth", &[], q.len() as f64);
                drop(q);
                self.queue.notify.notify_waiters();
                self.metrics.inc("pool_queue_timeout_total", &[]);
                return Err(MakeErr::QueueTimeout);
            }
            let _ = tokio::time::timeout(Duration::from_millis(250), notified).await;
        }
    }

    async fn wait_on_host(&self, host: &str, deadline: Instant) -> Result<(), MakeErr> {
        loop {
            match self.place_on(host) {
                Ok(()) => return Ok(()),
                Err(None) => return Err(MakeErr::StateLost),
                Err(Some(_)) => {}
            }
            if Instant::now() >= deadline {
                self.metrics.inc("pool_queue_timeout_total", &[]);
                return Err(MakeErr::QueueTimeout);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    // ---------------------------------------------------------------------------------------------- background
    pub fn spawn_background(self: &P) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs_f64(me.cfg.health_interval_s));
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                iv.tick().await;
                let names: Vec<String> = me.hosts.lock().unwrap().keys().cloned().collect();
                let mut js = tokio::task::JoinSet::new();
                for n in names {
                    let m = me.clone();
                    js.spawn(async move { m.poll_host(n).await });
                }
                while js.join_next().await.is_some() {}
                me.update_gauges();
                me.remove_drained();
            }
        });
        let me = self.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs_f64(me.cfg.reap_interval_s));
            loop {
                iv.tick().await;
                me.clone().reap().await;
            }
        });
        if self.cfg.warm_per_host > 0 {
            let me = self.clone();
            tokio::spawn(async move {
                let mut iv = tokio::time::interval(Duration::from_secs(1));
                loop {
                    iv.tick().await;
                    me.clone().refill_warm();
                }
            });
        }
    }

    fn remove_drained(&self) {
        let busy: std::collections::HashSet<String> = self.envs.lock().unwrap().values().map(|e| e.host.clone()).collect();
        let mut hs = self.hosts.lock().unwrap();
        let gone: Vec<String> = hs.iter().filter(|(n, h)| h.state == HostState::Draining && h.inflight == 0 && h.warm.is_empty() && h.warm_inflight == 0 && !busy.contains(*n)).map(|(n, _)| n.clone()).collect();
        for n in &gone {
            hs.remove(n);
        }
        drop(hs);
        for n in gone {
            self.event(json!({"ev": "host_removed", "host": n}));
        }
    }

    /// Keep warm_per_host warm slots on every healthy host, only while no create is waiting (the queue has priority).
    fn refill_warm(self: P) {
        if !self.queue.tickets.lock().unwrap().is_empty() {
            return;
        }
        let now = Instant::now();
        let mut todo = vec![];
        {
            let mut hs = self.hosts.lock().unwrap();
            for (n, h) in hs.iter_mut() {
                let have = h.warm.len() as u32 + h.warm_inflight;
                if have < self.cfg.warm_per_host && h.warm_inflight < self.cfg.warm_concurrency_per_host && headroom(&self.cfg, h, now).is_ok() {
                    h.warm_inflight += 1;
                    todo.push(n.clone());
                }
            }
        }
        for host in todo {
            let me = self.clone();
            tokio::spawn(async move {
                let body = json!({"checkpoint": me.cfg.warm_image, "idle_timeout_s": 0, "ttl_s": 0});
                let r = me.upstream(&host, reqwest::Method::POST, "/v1/slots", Some(&body), me.cfg.create_timeout_s).await;
                let (outcome, to_delete) = {
                    let mut hs = me.hosts.lock().unwrap();
                    let Some(h) = hs.get_mut(&host) else { return };
                    h.warm_inflight = h.warm_inflight.saturating_sub(1);
                    match r {
                        Ok((s, b, _)) if s < 300 => {
                            let rec: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
                            let id = rec["id"].as_str().map(|x| x.to_string());
                            if rec["state"] == "ready" && h.state != HostState::Dead {
                                if let Some(id) = id {
                                    h.warm.push(id);
                                }
                                ("ok", None)
                            } else {
                                ("not_ready", id)
                            }
                        }
                        Ok((s, _, _)) => {
                            if s == 429 || s == 503 {
                                h.penalty_until = Some(Instant::now() + Duration::from_secs_f64(me.cfg.refuse_penalty_s));
                            }
                            ("refused", None)
                        }
                        Err(_) => ("unreachable", None),
                    }
                };
                me.metrics.inc("pool_warm_created_total", &[("host", &host), ("outcome", outcome)]);
                if let Some(id) = to_delete {
                    let _ = me.upstream(&host, reqwest::Method::DELETE, &format!("/v1/slots/{id}"), None, 60.0).await;
                }
                me.queue.notify.notify_waiters();
            });
        }
    }

    /// Lease reclaim: envs idle past their idle timeout or older than their ttl are closed on their host.
    async fn reap(self: P) {
        let now = Instant::now();
        let due: Vec<(String, String, String, &'static str)> = {
            let envs = self.envs.lock().unwrap();
            envs.values()
                .filter_map(|e| {
                    let idle = now.duration_since(e.last_used).as_secs_f64();
                    let age = now.duration_since(e.created).as_secs_f64();
                    if e.ttl_s > 0.0 && age > e.ttl_s {
                        Some((e.id.clone(), e.host.clone(), e.slot.clone(), "lease_ttl"))
                    } else if e.idle_timeout_s > 0.0 && idle > e.idle_timeout_s {
                        Some((e.id.clone(), e.host.clone(), e.slot.clone(), "lease_idle"))
                    } else {
                        None
                    }
                })
                .collect()
        };
        for (id, host, slot, why) in due {
            let st = self.envs.lock().unwrap().remove(&id).map(|e| e.status);
            self.bury(&id, why);
            self.metrics.inc("pool_lease_reclaimed_total", &[("why", why)]);
            self.event(json!({"ev": "lease_reclaimed", "env": id, "host": host, "why": why}));
            if matches!(st, Some(EnvStatus::Live) | Some(EnvStatus::NeedsReset { .. })) {
                self.disown(&host);
                let me = self.clone();
                tokio::spawn(async move {
                    let _ = me.upstream(&host, reqwest::Method::DELETE, &format!("/v1/slots/{slot}"), None, 120.0).await;
                });
            }
        }
    }

    pub fn update_gauges(&self) {
        let now = Instant::now();
        let m = &self.metrics;
        for g in ["pool_host_up", "pool_host_state", "pool_host_envs", "pool_host_warm", "pool_host_inflight", "pool_host_mem_total_bytes", "pool_host_mem_available_bytes", "pool_host_load1", "pool_host_cpus", "pool_host_saturation", "pool_host_env_mem_mib_estimate", "pool_host_slots_live", "pool_host_max_envs", "pool_host_headroom", "pool_host_health_rtt_seconds", "pool_host_admission_refusals"] {
            m.clear_gauge(g);
        }
        let mut by_state: HashMap<&'static str, u32> = HashMap::new();
        {
            let hs = self.hosts.lock().unwrap();
            for (n, h) in hs.iter() {
                *by_state.entry(h.state.name()).or_default() += 1;
                let r = h.spec.region.as_str();
                let l = [("host", n.as_str()), ("region", r)];
                m.set("pool_host_up", &l, if h.state == HostState::Healthy { 1.0 } else { 0.0 });
                m.set("pool_host_state", &[("host", n), ("region", r), ("state", h.state.name())], 1.0);
                m.set("pool_host_envs", &l, h.owned as f64);
                m.set("pool_host_warm", &l, h.warm.len() as f64);
                m.set("pool_host_inflight", &l, (h.inflight + h.warm_inflight) as f64);
                m.set("pool_host_cpus", &l, h.spec.cpus as f64);
                m.set("pool_host_max_envs", &l, h.spec.max_envs as f64);
                m.set("pool_host_env_mem_mib_estimate", &l, h.env_mib_est);
                if let Some(s) = &h.snap {
                    m.set("pool_host_mem_total_bytes", &l, s.mem_total_mib * 1048576.0);
                    m.set("pool_host_mem_available_bytes", &l, s.mem_avail_mib * 1048576.0);
                    m.set("pool_host_load1", &l, s.load1);
                    m.set("pool_host_slots_live", &l, s.live_slots as f64);
                    m.set("pool_host_health_rtt_seconds", &l, s.rtt_s);
                    m.set("pool_host_admission_refusals", &l, s.admission_refusals);
                    let floor = s.mem_total_mib * self.cfg.mem_floor_pct / 100.0;
                    let memsat = 1.0 - ((s.mem_avail_mib - floor) / (s.mem_total_mib - floor).max(1.0)).clamp(0.0, 1.0);
                    m.set("pool_host_saturation", &[("host", n), ("region", r), ("resource", "memory")], memsat);
                    m.set("pool_host_saturation", &[("host", n), ("region", r), ("resource", "cpu")], s.load1 / (h.spec.cpus.max(1) as f64 * self.cfg.max_load_per_cpu));
                    m.set("pool_host_saturation", &[("host", n), ("region", r), ("resource", "slots")], (h.owned as f64 + h.warm.len() as f64).max(s.live_slots as f64) / h.spec.max_envs.max(1) as f64);
                }
                m.set("pool_host_headroom", &l, headroom(&self.cfg, h, now).unwrap_or(0.0));
            }
        }
        m.clear_gauge("pool_hosts");
        for (s, c) in by_state {
            m.set("pool_hosts", &[("state", s)], c as f64);
        }
        m.clear_gauge("pool_envs");
        let mut es: HashMap<&'static str, u32> = HashMap::new();
        for e in self.envs.lock().unwrap().values() {
            let k = match e.status {
                EnvStatus::Live => "live",
                EnvStatus::Replacing { .. } => "replacing",
                EnvStatus::NeedsReset { .. } => "needs_reset",
                EnvStatus::Lost { .. } => "lost",
            };
            *es.entry(k).or_default() += 1;
        }
        for (k, c) in es {
            m.set("pool_envs", &[("status", k)], c as f64);
        }
        m.set("pool_queue_depth", &[], self.queue.tickets.lock().unwrap().len() as f64);
        m.set("pool_uptime_seconds", &[], self.started.elapsed().as_secs_f64());
    }
}

#[derive(Debug)]
pub enum MakeErr {
    QueueTimeout,
    QueueFull,
    StateLost,
    Unavailable(String),
    Upstream(u16, Vec<u8>, String),
}

fn describe(m: &Metrics) {
    for (n, h) in [
        ("pool_request_seconds", "Client request latency through the pool, by op (pool time including the host)"),
        ("pool_upstream_seconds", "Time spent waiting on the host, by op"),
        ("pool_overhead_seconds", "Pool's own time per request (request minus upstream), by op"),
        ("pool_queue_wait_seconds", "Time a create waited for capacity"),
        ("pool_queue_depth", "Creates waiting for capacity"),
        ("pool_create_total", "Creates by outcome and kind (warm slot or cold restore)"),
        ("pool_place_blocked_total", "Host placement attempts blocked, by binding constraint"),
        ("pool_host_saturation", "Fraction of a host's capacity in use, by resource"),
        ("pool_verify_total", "Rewards returned by verify, by validity"),
        ("pool_episodes_lost_total", "Episodes cut short by a host failure"),
        ("pool_failover_total", "Env failovers by outcome"),
        ("pool_failover_replace_seconds", "Time from host-dead to a ready replacement env"),
        ("pool_host_dead_total", "Hosts declared dead"),
        ("pool_lease_reclaimed_total", "Envs closed by the pool's lease reaper"),
        ("pool_health_poll_seconds", "Health poll round trip per host"),
    ] {
        m.describe(n, h);
    }
}
