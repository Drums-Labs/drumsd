//! Admission control: refuse work that would push the host into memory thrash.
//!
//! Scale run L256v3 (r8i.16xlarge, 496 GiB, no swap): with nothing refusing new envs, ~650 live trees plus ~175
//! saved states took MemAvailable under 7 GiB, memory PSI `full` above 90 %, load above 5000 and /v1/health over
//! 120 s; restores, gates and checkers then failed in bulk. Here every create, capture and fork is checked first
//! against the host's memory: what is available now, minus what admitted operations still in flight are expected
//! to take, minus what this one needs, must stay above a floor; memory pressure (PSI `some` avg10) and the state
//! tmpfs' free space must be below/above their limits. A refusal is an error with its reason (HTTP 429, code
//! `overloaded`); nothing is started.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AdmissionConfig {
    /// Off: everything is admitted (the old behaviour).
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// MemAvailable that must remain after the admitted work: this percent of MemTotal...
    #[serde(default = "d_floor_pct")]
    pub min_available_pct: f64,
    /// ...and at least this many MiB.
    #[serde(default = "d_floor_mib")]
    pub min_available_mib: u64,
    /// Expected memory of one new env (restored desktop, its upper layer, its share of shared pages).
    #[serde(default = "d_env_mib")]
    pub env_mib: u64,
    /// Expected memory of one capture (images and upper copy on the state tmpfs, its template server).
    #[serde(default = "d_capture_mib")]
    pub capture_mib: u64,
    /// Refuse while /proc/pressure/memory `some` avg10 is above this (percent of time stalled on memory).
    #[serde(default = "d_psi")]
    pub max_memory_psi_some_avg10: f64,
    /// Free space the state tmpfs must keep after the admitted work.
    #[serde(default = "d_state_floor")]
    pub min_state_free_mib: u64,
}

fn d_true() -> bool {
    true
}
fn d_floor_pct() -> f64 {
    8.0
}
fn d_floor_mib() -> u64 {
    4096
}
fn d_env_mib() -> u64 {
    1024
}
fn d_capture_mib() -> u64 {
    2048
}
fn d_psi() -> f64 {
    10.0
}
fn d_state_floor() -> u64 {
    8192
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        AdmissionConfig {
            enabled: true,
            min_available_pct: d_floor_pct(),
            min_available_mib: d_floor_mib(),
            env_mib: d_env_mib(),
            capture_mib: d_capture_mib(),
            max_memory_psi_some_avg10: d_psi(),
            min_state_free_mib: d_state_floor(),
        }
    }
}

/// What the host looks like right now.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HostMem {
    pub mem_total_mib: u64,
    pub mem_available_mib: u64,
    pub memory_psi_some_avg10: f64,
    pub state_free_mib: Option<u64>,
}

/// One request: how many new envs and captures it would start.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Need {
    pub envs: u64,
    pub captures: u64,
}

impl Need {
    pub fn mib(&self, c: &AdmissionConfig) -> u64 {
        self.envs * c.env_mib + self.captures * c.capture_mib
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Refusal {
    /// memory | memory_pressure | state_space
    pub reason: String,
    pub message: String,
    pub need_mib: u64,
    pub in_flight_mib: u64,
    pub host: HostMem,
    pub floor_mib: u64,
}

pub fn floor_mib(c: &AdmissionConfig, h: &HostMem) -> u64 {
    ((h.mem_total_mib as f64 * c.min_available_pct / 100.0) as u64).max(c.min_available_mib)
}

/// Admit `need` given `in_flight_mib` already promised to admitted work still running.
pub fn decide(c: &AdmissionConfig, h: &HostMem, in_flight_mib: u64, need: Need) -> Result<(), Refusal> {
    if !c.enabled || (need.envs == 0 && need.captures == 0) {
        return Ok(());
    }
    let need_mib = need.mib(c);
    let floor = floor_mib(c, h);
    let refuse = |reason: &str, message: String| Refusal { reason: reason.into(), message, need_mib, in_flight_mib, host: h.clone(), floor_mib: floor };
    if h.memory_psi_some_avg10 > c.max_memory_psi_some_avg10 {
        return Err(refuse("memory_pressure", format!("memory pressure {:.1} % (PSI some avg10) is above {:.1} %", h.memory_psi_some_avg10, c.max_memory_psi_some_avg10)));
    }
    let left = h.mem_available_mib as i64 - in_flight_mib as i64 - need_mib as i64;
    if left < floor as i64 {
        return Err(refuse("memory", format!("{} MiB available, {} MiB promised to running operations, {} MiB needed: {} MiB would remain, below the floor of {} MiB",
                                            h.mem_available_mib, in_flight_mib, need_mib, left, floor)));
    }
    if let Some(free) = h.state_free_mib {
        let state_need = need.captures * c.capture_mib + need.envs * 256;
        if (free as i64) - (state_need as i64) < c.min_state_free_mib as i64 {
            return Err(refuse("state_space", format!("state tmpfs has {free} MiB free, {state_need} MiB needed, floor {} MiB", c.min_state_free_mib)));
        }
    }
    Ok(())
}

/// Parse /proc/meminfo (MemTotal, MemAvailable) and /proc/pressure/memory (some avg10).
pub fn parse_host(meminfo: &str, psi_memory: &str) -> HostMem {
    let kb = |key: &str| meminfo.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let psi = psi_memory
        .lines()
        .find(|l| l.starts_with("some"))
        .and_then(|l| l.split_whitespace().find(|f| f.starts_with("avg10=")))
        .and_then(|f| f[6..].parse::<f64>().ok())
        .unwrap_or(0.0);
    HostMem { mem_total_mib: kb("MemTotal:") / 1024, mem_available_mib: kb("MemAvailable:") / 1024, memory_psi_some_avg10: psi, state_free_mib: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(avail_gib: u64, psi: f64) -> HostMem {
        HostMem { mem_total_mib: 496 * 1024, mem_available_mib: avail_gib * 1024, memory_psi_some_avg10: psi, state_free_mib: Some(200 * 1024) }
    }

    #[test]
    fn admits_with_headroom_and_refuses_near_the_floor() {
        let c = AdmissionConfig::default(); // floor 8 % of 496 GiB = ~39.7 GiB
        assert!(decide(&c, &host(300, 0.0), 0, Need { envs: 4, captures: 1 }).is_ok());
        let r = decide(&c, &host(42, 0.0), 0, Need { envs: 4, captures: 1 }).unwrap_err();
        assert_eq!(r.reason, "memory");
        assert_eq!(r.need_mib, 4 * 1024 + 2048);
    }

    #[test]
    fn work_in_flight_counts_against_what_is_available() {
        // 100 GiB available but 70 GiB already promised to running creates/forks: a fork(4) must wait
        let c = AdmissionConfig::default();
        assert!(decide(&c, &host(100, 0.0), 0, Need { envs: 4, captures: 1 }).is_ok());
        assert_eq!(decide(&c, &host(100, 0.0), 70 * 1024, Need { envs: 4, captures: 1 }).unwrap_err().reason, "memory");
    }

    #[test]
    fn memory_pressure_refuses_even_with_memory_available() {
        let c = AdmissionConfig::default();
        assert_eq!(decide(&c, &host(300, 35.0), 0, Need { envs: 1, captures: 0 }).unwrap_err().reason, "memory_pressure");
    }

    #[test]
    fn state_tmpfs_space_is_checked() {
        let c = AdmissionConfig::default();
        let mut h = host(300, 0.0);
        h.state_free_mib = Some(9 * 1024);
        assert_eq!(decide(&c, &h, 0, Need { envs: 0, captures: 1 }).unwrap_err().reason, "state_space");
    }

    #[test]
    fn disabled_or_empty_requests_are_admitted() {
        let mut c = AdmissionConfig::default();
        assert!(decide(&c, &host(1, 99.0), 0, Need::default()).is_ok());
        c.enabled = false;
        assert!(decide(&c, &host(1, 99.0), 0, Need { envs: 64, captures: 1 }).is_ok());
    }

    #[test]
    fn parses_proc_files() {
        let h = parse_host("MemTotal:       519850128 kB\nMemFree:  2585216 kB\nMemAvailable:    6875704 kB\n",
                           "some avg10=47.66 avg60=34.71 avg300=24.70 total=2933293261\nfull avg10=5.42 avg60=2.98 avg300=1.26 total=1\n");
        assert_eq!(h.mem_total_mib, 507666);
        assert_eq!(h.mem_available_mib, 6714);
        assert!((h.memory_psi_some_avg10 - 47.66).abs() < 1e-9);
    }
}
