//! HTTP/JSON API (spec: api/openapi.json, served at GET /openapi.json).

use crate::manager::{view, ApiError, Manager, QuiesceOpts};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[cfg(test)]
mod tests {
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
}
#[derive(Deserialize)]
pub struct BranchReq {
    pub n: usize,
    pub quiesce_timeout_s: Option<f64>,
    pub quiesce_window_s: Option<f64>,
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
    if req.uri().path() == "/healthz" {
        return next.run(req).await;
    }
    let ok = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(|t| token_eq(t.trim().as_bytes(), m.token.as_bytes())).unwrap_or(false);
    if !ok {
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], Json(json!({"error": "missing or wrong bearer token (Authorization: Bearer <token from the install>)"}))).into_response();
    }
    next.run(req).await
}
async fn metrics(State(m): S) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], m.metrics.render())
}
async fn openapi() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], OPENAPI)
}
async fn list_ckpts(State(m): S) -> Json<Value> {
    Json(json!(m.ckpts.read().unwrap().values().cloned().collect::<Vec<_>>()))
}
async fn adopt(State(m): S, Json(r): Json<AdoptReq>) -> J {
    Ok(Json(json!(m.adopt_checkpoint(r.id, r.slot, r.expected_state_hash, None).await?)))
}
async fn get_ckpt(State(m): S, Path(id): Path<String>) -> J {
    Ok(Json(json!(m.ckpt(&id)?)))
}
async fn delete_ckpt(State(m): S, Path(id): Path<String>) -> J {
    Ok(Json(m.delete_checkpoint(&id).await?))
}
async fn list_slots(State(m): S) -> Json<Value> {
    let v: Vec<Value> = m.slots.read().unwrap().values().map(|s| view(&s.view.read().unwrap())).collect();
    Json(json!(v))
}
async fn create(State(m): S, Json(r): Json<CreateReq>) -> Result<(StatusCode, Json<Value>), ApiError> {
    let rec = m.create_with(r.checkpoint, r.task, r.setup).await?;
    Ok((StatusCode::CREATED, Json(view(&rec))))
}
async fn get_slot(State(m): S, Path(id): Path<String>) -> J {
    Ok(Json(view(&m.slot(&id)?.view.read().unwrap())))
}
async fn gates(State(m): S, Path(id): Path<String>) -> J {
    let s = m.slot(&id)?;
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
    Ok(Json(json!(m.capture_with(&id, r.id, o).await?)))
}
async fn branch(State(m): S, Path(id): Path<String>, Json(r): Json<BranchReq>) -> J {
    let o = QuiesceOpts { timeout_s: r.quiesce_timeout_s, window_s: r.quiesce_window_s };
    Ok(Json(m.branch_with(&id, r.n, o).await?))
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
        .route("/v1/slots/:id/quiescent", post(quiescent))
        .route("/v1/slots/:id/quarantine", post(quarantine))
        .route("/v1/slots/:id/release", post(release))
        .layer(middleware::from_fn_with_state(m.clone(), auth))
        .with_state(m)
}
