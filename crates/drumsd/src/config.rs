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
}

fn yes() -> bool {
    true
}

impl Config {
    pub fn load(p: &str) -> Result<Config, String> {
        let b = std::fs::read(p).map_err(|e| format!("{p}: {e}"))?;
        serde_json::from_slice(&b).map_err(|e| format!("{p}: {e}"))
    }
}
