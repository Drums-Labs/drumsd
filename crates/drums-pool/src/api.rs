//! The pool's HTTP API: drumsd's API (api/openapi.json) over many hosts, plus /pool/v1 for operators.
//!
//! A client (the drums SDK, the gateway) points DRUMS_URL at the pool and changes nothing else. Differences a client
//! can see, all additive: records carry `pool: {host, region}` and no host-local `url`; a create waits for capacity
//! instead of answering 429 (X-Drums-Queue-Timeout bounds the wait); an env whose host died answers 409
//! `episode_lost` (reset continues on a new host) or 410 `host_lost`.

use crate::config::HostSpec;
use crate::pool::{now_epoch, EnvStatus, HostState, MakeErr, PoolEnv, SavedState, P};
use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::{json, Value};
use std::time::Instant;

const MAX_BODY: usize = 64 << 20;

pub fn router(p: P) -> Router {
    Router::new().fallback(handle).with_state(p)
}

fn token_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|s| s.strip_prefix("Bearer "))
}

fn jerr(status: u16, msg: impl Into<String>, extra: Value) -> Response {
    let mut b = json!({"error": msg.into()});
    if let Value::Object(m) = extra {
        for (k, v) in m {
            b[k] = v;
        }
    }
    (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), axum::Json(b)).into_response()
}

fn jok(v: Value) -> Response {
    (StatusCode::OK, axum::Json(v)).into_response()
}

fn raw(status: u16, body: Vec<u8>, ct: &str) -> Response {
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    if !ct.is_empty() {
        if let Ok(v) = ct.parse() {
            r.headers_mut().insert(header::CONTENT_TYPE, v);
        }
    }
    r
}

/// Op label for metrics: the slot sub-resource, or the collection verb.
fn op_of(method: &Method, segs: &[&str]) -> String {
    match (method.as_str(), segs) {
        ("POST", ["v1", "slots"]) => "create".into(),
        ("GET", ["v1", "slots"]) => "list".into(),
        ("GET", ["v1", "slots", _]) => "get".into(),
        ("DELETE", ["v1", "slots", _]) => "close".into(),
        (_, ["v1", "slots", _, op]) => op.to_string(),
        (_, ["v1", x, ..]) => x.to_string(),
        (_, ["pool", ..]) => "admin".into(),
        _ => "other".into(),
    }
}

async fn handle(State(p): State<P>, req: Request) -> Response {
    let t0 = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
    let headers = req.headers().clone();
    let segs: Vec<String> = path.trim_matches('/').split('/').map(|s| percent_decode(s)).collect();
    let segs_ref: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    let op = op_of(&method, &segs_ref);
    if path == "/healthz" {
        return jok(json!({"ok": true}));
    }
    let admin = segs_ref.first() == Some(&"pool");
    let want = if admin { p.admin_token.as_bytes() } else { p.token.as_bytes() };
    if !bearer(&headers).is_some_and(|t| token_eq(t.as_bytes(), want)) {
        return jerr(401, "missing or wrong bearer token", json!({}));
    }
    let body = match to_bytes(req.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(e) => return jerr(400, format!("body: {e}"), json!({})),
    };
    let jbody: Option<Value> = if body.is_empty() { None } else {
        match serde_json::from_slice(&body) {
            Ok(v) => Some(v),
            Err(e) => return jerr(400, format!("body is not JSON: {e}"), json!({})),
        }
    };
    let mut up = 0.0f64;
    let resp = route(&p, &method, &segs_ref, &query, &headers, jbody, &mut up).await;
    let total = t0.elapsed().as_secs_f64();
    let code = resp.status().as_u16().to_string();
    p.metrics.observe("pool_request_seconds", &[("op", &op)], total);
    p.metrics.inc("pool_requests_total", &[("op", &op), ("code", &code)]);
    if up > 0.0 {
        p.metrics.observe("pool_overhead_seconds", &[("op", &op)], (total - up).max(0.0));
    }
    resp
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn route(p: &P, m: &Method, s: &[&str], query: &str, headers: &HeaderMap, body: Option<Value>, up: &mut f64) -> Response {
    match (m.as_str(), s) {
        ("GET", ["metrics"]) => {
            p.update_gauges();
            raw(200, p.metrics.render().into_bytes(), "text/plain; version=0.0.4")
        }
        ("GET", ["openapi.json"]) => any_host(p, "GET", "/openapi.json", up).await,
        ("GET", ["v1", "health"]) => jok(health(p)),
        ("GET", ["v1", "metrics"]) => fanout(p, "GET", "/v1/metrics", up).await,
        ("POST", ["v1", "metrics", "reset"]) => fanout(p, "POST", "/v1/metrics/reset", up).await,
        ("GET", ["v1", "tasks"]) => any_host(p, "GET", &format!("/v1/tasks{query}"), up).await,
        ("GET", ["v1", "tasks", id]) => any_host(p, "GET", &format!("/v1/tasks/{}", enc(id)), up).await,
        ("GET", ["v1", "checkpoints"]) => checkpoints(p, up).await,
        (_, ["v1", "checkpoints", id]) => checkpoint_one(p, m, id, up).await,
        ("GET", ["v1", "slots"]) => list(p, query),
        ("POST", ["v1", "slots"]) => create(p, headers, body, up).await,
        (_, ["v1", "slots", id]) => env_op(p, m, id, "", query, body, up).await,
        (_, ["v1", "slots", id, op]) => env_op(p, m, id, op, query, body, up).await,
        ("GET", ["pool", "v1", "hosts"]) => jok(hosts_view(p)),
        ("POST", ["pool", "v1", "hosts"]) => register(p, body).await,
        ("POST", ["pool", "v1", "hosts", name, "drain"]) | ("DELETE", ["pool", "v1", "hosts", name]) => drain(p, name),
        ("GET", ["pool", "v1", "envs"]) => jok(envs_view(p)),
        _ => jerr(404, "not found", json!({})),
    }
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn healthy_hosts(p: &P) -> Vec<String> {
    let hs = p.hosts.lock().unwrap();
    let mut v: Vec<(f64, String)> = hs.iter().filter(|(_, h)| h.state == HostState::Healthy).map(|(n, h)| (h.snap.as_ref().map(|s| s.rtt_s).unwrap_or(9.0), n.clone())).collect();
    v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    v.into_iter().map(|x| x.1).collect()
}

async fn any_host(p: &P, m: &str, path: &str, up: &mut f64) -> Response {
    for h in healthy_hosts(p).into_iter().take(3) {
        let t = Instant::now();
        let r = p.upstream(&h, m.parse().unwrap(), path, None, p.cfg.request_timeout_s).await;
        *up += t.elapsed().as_secs_f64();
        if let Ok((s, b, ct)) = r {
            return raw(s, b, &ct);
        }
    }
    jerr(503, "no healthy host in the pool", json!({"code": "no_host"}))
}

async fn fanout(p: &P, m: &str, path: &str, up: &mut f64) -> Response {
    let t = Instant::now();
    let mut js = tokio::task::JoinSet::new();
    for h in healthy_hosts(p) {
        let p2 = p.clone();
        let (m, path) = (m.to_string(), path.to_string());
        js.spawn(async move {
            let r = p2.upstream(&h, m.parse().unwrap(), &path, Some(&json!({})).filter(|_| m == "POST"), 60.0).await;
            (h, r.ok().and_then(|(s, b, _)| if s < 300 { serde_json::from_slice::<Value>(&b).ok() } else { None }))
        });
    }
    let mut hosts = serde_json::Map::new();
    while let Some(Ok((h, v))) = js.join_next().await {
        hosts.insert(h, v.unwrap_or(Value::Null));
    }
    *up += t.elapsed().as_secs_f64();
    jok(json!({"pool": true, "hosts": hosts}))
}

/// Pool health in drumsd's shape. The fingerprint (version, image_sha256, criu, limits, leases) is reported only
/// when every healthy host agrees on it: a pool of mixed runtimes answers ok=false (a gateway then refuses creates).
fn health(p: &P) -> Value {
    let hs = p.hosts.lock().unwrap();
    let mut warnings: Vec<String> = vec![];
    let mut fp: Option<Value> = None;
    let mut agree = true;
    let mut by_state = serde_json::Map::new();
    let mut hv = vec![];
    for (n, h) in hs.iter() {
        hv.push(json!({"name": n, "state": h.state.name(), "region": h.spec.region, "envs": h.owned, "warm": h.warm.len()}));
        if h.state != HostState::Healthy {
            if h.state != HostState::Draining {
                warnings.push(format!("host {n} is {}", h.state.name()));
            }
            continue;
        }
        let Some(s) = &h.snap else { continue };
        let r = &s.raw;
        let f = json!({"version": r["version"], "image_sha256": r["image_sha256"], "criu": {"sha256": r["criu"]["sha256"], "nested_pidns": r["criu"]["nested_pidns"]},
                       "limits": {"vcpus": r["limits"]["vcpus"], "memory_mib": r["limits"]["memory_mib"], "swap_mib": r["limits"]["swap_mib"]}, "leases": r["leases"]["defaults"]});
        match &fp {
            None => fp = Some(f),
            Some(x) if *x != f => agree = false,
            _ => {}
        }
        if let Some(m) = r["slots_by_state"].as_object() {
            for (k, v) in m {
                let c = by_state.get(k).and_then(|x| x.as_u64()).unwrap_or(0) + v.as_u64().unwrap_or(0);
                by_state.insert(k.clone(), json!(c));
            }
        }
    }
    let healthy = hs.values().filter(|h| h.state == HostState::Healthy).count();
    drop(hs);
    if !agree {
        warnings.push("healthy hosts report different runtimes (version, image, criu or limits): the pool reports no fingerprint".into());
    }
    let fpv = if agree { fp.clone().unwrap_or(Value::Null) } else { Value::Null };
    let ckpts: Vec<Value> = vec![json!({"id": p.cfg.warm_image, "stale_reason": null})];
    json!({"ok": healthy > 0 && agree, "pool": true, "pool_version": env!("CARGO_PKG_VERSION"),
           "version": fpv["version"], "image_sha256": fpv["image_sha256"], "criu": fpv["criu"], "limits": fpv["limits"],
           "leases": {"defaults": fpv["leases"]}, "checkpoints": ckpts,
           "slots_by_state": by_state, "hosts": hv, "hosts_healthy": healthy,
           "queue_depth": p.queue.tickets.lock().unwrap().len(), "warnings": warnings, "uptime_s": p.started.elapsed().as_secs_f64()})
}

fn hosts_view(p: &P) -> Value {
    let now = Instant::now();
    let hs = p.hosts.lock().unwrap();
    let v: Vec<Value> = hs.iter().map(|(n, h)| {
        let s = h.snap.clone().unwrap_or_default();
        json!({"name": n, "url": h.spec.url, "region": h.spec.region, "state": h.state.name(), "cpus": h.spec.cpus, "max_envs": h.spec.max_envs,
               "owned": h.owned, "warm": h.warm.len(), "inflight": h.inflight, "warm_inflight": h.warm_inflight,
               "mem_total_mib": s.mem_total_mib, "mem_available_mib": s.mem_avail_mib, "load1": s.load1, "live_slots": s.live_slots,
               "env_mib_estimate": h.env_mib_est, "health_rtt_s": s.rtt_s, "fails": h.fails,
               "headroom": crate::pool::headroom(&p.cfg, h, now).map_err(|b| b.name())})
    }).collect();
    json!({"hosts": v})
}

fn envs_view(p: &P) -> Value {
    let envs = p.envs.lock().unwrap();
    let v: Vec<Value> = envs.values().map(|e| json!({"id": e.id, "host": e.host, "slot": e.slot, "status": format!("{:?}", e.status), "failovers": e.failovers})).collect();
    json!({"envs": v})
}

async fn register(p: &P, body: Option<Value>) -> Response {
    let Some(b) = body else { return jerr(400, "body: a host spec", json!({})) };
    let spec: HostSpec = match serde_json::from_value(b) {
        Ok(s) => s,
        Err(e) => return jerr(400, format!("host spec: {e}"), json!({})),
    };
    let name = spec.name.clone();
    if let Err(e) = p.add_host(spec) {
        return jerr(409, e, json!({}));
    }
    p.clone().poll_host(name.clone()).await;
    let st = p.hosts.lock().unwrap().get(&name).map(|h| h.state.name()).unwrap_or("gone");
    jok(json!({"name": name, "state": st}))
}

fn drain(p: &P, name: &str) -> Response {
    let mut hs = p.hosts.lock().unwrap();
    match hs.get_mut(name) {
        Some(h) => {
            h.state = HostState::Draining;
            drop(hs);
            p.event(json!({"ev": "host_draining", "host": name}));
            jok(json!({"name": name, "state": "draining"}))
        }
        None => jerr(404, format!("no host {name}"), json!({})),
    }
}

// ---------------------------------------------------------------------------------------------- checkpoints
async fn checkpoints(p: &P, up: &mut f64) -> Response {
    let mut out: Vec<Value> = vec![];
    if let Some(h) = healthy_hosts(p).first() {
        let t = Instant::now();
        if let Ok((200, b, _)) = p.upstream(h, reqwest::Method::GET, "/v1/checkpoints", None, 30.0).await {
            if let Ok(Value::Array(a)) = serde_json::from_slice::<Value>(&b) {
                out.extend(a.into_iter().filter(|c| c["captured_from"].is_null()));
            }
        }
        *up += t.elapsed().as_secs_f64();
    }
    for s in p.states.lock().unwrap().values() {
        out.push(json!({"id": s.id, "pool": {"host": s.host}, "lost": s.lost}));
    }
    jok(Value::Array(out))
}

async fn checkpoint_one(p: &P, m: &Method, id: &str, up: &mut f64) -> Response {
    if id.starts_with("ps-") {
        let st = p.states.lock().unwrap().get(id).cloned();
        let Some(s) = st else { return jerr(404, format!("no saved state {id}"), json!({})) };
        if s.lost {
            if m == Method::DELETE {
                p.states.lock().unwrap().remove(id);
                return jok(json!({"id": id, "deleted": true, "lost": true}));
            }
            return jerr(410, format!("saved state {id} was on host {} which died", s.host), json!({"code": "state_lost"}));
        }
        let t = Instant::now();
        let r = p.upstream(&s.host, m.as_str().parse().unwrap(), &format!("/v1/checkpoints/{}", enc(&s.host_id)), None, p.cfg.request_timeout_s).await;
        *up += t.elapsed().as_secs_f64();
        return match r {
            Ok((st, b, ct)) => {
                if m == Method::DELETE && (st < 300 || st == 404) {
                    p.states.lock().unwrap().remove(id);
                }
                let v = serde_json::from_slice::<Value>(&b).ok().map(|mut v| {
                    rewrite_ckpt(&mut v, id, &s.host);
                    v
                });
                match v {
                    Some(v) => (StatusCode::from_u16(st).unwrap_or(StatusCode::BAD_GATEWAY), axum::Json(v)).into_response(),
                    None => raw(st, b, &ct),
                }
            }
            Err(e) => jerr(503, format!("host {}: {e}", s.host), json!({})),
        };
    }
    if m != Method::GET {
        return jerr(400, "images are installed per host; only saved states (ps-...) can be deleted through the pool", json!({}));
    }
    any_host(p, "GET", &format!("/v1/checkpoints/{}", enc(id)), up).await
}

fn rewrite_ckpt(v: &mut Value, pool_id: &str, host: &str) {
    if v.is_object() {
        v["id"] = json!(pool_id);
        v["pool"] = json!({"host": host});
        if let Some(o) = v.as_object_mut() {
            for k in ["socket", "argv_marker", "server_pid"] {
                o.remove(k);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------- envs
fn region_of(p: &P, host: &str) -> String {
    p.hosts.lock().unwrap().get(host).map(|h| h.spec.region.clone()).unwrap_or_default()
}

fn rewrite_slot(p: &P, v: &mut Value, pool_id: &str, host: &str, slot: &str) {
    p.pool_view(v, pool_id, host, slot)
}

fn state_pool_id(p: &P, host: &str, host_id: &str) -> Option<String> {
    p.states.lock().unwrap().values().find(|s| s.host == host && s.host_id == host_id).map(|s| s.id.clone())
}

fn list(p: &P, query: &str) -> Response {
    let envs = p.envs.lock().unwrap();
    let mut v: Vec<&PoolEnv> = envs.values().collect();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    let recs: Vec<Value> = v.iter().map(|e| {
        let mut r = e.record.clone();
        if !r.is_object() {
            r = json!({"id": e.id});
        }
        r["id"] = json!(e.id);
        r
    }).collect();
    drop(envs);
    let q: std::collections::HashMap<String, String> = query.trim_start_matches('?').split('&').filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect();
    if let Some(limit) = q.get("limit").and_then(|l| l.parse::<usize>().ok()) {
        let after = q.get("after").cloned();
        let items: Vec<Value> = recs.into_iter().filter(|r| after.as_ref().is_none_or(|a| r["id"].as_str().unwrap_or("") > a.as_str())).take(limit).collect();
        let next = if items.len() == limit { items.last().and_then(|r| r["id"].as_str().map(|s| s.to_string())) } else { None };
        return jok(json!({"items": items, "next_after": next}));
    }
    jok(Value::Array(recs))
}

async fn create(p: &P, headers: &HeaderMap, body: Option<Value>, up: &mut f64) -> Response {
    let b = body.unwrap_or(json!({}));
    let Some(ck) = b.get("checkpoint").or(b.get("image")).or(b.get("from_state")).and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return jerr(400, "checkpoint (or image / from_state) is required", json!({}));
    };
    let task = b.get("task").cloned().filter(|t| !t.is_null());
    let setup = b.get("setup").and_then(|v| v.as_bool());
    let idle = b.get("idle_timeout_s").and_then(|v| v.as_f64()).unwrap_or(p.cfg.default_idle_timeout_s);
    let ttl = b.get("ttl_s").and_then(|v| v.as_f64()).unwrap_or(p.cfg.default_ttl_s);
    if !idle.is_finite() || idle < 0.0 || !ttl.is_finite() || ttl < 0.0 {
        return jerr(400, "idle_timeout_s / ttl_s: finite and >= 0", json!({}));
    }
    let qt = headers.get("x-drums-queue-timeout").and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(p.cfg.queue_timeout_s);
    let host_ckpt = if ck.starts_with("ps-") {
        match p.states.lock().unwrap().get(&ck) {
            None => return jerr(404, format!("no saved state {ck}"), json!({})),
            Some(s) if s.lost => return jerr(410, format!("saved state {ck} was on host {} which died", s.host), json!({"code": "state_lost"})),
            Some(s) => Some((s.host.clone(), s.host_id.clone())),
        }
    } else {
        None
    };
    let t = Instant::now();
    let r = p.make_env(&ck, host_ckpt, task.clone(), setup, idle, ttl, false, qt).await;
    *up += t.elapsed().as_secs_f64();
    match r {
        Ok((host, slot, mut rec)) => {
            let id = p.new_env_id();
            rewrite_slot(p, &mut rec, &id, &host, &slot);
            let now = Instant::now();
            p.envs.lock().unwrap().insert(id.clone(), PoolEnv { id: id.clone(), host, slot, checkpoint: ck, task, setup, idle_timeout_s: idle, ttl_s: ttl,
                created_epoch: now_epoch(), created: now, last_used: now, status: EnvStatus::Live, record: rec.clone(), failovers: 0 });
            jok(rec)
        }
        Err(MakeErr::QueueTimeout) => jerr(503, format!("no capacity in the pool within {qt} s (backpressure: the create waited in the queue)"), json!({"code": "queue_timeout"})),
        Err(MakeErr::QueueFull) => jerr(503, "the pool's create queue is full", json!({"code": "queue_full"})),
        Err(MakeErr::StateLost) => jerr(410, format!("saved state {ck} is gone (its host died)"), json!({"code": "state_lost"})),
        Err(MakeErr::Unavailable(e)) => jerr(503, e, json!({"code": "host_unavailable"})),
        Err(MakeErr::Upstream(s, b, ct)) => raw(s, b, &ct),
    }
}

fn lost_msg(id: &str, from: &str) -> String {
    format!("env {id}: its host {from} died and the episode was lost; the env was recreated on another host: reset it to continue")
}

async fn env_op(p: &P, m: &Method, id: &str, op: &str, query: &str, body: Option<Value>, up: &mut f64) -> Response {
    let e = p.envs.lock().unwrap().get(id).cloned();
    let Some(mut e) = e else {
        if let Some(why) = p.grave(id) {
            if m == Method::DELETE {
                return jerr(410, format!("env {id} is closed ({why})"), json!({"code": "env_gone", "closed_reason": why}));
            }
            return jerr(410, format!("env {id} is closed ({why})"), json!({"code": "env_gone", "closed_reason": why}));
        }
        return jerr(404, format!("no env {id}"), json!({}));
    };
    let is_close = m == Method::DELETE && op.is_empty();
    let is_get = m == Method::GET && op.is_empty();
    let is_reset = m == Method::POST && op == "reset";
    // a replacement still being made: a reset waits for it; everything else learns the episode is gone
    if matches!(e.status, EnvStatus::Replacing { .. }) && is_reset {
        let t = Instant::now();
        while t.elapsed().as_secs_f64() < p.cfg.queue_timeout_s {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            match p.envs.lock().unwrap().get(id).cloned() {
                Some(x) if matches!(x.status, EnvStatus::Replacing { .. }) => continue,
                Some(x) => {
                    e = x;
                    break;
                }
                None => return jerr(410, format!("env {id} is closed"), json!({"code": "env_gone"})),
            }
        }
    }
    match e.status.clone() {
        EnvStatus::Lost { reason } => {
            if is_close {
                p.envs.lock().unwrap().remove(id);
                p.bury(id, "host_lost");
                return jok(json!({"id": id, "state": "destroyed", "closed_reason": reason}));
            }
            return jerr(410, format!("env {id}: {reason}"), json!({"code": "host_lost", "closed_reason": reason}));
        }
        EnvStatus::Replacing { from_host, .. } => {
            if is_close {
                // the failover task sees the env gone and deletes the replacement
                p.envs.lock().unwrap().remove(id);
                p.bury(id, "closed");
                return jok(json!({"id": id, "state": "destroyed", "closed_reason": "closed during failover"}));
            }
            return jerr(409, lost_msg(id, &from_host), json!({"code": "episode_lost", "from_host": from_host}));
        }
        EnvStatus::NeedsReset { from_host, at } => {
            if is_reset {
                // the replacement is a fresh episode nobody has touched: that is what a reset makes
                let mut rec = e.record.clone();
                rec["pool_failover"] = json!({"from_host": from_host, "at": at});
                if let Some(x) = p.envs.lock().unwrap().get_mut(id) {
                    x.status = EnvStatus::Live;
                    x.last_used = Instant::now();
                    x.record = rec.clone();
                }
                p.metrics.inc("pool_failover_resets_total", &[]);
                return jok(rec);
            }
            if is_get {
                let mut rec = e.record.clone();
                rec["pool_failover"] = json!({"from_host": from_host, "at": at, "needs_reset": true});
                return jok(rec);
            }
            if !is_close {
                return jerr(409, lost_msg(id, &from_host), json!({"code": "episode_lost", "from_host": from_host}));
            }
        }
        EnvStatus::Live => {}
    }
    if let Some(x) = p.envs.lock().unwrap().get_mut(id) {
        x.last_used = Instant::now();
    }
    let path = if op.is_empty() { format!("/v1/slots/{}{query}", enc(&e.slot)) } else { format!("/v1/slots/{}/{op}{query}", enc(&e.slot)) };
    let timeout = match op {
        "branch" | "capture" | "reset" | "task" => p.cfg.create_timeout_s,
        _ => p.cfg.request_timeout_s,
    };
    let t = Instant::now();
    let r = p.upstream(&e.host, m.as_str().parse().unwrap(), &path, body.as_ref(), timeout).await;
    let ut = t.elapsed().as_secs_f64();
    *up += ut;
    p.metrics.observe("pool_upstream_seconds", &[("op", if op.is_empty() { m.as_str() } else { op }), ("host", &e.host)], ut);
    let (st, b, ct) = match r {
        Ok(x) => x,
        Err(err) if err == "host_dead" => {
            if is_close {
                p.envs.lock().unwrap().remove(id);
                p.bury(id, "host_lost");
                return jok(json!({"id": id, "state": "destroyed", "closed_reason": "host died"}));
            }
            // the failover task owns the env now; the client learns its episode is gone
            return jerr(409, lost_msg(id, &e.host), json!({"code": "episode_lost", "from_host": e.host}));
        }
        Err(err) => {
            // the host did not answer: hold the request until the pool has judged the host (dead -> the episode is lost
            // and the env fails over; healthy again -> the outcome is unknown, as before), so a client sees one signal
            if p.await_host_verdict(&e.host).await {
                if is_close {
                    p.envs.lock().unwrap().remove(id);
                    p.bury(id, "host_lost");
                    return jok(json!({"id": id, "state": "destroyed", "closed_reason": "host died"}));
                }
                return jerr(409, lost_msg(id, &e.host), json!({"code": "episode_lost", "from_host": e.host}));
            }
            return jerr(503, format!("host {} unreachable on {} {}: {err} (the outcome is unknown; GET the env)", e.host, m, op), json!({"code": "host_unreachable"}));
        }
    };
    if is_close && (st < 300 || st == 404 || st == 410) {
        if p.envs.lock().unwrap().remove(id).is_some() {
            p.disown(&e.host);
        }
        p.bury(id, "closed");
    }
    if !ct.starts_with("application/json") {
        return raw(st, b, &ct);
    }
    let Ok(mut v) = serde_json::from_slice::<Value>(&b) else { return raw(st, b, &ct) };
    match op {
        "verify" if st < 300 => {
            let valid = v["valid"].as_bool().unwrap_or(false);
            let cerr = !v["checker_error"].is_null();
            p.metrics.inc("pool_verify_total", &[("valid", if valid { "true" } else { "false" }), ("checker_error", if cerr { "true" } else { "false" })]);
            if v["id"].as_str() == Some(&e.slot) {
                v["id"] = json!(id);
            }
        }
        "capture" if st < 300 => {
            let hid = v["id"].as_str().unwrap_or("").to_string();
            let sid = p.new_state_id();
            p.states.lock().unwrap().insert(sid.clone(), SavedState { id: sid.clone(), host: e.host.clone(), host_id: hid, lost: false });
            rewrite_ckpt(&mut v, &sid, &e.host);
            if v["captured_from"].as_str() == Some(&e.slot) {
                v["captured_from"] = json!(id);
            }
        }
        "branch" if st < 300 => {
            let hid = v["state"].as_str().or(v["checkpoint"]["id"].as_str()).unwrap_or("").to_string();
            let sid = p.new_state_id();
            p.states.lock().unwrap().insert(sid.clone(), SavedState { id: sid.clone(), host: e.host.clone(), host_id: hid, lost: false });
            v["state"] = json!(sid);
            rewrite_ckpt(&mut v["checkpoint"], &sid, &e.host);
            if v["checkpoint"]["captured_from"].as_str() == Some(&e.slot) {
                v["checkpoint"]["captured_from"] = json!(id);
            }
            let bi = body.clone().unwrap_or(json!({}));
            let cidle = bi["idle_timeout_s"].as_f64().unwrap_or(p.cfg.default_idle_timeout_s);
            let cttl = bi["ttl_s"].as_f64().unwrap_or(p.cfg.default_ttl_s);
            if let Some(children) = v["children"].as_array_mut() {
                for c in children.iter_mut() {
                    let Some(cslot) = c["id"].as_str().map(|s| s.to_string()) else { continue };
                    let cid = p.new_env_id();
                    rewrite_slot(p, c, &cid, &e.host, &cslot);
                    let now = Instant::now();
                    p.envs.lock().unwrap().insert(cid.clone(), PoolEnv { id: cid, host: e.host.clone(), slot: cslot, checkpoint: sid.clone(), task: e.task.clone(), setup: e.setup,
                        idle_timeout_s: cidle, ttl_s: cttl, created_epoch: now_epoch(), created: now, last_used: now, status: EnvStatus::Live, record: c.clone(), failovers: 0 });
                    if let Some(h) = p.hosts.lock().unwrap().get_mut(&e.host) {
                        h.owned += 1;
                    }
                }
            }
        }
        _ => {
            if v["id"].as_str() == Some(&e.slot) {
                rewrite_slot(p, &mut v, id, &e.host, &e.slot);
                if st < 300 && !is_close {
                    if let Some(x) = p.envs.lock().unwrap().get_mut(id) {
                        x.record = v.clone();
                    }
                }
            }
        }
    }
    (StatusCode::from_u16(st).unwrap_or(StatusCode::BAD_GATEWAY), axum::Json(v)).into_response()
}
