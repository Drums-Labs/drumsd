//! The "processes alive after resume" gate.
//!
//! Reference set: every user-space process of the restored tree as CRIU's pre-resume action sees it (pid inside the
//! slot's pid namespace + comm). That is exactly the checkpoint's process set, read from the host with no guest round
//! trip. At least `min_after_s` after the slot answered, every reference process must still exist with the same
//! namespace pid and comm. Processes that exit on their own with no restore at all (measured by a no-restore control
//! and listed per host recipe) are reported but not counted.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AliveVerdict {
    pub ok: bool,
    pub reference_n: usize,
    pub missing: Vec<(String, String)>,
    pub counted: Vec<(String, String)>,
    pub natural_excluded: Vec<(String, String)>,
    pub after_s: f64,
}

/// `reference` and `now` map namespace pid -> comm.
pub fn judge(reference: &BTreeMap<String, String>, now: &BTreeMap<String, String>, natural: &BTreeSet<String>, after_s: f64) -> AliveVerdict {
    let missing: Vec<(String, String)> = reference
        .iter()
        .filter(|(pid, comm)| now.get(*pid) != Some(*comm))
        .map(|(p, c)| (p.clone(), c.clone()))
        .collect();
    let counted: Vec<_> = missing.iter().filter(|(_, c)| !natural.contains(c)).cloned().collect();
    let natural_excluded: Vec<_> = missing.iter().filter(|(_, c)| natural.contains(c)).cloned().collect();
    AliveVerdict { ok: counted.is_empty(), reference_n: reference.len(), missing, counted, natural_excluded, after_s }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: &[(&str, &str)]) -> BTreeMap<String, String> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn a_died_process_fails_and_a_natural_exit_does_not() {
        let r = m(&[("1", "python3"), ("16", "gnome-shell"), ("119", "geoclue"), ("328", "rtkit-daemon")]);
        let nat: BTreeSet<String> = ["geoclue".to_string()].into_iter().collect();
        let v = judge(&r, &m(&[("1", "python3"), ("16", "gnome-shell")]), &nat, 5.1);
        assert!(!v.ok);
        assert_eq!(v.counted, vec![("328".to_string(), "rtkit-daemon".to_string())]);
        assert_eq!(v.natural_excluded, vec![("119".to_string(), "geoclue".to_string())]);
        let v2 = judge(&r, &m(&[("1", "python3"), ("16", "gnome-shell"), ("328", "rtkit-daemon"), ("400", "sh")]), &nat, 5.0);
        assert!(v2.ok);
    }

    #[test]
    fn a_reused_pid_with_another_comm_counts_as_missing() {
        let r = m(&[("16", "gnome-shell")]);
        let v = judge(&r, &m(&[("16", "bash")]), &BTreeSet::new(), 5.0);
        assert!(!v.ok);
    }
}
