//! HTTP/JSON API (spec: api/openapi.json, served at GET /openapi.json).

use crate::manager::{view, ApiError, Manager};
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub const OPENAPI: &str = include_str!("../../../api/openapi.json");

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (code, Json(json!({"error": self.message}))).into_response()
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
    pub checkpoint: String,
    pub task: Option<Value>,
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
}
#[derive(Deserialize)]
pub struct BranchReq {
    pub n: usize,
}
#[derive(Deserialize, Default)]
pub struct QuarantineReq {
    pub reason: Option<String>,
}

async fn healthz(State(m): S) -> Json<Value> {
    let n = m.slots.read().unwrap().len();
    Json(json!({"ok": true, "slots": n, "checkpoints": m.ckpts.read().unwrap().len()}))
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
async fn list_slots(State(m): S) -> Json<Value> {
    let v: Vec<Value> = m.slots.read().unwrap().values().map(|s| view(&s.view.read().unwrap())).collect();
    Json(json!(v))
}
async fn create(State(m): S, Json(r): Json<CreateReq>) -> Result<(StatusCode, Json<Value>), ApiError> {
    let rec = m.create(r.checkpoint, r.task).await?;
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
    Ok(Json(json!(m.capture(&id, r.id).await?)))
}
async fn branch(State(m): S, Path(id): Path<String>, Json(r): Json<BranchReq>) -> J {
    Ok(Json(m.branch(&id, r.n).await?))
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
        .route("/metrics", get(metrics))
        .route("/openapi.json", get(openapi))
        .route("/v1/checkpoints", get(list_ckpts).post(adopt))
        .route("/v1/checkpoints/:id", get(get_ckpt))
        .route("/v1/slots", get(list_slots).post(create))
        .route("/v1/slots/:id", get(get_slot).delete(destroy))
        .route("/v1/slots/:id/gates", get(gates))
        .route("/v1/slots/:id/reset", post(reset))
        .route("/v1/slots/:id/step", post(step))
        .route("/v1/slots/:id/screenshot", get(screenshot))
        .route("/v1/slots/:id/capture", post(capture))
        .route("/v1/slots/:id/branch", post(branch))
        .route("/v1/slots/:id/quarantine", post(quarantine))
        .route("/v1/slots/:id/release", post(release))
        .with_state(m)
}
