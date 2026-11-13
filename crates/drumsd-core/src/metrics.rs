//! A small Prometheus text-format (0.0.4) registry: counters, gauges and histograms with label sets.
//! Dependency-free on purpose; the daemon renders it at `GET /metrics`.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;

type Labels = Vec<(String, String)>;

#[derive(Clone)]
struct Hist {
    buckets: Vec<f64>,
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

enum Family {
    Counter(BTreeMap<Labels, f64>),
    Gauge(BTreeMap<Labels, f64>),
    Histogram(Vec<f64>, BTreeMap<Labels, Hist>),
}

pub struct Registry {
    inner: Mutex<BTreeMap<String, (String, Family)>>,
}

/// Seconds buckets suited to restore/reset latencies (50 ms .. 60 s).
pub const LATENCY_BUCKETS: &[f64] = &[0.05, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 1.0, 1.25, 1.5, 2.0, 3.0, 5.0, 10.0, 30.0, 60.0];

fn labels(l: &[(&str, &str)]) -> Labels {
    let mut v: Labels = l.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    v.sort();
    v
}

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn fmt_labels(l: &Labels, extra: Option<(&str, String)>) -> String {
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

fn fmt_f(v: f64) -> String {
    if v == f64::INFINITY {
        "+Inf".into()
    } else if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

impl Default for Registry {
    fn default() -> Self {
        Registry { inner: Mutex::new(BTreeMap::new()) }
    }
}

impl Registry {
    pub fn counter(&self, name: &str, help: &str) {
        self.inner.lock().unwrap().entry(name.into()).or_insert((help.into(), Family::Counter(BTreeMap::new())));
    }
    pub fn gauge(&self, name: &str, help: &str) {
        self.inner.lock().unwrap().entry(name.into()).or_insert((help.into(), Family::Gauge(BTreeMap::new())));
    }
    pub fn histogram(&self, name: &str, help: &str, buckets: &[f64]) {
        self.inner.lock().unwrap().entry(name.into()).or_insert((help.into(), Family::Histogram(buckets.to_vec(), BTreeMap::new())));
    }

    pub fn inc(&self, name: &str, l: &[(&str, &str)], by: f64) {
        let mut g = self.inner.lock().unwrap();
        if let Some((_, Family::Counter(m))) = g.get_mut(name) {
            *m.entry(labels(l)).or_insert(0.0) += by;
        }
    }

    pub fn set(&self, name: &str, l: &[(&str, &str)], v: f64) {
        let mut g = self.inner.lock().unwrap();
        if let Some((_, Family::Gauge(m))) = g.get_mut(name) {
            m.insert(labels(l), v);
        }
    }

    pub fn observe(&self, name: &str, l: &[(&str, &str)], v: f64) {
        let mut g = self.inner.lock().unwrap();
        if let Some((_, Family::Histogram(b, m))) = g.get_mut(name) {
            let h = m.entry(labels(l)).or_insert_with(|| Hist { buckets: b.clone(), counts: vec![0; b.len()], sum: 0.0, count: 0 });
            for (i, ub) in h.buckets.iter().enumerate() {
                if v <= *ub {
                    h.counts[i] += 1;
                }
            }
            h.sum += v;
            h.count += 1;
        }
    }

    pub fn render(&self) -> String {
        let g = self.inner.lock().unwrap();
        let mut out = String::new();
        for (name, (help, fam)) in g.iter() {
            let _ = writeln!(out, "# HELP {name} {help}");
            match fam {
                Family::Counter(m) => {
                    let _ = writeln!(out, "# TYPE {name} counter");
                    for (l, v) in m {
                        let _ = writeln!(out, "{name}{} {}", fmt_labels(l, None), fmt_f(*v));
                    }
                }
                Family::Gauge(m) => {
                    let _ = writeln!(out, "# TYPE {name} gauge");
                    for (l, v) in m {
                        let _ = writeln!(out, "{name}{} {}", fmt_labels(l, None), fmt_f(*v));
                    }
                }
                Family::Histogram(_, m) => {
                    let _ = writeln!(out, "# TYPE {name} histogram");
                    for (l, h) in m {
                        for (i, ub) in h.buckets.iter().enumerate() {
                            let _ = writeln!(out, "{name}_bucket{} {}", fmt_labels(l, Some(("le", fmt_f(*ub)))), h.counts[i]);
                        }
                        let _ = writeln!(out, "{name}_bucket{} {}", fmt_labels(l, Some(("le", "+Inf".into()))), h.count);
                        let _ = writeln!(out, "{name}_sum{} {}", fmt_labels(l, None), h.sum);
                        let _ = writeln!(out, "{name}_count{} {}", fmt_labels(l, None), h.count);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_prometheus_text() {
        let r = Registry::default();
        r.counter("drumsd_resets_total", "Resets by outcome");
        r.gauge("drumsd_slots", "Slots by state");
        r.histogram("drumsd_reset_seconds", "Reset latency", &[0.5, 1.0]);
        r.inc("drumsd_resets_total", &[("outcome", "ok")], 1.0);
        r.inc("drumsd_resets_total", &[("outcome", "ok")], 1.0);
        r.set("drumsd_slots", &[("state", "ready")], 3.0);
        r.observe("drumsd_reset_seconds", &[], 0.4);
        r.observe("drumsd_reset_seconds", &[], 0.9);
        let t = r.render();
        assert!(t.contains("# TYPE drumsd_resets_total counter\ndrumsd_resets_total{outcome=\"ok\"} 2\n"), "{t}");
        assert!(t.contains("drumsd_slots{state=\"ready\"} 3\n"));
        assert!(t.contains("drumsd_reset_seconds_bucket{le=\"0.5\"} 1\n"));
        assert!(t.contains("drumsd_reset_seconds_bucket{le=\"1\"} 2\n"));
        assert!(t.contains("drumsd_reset_seconds_bucket{le=\"+Inf\"} 2\n"));
        assert!(t.contains("drumsd_reset_seconds_count 2\n"));
    }

    #[test]
    fn label_values_are_escaped() {
        let r = Registry::default();
        r.counter("c", "h");
        r.inc("c", &[("e", "a\"b\\c\nd")], 1.0);
        assert!(r.render().contains(r#"c{e="a\"b\\c\nd"} 1"#));
    }
}
