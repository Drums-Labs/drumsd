//! Gates as first-class results.
//!
//! Every restore (create, reset, branch child) produces a [`GateReport`] for the episode it starts. An episode is
//! **valid** only when every gate in it passed; a pending gate (the alive gate resolves 5 s after the slot
//! answered) makes the verdict pending, never valid. A trainer must drop rollouts whose episode is not valid.
//!
//! Gate names are stable API strings:
//!
//! | gate | what it proves | when |
//! |---|---|---|
//! | `lock_applied` | the file locks CRIU applied (restore log) equal the ones it dumped (dump log + filelocks.img hash) | before resume |
//! | `lock_pre_resume` | the lock census taken by CRIU's pre-resume action equals the dumped set | before resume |
//! | `cgroup` | the tree is in the slot's own cgroup with the configured cpu/memory/swap limits | after restore |
//! | `init_identity` | exactly one init with the slot's tree marker, pid 1 of its own pid namespace, the restored pid | after restore |
//! | `mount_table` | the init's mount table equals the dumped one | after restore |
//! | `netns` | the init's network namespace equals the dumped one (links, addresses, routes) | after restore |
//! | `guest_ready` | the guest server answers `/platform` = `Linux` through the slot's forwarder | after restore |
//! | `state_hash` | the desktop state hash (Desktop listing + file hashes + home listing) equals the checkpoint's | after ready |
//! | `isolation` | pid/mnt/net/ipc/uts namespaces distinct from the host and every other live slot; no foreign process in the cgroup | after ready |
//! | `alive_after_resume` | every process restored (seen at pre-resume) is still there >= 5 s after the slot answered, natural exits excluded | ready + 5 s |
//! | `no_oom_kill` | added as **failed** when the OOM killer took a task of the slot during the episode (its cgroup's memory.events oom_kill grew since ready); never added as passed, so it is not in [`Gate::ALL`] | each step and the episode's end |
//! | `not_quarantined` | added as **failed** when the env was quarantined during the episode (by a client, a failed gate, a failed capture or task setup, or a daemon restart mid-capture); its detail names the reason. Like `no_oom_kill` it is never added as passed and is not in [`Gate::ALL`]. A quarantined env's state is not trusted, so its episode is invalid, and every validity signal (verdict, gates, `valid`, metrics) says so | when the env is quarantined |

use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    LockApplied,
    LockPreResume,
    Cgroup,
    InitIdentity,
    MountTable,
    Netns,
    GuestReady,
    StateHash,
    Isolation,
    AliveAfterResume,
    NoOomKill,
    NotQuarantined,
}

impl Gate {
    pub const ALL: [Gate; 10] = [
        Gate::LockApplied,
        Gate::LockPreResume,
        Gate::Cgroup,
        Gate::InitIdentity,
        Gate::MountTable,
        Gate::Netns,
        Gate::GuestReady,
        Gate::StateHash,
        Gate::Isolation,
        Gate::AliveAfterResume,
    ];
    pub fn as_str(&self) -> &'static str {
        match self {
            Gate::LockApplied => "lock_applied",
            Gate::LockPreResume => "lock_pre_resume",
            Gate::Cgroup => "cgroup",
            Gate::InitIdentity => "init_identity",
            Gate::MountTable => "mount_table",
            Gate::Netns => "netns",
            Gate::GuestReady => "guest_ready",
            Gate::StateHash => "state_hash",
            Gate::Isolation => "isolation",
            Gate::AliveAfterResume => "alive_after_resume",
            Gate::NoOomKill => "no_oom_kill",
            Gate::NotQuarantined => "not_quarantined",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Pass,
    Fail,
    Pending,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateResult {
    pub gate: Gate,
    pub status: GateStatus,
    /// Seconds the check itself took.
    pub seconds: f64,
    /// Machine-readable evidence (expected/got, missing processes, ...).
    pub detail: serde_json::Value,
}

impl GateResult {
    pub fn pass(gate: Gate, seconds: f64, detail: serde_json::Value) -> Self {
        GateResult { gate, status: GateStatus::Pass, seconds, detail }
    }
    pub fn fail(gate: Gate, seconds: f64, detail: serde_json::Value) -> Self {
        GateResult { gate, status: GateStatus::Fail, seconds, detail }
    }
    pub fn pending(gate: Gate) -> Self {
        GateResult { gate, status: GateStatus::Pending, seconds: 0.0, detail: serde_json::Value::Null }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Validity {
    Valid,
    Pending { gates: Vec<Gate> },
    Invalid { gates: Vec<Gate> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct GateReport {
    pub gates: Vec<GateResult>,
}

impl GateReport {
    pub fn push(&mut self, r: GateResult) {
        self.gates.retain(|g| g.gate != r.gate);
        self.gates.push(r);
    }

    pub fn get(&self, gate: Gate) -> Option<&GateResult> {
        self.gates.iter().find(|g| g.gate == gate)
    }

    /// The first failed gate, in check order.
    pub fn first_failure(&self) -> Option<&GateResult> {
        self.gates.iter().find(|g| g.status == GateStatus::Fail)
    }

    /// Valid only when every required gate is present and passed. A missing required gate counts as failed:
    /// a report can never be valid by omission.
    pub fn validity(&self, required: &[Gate]) -> Validity {
        let mut failed = vec![];
        let mut pending = vec![];
        for g in required {
            match self.get(*g).map(|r| &r.status) {
                Some(GateStatus::Pass) => {}
                Some(GateStatus::Pending) => pending.push(*g),
                Some(GateStatus::Fail) | None => failed.push(*g),
            }
        }
        for r in &self.gates {
            if r.status == GateStatus::Fail && !failed.contains(&r.gate) {
                failed.push(r.gate);
            }
        }
        if !failed.is_empty() {
            Validity::Invalid { gates: failed }
        } else if !pending.is_empty() {
            Validity::Pending { gates: pending }
        } else {
            Validity::Valid
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_only_when_all_required_pass() {
        let mut r = GateReport::default();
        for g in Gate::ALL {
            r.push(GateResult::pass(g, 0.01, json!({})));
        }
        assert_eq!(r.validity(&Gate::ALL), Validity::Valid);
        r.push(GateResult::pending(Gate::AliveAfterResume));
        assert_eq!(r.validity(&Gate::ALL), Validity::Pending { gates: vec![Gate::AliveAfterResume] });
        r.push(GateResult::fail(Gate::StateHash, 0.1, json!({"expected": "a", "got": "b"})));
        assert_eq!(r.validity(&Gate::ALL), Validity::Invalid { gates: vec![Gate::StateHash] });
    }

    #[test]
    fn missing_gate_is_never_valid() {
        let mut r = GateReport::default();
        r.push(GateResult::pass(Gate::StateHash, 0.0, json!(null)));
        assert_eq!(r.validity(&[Gate::StateHash, Gate::Isolation]), Validity::Invalid { gates: vec![Gate::Isolation] });
    }

    #[test]
    fn a_failed_gate_outside_the_required_set_still_invalidates() {
        let mut r = GateReport::default();
        r.push(GateResult::pass(Gate::StateHash, 0.0, json!(null)));
        r.push(GateResult::fail(Gate::Netns, 0.0, json!(null)));
        assert_eq!(r.validity(&[Gate::StateHash]), Validity::Invalid { gates: vec![Gate::Netns] });
    }

    #[test]
    fn push_replaces_a_pending_result() {
        let mut r = GateReport::default();
        r.push(GateResult::pending(Gate::AliveAfterResume));
        r.push(GateResult::pass(Gate::AliveAfterResume, 5.0, json!({})));
        assert_eq!(r.gates.len(), 1);
        assert_eq!(r.gates[0].status, GateStatus::Pass);
    }

    #[test]
    fn an_oom_kill_in_the_episode_invalidates_a_fully_passed_report() {
        let mut r = GateReport::default();
        for g in Gate::ALL {
            r.push(GateResult::pass(g, 0.01, json!({})));
        }
        assert_eq!(r.validity(&Gate::ALL), Validity::Valid);
        r.push(GateResult::fail(Gate::NoOomKill, 0.0, json!({"oom_kills": 1})));
        assert_eq!(r.validity(&Gate::ALL), Validity::Invalid { gates: vec![Gate::NoOomKill] });
        assert_eq!(serde_json::to_value(Gate::NoOomKill).unwrap(), json!("no_oom_kill"));
    }

    #[test]
    fn a_quarantine_invalidates_a_fully_passed_report_and_is_named() {
        let mut r = GateReport::default();
        for g in Gate::ALL {
            r.push(GateResult::pass(g, 0.01, json!({})));
        }
        r.push(GateResult::fail(Gate::NotQuarantined, 0.0, json!({"reason": "client"})));
        assert_eq!(r.validity(&Gate::ALL), Validity::Invalid { gates: vec![Gate::NotQuarantined] });
        assert_eq!(serde_json::to_value(Gate::NotQuarantined).unwrap(), json!("not_quarantined"));
        assert_eq!(Gate::NotQuarantined.as_str(), "not_quarantined");
        assert!(!Gate::ALL.contains(&Gate::NotQuarantined), "only ever added as failed");
        // still pending gates do not hide it: failed wins
        let mut p = GateReport::default();
        p.push(GateResult::pending(Gate::AliveAfterResume));
        p.push(GateResult::fail(Gate::NotQuarantined, 0.0, json!({"reason": "client"})));
        assert!(matches!(p.validity(&[Gate::AliveAfterResume]), Validity::Invalid { .. }));
    }

    #[test]
    fn names_are_stable() {
        let names: Vec<_> = Gate::ALL.iter().map(|g| serde_json::to_value(g).unwrap()).collect();
        let strs: Vec<_> = Gate::ALL.iter().map(|g| json!(g.as_str())).collect();
        assert_eq!(names, strs);
    }
}
