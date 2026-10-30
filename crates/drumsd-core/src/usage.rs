//! Usage ledger: what a customer pays for and what they got.
//!
//! Two ledgers with the same shape, one per slot (kept in its record) and one for the daemon (kept in the journal as
//! `meta/usage.json`, so a restart does not lose it):
//!
//! * `ops`: per operation kind (create, reset, fork, capture, step, verify, task_setup), the count, how many failed,
//!   total seconds, and the latency samples (last 4096) for p50/p95. Seconds are wall time inside drumsd, from the
//!   request to the reply, failed attempts included.
//! * `episodes`: episodes by final verdict. An episode is counted once, when its verdict stops being pending
//!   (`valid`/`invalid`), or as `ended_pending` when it was ended (reset, destroy) before the alive gate resolved.
//! * `failures` / `retries`: by cause (`detach_race`, `template_server_down`, `restore_failed`, `gate:<name>`,
//!   `task_setup`, `infra`, ...). A retry is an automatic re-attempt drumsd made; a failure is an operation that
//!   ended in error.
//! * `verify`: rewards handed out from a valid episode (`valid`), rewards from an invalid or pending episode
//!   (`flagged_invalid`, always returned with `valid: false`), and checker errors (`checker_error`, no reward).
//!
//! Cost: `host_usd = host_usd_per_hour * wall_hours(window)`. The host is billed for wall time whether slots are
//! busy or not, so every derived cost divides the whole host cost:
//! `usd_per_valid_episode = host_usd / episodes.valid`, `usd_per_verified_reward = host_usd / verify.valid`.
//! A slot's share is `host_usd * slot_seconds / sum(slot_seconds)` (slot-seconds = time a slot existed in the window).
//! The price is the operator's configured number, not a measured bill.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_SAMPLES: usize = 4096;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpStat {
    pub n: u64,
    pub failed: u64,
    pub seconds_total: f64,
    #[serde(default)]
    pub samples: Vec<f64>,
}

impl OpStat {
    pub fn add(&mut self, seconds: f64, ok: bool) {
        self.n += 1;
        if !ok {
            self.failed += 1;
        }
        self.seconds_total += seconds;
        self.samples.push(seconds);
        if self.samples.len() > MAX_SAMPLES {
            let k = self.samples.len() - MAX_SAMPLES;
            self.samples.drain(..k);
        }
    }

    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut s = self.samples.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        // nearest-rank
        let rank = ((q * s.len() as f64).ceil() as usize).clamp(1, s.len());
        Some(s[rank - 1])
    }

    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "n": self.n, "failed": self.failed, "seconds_total": r3(self.seconds_total),
            "mean_s": if self.n > 0 { Some(r3(self.seconds_total / self.n as f64)) } else { None },
            "p50_s": self.quantile(0.5).map(r3), "p95_s": self.quantile(0.95).map(r3),
            "samples_kept": self.samples.len(),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Episodes {
    pub valid: u64,
    pub invalid: u64,
    pub ended_pending: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Verify {
    pub valid: u64,
    pub flagged_invalid: u64,
    pub checker_error: u64,
    /// Sum of scores of rewards from valid episodes.
    pub valid_score_sum: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    pub ops: BTreeMap<String, OpStat>,
    pub episodes: Episodes,
    pub failures: BTreeMap<String, u64>,
    pub retries: BTreeMap<String, u64>,
    pub verify: Verify,
}

impl Ledger {
    pub fn op(&mut self, kind: &str, seconds: f64, ok: bool) {
        self.ops.entry(kind.to_string()).or_default().add(seconds, ok);
    }
    pub fn failure(&mut self, cause: &str) {
        *self.failures.entry(cause.to_string()).or_default() += 1;
    }
    pub fn retry(&mut self, cause: &str) {
        *self.retries.entry(cause.to_string()).or_default() += 1;
    }
    pub fn episode(&mut self, verdict: &str) {
        match verdict {
            "valid" => self.episodes.valid += 1,
            "invalid" => self.episodes.invalid += 1,
            _ => self.episodes.ended_pending += 1,
        }
    }
    pub fn verified(&mut self, score: Option<f64>, valid: bool) {
        match (score, valid) {
            (None, _) => self.verify.checker_error += 1,
            (Some(s), true) => {
                self.verify.valid += 1;
                self.verify.valid_score_sum += s;
            }
            (Some(_), false) => self.verify.flagged_invalid += 1,
        }
    }

    pub fn summary(&self) -> serde_json::Value {
        let ops: BTreeMap<&String, serde_json::Value> = self.ops.iter().map(|(k, v)| (k, v.summary())).collect();
        let ep = &self.episodes;
        let judged = ep.valid + ep.invalid;
        serde_json::json!({
            "ops": ops,
            "episodes": {"valid": ep.valid, "invalid": ep.invalid, "ended_pending": ep.ended_pending,
                         "valid_fraction_of_judged": if judged > 0 { Some(r3(ep.valid as f64 / judged as f64)) } else { None }},
            "failures_by_cause": self.failures,
            "retries_by_cause": self.retries,
            "verify": {"valid": self.verify.valid, "flagged_invalid": self.verify.flagged_invalid, "checker_error": self.verify.checker_error,
                       "mean_valid_score": if self.verify.valid > 0 { Some(r3(self.verify.valid_score_sum / self.verify.valid as f64)) } else { None }},
        })
    }
}

/// The daemon's ledger plus its cost window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub window_start_epoch: f64,
    pub ledger: Ledger,
    /// Slot-seconds of slots that no longer exist (destroyed) in this window.
    pub closed_slot_seconds: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Cost {
    pub host_usd_per_hour: f64,
    pub wall_hours: f64,
    pub host_usd: f64,
    pub slot_seconds: f64,
    pub usd_per_valid_episode: Option<f64>,
    pub usd_per_verified_reward: Option<f64>,
    pub mean_concurrent_slots: f64,
}

pub fn cost(host_usd_per_hour: f64, wall_s: f64, slot_seconds: f64, ledger: &Ledger) -> Cost {
    let host_usd = host_usd_per_hour * wall_s / 3600.0;
    let div = |n: u64| if n > 0 { Some(r6(host_usd / n as f64)) } else { None };
    Cost {
        host_usd_per_hour,
        wall_hours: r6(wall_s / 3600.0),
        host_usd: r6(host_usd),
        slot_seconds: r3(slot_seconds),
        usd_per_valid_episode: div(ledger.episodes.valid),
        usd_per_verified_reward: div(ledger.verify.valid),
        mean_concurrent_slots: if wall_s > 0.0 { r3(slot_seconds / wall_s) } else { 0.0 },
    }
}

/// A slot's share of the host cost by slot-seconds.
pub fn share(host_usd: f64, slot_seconds: f64, all_slot_seconds: f64) -> f64 {
    if all_slot_seconds <= 0.0 {
        0.0
    } else {
        r6(host_usd * slot_seconds / all_slot_seconds)
    }
}

/// Cause string for a failure class or gate.
pub fn cause_of(class: &crate::recovery::FailureClass, gate: Option<&str>) -> String {
    use crate::recovery::FailureClass::*;
    match class {
        GateFailed => format!("gate:{}", gate.unwrap_or("?")),
        DetachRace => "detach_race".into(),
        TemplateRefused => "template_refused".into(),
        TemplateServerDown => "template_server_down".into(),
        RestoreFailed => "restore_failed".into(),
        Infra => "infra".into(),
    }
}

fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}
fn r6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_are_nearest_rank() {
        let mut s = OpStat::default();
        for x in [5.0, 1.0, 3.0, 2.0, 4.0] {
            s.add(x, true);
        }
        assert_eq!(s.quantile(0.5), Some(3.0));
        assert_eq!(s.quantile(0.95), Some(5.0));
        assert_eq!(s.n, 5);
        assert_eq!(OpStat::default().quantile(0.5), None);
    }

    #[test]
    fn samples_are_capped_counts_are_not() {
        let mut s = OpStat::default();
        for i in 0..(MAX_SAMPLES + 10) {
            s.add(i as f64, i % 2 == 0);
        }
        assert_eq!(s.samples.len(), MAX_SAMPLES);
        assert_eq!(s.n as usize, MAX_SAMPLES + 10);
        assert_eq!(s.failed as usize, (MAX_SAMPLES + 10) / 2);
    }

    #[test]
    fn invalid_rewards_never_count_as_verified() {
        let mut l = Ledger::default();
        l.verified(Some(1.0), true);
        l.verified(Some(1.0), false);
        l.verified(None, true);
        assert_eq!((l.verify.valid, l.verify.flagged_invalid, l.verify.checker_error), (1, 1, 1));
        assert_eq!(l.verify.valid_score_sum, 1.0);
    }

    #[test]
    fn cost_divides_the_whole_host_bill() {
        let mut l = Ledger::default();
        for _ in 0..3 {
            l.episode("valid");
        }
        l.episode("invalid");
        l.verified(Some(0.0), true);
        l.verified(Some(1.0), true);
        // 2 h at 0.75 USD/h = 1.5 USD; 3 valid episodes; 2 verified rewards
        let c = cost(0.75, 7200.0, 3600.0, &l);
        assert_eq!(c.host_usd, 1.5);
        assert_eq!(c.usd_per_valid_episode, Some(0.5));
        assert_eq!(c.usd_per_verified_reward, Some(0.75));
        assert_eq!(c.mean_concurrent_slots, 0.5);
        assert_eq!(cost(0.75, 10.0, 0.0, &Ledger::default()).usd_per_valid_episode, None);
        assert_eq!(share(1.5, 900.0, 3600.0), 0.375);
    }

    #[test]
    fn causes_name_the_gate() {
        use crate::recovery::FailureClass;
        assert_eq!(cause_of(&FailureClass::GateFailed, Some("state_hash")), "gate:state_hash");
        assert_eq!(cause_of(&FailureClass::DetachRace, None), "detach_race");
    }
}
