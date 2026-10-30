//! Leases: an env or a saved state that nobody uses is reaped, so a trainer that crashed (or was `kill -9`ed) cannot
//! hold the host's memory and state tmpfs forever.
//!
//! * `idle_timeout_s`: reaped when nothing used it for this long. An env is used by every API request that names it
//!   (step, observe, reset, verify, fork, GET of its record, ...). A saved state is used while any env made from it
//!   exists; its idle time counts from its creation or from when its last env closed, whichever is later.
//! * `ttl_s`: reaped this long after it was created, used or not. A saved state whose TTL ran out is deleted once no
//!   env made from it exists (envs have their own leases).
//!
//! `None` (or 0 in a request) means no limit of that kind. The daemon's config sets the defaults (`lease` in
//! drumsd.json); create, fork and save can override them per env or state. Installed images are never reaped.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    pub idle_timeout_s: Option<f64>,
    pub ttl_s: Option<f64>,
}

/// A request's override: absent -> the default; 0 or less -> no limit; otherwise the value.
pub fn pick(req: Option<f64>, default: Option<f64>) -> Option<f64> {
    match req {
        None => default.filter(|x| *x > 0.0),
        Some(x) if x <= 0.0 => None,
        Some(x) => Some(x),
    }
}

impl Lease {
    pub fn resolve(idle: Option<f64>, ttl: Option<f64>, default: Lease) -> Lease {
        Lease { idle_timeout_s: pick(idle, default.idle_timeout_s), ttl_s: pick(ttl, default.ttl_s) }
    }

    /// Why the lease ran out at `now` ("ttl" or "idle"), given when the object was created and last used.
    pub fn expired(&self, created: f64, last_used: f64, now: f64) -> Option<&'static str> {
        if self.ttl_s.is_some_and(|t| now - created >= t) {
            return Some("ttl");
        }
        if self.idle_timeout_s.is_some_and(|i| now - last_used.max(created) >= i) {
            return Some("idle");
        }
        None
    }

    /// When it runs out if it is not used again (epoch seconds), or None.
    pub fn expires_at(&self, created: f64, last_used: f64) -> Option<f64> {
        let a = self.ttl_s.map(|t| created + t);
        let b = self.idle_timeout_s.map(|i| last_used.max(created) + i);
        match (a, b) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, y) => x.or(y),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_defaults_and_off() {
        let d = Lease { idle_timeout_s: Some(1800.0), ttl_s: None };
        assert_eq!(Lease::resolve(None, None, d), d);
        assert_eq!(Lease::resolve(Some(60.0), Some(600.0), d), Lease { idle_timeout_s: Some(60.0), ttl_s: Some(600.0) });
        assert_eq!(Lease::resolve(Some(0.0), None, d), Lease { idle_timeout_s: None, ttl_s: None }, "0 turns the default off");
        assert_eq!(Lease::resolve(None, None, Lease { idle_timeout_s: Some(0.0), ttl_s: None }).idle_timeout_s, None);
    }

    #[test]
    fn idle_counts_from_the_last_use_and_ttl_from_creation() {
        let l = Lease { idle_timeout_s: Some(60.0), ttl_s: Some(3600.0) };
        assert_eq!(l.expired(1000.0, 1000.0, 1059.0), None);
        assert_eq!(l.expired(1000.0, 1000.0, 1060.0), Some("idle"));
        assert_eq!(l.expired(1000.0, 1050.0, 1100.0), None, "used at 1050: idle until 1110");
        assert_eq!(l.expired(1000.0, 4590.0, 4600.0), Some("ttl"), "busy, but its time is up");
        assert_eq!(l.expires_at(1000.0, 1050.0), Some(1110.0));
        assert_eq!(Lease::default().expired(0.0, 0.0, 1e12), None, "no lease: never");
        assert_eq!(Lease::default().expires_at(0.0, 0.0), None);
        // a last-used time before creation (an old record) counts from creation
        assert_eq!(l.expired(1000.0, 0.0, 1059.0), None);
    }
}
