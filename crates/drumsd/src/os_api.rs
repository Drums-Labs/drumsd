//! HTTP/JSON API of `drumsd serve-os`: Android and Windows pools (drumsd-os) behind the same routes, auth and
//! response shapes as the Linux slots. Routes a backend cannot serve answer 501 with the reason.

use crate::api::token_eq;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use drumsd_os::pools::{OsError, OsPools};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct OsApp {
    pub pools: Arc<OsPools>,
    pub token: String,
}
type S = State<Arc<OsApp>>;
pub struct E(OsError);
impl From<OsError> for E {
    fn from(e: OsError) -> E {
        E(e)
    }
}
impl IntoResponse for E {
    fn into_response(self) -> Response {
        (StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), Json(json!({"error": self.0.message}))).into_response()
    }
}
type J = Result<Json<Value>, E>;

#[derive(Deserialize)]
struct CreateReq {
    #[serde(alias = "image", alias = "from_state")]
    checkpoint: String,
    task: Option<Value>,
}
#[derive(Deserialize)]
struct StepReq {
    action: Value,
    #[serde(default)]
    screenshot: bool,
}
#[derive(Deserialize, Default)]
struct CaptureReq {
    id: Option<String>,
}
#[derive(Deserialize)]
struct BranchReq {
    n: usize,
}
#[derive(Deserialize)]
struct TaskReq {
    task: Value,
}
#[derive(Deserialize, Default)]
struct QuarantineReq {
    reason: Option<String>,
}
#[derive(Deserialize, Default)]
struct SlotsQ {
    include_destroyed: Option<bool>,
}
#[derive(Deserialize, Default)]
struct TasksQ {
    os: Option<String>,
}

fn v<T: serde::Serialize>(x: T) -> Json<Value> {
    Json(serde_json::to_value(x).unwrap())
}

async fn healthz() -> Json<Value> {
    Json(json!({"ok": true}))
}
async fn health(State(a): S) -> Json<Value> {
    Json(a.pools.health())
}
async fn images(State(a): S) -> Json<Value> {
    Json(json!(a.pools.images()))
}
async fn list(State(a): S, Query(q): Query<SlotsQ>) -> Json<Value> {
    v(a.pools.list(q.include_destroyed.unwrap_or(false)))
}
async fn create(State(a): S, Json(r): Json<CreateReq>) -> Result<(StatusCode, Json<Value>), E> {
    Ok((StatusCode::CREATED, v(a.pools.create(&r.checkpoint, r.task).await?)))
}
async fn get_slot(State(a): S, Path(id): Path<String>) -> J {
    Ok(v(a.pools.slot(&id)?.view.read().unwrap().clone()))
}
async fn gates(State(a): S, Path(id): Path<String>) -> J {
    let s = a.pools.slot(&id)?;
    let x = s.view.read().unwrap();
    Ok(Json(json!({"slot": x.id, "episode": x.episode, "validity": {"verdict": x.verdict, "gates": x.gates}, "cost": x.cost})))
}
async fn reset(State(a): S, Path(id): Path<String>) -> J {
    Ok(v(a.pools.reset(&id).await?))
}
async fn step(State(a): S, Path(id): Path<String>, Json(r): Json<StepReq>) -> J {
    Ok(Json(a.pools.step(&id, r.action, r.screenshot).await?))
}
async fn screenshot(State(a): S, Path(id): Path<String>) -> Result<Response, E> {
    let png = a.pools.screenshot(&id).await?;
    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}
async fn capture(State(a): S, Path(id): Path<String>, body: Option<Json<CaptureReq>>) -> J {
    Ok(Json(a.pools.capture(&id, body.and_then(|b| b.0.id)).await?))
}
async fn branch(State(a): S, Path(id): Path<String>, Json(r): Json<BranchReq>) -> J {
    Ok(Json(a.pools.branch(&id, r.n).await?))
}
async fn verify(State(a): S, Path(id): Path<String>) -> J {
    Ok(Json(a.pools.verify(&id).await?))
}
async fn load_task(State(a): S, Path(id): Path<String>, Json(r): Json<TaskReq>) -> J {
    Ok(v(a.pools.load_task(&id, r.task).await?))
}
async fn quarantine(State(a): S, Path(id): Path<String>, body: Option<Json<QuarantineReq>>) -> J {
    Ok(v(a.pools.quarantine(&id, body.and_then(|b| b.0.reason).unwrap_or_else(|| "operator".into())).await?))
}
async fn release(State(a): S, Path(id): Path<String>) -> J {
    Ok(v(a.pools.release(&id).await?))
}
async fn destroy(State(a): S, Path(id): Path<String>) -> J {
    Ok(v(a.pools.destroy(&id).await?))
}
async fn tasks(State(a): S, Query(q): Query<TasksQ>) -> Json<Value> {
    Json(json!(a.pools.tasks(q.os.as_deref())))
}
async fn task(State(a): S, Path(id): Path<String>) -> J {
    a.pools.task(&id).map(Json).ok_or_else(|| E(OsError::new(404, format!("no task {id}"))))
}
async fn openapi() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], crate::api::OPENAPI)
}

async fn auth(State(a): S, req: Request, next: Next) -> Response {
    if req.uri().path() == "/healthz" {
        return next.run(req).await;
    }
    let ok = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(|t| token_eq(t.trim().as_bytes(), a.token.as_bytes())).unwrap_or(false);
    if !ok {
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], Json(json!({"error": "missing or wrong bearer token"}))).into_response();
    }
    next.run(req).await
}

pub fn router(a: Arc<OsApp>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/health", get(health))
        .route("/openapi.json", get(openapi))
        .route("/v1/checkpoints", get(images))
        .route("/v1/tasks", get(tasks))
        .route("/v1/tasks/:id", get(task))
        .route("/v1/slots", get(list).post(create))
        .route("/v1/slots/:id", get(get_slot).delete(destroy))
        .route("/v1/slots/:id/gates", get(gates))
        .route("/v1/slots/:id/reset", post(reset))
        .route("/v1/slots/:id/step", post(step))
        .route("/v1/slots/:id/screenshot", get(screenshot))
        .route("/v1/slots/:id/capture", post(capture))
        .route("/v1/slots/:id/branch", post(branch))
        .route("/v1/slots/:id/verify", post(verify))
        .route("/v1/slots/:id/task", post(load_task))
        .route("/v1/slots/:id/quarantine", post(quarantine))
        .route("/v1/slots/:id/release", post(release))
        .layer(middleware::from_fn_with_state(a.clone(), auth))
        .with_state(a)
}

pub async fn serve(cfg_path: &str) {
    let cfg = match drumsd_os::pools::OsConfig::load(cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("drumsd serve-os: {e}");
            std::process::exit(2);
        }
    };
    let ip: Option<std::net::IpAddr> = cfg.listen.rsplit_once(':').and_then(|(h, _)| h.trim_matches(['[', ']']).parse().ok());
    if !cfg.allow_remote && !ip.is_some_and(|i| i.is_loopback()) {
        eprintln!("drumsd serve-os: listen {} is not loopback; set allow_remote", cfg.listen);
        std::process::exit(2);
    }
    let token = match cfg.token_file.as_ref().map(std::fs::read_to_string) {
        Some(Ok(t)) if t.trim().len() >= 32 => t.trim().to_string(),
        _ => {
            eprintln!("drumsd serve-os: token_file missing, unreadable or shorter than 32 characters");
            std::process::exit(2);
        }
    };
    let listen = cfg.listen.clone();
    let pools = match OsPools::new(cfg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("drumsd serve-os: {e}");
            std::process::exit(2);
        }
    };
    pools.cleanup_stale().await;
    let app = Arc::new(OsApp { pools, token });
    let l = tokio::net::TcpListener::bind(&listen).await.expect("bind");
    tracing::info!("drumsd serve-os listening on {listen}");
    axum::serve(l, router(app)).await.unwrap();
}
