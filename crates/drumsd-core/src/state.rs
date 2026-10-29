//! The slot state machine.
//!
//! A slot is one isolated desktop: its own overlay root, namespaces and cgroup, running a process tree restored
//! from a checkpoint. Every operation is a pair of transitions: `begin(op)` moves the slot into the operation's
//! transient state (or refuses), and `finish(op, outcome)` moves it to where the outcome leads. The transient
//! state is written to the journal before any work starts, so a daemon that dies mid-operation finds the slot in
//! a transient state on restart and reconciles it (see [`SlotState::is_transient`]).
//!
//! ```text
//!            create                      reset (restore + gates)
//!   (none) ---------> Creating --ok--> Ready <---------------------+
//!                        |              |  |  \                    |
//!                     gate fail      capture  step (no change)     |
//!                        |              v                          |
//!                        |          Capturing --ok--> Ready        |
//!                        v                                         |
//!                    Quarantined <---- any gate fail after retries |
//!                        |                                         |
//!                     release --> Down ------- reset -------------+
//!   any non-terminal -- destroy --> Destroying --> Destroyed
//! ```

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SlotState {
    /// First restore of a new slot (branch from a checkpoint) is in progress.
    Creating,
    /// The tree runs, every gate of the current episode passed (the alive gate may still be pending).
    Ready,
    /// A reset (kill tree, fresh upper layer, restore, gates) is in progress.
    Resetting,
    /// A checkpoint of the running tree is being taken; the tree keeps running.
    Capturing,
    /// The tree is not running (a failed restore that was cleaned up, or found dead after a daemon restart).
    /// The next reset restores it.
    Down { reason: String },
    /// A gate failed (or recovery gave up). Nothing is restored into it until it is released or destroyed.
    Quarantined { reason: String },
    /// The slot's processes and bulky state are being removed (records are kept).
    Destroying,
    /// Terminal.
    Destroyed,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Create,
    Reset,
    Capture,
    Step,
    Release,
    Quarantine,
    Destroy,
}

impl Op {
    pub fn as_str(&self) -> &'static str {
        match self {
            Op::Create => "create",
            Op::Reset => "reset",
            Op::Capture => "capture",
            Op::Step => "step",
            Op::Release => "release",
            Op::Quarantine => "quarantine",
            Op::Destroy => "destroy",
        }
    }
}

/// How an operation ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// The work finished and every synchronous gate passed.
    Ok,
    /// A gate failed (after the recovery policy ran out of attempts): the slot is quarantined.
    GateFailed { gate: String, detail: String },
    /// The work failed before the tree ran (e.g. restore refused); the slot was cleaned up and is down.
    Failed { error: String },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TransitionError {
    #[error("slot is {state}: {op} is not allowed (allowed from: {allowed})")]
    NotAllowed { state: String, op: &'static str, allowed: &'static str },
}

impl SlotState {
    pub fn name(&self) -> &'static str {
        match self {
            SlotState::Creating => "creating",
            SlotState::Ready => "ready",
            SlotState::Resetting => "resetting",
            SlotState::Capturing => "capturing",
            SlotState::Down { .. } => "down",
            SlotState::Quarantined { .. } => "quarantined",
            SlotState::Destroying => "destroying",
            SlotState::Destroyed => "destroyed",
        }
    }

    /// A state that only exists while an operation runs. Found in the journal after a restart, it means the
    /// daemon died mid-operation and the slot must be reconciled against the kernel.
    pub fn is_transient(&self) -> bool {
        matches!(self, SlotState::Creating | SlotState::Resetting | SlotState::Capturing | SlotState::Destroying)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, SlotState::Destroyed)
    }

    /// Start `op`. Returns the state to journal before the work begins.
    pub fn begin(&self, op: Op) -> Result<SlotState, TransitionError> {
        use SlotState::*;
        let refuse = |allowed: &'static str| {
            Err(TransitionError::NotAllowed { state: self.name().to_string(), op: op.as_str(), allowed })
        };
        match op {
            // `create` is the first transition of a slot record; it is begun from Down("new") by the manager.
            Op::Create => match self {
                Down { .. } => Ok(Creating),
                _ => refuse("down (a new record)"),
            },
            Op::Reset => match self {
                Ready | Down { .. } => Ok(Resetting),
                _ => refuse("ready, down"),
            },
            Op::Capture => match self {
                Ready => Ok(Capturing),
                _ => refuse("ready"),
            },
            Op::Step => match self {
                Ready => Ok(Ready),
                _ => refuse("ready"),
            },
            Op::Release => match self {
                Quarantined { reason } => Ok(Down { reason: format!("released from quarantine: {reason}") }),
                _ => refuse("quarantined"),
            },
            Op::Quarantine => match self {
                Destroying | Destroyed => refuse("any state but destroying, destroyed"),
                _ => Ok(Quarantined { reason: "operator".into() }),
            },
            Op::Destroy => match self {
                Destroyed => refuse("any state but destroyed"),
                _ => Ok(Destroying),
            },
        }
    }

    /// Where `op` leads from its transient state, given how it ended.
    pub fn finish(op: Op, outcome: &Outcome) -> SlotState {
        use SlotState::*;
        match (op, outcome) {
            (Op::Destroy, Outcome::Ok) => Destroyed,
            (Op::Destroy, Outcome::Failed { error }) => Quarantined { reason: format!("destroy failed: {error}") },
            (Op::Destroy, Outcome::GateFailed { gate, detail }) => Quarantined { reason: format!("destroy: {gate}: {detail}") },
            (Op::Capture, Outcome::Ok) => Ready,
            // a failed capture leaves the running tree as it was (the dump is --leave-running) but the
            // parent's validity can no longer be proven, so it is quarantined rather than reused
            (Op::Capture, Outcome::Failed { error }) => Quarantined { reason: format!("capture failed: {error}") },
            (_, Outcome::Ok) => Ready,
            (_, Outcome::GateFailed { gate, detail }) => Quarantined { reason: format!("gate {gate} failed: {detail}") },
            (_, Outcome::Failed { error }) => Down { reason: error.clone() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_states() -> Vec<SlotState> {
        vec![
            SlotState::Creating,
            SlotState::Ready,
            SlotState::Resetting,
            SlotState::Capturing,
            SlotState::Down { reason: "x".into() },
            SlotState::Quarantined { reason: "y".into() },
            SlotState::Destroying,
            SlotState::Destroyed,
        ]
    }
    const OPS: [Op; 7] = [Op::Create, Op::Reset, Op::Capture, Op::Step, Op::Release, Op::Quarantine, Op::Destroy];

    #[test]
    fn transition_table_is_exactly_the_documented_one() {
        // (state, op) pairs that may begin; everything else must be refused.
        let allowed: &[(&str, Op)] = &[
            ("down", Op::Create),
            ("ready", Op::Reset),
            ("down", Op::Reset),
            ("ready", Op::Capture),
            ("ready", Op::Step),
            ("quarantined", Op::Release),
            ("creating", Op::Quarantine),
            ("ready", Op::Quarantine),
            ("resetting", Op::Quarantine),
            ("capturing", Op::Quarantine),
            ("down", Op::Quarantine),
            ("quarantined", Op::Quarantine),
            ("creating", Op::Destroy),
            ("ready", Op::Destroy),
            ("resetting", Op::Destroy),
            ("capturing", Op::Destroy),
            ("down", Op::Destroy),
            ("quarantined", Op::Destroy),
            ("destroying", Op::Destroy),
        ];
        for s in all_states() {
            for op in OPS {
                let want = allowed.iter().any(|(n, o)| *n == s.name() && *o == op);
                assert_eq!(s.begin(op).is_ok(), want, "{} + {:?}", s.name(), op);
            }
        }
    }

    #[test]
    fn quarantine_is_sticky_until_release() {
        let q = SlotState::Quarantined { reason: "state_hash".into() };
        assert!(q.begin(Op::Reset).is_err());
        assert!(q.begin(Op::Step).is_err());
        assert!(q.begin(Op::Capture).is_err());
        let d = q.begin(Op::Release).unwrap();
        assert_eq!(d.name(), "down");
        assert_eq!(d.begin(Op::Reset).unwrap(), SlotState::Resetting);
    }

    #[test]
    fn outcomes_lead_where_documented() {
        let gf = Outcome::GateFailed { gate: "state_hash".into(), detail: "a != b".into() };
        let f = Outcome::Failed { error: "criu restore rc 1".into() };
        assert_eq!(SlotState::finish(Op::Reset, &Outcome::Ok), SlotState::Ready);
        assert_eq!(SlotState::finish(Op::Reset, &gf).name(), "quarantined");
        assert_eq!(SlotState::finish(Op::Reset, &f).name(), "down");
        assert_eq!(SlotState::finish(Op::Create, &f).name(), "down");
        assert_eq!(SlotState::finish(Op::Capture, &Outcome::Ok), SlotState::Ready);
        assert_eq!(SlotState::finish(Op::Capture, &f).name(), "quarantined");
        assert_eq!(SlotState::finish(Op::Destroy, &Outcome::Ok), SlotState::Destroyed);
        assert_eq!(SlotState::finish(Op::Destroy, &f).name(), "quarantined");
    }

    #[test]
    fn transient_and_terminal() {
        let t: Vec<_> = all_states().into_iter().filter(|s| s.is_transient()).map(|s| s.name()).collect();
        assert_eq!(t, vec!["creating", "resetting", "capturing", "destroying"]);
        assert!(SlotState::Destroyed.is_terminal());
        assert!(SlotState::Destroyed.begin(Op::Destroy).is_err());
    }

    #[test]
    fn serde_round_trip_is_tagged() {
        let s = SlotState::Quarantined { reason: "alive_after_resume".into() };
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(j, r#"{"state":"quarantined","reason":"alive_after_resume"}"#);
        assert_eq!(serde_json::from_str::<SlotState>(&j).unwrap(), s);
    }
}
