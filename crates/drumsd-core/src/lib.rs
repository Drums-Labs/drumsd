//! drumsd-core: everything about a Drums slot's lifecycle that does not touch the kernel.
//!
//! * [`state`]: the typed slot state machine (create, ready, reset, capture, branch, destroy, quarantine).
//! * [`gates`]: gate results as first-class values; an episode is valid only when every gate passed.
//! * [`recovery`]: which failures are retried, what each attempt cost, and how validity is kept.
//! * [`journal`]: crash-safe per-slot records (write, fsync, rename) and an append-only event log.
//! * [`metrics`]: a small Prometheus text-format registry.
//! * [`quiesce`]: capture readiness (no task leaving, a stable tree for a window, the dump matches it).
//! * [`admission`]: refuse creates, captures and forks that would push the host into memory thrash.
//! * [`usage`]: per-slot and daemon ledgers (op latency, episodes by verdict, failures/retries by cause, rewards) and cost.
//! * [`locks`], [`mounts`], [`net`], [`statehash`], [`alive`]: pure ports of the Python runtime's checks
//!   (sharedos_rootfs/locks.py, candidate.py, iarun/checks.py, fcbench.py) so both paths judge a restore the same way.

pub mod admission;
pub mod alive;
pub mod gates;
pub mod journal;
pub mod locks;
pub mod metrics;
pub mod mounts;
pub mod net;
pub mod quiesce;
pub mod recovery;
pub mod state;
pub mod statehash;
pub mod usage;
