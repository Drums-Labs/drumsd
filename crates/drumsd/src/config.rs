//! drumsd configuration (one JSON file).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn d_listen() -> String {
    "127.0.0.1:7070".into()
}
fn d_guest_port() -> u16 {
    5000
}
fn d_alive() -> f64 {
    5.0
}
fn d_conc() -> usize {
    4
}
fn d_attempts() -> u32 {
    3
}
fn d_range() -> (u32, u32) {
    (100, 2000)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "d_listen")]
    pub listen: String,
    /// The Python runtime's state dir (slot<N>/ directories live here; shared with the Python verbs).
    pub state_dir: PathBuf,
    /// drumsd's own journal (slot and checkpoint records, events.jsonl).
    pub journal_dir: PathBuf,
    /// Read-only lower layer (the attached OSWorld image).
    pub lower: PathBuf,
    pub image_sha256: String,
    /// Checkout whose `sharedos_rootfs/` the slots were made by (source hash, and the reused `capture` verb).
    pub py_src: PathBuf,
    /// SHAREDOS_ROOTFS_CONFIG for the reused Python verbs.
    pub py_config: PathBuf,
    pub python: Option<PathBuf>,
    /// Our CRIU fork binary (template servers, private restores, the capture verb's dump).
    pub criu: PathBuf,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// Swap per env (memory.swap.max), MiB. 0 (default) = none. Needs host swap to have any effect.
    #[serde(default)]
    pub swap_mib: u64,
    /// The per-env memory/CPU view mount (`drumsd procview`). Checkpoints whose mount table binds its files are
    /// restored with those binds re-made from here; health warns when it is not live.
    #[serde(default = "d_procview")]
    pub procview_dir: PathBuf,
    /// Guest-server port of slot N = port_base + N (as the Python runtime and the final-comparison harness).
    pub port_base: u16,
    #[serde(default = "d_guest_port")]
    pub guest_port: u16,
    /// Extra forwarders: (host base, guest port); slot N listens on base + N.
    #[serde(default)]
    pub extra_ports: Vec<(u16, u16)>,
    /// Slot numbers drumsd allocates from (inclusive start, exclusive end).
    #[serde(default = "d_range")]
    pub slot_range: (u32, u32),
    #[serde(default = "d_conc")]
    pub max_concurrent_restores: usize,
    /// comms that exit by themselves with no restore (measured by a no-restore control on this recipe).
    #[serde(default)]
    pub natural_exits: Vec<String>,
    #[serde(default = "d_alive")]
    pub alive_after_s: f64,
    #[serde(default = "d_attempts")]
    pub max_attempts: u32,
    /// Warm spare upper/work dirs (false = copy upper.snap on the critical path, for A/B measurement).
    #[serde(default = "yes")]
    pub warm_upper: bool,
    /// Two leaf cgroups per slot: a reset kills the old tree without waiting and restores into the other leaf.
    #[serde(default = "yes")]
    pub async_kill: bool,
    /// Labelled variant: `--empty-ns net` + lo brought up by the hook instead of CRIU's ip/iptables restore.
    #[serde(default)]
    pub netns_fast: bool,
    /// Labelled variant: lift cpu.max while CRIU restores (every restored task is stopped) and put it back in the
    /// pre-resume action, before any restored task runs. The cgroup gate proves the limits afterwards.
    #[serde(default)]
    pub restore_cpu_boost: bool,
    /// drums-criu app-compat build: name `--nested-pidns` on restore (dump side is the capture verb's), so sandboxed
    /// apps (Chrome, Electron) that nest pid/user/net namespaces restore.
    #[serde(default)]
    pub nested_pidns: bool,
    /// File holding the API bearer token (generated at install, mode 0600). Every route except `GET /healthz` needs
    /// `Authorization: Bearer <token>`. drumsd refuses to start without one.
    pub token_file: Option<PathBuf>,
    /// Allow a non-loopback `listen` address. Off by default: the API is bound to localhost unless this is set.
    #[serde(default)]
    pub allow_remote: bool,
    /// OSWorld task setup and checkers (the pinned OSWorld checkout run by an unprivileged helper).
    pub osw: Option<OswConfig>,
    /// Host price in USD per hour (on-demand or what the operator pays), for the cost metrics. 0 = cost not reported.
    #[serde(default)]
    pub host_usd_per_hour: f64,
    /// Capture readiness: before every capture (save, fork) the tree must be quiescent, and the dump must match it.
    #[serde(default)]
    pub quiesce: QuiesceConfig,
    /// Admission control (memory, memory pressure, state tmpfs space) for create, capture and fork.
    #[serde(default)]
    pub admission: drumsd_core::admission::AdmissionConfig,
    /// Where records of destroyed envs, deleted states and rejected dumps go (one .tar.zst each). Must be on disk,
    /// not the state tmpfs. Default: `<parent of journal_dir>/forgotten`.
    #[serde(default)]
    pub forgotten_dir: Option<PathBuf>,
    /// Cap over every kept record of both classes: None (default) sets no such cap; a number deletes the oldest
    /// records (summaries and full) beyond it. Setting it is an explicit cap, so it may delete failure records.
    #[serde(default)]
    pub forgotten_max_gib: Option<f64>,
    /// What is kept of a destroyed env, and the per-class caps (see the README, "Retention of destroyed-env records").
    #[serde(default)]
    pub retention: RetentionConfig,
    /// Health warns when `forgotten_dir` holds more than this (default 50 GiB).
    #[serde(default = "d_forgotten_warn")]
    pub forgotten_warn_gib: f64,
    /// Restores and captures are refused (a clear `state_dir_full` error, never a half-written upper layer) when
    /// the state dir has less free space than this (default 1024 MiB).
    #[serde(default = "d_state_min_free")]
    pub state_min_free_mib: u64,
    /// Health warns when the state dir's free space falls below this percentage (default 15).
    #[serde(default = "d_state_warn_pct")]
    pub state_warn_free_pct: f64,
    /// Leases: envs and saved states nobody uses are reaped (see `drumsd_core::lease`).
    #[serde(default)]
    pub lease: LeaseConfig,
}

/// `lease` in the config file: the defaults for envs and saved states (create, fork and save can override them).
/// `null` or 0 turns a limit off.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LeaseConfig {
    /// An env nobody used (no API request naming it) for this long is destroyed. Default 1800 s.
    #[serde(default = "d_env_idle")]
    pub env_idle_timeout_s: Option<f64>,
    /// An env is destroyed this long after it was created. Default none.
    #[serde(default)]
    pub env_ttl_s: Option<f64>,
    /// A saved state with no env made from it for this long is deleted. Default 1800 s.
    #[serde(default = "d_state_idle")]
    pub state_idle_timeout_s: Option<f64>,
    /// A saved state is deleted this long after it was made (once no env made from it exists). Default none.
    #[serde(default)]
    pub state_ttl_s: Option<f64>,
    /// How often the reaper looks (default 15 s).
    #[serde(default = "d_reap")]
    pub reap_interval_s: f64,
}

impl Default for LeaseConfig {
    fn default() -> Self {
        LeaseConfig { env_idle_timeout_s: d_env_idle(), env_ttl_s: None, state_idle_timeout_s: d_state_idle(), state_ttl_s: None, reap_interval_s: d_reap() }
    }
}

impl LeaseConfig {
    pub fn env(&self) -> drumsd_core::lease::Lease {
        drumsd_core::lease::Lease::resolve(None, None, drumsd_core::lease::Lease { idle_timeout_s: self.env_idle_timeout_s, ttl_s: self.env_ttl_s })
    }
    pub fn state(&self) -> drumsd_core::lease::Lease {
        drumsd_core::lease::Lease::resolve(None, None, drumsd_core::lease::Lease { idle_timeout_s: self.state_idle_timeout_s, ttl_s: self.state_ttl_s })
    }
}

fn d_env_idle() -> Option<f64> {
    Some(1800.0)
}
fn d_state_idle() -> Option<f64> {
    Some(1800.0)
}
fn d_reap() -> f64 {
    15.0
}

/// `retention` in the config file.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionConfig {
    /// `failures_full` (default): failed, invalid, quarantined and rejected envs keep a full archive, valid envs a
    /// small summary. `keep_all`: every env keeps a full archive.
    #[serde(default)]
    pub mode: drumsd_linux::forget::Mode,
    /// Summaries beyond this many GiB are deleted, oldest first (default 2).
    #[serde(default = "d_summary_max")]
    pub summary_max_gib: Option<f64>,
    /// Full records beyond this many GiB are deleted, oldest first. Default None: failure records are never pruned.
    #[serde(default)]
    pub full_max_gib: Option<f64>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        RetentionConfig { mode: Default::default(), summary_max_gib: d_summary_max(), full_max_gib: None }
    }
}

fn d_summary_max() -> Option<f64> {
    Some(2.0)
}
fn gib(g: f64) -> u64 {
    (g.max(0.0) * (1u64 << 30) as f64) as u64
}

impl Config {
    /// The caps the retention policy sets, in bytes.
    pub fn forget_caps(&self) -> drumsd_linux::forget::Caps {
        drumsd_linux::forget::Caps {
            summary: self.retention.summary_max_gib.map(gib),
            full: self.retention.full_max_gib.map(gib),
            total: self.forgotten_max_gib.map(gib),
        }
    }
}

fn d_procview() -> PathBuf {
    drumsd_linux::procview::MOUNTPOINT.into()
}
/// `retention` in the config file.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionConfig {
    /// `failures_full` (default): failed, invalid, quarantined and rejected envs keep a full archive, valid envs a
    /// small summary. `keep_all`: every env keeps a full archive.
    #[serde(default)]
    pub mode: drumsd_linux::forget::Mode,
    /// Summaries beyond this many GiB are deleted, oldest first (default 2).
    #[serde(default = "d_summary_max")]
    pub summary_max_gib: Option<f64>,
    /// Full records beyond this many GiB are deleted, oldest first. Default None: failure records are never pruned.
    #[serde(default)]
    pub full_max_gib: Option<f64>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        RetentionConfig { mode: Default::default(), summary_max_gib: d_summary_max(), full_max_gib: None }
    }
}

fn d_summary_max() -> Option<f64> {
    Some(2.0)
}
fn gib(g: f64) -> u64 {
    (g.max(0.0) * (1u64 << 30) as f64) as u64
}

impl Config {
    /// The caps the retention policy sets, in bytes.
    pub fn forget_caps(&self) -> drumsd_linux::forget::Caps {
        drumsd_linux::forget::Caps {
            summary: self.retention.summary_max_gib.map(gib),
            full: self.retention.full_max_gib.map(gib),
            total: self.forgotten_max_gib.map(gib),
        }
    }
}

fn d_forgotten_warn() -> f64 {
    50.0
}
fn d_state_min_free() -> u64 {
    1024
}
fn d_state_warn_pct() -> f64 {
    15.0
}

impl Config {
    pub fn forgotten_dir(&self) -> PathBuf {
        self.forgotten_dir.clone().unwrap_or_else(|| self.journal_dir.parent().map(|p| p.join("forgotten")).unwrap_or_else(|| self.journal_dir.join("forgotten")))
    }
}

/// Capture readiness (see `drumsd_core::quiesce`). A capture waits until the slot's tree shows no task leaving and
/// no process or thread starting or exiting for `window_s`, dumps it, and rejects the dump if CRIU's seized tree
/// differs from the quiescent sample or a dumped process exits in the source right after; it then waits and dumps
/// again, at most `max_attempts` dumps within `timeout_s`. Otherwise the capture fails with `not_quiescent`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct QuiesceConfig {
    #[serde(default = "d_q_window")]
    pub window_s: f64,
    #[serde(default = "d_q_timeout")]
    pub timeout_s: f64,
    #[serde(default = "d_q_poll")]
    pub poll_ms: u64,
    #[serde(default = "d_q_attempts")]
    pub max_attempts: u32,
    /// Optional: hold the new checkpoint until this many seconds after the dump started before the source check
    /// (catches a process that sits idle and then exits within that time; costs up to that much per capture). 0 = the
    /// check runs as soon as the checkpoint is adopted.
    #[serde(default)]
    pub confirm_s: f64,
}

impl Default for QuiesceConfig {
    fn default() -> Self {
        QuiesceConfig { window_s: d_q_window(), timeout_s: d_q_timeout(), poll_ms: d_q_poll(), max_attempts: d_q_attempts(), confirm_s: 0.0 }
    }
}

fn d_q_window() -> f64 {
    1.0
}
fn d_q_timeout() -> f64 {
    30.0
}
fn d_q_poll() -> u64 {
    50
}
fn d_q_attempts() -> u32 {
    3
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OswConfig {
    /// Python of the OSWorld venv.
    pub python: PathBuf,
    /// drums_osw.py (setup / evaluate / stage).
    pub helper: PathBuf,
    /// OSWorld checkout at the pinned commit.
    pub osworld_dir: PathBuf,
    /// Shared download cache (OSWorld layout: <cache>/<task id>/...).
    pub cache_dir: PathBuf,
    /// The helper runs as this uid/gid (never root).
    pub uid: u32,
    pub gid: u32,
    #[serde(default = "d_setup_t")]
    pub setup_timeout_s: u64,
    #[serde(default = "d_eval_t")]
    pub eval_timeout_s: u64,
    /// sudo password of the image's user (OSWorld's `client_password`).
    #[serde(default = "d_pw")]
    pub client_password: String,
    /// Lab verifiers (`"func": "drums_verifier"` evaluators): NAME.py files owned by root (docs/custom-tasks.md).
    /// None = the helper's default, /etc/drums/verifiers.
    #[serde(default)]
    pub verifiers_dir: Option<PathBuf>,
}

fn d_setup_t() -> u64 {
    600
}
fn d_eval_t() -> u64 {
    600
}
fn d_pw() -> String {
    "password".into()
}

fn yes() -> bool {
    true
}

impl Config {
    pub fn load(p: &str) -> Result<Config, String> {
        let b = std::fs::read(p).map_err(|e| format!("{p}: {e}"))?;
        serde_json::from_slice(&b).map_err(|e| format!("{p}: {e}"))
    }

    /// Refuse unsafe exposure: a non-loopback address needs `allow_remote`, and a token is always required.
    pub fn check_listen(&self) -> Result<(), String> {
        let addr: std::net::SocketAddr = self.listen.parse().map_err(|e| format!("listen {:?}: {e}", self.listen))?;
        if !addr.ip().is_loopback() && !self.allow_remote {
            return Err(format!("listen {} is not a loopback address; set \"allow_remote\": true to expose the API (it is token-protected, not encrypted: use a tunnel or a private network)", self.listen));
        }
        Ok(())
    }

    /// The API token, read from `token_file` (whitespace trimmed). At least 32 characters.
    pub fn load_token(&self) -> Result<String, String> {
        let p = self.token_file.as_ref().ok_or("token_file is required (the API is never served without auth)")?;
        let t = std::fs::read_to_string(p).map_err(|e| format!("token_file {}: {e}", p.display()))?.trim().to_string();
        if t.len() < 32 {
            return Err(format!("token in {} is shorter than 32 characters", p.display()));
        }
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(listen: &str, remote: bool, token: Option<PathBuf>) -> Config {
        let mut v = serde_json::json!({"state_dir": "/s", "journal_dir": "/j", "lower": "/l", "image_sha256": "x", "py_src": "/p",
            "py_config": "/c", "criu": "/criu", "vcpus": 4, "memory_mib": 4096, "port_base": 25000, "listen": listen, "allow_remote": remote});
        if let Some(t) = token {
            v["token_file"] = serde_json::json!(t);
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn remote_listen_needs_the_flag() {
        assert!(cfg("127.0.0.1:7070", false, None).check_listen().is_ok());
        assert!(cfg("[::1]:7070", false, None).check_listen().is_ok());
        assert!(cfg("0.0.0.0:7070", false, None).check_listen().is_err());
        assert!(cfg("10.0.0.5:7070", false, None).check_listen().is_err());
        assert!(cfg("0.0.0.0:7070", true, None).check_listen().is_ok());
    }

    #[test]
    fn token_is_required_and_long() {
        assert!(cfg("127.0.0.1:1", false, None).load_token().is_err());
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t");
        std::fs::write(&p, "short\n").unwrap();
        assert!(cfg("127.0.0.1:1", false, Some(p.clone())).load_token().is_err());
        std::fs::write(&p, "abcdefghijklmnopqrstuvwxyz0123456789\n").unwrap();
        assert_eq!(cfg("127.0.0.1:1", false, Some(p)).load_token().unwrap(), "abcdefghijklmnopqrstuvwxyz0123456789");
    }

    #[test]
    fn old_configs_still_load() {
        let c = cfg("127.0.0.1:7070", false, None);
        assert!(!c.nested_pidns && c.osw.is_none() && c.host_usd_per_hour == 0.0);
        assert_eq!(c.quiesce, QuiesceConfig::default(), "an old config gets the readiness check with its defaults");
        assert_eq!(c.forgotten_dir(), PathBuf::from("/forgotten"), "records go next to the journal, not to the state dir");
        assert!(c.forgotten_max_gib.is_none(), "no cap over everything by default");
        assert_eq!(c.retention, RetentionConfig::default(), "an old config gets the default retention");
        assert_eq!(c.retention.mode, drumsd_linux::forget::Mode::FailuresFull);
        assert_eq!((c.retention.summary_max_gib, c.retention.full_max_gib), (Some(2.0), None));
        let caps = c.forget_caps();
        assert_eq!((caps.summary, caps.full, caps.total), (Some(2 << 30), None, None), "failure records are never pruned by default");
        assert_eq!(c.state_min_free_mib, 1024);
        assert_eq!(c.lease, LeaseConfig::default(), "an old config gets the default leases");
        assert_eq!(c.lease.env().idle_timeout_s, Some(1800.0));
        assert_eq!(c.lease.state().idle_timeout_s, Some(1800.0));
        assert_eq!((c.lease.env().ttl_s, c.lease.state().ttl_s), (None, None));
    }

    #[test]
    fn lease_config_parses_and_null_or_zero_is_off() {
        let mut v = serde_json::json!({"state_dir": "/s", "journal_dir": "/j", "lower": "/l", "image_sha256": "x", "py_src": "/p",
            "py_config": "/c", "criu": "/criu", "vcpus": 4, "memory_mib": 4096, "port_base": 25000, "listen": "127.0.0.1:1"});
        v["lease"] = serde_json::json!({"env_idle_timeout_s": null, "env_ttl_s": 7200, "state_idle_timeout_s": 0, "reap_interval_s": 5});
        let c: Config = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(c.lease.env().idle_timeout_s, None);
        assert_eq!(c.lease.env().ttl_s, Some(7200.0));
        assert_eq!(c.lease.state().idle_timeout_s, None);
        assert_eq!(c.lease.reap_interval_s, 5.0);
        v["lease"] = serde_json::json!({"env_idle_timeout": 5});
        assert!(serde_json::from_value::<Config>(v).is_err(), "a misspelt lease field is refused");
    }

    #[test]
    fn retention_and_the_old_cap_parse() {
        let mut v = serde_json::json!({"state_dir": "/s", "journal_dir": "/j", "lower": "/l", "image_sha256": "x", "py_src": "/p",
            "py_config": "/c", "criu": "/criu", "vcpus": 4, "memory_mib": 4096, "port_base": 25000, "listen": "127.0.0.1:1"});
        v["forgotten_max_gib"] = serde_json::json!(100.0);
        let c: Config = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(c.forget_caps().total, Some(100 << 30));
        v["retention"] = serde_json::json!({"mode": "keep_all", "summary_max_gib": null, "full_max_gib": 10});
        let c: Config = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(c.retention.mode, drumsd_linux::forget::Mode::KeepAll);
        let caps = c.forget_caps();
        assert_eq!((caps.summary, caps.full), (None, Some(10 << 30)));
        v["retention"] = serde_json::json!({"mode": "everything"});
        assert!(serde_json::from_value::<Config>(v.clone()).is_err(), "a misspelt mode is refused");
        v["retention"] = serde_json::json!({"mode": "keep_all"});
        let c: Config = serde_json::from_value(v).unwrap();
        assert_eq!(c.retention.summary_max_gib, Some(2.0), "unset fields keep their defaults");
    }

    #[test]
    fn retention_and_the_old_cap_parse() {
        let mut v = serde_json::json!({"state_dir": "/s", "journal_dir": "/j", "lower": "/l", "image_sha256": "x", "py_src": "/p",
            "py_config": "/c", "criu": "/criu", "vcpus": 4, "memory_mib": 4096, "port_base": 25000, "listen": "127.0.0.1:1"});
        v["forgotten_max_gib"] = serde_json::json!(100.0);
        let c: Config = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(c.forget_caps().total, Some(100 << 30));
        v["retention"] = serde_json::json!({"mode": "keep_all", "summary_max_gib": null, "full_max_gib": 10});
        let c: Config = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(c.retention.mode, drumsd_linux::forget::Mode::KeepAll);
        let caps = c.forget_caps();
        assert_eq!((caps.summary, caps.full), (None, Some(10 << 30)));
        v["retention"] = serde_json::json!({"mode": "everything"});
        assert!(serde_json::from_value::<Config>(v.clone()).is_err(), "a misspelt mode is refused");
        v["retention"] = serde_json::json!({"mode": "keep_all"});
        let c: Config = serde_json::from_value(v).unwrap();
        assert_eq!(c.retention.summary_max_gib, Some(2.0), "unset fields keep their defaults");
    }
}
