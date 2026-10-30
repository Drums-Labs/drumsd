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
    }
}
