//! Prometheus text metrics: counters, gauges and fixed-bucket histograms, keyed by name and sorted labels.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;

/// Latency buckets in seconds: 0.1 ms .. 15 min (control-plane overhead sits in the low buckets, creates in the high ones).
pub const BUCKETS: &[f64] = &[
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 0.75, 1.0, 1.5, 2.5, 3.0, 5.0, 7.5, 10.0, 15.0, 20.0, 30.0, 60.0, 120.0,
    300.0, 900.0,
];

type Key = (String, Vec<(String, String)>);

#[derive(Default)]
struct Hist {
    counts: Vec<u64>,
    sum: f64,
    n: u64,
}

#[derive(Default)]
pub struct Metrics {
    counters: Mutex<BTreeMap<Key, f64>>,
    gauges: Mutex<BTreeMap<Key, f64>>,
    hists: Mutex<BTreeMap<Key, Hist>>,
    help: Mutex<BTreeMap<String, &'static str>>,
}

fn key(name: &str, labels: &[(&str, &str)]) -> Key {
    let mut l: Vec<(String, String)> = labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    l.sort();
    (name.to_string(), l)
}

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn fmt_labels(l: &[(String, String)], extra: Option<(&str, String)>) -> String {
    let mut parts: Vec<String> = l.iter().map(|(k, v)| format!("{k}=\"{}\"", esc(v))).collect();
    if let Some((k, v)) = extra {
        parts.push(format!("{k}=\"{v}\""));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", parts.join(","))
    }
}

impl Metrics {
    pub fn describe(&self, name: &str, help: &'static str) {
        self.help.lock().unwrap().insert(name.to_string(), help);
    }
    pub fn inc(&self, name: &str, labels: &[(&str, &str)]) {
        self.add(name, labels, 1.0)
    }
    pub fn add(&self, name: &str, labels: &[(&str, &str)], v: f64) {
        *self.counters.lock().unwrap().entry(key(name, labels)).or_default() += v;
    }
    pub fn set(&self, name: &str, labels: &[(&str, &str)], v: f64) {
        self.gauges.lock().unwrap().insert(key(name, labels), v);
    }
    /// Drop every gauge series of `name` (before re-setting the current ones, so a removed host's series disappear).
    pub fn clear_gauge(&self, name: &str) {
        self.gauges.lock().unwrap().retain(|k, _| k.0 != name);
    }
    pub fn observe(&self, name: &str, labels: &[(&str, &str)], v: f64) {
        let mut h = self.hists.lock().unwrap();
        let e = h.entry(key(name, labels)).or_insert_with(|| Hist { counts: vec![0; BUCKETS.len()], sum: 0.0, n: 0 });
        for (i, b) in BUCKETS.iter().enumerate() {
            if v <= *b {
                e.counts[i] += 1;
            }
        }
        e.sum += v;
        e.n += 1;
    }
    pub fn counter(&self, name: &str, labels: &[(&str, &str)]) -> f64 {
        self.counters.lock().unwrap().get(&key(name, labels)).copied().unwrap_or(0.0)
    }
    pub fn gauge(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        self.gauges.lock().unwrap().get(&key(name, labels)).copied()
    }

    pub fn render(&self) -> String {
        let help = self.help.lock().unwrap().clone();
        let mut out = String::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut head = |out: &mut String, name: &str, ty: &str| {
            if seen.insert(name.to_string()) {
                if let Some(h) = help.get(name) {
                    let _ = writeln!(out, "# HELP {name} {h}");
                }
                let _ = writeln!(out, "# TYPE {name} {ty}");
            }
        };
        for ((n, l), v) in self.counters.lock().unwrap().iter() {
            head(&mut out, n, "counter");
            let _ = writeln!(out, "{n}{} {v}", fmt_labels(l, None));
        }
        for ((n, l), v) in self.gauges.lock().unwrap().iter() {
            head(&mut out, n, "gauge");
            let _ = writeln!(out, "{n}{} {v}", fmt_labels(l, None));
        }
        for ((n, l), h) in self.hists.lock().unwrap().iter() {
            head(&mut out, n, "histogram");
            for (i, b) in BUCKETS.iter().enumerate() {
                let _ = writeln!(out, "{n}_bucket{} {}", fmt_labels(l, Some(("le", format!("{b}")))), h.counts[i]);
            }
            let _ = writeln!(out, "{n}_bucket{} {}", fmt_labels(l, Some(("le", "+Inf".into()))), h.n);
            let _ = writeln!(out, "{n}_sum{} {}", fmt_labels(l, None), h.sum);
            let _ = writeln!(out, "{n}_count{} {}", fmt_labels(l, None), h.n);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_all_kinds() {
        let m = Metrics::default();
        m.describe("pool_x_total", "x");
        m.inc("pool_x_total", &[("op", "step")]);
        m.set("pool_g", &[], 2.0);
        m.observe("pool_h_seconds", &[("op", "a\"b")], 0.003);
        let t = m.render();
        assert!(t.contains("# TYPE pool_x_total counter"));
        assert!(t.contains("pool_x_total{op=\"step\"} 1"));
        assert!(t.contains("pool_g 2"));
        assert!(t.contains("pool_h_seconds_bucket{op=\"a\\\"b\",le=\"0.005\"} 1"));
        assert!(t.contains("pool_h_seconds_bucket{op=\"a\\\"b\",le=\"0.0025\"} 0"));
        assert!(t.contains("pool_h_seconds_count{op=\"a\\\"b\"} 1"));
    }
}
