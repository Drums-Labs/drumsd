//! drums-pool configuration (one JSON file; every field optional except where noted).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostSpec {
    /// Unique short name (also the label in metrics), e.g. "use2-a".
    pub name: String,
    /// Base URL of the host's drumsd, e.g. "http://10.0.0.5:7070".
    pub url: String,
    /// The host's drumsd bearer token, inline or in a file (one of the two).
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    #[serde(default)]
    pub region: String,
    /// Host vCPUs (drumsd's /v1/health does not report the host's CPU count).
    pub cpus: u32,
    /// Most envs the pool places on this host, warm slots included.
    #[serde(default = "d_max_envs")]
    pub max_envs: u32,
    /// When joining: delete every live slot on the host that the pool does not own (default false: a host that already
    /// serves envs, such as an existing single-host install, keeps them; they count against its headroom).
    #[serde(default)]
    pub reclaim_orphans: bool,
}

fn d_max_envs() -> u32 {
    300
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: String,
    /// Bearer token clients (the SDK, the gateway) must send; required.
    pub token_file: PathBuf,
    /// Token for /pool/v1/* (host registration, drain). Defaults to token_file.
    pub admin_token_file: Option<PathBuf>,
    pub hosts: Vec<HostSpec>,
    /// Health poll period and per-poll timeout.
    pub health_interval_s: f64,
    pub health_timeout_s: f64,
    /// Consecutive failed polls before a host stops getting new envs (suspect).
    pub suspect_after: u32,
    /// Seconds without a good poll before a host is declared dead (its envs fail over).
    pub dead_after_s: f64,
    /// Warm slots kept ready per host, made from `warm_image`.
    pub warm_per_host: u32,
    pub warm_image: String,
    pub warm_concurrency_per_host: u32,
    /// A host gets no new env while this many creates (warm ones included) are already in flight on it: drumsd runs a
    /// few restores at a time (max_concurrent_restores), so more only queue there while load1 catches up.
    pub max_inflight_per_host: u32,
    /// Placement limits: a host gets no new env while load1/cpus is at or above this ...
    pub max_load_per_cpu: f64,
    /// ... or while MemAvailable minus expected memory of envs in flight would drop below this percent of RAM.
    pub mem_floor_pct: f64,
    /// Expected memory of one env before the pool has learned it from a host (MiB).
    pub env_mib_default: f64,
    /// Envs younger than this count as still growing (their memory is not yet in MemAvailable).
    pub young_env_s: f64,
    /// Backpressure: a create waits this long for capacity (or the X-Drums-Queue-Timeout header) before 503.
    pub queue_timeout_s: f64,
    pub max_queue: usize,
    /// A host that refused a create (429/503) gets no placement for this long.
    pub refuse_penalty_s: f64,
    /// Lease defaults (seconds; 0 = none) for envs created without idle_timeout_s / ttl_s.
    pub default_idle_timeout_s: f64,
    pub default_ttl_s: f64,
    /// Margin added to the lease the pool asks drumsd for, so the host reaps envs if the pool is gone.
    pub host_lease_margin_s: f64,
    pub reap_interval_s: f64,
    /// "replace": envs on a dead host are recreated elsewhere and their next request answers 409 episode_lost
    /// (reset continues on the new host); "gone": they answer 410 host_lost.
    pub failover: String,
    /// Upstream timeouts (seconds).
    pub create_timeout_s: f64,
    pub request_timeout_s: f64,
    /// JSON-lines event log (host transitions, placements refused, failovers, reclaims).
    pub event_log: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: "127.0.0.1:7080".into(),
            token_file: PathBuf::from("/etc/drums-pool/token"),
            admin_token_file: None,
            hosts: vec![],
            health_interval_s: 2.0,
            health_timeout_s: 3.0,
            suspect_after: 2,
            dead_after_s: 10.0,
            warm_per_host: 0,
            warm_image: "osworld-ubuntu".into(),
            warm_concurrency_per_host: 2,
            max_inflight_per_host: 16,
            max_load_per_cpu: 4.0,
            mem_floor_pct: 10.0,
            env_mib_default: 1024.0,
            young_env_s: 30.0,
            queue_timeout_s: 900.0,
            max_queue: 20000,
            refuse_penalty_s: 3.0,
            default_idle_timeout_s: 1800.0,
            default_ttl_s: 0.0,
            host_lease_margin_s: 600.0,
            reap_interval_s: 5.0,
            failover: "replace".into(),
            create_timeout_s: 900.0,
            request_timeout_s: 300.0,
            event_log: None,
        }
    }
}

impl HostSpec {
    pub fn token(&self) -> Result<String, String> {
        if let Some(t) = &self.token {
            return Ok(t.trim().to_string());
        }
        match &self.token_file {
            Some(p) => std::fs::read_to_string(p).map(|s| s.trim().to_string()).map_err(|e| format!("host {}: token_file {}: {e}", self.name, p.display())),
            None => Err(format!("host {}: token or token_file is required", self.name)),
        }
    }
}

pub fn read_token(p: &PathBuf) -> Result<String, String> {
    let t = std::fs::read_to_string(p).map_err(|e| format!("token file {}: {e}", p.display()))?;
    let t = t.trim().to_string();
    if t.len() < 16 {
        return Err(format!("token file {}: token shorter than 16 characters", p.display()));
    }
    Ok(t)
}
