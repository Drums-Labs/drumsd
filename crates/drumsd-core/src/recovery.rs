//! Recovery: which restore failures are retried, and how validity and cost are kept honest.
//!
//! Rules:
//! 1. A retry is a complete new restore (kill, fresh upper, restore, every gate). Nothing from a failed attempt is
//!    reused, so a recovered episode is judged only by its own final attempt's gates.
//! 2. Only failures that happened **before any restored task ran unsupervised** are retried automatically:
//!    the CRIU detach race (`Unable to detach ... No such process`), a template refusal (falls back to a private
//!    restore, recorded), and a template server that is not there (restarted once). Everything else, and every gate
//!    failure after the tree ran, quarantines the slot: a state that was observed wrong is never silently replaced.
//! 3. Every attempt is recorded with its seconds and error; the episode reports `attempts`, `recovered`, and
//!    `total_seconds` including the failed attempts, so cost per valid rollout counts the waste.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// CRIU's thread-by-thread PTRACE_DETACH lost a thread (linux-criu-detach report). The tree was killed by CRIU.
    DetachRace,
    /// The template server refused this checkpoint (a VMA it cannot template). Retried as a private restore.
    TemplateRefused,
    /// No template server answered on the socket.
    TemplateServerDown,
    /// criu restore failed for another reason.
    RestoreFailed,
    /// The restore finished but a gate failed.
    GateFailed,
    /// Host-side plumbing failed (cgroup, mount, files).
    Infra,
    /// The state tmpfs has less free space than the configured reserve: refused before anything was restored.
    StateDirFull,
}

/// Classify a failed restore from its error text and CRIU log tail.
pub fn classify(error: &str, criu_log_tail: &str) -> FailureClass {
    let both = format!("{error}\n{criu_log_tail}");
    if both.contains("Unable to detach") {
        FailureClass::DetachRace
    } else if both.contains("not supported with a template")
        || both.contains("does not match the template")
        || both.contains("The memory template has no entry")
    {
        FailureClass::TemplateRefused
    } else if both.contains("no template server") || both.contains("Connection refused") && both.contains(".sock") {
        FailureClass::TemplateServerDown
    } else if both.contains("criu restore") || both.contains("Error (") {
        FailureClass::RestoreFailed
    } else {
        FailureClass::Infra
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Run the whole restore again, same mode.
    Retry,
    /// Run the whole restore again as a private (non-template) restore, and record that this slot fell back.
    RetryPrivate,
    /// Restart the checkpoint's template server, then retry.
    RestartServerAndRetry,
    /// Stop: quarantine (gate failure) or down (restore failure).
    GiveUp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Policy {
    /// Total attempts allowed for one reset/create (first try included).
    pub max_attempts: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Policy { max_attempts: 3 }
    }
}

impl Policy {
    /// What to do after attempt number `attempt` (1-based) failed with `class`.
    pub fn next(&self, attempt: u32, class: &FailureClass, already_private: bool) -> Action {
        if attempt >= self.max_attempts {
            return Action::GiveUp;
        }
        match class {
            FailureClass::DetachRace => Action::Retry,
            FailureClass::TemplateRefused if !already_private => Action::RetryPrivate,
            FailureClass::TemplateServerDown => Action::RestartServerAndRetry,
            _ => Action::GiveUp,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub n: u32,
    pub mode: String,
    pub seconds: f64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<FailureClass>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
}

/// The cost record of one create/reset: every attempt, failed ones included.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CostRecord {
    pub attempts: Vec<Attempt>,
}

impl CostRecord {
    pub fn total_seconds(&self) -> f64 {
        self.attempts.iter().map(|a| a.seconds).sum()
    }
    pub fn recovered(&self) -> bool {
        self.attempts.len() > 1 && self.attempts.last().map(|a| a.ok).unwrap_or(false)
    }
    pub fn wasted_seconds(&self) -> f64 {
        self.attempts.iter().filter(|a| !a.ok).map(|a| a.seconds).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_the_known_failures() {
        assert_eq!(classify("criu restore failed", "Error (criu/cr-restore.c:2053): Unable to detach 71: No such process"), FailureClass::DetachRace);
        assert_eq!(classify("x", "Error: needs a premap; not supported with a template"), FailureClass::TemplateRefused);
        assert_eq!(classify("pageshare_client: no template server at /s.sock", ""), FailureClass::TemplateServerDown);
        assert_eq!(classify("criu restore rc=1", "Error (criu/files.c:1): boom"), FailureClass::RestoreFailed);
        assert_eq!(classify("mount overlay: EINVAL", ""), FailureClass::Infra);
    }

    #[test]
    fn policy_retries_only_pre_run_failures() {
        let p = Policy::default();
        assert_eq!(p.next(1, &FailureClass::DetachRace, false), Action::Retry);
        assert_eq!(p.next(1, &FailureClass::TemplateRefused, false), Action::RetryPrivate);
        assert_eq!(p.next(1, &FailureClass::TemplateRefused, true), Action::GiveUp);
        assert_eq!(p.next(1, &FailureClass::TemplateServerDown, false), Action::RestartServerAndRetry);
        assert_eq!(p.next(1, &FailureClass::GateFailed, false), Action::GiveUp);
        assert_eq!(p.next(1, &FailureClass::RestoreFailed, false), Action::GiveUp);
        assert_eq!(p.next(3, &FailureClass::DetachRace, false), Action::GiveUp);
    }

    #[test]
    fn cost_counts_failed_attempts() {
        let c = CostRecord {
            attempts: vec![
                Attempt {
                    n: 1,
                    mode: "template".into(),
                    seconds: 0.7,
                    ok: false,
                    class: Some(FailureClass::DetachRace),
                    error: Some("e".into()),
                    action: Some(Action::Retry),
                },
                Attempt { n: 2, mode: "template".into(), seconds: 0.6, ok: true, class: None, error: None, action: None },
            ],
        };
        assert!(c.recovered());
        assert!((c.total_seconds() - 1.3).abs() < 1e-9);
        assert!((c.wasted_seconds() - 0.7).abs() < 1e-9);
    }
}
