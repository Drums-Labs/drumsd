//! Network-namespace comparison (ports of `candidate.normalize_net`, `diff_net`, `loopback_ok`) over `ip -j link/addr/route show`.

use serde_json::{json, Map, Value};

/// The comparable view: {"links": {name: {ifindex, mtu, up}}, "addresses": [[ifname, family, local, prefixlen, scope]], "routes": [[dst, dev, gateway]]}
pub fn normalize(links: &Value, addrs: &Value, routes: &Value) -> Value {
    let mut l = Map::new();
    for x in links.as_array().into_iter().flatten() {
        let up = x["flags"].as_array().map(|f| f.iter().any(|v| v == "UP")).unwrap_or(false);
        l.insert(x["ifname"].as_str().unwrap_or("").to_string(), json!({"ifindex": x["ifindex"], "mtu": x["mtu"], "up": up}));
    }
    let mut a = vec![];
    for x in addrs.as_array().into_iter().flatten() {
        for i in x["addr_info"].as_array().into_iter().flatten() {
            a.push(json!([x["ifname"], i["family"], i["local"], i["prefixlen"], i["scope"]]));
        }
    }
    let mut r = vec![];
    for x in routes.as_array().into_iter().flatten() {
        r.push(json!([x["dst"], x["dev"], x["gateway"]]));
    }
    json!({"links": l, "addresses": a, "routes": r})
}

pub fn loopback_ok(net: &Value) -> bool {
    let lo = &net["links"]["lo"];
    lo["up"] == true && net["addresses"].as_array().map(|a| a.contains(&json!(["lo", "inet", "127.0.0.1", 8, "host"]))).unwrap_or(false)
}

/// Differences between the checkpointed and the restored namespace (empty = equal).
pub fn diff(want: &Value, got: &Value) -> Vec<String> {
    let mut d = vec![];
    let empty = Map::new();
    let wl = want["links"].as_object().unwrap_or(&empty);
    let gl = got["links"].as_object().unwrap_or(&empty);
    let mut names: Vec<&String> = wl.keys().chain(gl.keys()).collect();
    names.sort();
    names.dedup();
    for n in names {
        if wl.get(n) != gl.get(n) {
            d.push(format!("link {n}: before {:?} after {:?}", wl.get(n), gl.get(n)));
        }
    }
    for k in ["addresses", "routes"] {
        let w = want[k].as_array().cloned().unwrap_or_default();
        let g = got[k].as_array().cloned().unwrap_or_default();
        for i in &w {
            if !g.contains(i) {
                d.push(format!("{k} missing after restore: {i}"));
            }
        }
        for i in &g {
            if !w.contains(i) {
                d.push(format!("{k} appeared after restore: {i}"));
            }
        }
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lo() -> Value {
        normalize(
            &json!([{"ifindex": 1, "ifname": "lo", "flags": ["LOOPBACK", "UP", "LOWER_UP"], "mtu": 65536, "operstate": "UNKNOWN"}]),
            &json!([{"ifindex": 1, "ifname": "lo", "addr_info": [{"family": "inet", "local": "127.0.0.1", "prefixlen": 8, "scope": "host"},
                                                                  {"family": "inet6", "local": "::1", "prefixlen": 128, "scope": "host"}]}]),
            &json!([]),
        )
    }

    #[test]
    fn loopback_namespace_compares_equal_to_itself() {
        let n = lo();
        assert!(loopback_ok(&n));
        assert!(diff(&n, &n).is_empty());
    }

    #[test]
    fn a_down_link_or_lost_address_is_reported() {
        let want = lo();
        let mut got = lo();
        got["links"]["lo"]["up"] = json!(false);
        got["addresses"].as_array_mut().unwrap().remove(1);
        let d = diff(&want, &got);
        assert_eq!(d.len(), 2, "{d:?}");
        assert!(!loopback_ok(&got));
    }

    #[test]
    fn matches_a_python_written_record() {
        // netstate-pre-dump.json as candidate.py writes it (json.dump of normalize_net)
        let py = json!({"addresses": [["lo", "inet", "127.0.0.1", 8, "host"], ["lo", "inet6", "::1", 128, "host"]],
                        "links": {"lo": {"ifindex": 1, "mtu": 65536, "up": true}}, "routes": []});
        assert!(diff(&py, &lo()).is_empty());
    }
}
