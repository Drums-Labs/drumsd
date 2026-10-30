//! Linux mechanics of drumsd. Everything here talks to the kernel or to the existing Drums runtime on disk; the
//! decisions (state machine, gates, recovery) live in `drumsd-core`.
//!
//! Compatibility contract with the Python runtime (`sharedos_rootfs` at drums-lab cafd3a5): a slot made by drumsd has
//! the same directory layout, ownership marker, `slot.json` fields, cgroup name and restore command line as one made by
//! `pageshare.py branch`, so the Python verbs (`capture`, `drop`, `cleanup`) still work on it.

pub mod cgroup;
pub mod guest;
pub mod hook;
pub mod pidfd;
pub mod procfs;
pub mod proxy;
pub mod quiesce;
pub mod restore;
pub mod slotfs;
pub mod checks;
pub mod forget;
pub mod template;

pub fn now_epoch() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}
