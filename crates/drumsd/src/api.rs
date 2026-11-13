//! HTTP/JSON API (spec: api/openapi.json, served at GET /openapi.json).

use crate::manager::{view, ApiError, LeaseOpts, Manager, QuiesceOpts};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use drumsd_core::admission::Need;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::{page_ids, SlotsQ, DEFAULT_PAGE, MAX_PAGE};

    fn ids(n: usize) -> Vec<(String, bool)> {
        // s100..: every third one destroyed
        let mut v: Vec<(String, bool)> = (0..n).map(|i| (format!("s{}", 100 + i), i % 3 == 0)).collect();
        v.sort();
        v
    }
    fn q(incl: Option<bool>, limit: Option<usize>, after: Option<&str>) -> SlotsQ {
        SlotsQ { include_destroyed: incl, limit, after: after.map(|s| s.to_string()) }
    }

    #[test]
    fn slots_default_is_live_only_unpaged() {
        let all = ids(30);
        let (out, next, paged) = page_ids(all.iter().map(|(i, d)| (i.as_str(), *d)), &q(None, None, None)).unwrap();
        assert!(!paged && next.is_none());
        assert_eq!(out.len(), 20);
        assert!(out.iter().all(|i| !all.iter().any(|(j, d)| j == i && *d)));
    }

    #[test]
    fn slots_pages_cover_everything_once() {
        let all = ids(1234);
        for incl in [false, true] {
            let mut seen = vec![];
            let mut after: Option<String> = None;
            loop {
                let (out, next, paged) = page_ids(all.iter().map(|(i, d)| (i.as_str(), *d)), &q(Some(incl), Some(100), after.as_deref())).unwrap();
                assert!(paged && out.len() <= 100);
                seen.extend(out);
                match next {
                    Some(n) => after = Some(n),
                    None => break,
                }
            }
            let want: Vec<String> = all.iter().filter(|(_, d)| incl || !d).map(|(i, _)| i.clone()).collect();
            assert_eq!(seen, want);
        }
    }

    #[test]
    fn slots_include_destroyed_has_a_default_page_and_limits() {
        let all = ids(DEFAULT_PAGE + 7);
        let (out, next, _) = page_ids(all.iter().map(|(i, d)| (i.as_str(), *d)), &q(Some(true), None, None)).unwrap();
        assert_eq!(out.len(), DEFAULT_PAGE);
        assert_eq!(next.as_deref(), out.last().map(|s| s.as_str()));
        // the last page ends with no cursor
        let (out2, next2, _) = page_ids(all.iter().map(|(i, d)| (i.as_str(), *d)), &q(Some(true), None, next.as_deref())).unwrap();
        assert_eq!(out2.len(), 7);
        assert!(next2.is_none());
        assert!(page_ids(std::iter::empty(), &q(None, Some(0), None)).is_err());
        assert!(page_ids(std::iter::empty(), &q(None, Some(MAX_PAGE + 1), None)).is_err());
        // a page that ends exactly at the end has no cursor
        let (_, n3, _) = page_ids(all.iter().map(|(i, d)| (i.as_str(), *d)), &q(Some(true), Some(all.len()), None)).unwrap();
        assert!(n3.is_none());
    }

    #[test]
    fn requests_that_name_an_env_are_found() {
        assert_eq!(super::env_of("/v1/slots/s104/step"), Some("s104"));
        assert_eq!(super::env_of("/v1/slots/s104"), Some("s104"));
        assert_eq!(super::env_of("/v1/slots"), None);
        assert_eq!(super::env_of("/v1/slots/"), None);
        assert_eq!(super::env_of("/v1/checkpoints/x"), None);
    }

    #[test]
    fn token_compare() {
        assert!(super::token_eq(b"abc", b"abc"));
        assert!(!super::token_eq(b"abc", b"abd"));
        assert!(!super::token_eq(b"abc", b"abcd"));
    }
}

pub const OPENAPI: &str = include_str!("../../../api/openapi.json");

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut body = json!({"error": self.message});
        if let Some(Value::Object(m)) = self.detail {
            for (k, v) in m {
                if k != "error" {
                    body[k] = v;
                }
            }
        }
        (code, Json(body)).into_response()
    }
}

type S = State<Arc<Manager>>;
type J = Result<Json<Value>, ApiError>;

#[derive(Deserialize)]
pub struct AdoptReq {
    pub id: String,
    pub slot: u32,
    pub expected_state_hash: Option<String>,
}
#[derive(Deserialize)]
pub struct CreateReq {
    /// Checkpoint id: an installed image (e.g. `osworld-ubuntu`) or a saved state. `image` and `from_state` are aliases.
    #[serde(alias = "image", alias = "from_state")]
    pub checkpoint: String,
    pub task: Option<Value>,
    /// Run the task's setup after every restore (default: true for a task given here).
    pub setup: Option<bool>,
    /// Lease: destroy the env after this many seconds with no request naming it (default: the daemon's
    /// `lease.env_idle_timeout_s`, 1800; 0 = never).
    pub idle_timeout_s: Option<f64>,
    /// Lease: destroy the env this many seconds after it was created (default: `lease.env_ttl_s`, none; 0 = never).
    pub ttl_s: Option<f64>,
}
#[derive(Deserialize)]
pub struct TaskReq {
    pub task: Value,
}
#[derive(Deserialize)]
pub struct TasksQ {
    pub domain: Option<String>,
}
#[derive(Deserialize)]
pub struct StepReq {
    pub action: Value,
    #[serde(default)]
    pub screenshot: bool,
}
#[derive(Deserialize, Default)]
pub struct CaptureReq {
    pub id: Option<String>,
    /// Capture readiness overrides (seconds): how long to wait for a quiescent tree, and the quiet window.
    pub quiesce_timeout_s: Option<f64>,
    pub quiesce_window_s: Option<f64>,
    /// The saved state's lease: deleted after this many seconds with no env made from it (default
    /// `lease.state_idle_timeout_s`, 1800; 0 = never) ...
    pub idle_timeout_s: Option<f64>,
    /// ... or this many seconds after it was made, once no env made from it exists (default none; 0 = never).
    pub ttl_s: Option<f64>,
}
#[derive(Deserialize)]
pub struct BranchReq {
    pub n: usize,
    pub quiesce_timeout_s: Option<f64>,
    pub quiesce_window_s: Option<f64>,
    /// The children's lease (as in create).
    pub idle_timeout_s: Option<f64>,
    pub ttl_s: Option<f64>,
    /// The saved state's lease (as in capture).
    pub state_idle_timeout_s: Option<f64>,
    pub state_ttl_s: Option<f64>,
    /// Delete the saved state the fork made as soon as no env made from it exists (all children closed).
    #[serde(default)]
    pub delete_state_with_children: bool,
}
#[derive(Deserialize, Default)]
pub struct QuiescentReq {
    pub timeout_s: Option<f64>,
    pub window_s: Option<f64>,
}
#[derive(Deserialize, Default)]
pub struct QuarantineReq {
    pub reason: Option<String>,
}

/// Unauthenticated liveness: no counts, no names.
async fn healthz() -> Json<Value> {
    Json(json!({"ok": true}))
}
async fn health(State(m): S) -> Json<Value> {
    Json(m.health())
}
async fn metrics_json(State(m): S) -> Json<Value> {
    Json(m.metrics_summary())
}
async fn metrics_reset(State(m): S) -> Json<Value> {
    Json(m.reset_metrics())
}
async fn slot_metrics(State(m): S, Path(id): Path<String>) -> J {
    Ok(Json(m.slot_metrics(&id)?))
}
async fn verify(State(m): S, Path(id): Path<String>) -> J {
    Ok(Json(m.verify(&id).await?))
}
async fn load_task(State(m): S, Path(id): Path<String>, Json(r): Json<TaskReq>) -> J {
    Ok(Json(view(&m.load_task(&id, r.task).await?)))
}
async fn tasks(State(m): S, Query(q): Query<TasksQ>) -> J {
    Ok(Json(json!(m.tasks(q.domain.as_deref())?)))
}
async fn task(State(m): S, Path(id): Path<String>) -> J {
    Ok(Json(m.task(&id)?))
}

/// Constant-time comparison (length is not secret: tokens have a fixed length).
pub fn token_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn auth(State(m): S, req: Request, next: Next) -> Response {
    // /provision/<capability>/...: the OSWorld helper of a running setup call; the one-time capability in the path is
    // its authorization (checked by the handler), not the API token, which the unprivileged helper never holds
    if req.uri().path() == "/healthz" || req.uri().path().starts_with("/provision/") {
        return next.run(req).await;
    }
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| token_eq(t.trim().as_bytes(), m.token.as_bytes()))
        .unwrap_or(false);
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            Json(json!({"error": "missing or wrong bearer token (Authorization: Bearer <token from the install>)"})),
        )
            .into_response();
    }
    // every request that names an env is a use of it (its lease's idle clock), at the start and at the end
    let env = env_of(req.uri().path()).map(|s| s.to_string());
    if let Some(id) = &env {
        m.touch(id);
    }
    let resp = next.run(req).await;
    if let Some(id) = &env {
        m.touch(id);
    }
    resp
}

/// The env id a request path names (`/v1/slots/{id}` or `/v1/slots/{id}/...`).
pub fn env_of(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/v1/slots/")?;
    let id = rest.split('/').next()?;
    (!id.is_empty()).then_some(id)
}
async fn metrics(State(m): S) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], m.metrics.render())
}
async fn openapi() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], OPENAPI)
}
async fn list_ckpts(State(m): S) -> Json<Value> {
    Json(json!(m.ckpts.read().unwrap().values().map(|c| m.ckpt_view(c)).collect::<Vec<_>>()))
}
async fn adopt(State(m): S, Json(r): Json<AdoptReq>) -> J {
    Ok(Json(m.ckpt_view(&m.adopt_checkpoint(r.id, r.slot, r.expected_state_hash, None).await?)))
}
async fn get_ckpt(State(m): S, Path(id): Path<String>) -> J {
    Ok(Json(m.ckpt_view(&m.ckpt(&id)?)))
}
#[derive(Deserialize, Default)]
pub struct DeleteCkptQ {
    /// Also delete an installed image's checkpoint (refused while any env made from it exists).
    pub installed: Option<bool>,
}
async fn delete_ckpt(State(m): S, Path(id): Path<String>, Query(q): Query<DeleteCkptQ>) -> J {
    Ok(Json(m.delete_checkpoint(&id, q.installed.unwrap_or(false)).await?))
}
#[derive(Deserialize, Default)]
pub struct SlotsQ {
    /// Also list destroyed envs (their records are kept). Default: live envs only.
    pub include_destroyed: Option<bool>,
    /// Page size (1..=MAX_PAGE). With destroyed envs included and no limit, DEFAULT_PAGE.
    pub limit: Option<usize>,
    /// Cursor: list ids strictly after this one (ids in byte order; `next_after` of the previous page).
    pub after: Option<String>,
}

pub const DEFAULT_PAGE: usize = 500;
pub const MAX_PAGE: usize = 5000;

/// Which ids a `GET /v1/slots` page holds, from (id, destroyed) in id order, and the cursor of the next page.
/// `paged` is false for the plain call (no query): every live env, no cursor (the 0.2 SDK's shape).
pub fn page_ids<'a>(all: impl Iterator<Item = (&'a str, bool)>, q: &SlotsQ) -> Result<(Vec<String>, Option<String>, bool), String> {
    let paged = q.include_destroyed.is_some() || q.limit.is_some() || q.after.is_some();
    let incl = q.include_destroyed.unwrap_or(false);
    let limit = match q.limit {
        Some(0) => return Err("limit must be at least 1".into()),
        Some(n) if n > MAX_PAGE => return Err(format!("limit must be at most {MAX_PAGE}")),
        Some(n) => Some(n),
        None if incl => Some(DEFAULT_PAGE),
        None => None,
    };
    let mut out = vec![];
    let mut next = None;
    for (id, destroyed) in all {
        if q.after.as_deref().is_some_and(|a| id <= a) || (destroyed && !incl) {
            continue;
        }
        if limit.is_some_and(|l| out.len() >= l) {
            next = out.last().cloned();
            break;
        }
        out.push(id.to_string());
    }
    Ok((out, next, paged))
}

/// Live envs by default (destroyed ones are kept as records and grew this list without bound: 22 MB at 256 envs).
/// Any query parameter selects the paged form `{"items": [...], "next_after": id or null}`.
async fn list_slots(State(m): S, Query(q): Query<SlotsQ>) -> J {
    let map = m.slots.read().unwrap();
    let flags: Vec<(&str, bool)> = map.iter().map(|(id, s)| (id.as_str(), s.view.read().unwrap().state.is_terminal())).collect();
    let (ids, next, paged) = page_ids(flags.into_iter(), &q).map_err(|e| ApiError::new(400, e))?;
    let items: Vec<Value> = ids.iter().filter_map(|i| map.get(i)).map(|s| view(&s.view.read().unwrap())).collect();
    Ok(Json(if paged { json!({"items": items, "next_after": next}) } else { json!(items) }))
}
async fn create(State(m): S, Json(r): Json<CreateReq>) -> Result<(StatusCode, Json<Value>), ApiError> {
    let _admitted = m.admit("create", Need { envs: 1, captures: 0 })?;
    let rec = m.create_leased(r.checkpoint, r.task, r.setup, LeaseOpts { idle_timeout_s: r.idle_timeout_s, ttl_s: r.ttl_s }).await?;
    Ok((StatusCode::CREATED, Json(view(&rec))))
}
async fn get_slot(State(m): S, Path(id): Path<String>) -> J {
    let s = m.slot(&id)?;
    m.check_crash(&s); // the verdict a client reads includes any crash since the last step
    let v = view(&s.view.read().unwrap());
    Ok(Json(v))
}
async fn gates(State(m): S, Path(id): Path<String>) -> J {
    let s = m.slot(&id)?;
    m.check_crash(&s);
    let v = s.view.read().unwrap();
    Ok(Json(json!({"slot": v.id, "episode": v.episode, "validity": v.validity, "gates": v.gates, "cost": v.cost})))
}
async fn reset(State(m): S, Path(id): Path<String>) -> J {
    Ok(Json(view(&m.reset(&id).await?)))
}
async fn step(State(m): S, Path(id): Path<String>, Json(r): Json<StepReq>) -> J {
    Ok(Json(m.step(&id, r.action, r.screenshot).await?))
}
async fn screenshot(State(m): S, Path(id): Path<String>) -> Result<Response, ApiError> {
    let png = m.screenshot(&id).await?;
    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}
async fn capture(State(m): S, Path(id): Path<String>, body: Option<Json<CaptureReq>>) -> J {
    let r = body.map(|b| b.0).unwrap_or_default();
    let o = QuiesceOpts { timeout_s: r.quiesce_timeout_s, window_s: r.quiesce_window_s };
    let _admitted = m.admit("capture", Need { envs: 0, captures: 1 })?;
    Ok(Json(json!(m.capture_leased(&id, r.id, o, LeaseOpts { idle_timeout_s: r.idle_timeout_s, ttl_s: r.ttl_s }, false).await?)))
}
async fn branch(State(m): S, Path(id): Path<String>, Json(r): Json<BranchReq>) -> J {
    if let Some(v) = m.claim_prefork(&id, r.n, r.delete_state_with_children).await {
        return Ok(Json(v));
    }
    let o = QuiesceOpts { timeout_s: r.quiesce_timeout_s, window_s: r.quiesce_window_s };
    let _admitted = m.admit("fork", Need { envs: r.n as u64, captures: 1 })?;
    let child = LeaseOpts { idle_timeout_s: r.idle_timeout_s, ttl_s: r.ttl_s };
    let state = LeaseOpts { idle_timeout_s: r.state_idle_timeout_s, ttl_s: r.state_ttl_s };
    Ok(Json(m.branch_leased(&id, r.n, o, child, state, r.delete_state_with_children).await?))
}
/// Prepare a fork now (capture + children in the background) for the next fork of this env; see Manager::prefork.
async fn prefork(State(m): S, Path(id): Path<String>, Json(r): Json<BranchReq>) -> J {
    let o = QuiesceOpts { timeout_s: r.quiesce_timeout_s, window_s: r.quiesce_window_s };
    let child = LeaseOpts { idle_timeout_s: r.idle_timeout_s, ttl_s: r.ttl_s };
    let state = LeaseOpts { idle_timeout_s: r.state_idle_timeout_s, ttl_s: r.state_ttl_s };
    Ok(Json(m.prefork(&id, r.n, o, child, state, r.delete_state_with_children).await?))
}
async fn quiescent(State(m): S, Path(id): Path<String>, body: Option<Json<QuiescentReq>>) -> J {
    let r = body.map(|b| b.0).unwrap_or_default();
    Ok(Json(m.wait_quiescent(&id, QuiesceOpts { timeout_s: r.timeout_s, window_s: r.window_s }).await?))
}
async fn quarantine(State(m): S, Path(id): Path<String>, body: Option<Json<QuarantineReq>>) -> J {
    let reason = body.and_then(|b| b.0.reason).unwrap_or_else(|| "operator".into());
    Ok(Json(view(&m.quarantine(&id, reason).await?)))
}
async fn release(State(m): S, Path(id): Path<String>) -> J {
    Ok(Json(view(&m.release(&id).await?)))
}
async fn provision_step(State(m): S, Path(token): Path<String>, Json(body): Json<Value>) -> J {
    Ok(Json(m.provision_step(&token, body).await?))
}
async fn destroy(State(m): S, Path(id): Path<String>) -> J {
    Ok(Json(view(&m.destroy(&id).await?)))
}

pub fn router(m: Arc<Manager>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/health", get(health))
        .route("/v1/metrics", get(metrics_json))
        .route("/v1/metrics/reset", post(metrics_reset))
        .route("/v1/tasks", get(tasks))
        .route("/v1/tasks/:id", get(task))
        .route("/v1/slots/:id/verify", post(verify))
        .route("/v1/slots/:id/task", post(load_task))
        .route("/v1/slots/:id/metrics", get(slot_metrics))
        .route("/metrics", get(metrics))
        .route("/openapi.json", get(openapi))
        .route("/v1/checkpoints", get(list_ckpts).post(adopt))
        .route("/v1/checkpoints/:id", get(get_ckpt).delete(delete_ckpt))
        .route("/v1/slots", get(list_slots).post(create))
        .route("/v1/slots/:id", get(get_slot).delete(destroy))
        .route("/v1/slots/:id/gates", get(gates))
        .route("/v1/slots/:id/reset", post(reset))
        .route("/v1/slots/:id/step", post(step))
        .route("/v1/slots/:id/screenshot", get(screenshot))
        .route("/v1/slots/:id/capture", post(capture))
        .route("/v1/slots/:id/branch", post(branch))
        .route("/v1/slots/:id/prefork", post(prefork))
        .route("/v1/slots/:id/quiescent", post(quiescent))
        .route("/v1/slots/:id/quarantine", post(quarantine))
        .route("/v1/slots/:id/release", post(release))
        .route("/provision/:token/setup/execute", post(provision_step))
        .layer(middleware::from_fn_with_state(m.clone(), auth))
        .with_state(m)
}
