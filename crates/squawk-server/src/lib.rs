//! HTTP and WebSocket layer for the squawk server.
//!
//! The browser UI is the only client of this API today. It is deliberately
//! state-shaped rather than patch-shaped: every mutation returns the whole config plus
//! its validation problems, and the UI re-renders from that. Intercom configs are small
//! (tens of endpoints), and a UI that cannot drift out of step with the server is worth
//! far more than the bytes saved by returning deltas.

pub mod host;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use squawk_core::{Config, Endpoint, EndpointKind, KeyTarget, Partyline, Problem, TalkMode};

use crate::host::Host;

const INDEX_HTML: &str = include_str!("../static/index.html");

#[derive(Clone)]
pub struct AppState {
    config: Arc<RwLock<Config>>,
    host: Host,
    /// Where to persist on change. `None` in tests.
    path: Option<Arc<PathBuf>>,
}

impl AppState {
    pub fn new(config: Config, path: Option<PathBuf>) -> Self {
        let host = Host::spawn(config.clone());
        Self {
            config: Arc::new(RwLock::new(config)),
            host,
            path: path.map(Arc::new),
        }
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    pub fn config(&self) -> Config {
        self.config.read().expect("config lock").clone()
    }
}

/// Everything the UI needs to draw itself.
#[derive(Debug, Serialize)]
pub struct StateResponse {
    pub config: Config,
    pub problems: Vec<Problem>,
    /// Streams the engine mixes — one per key, regardless of transport.
    pub total_streams: usize,
    /// Of those, how many go out as discrete AES67 streams. The rest belong to
    /// endpoints whose keys get folded into one Opus mix.
    pub aes67_streams: usize,
    /// Outbound packets per second implied by `aes67_streams` at the configured packet
    /// time. Surfaced because packet rate, not bandwidth, is what limits this design.
    pub packets_per_second: u64,
}

type ApiError = (StatusCode, Json<serde_json::Value>);

fn bad_request(message: impl Into<String>) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": message.into() })),
    )
}

fn describe(config: &Config) -> StateResponse {
    let problems = config.validate();
    let aes67_streams = config.aes67_stream_count();
    let packets_per_second = if config.system.ptime_samples > 0 {
        let per_stream =
            config.system.sample_rate as u64 / config.system.ptime_samples as u64;
        per_stream * aes67_streams as u64
    } else {
        0
    };
    StateResponse {
        total_streams: config.endpoints.iter().map(|e| e.keys.len()).sum(),
        aes67_streams,
        packets_per_second,
        problems,
        config: config.clone(),
    }
}

/// Apply an edit: validate, persist, hand to the engine, and describe the result.
///
/// Errors reject the whole edit; warnings do not. A one-way direct key or an empty
/// partyline is a normal intermediate state while someone is patching, and refusing it
/// would make the UI unusable.
fn commit(state: &AppState, next: Config) -> Result<Json<StateResponse>, ApiError> {
    let described = describe(&next);
    if let Some(first) = described
        .problems
        .iter()
        .find(|p| p.severity == squawk_core::Severity::Error)
    {
        return Err(bad_request(first.message.clone()));
    }

    if let Some(path) = &state.path {
        match next.to_toml() {
            Ok(text) => {
                if let Err(err) = std::fs::write(path.as_ref(), text) {
                    tracing::error!(%err, path = %path.display(), "could not persist config");
                }
            }
            Err(err) => tracing::error!(%err, "could not serialise config"),
        }
    }

    *state.config.write().expect("config lock") = next.clone();
    state.host.rebuild(next);
    Ok(Json(described))
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn get_state(State(state): State<AppState>) -> Json<StateResponse> {
    Json(describe(&state.config()))
}

async fn get_meters(State(state): State<AppState>) -> Json<host::Snapshot> {
    Json(state.host.snapshot())
}

#[derive(Deserialize)]
pub struct NewPartyline {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub colour: Option<String>,
}

async fn add_partyline(
    State(state): State<AppState>,
    Json(req): Json<NewPartyline>,
) -> Result<Json<StateResponse>, ApiError> {
    let mut cfg = state.config();
    if cfg.partyline(&req.id.as_str().into()).is_some() {
        return Err(bad_request(format!("a partyline called '{}' already exists", req.id)));
    }
    let mut p = Partyline::new(req.id, req.name);
    p.colour = req.colour;
    cfg.partylines.push(p);
    commit(&state, cfg)
}

/// Removing a partyline also removes every key pointing at it.
///
/// The alternative — leaving the keys and reporting them as errors — would mean a
/// delete that puts the system into a state the engine refuses to build, which is a
/// worse place to leave someone than a cascade they can undo.
async fn delete_partyline(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<StateResponse>, ApiError> {
    let mut cfg = state.config();
    let before = cfg.partylines.len();
    cfg.partylines.retain(|p| p.id.as_str() != id);
    if cfg.partylines.len() == before {
        return Err(bad_request(format!("no partyline called '{id}'")));
    }
    let target = KeyTarget::Partyline(id.as_str().into());
    for e in &mut cfg.endpoints {
        e.keys.retain(|k| k.target != target);
    }
    commit(&state, cfg)
}

#[derive(Deserialize)]
pub struct NewEndpoint {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: EndpointKind,
}

async fn add_endpoint(
    State(state): State<AppState>,
    Json(req): Json<NewEndpoint>,
) -> Result<Json<StateResponse>, ApiError> {
    let mut cfg = state.config();
    if cfg.endpoint(&req.id.as_str().into()).is_some() {
        return Err(bad_request(format!("an endpoint called '{}' already exists", req.id)));
    }
    let mut e = Endpoint::new(req.id, req.name);
    e.kind = req.kind;
    cfg.endpoints.push(e);
    commit(&state, cfg)
}

/// Removing an endpoint also removes every direct key pointing at it, for the same
/// reason as [`delete_partyline`].
async fn delete_endpoint(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<StateResponse>, ApiError> {
    let mut cfg = state.config();
    let before = cfg.endpoints.len();
    cfg.endpoints.retain(|e| e.id.as_str() != id);
    if cfg.endpoints.len() == before {
        return Err(bad_request(format!("no endpoint called '{id}'")));
    }
    let target = KeyTarget::Direct(id.as_str().into());
    for e in &mut cfg.endpoints {
        e.keys.retain(|k| k.target != target);
    }
    commit(&state, cfg)
}

#[derive(Deserialize)]
pub struct AssignRequest {
    pub endpoint: String,
    pub target: KeyTarget,
}

/// Give an endpoint a key pointing at a target — the operation the matrix cell performs.
///
/// For a direct target this creates **both** halves of the pair. A direct key with no
/// key back carries silence, so making the UI able to produce one would be making it
/// able to produce a dead button.
async fn assign(
    State(state): State<AppState>,
    Json(req): Json<AssignRequest>,
) -> Result<Json<StateResponse>, ApiError> {
    let mut cfg = state.config();

    let Some(ep) = cfg.endpoint_mut(&req.endpoint.as_str().into()) else {
        return Err(bad_request(format!("no endpoint called '{}'", req.endpoint)));
    };
    if ep.keys.iter().any(|k| k.target == req.target) {
        return Err(bad_request(format!(
            "'{}' already has a key pointing there; a second one would break mix-minus",
            req.endpoint
        )));
    }
    if ep.assign(req.target.clone()).is_none() {
        return Err(bad_request(format!(
            "'{}' has no free key slots (limit is {})",
            req.endpoint,
            squawk_core::MAX_KEYS
        )));
    }

    if let KeyTarget::Direct(other_id) = &req.target {
        let back = KeyTarget::Direct(req.endpoint.as_str().into());
        let Some(other) = cfg.endpoint_mut(other_id) else {
            return Err(bad_request(format!("no endpoint called '{other_id}'")));
        };
        if !other.keys.iter().any(|k| k.target == back) && other.assign(back).is_none() {
            return Err(bad_request(format!(
                "'{other_id}' has no free key slot for the other half of the direct pair"
            )));
        }
    }

    commit(&state, cfg)
}

#[derive(Deserialize)]
pub struct UnassignRequest {
    pub endpoint: String,
    pub slot: u8,
}

async fn unassign(
    State(state): State<AppState>,
    Json(req): Json<UnassignRequest>,
) -> Result<Json<StateResponse>, ApiError> {
    let mut cfg = state.config();
    let Some(ep) = cfg.endpoint_mut(&req.endpoint.as_str().into()) else {
        return Err(bad_request(format!("no endpoint called '{}'", req.endpoint)));
    };
    let before = ep.keys.len();
    ep.keys.retain(|k| k.slot != req.slot);
    if ep.keys.len() == before {
        return Err(bad_request(format!(
            "'{}' has no key in slot {}",
            req.endpoint, req.slot
        )));
    }
    commit(&state, cfg)
}

#[derive(Deserialize)]
pub struct KeySettings {
    pub endpoint: String,
    pub slot: u8,
    #[serde(default)]
    pub listen_level_db: Option<f32>,
    #[serde(default)]
    pub listen_muted: Option<bool>,
    #[serde(default)]
    pub talk_mode: Option<TalkMode>,
}

async fn update_key(
    State(state): State<AppState>,
    Json(req): Json<KeySettings>,
) -> Result<Json<StateResponse>, ApiError> {
    let mut cfg = state.config();
    let Some(ep) = cfg.endpoint_mut(&req.endpoint.as_str().into()) else {
        return Err(bad_request(format!("no endpoint called '{}'", req.endpoint)));
    };
    let Some(key) = ep.keys.iter_mut().find(|k| k.slot == req.slot) else {
        return Err(bad_request(format!(
            "'{}' has no key in slot {}",
            req.endpoint, req.slot
        )));
    };
    if let Some(db) = req.listen_level_db {
        key.listen_level_db = db;
    }
    if let Some(muted) = req.listen_muted {
        key.listen_muted = muted;
    }
    if let Some(mode) = req.talk_mode {
        key.talk_mode = mode;
    }
    commit(&state, cfg)
}

#[derive(Deserialize)]
pub struct EndpointSettings {
    pub endpoint: String,
    #[serde(default)]
    pub input_gain_db: Option<f32>,
    #[serde(default)]
    pub input_muted: Option<bool>,
    #[serde(default)]
    pub kind: Option<EndpointKind>,
}

async fn update_endpoint(
    State(state): State<AppState>,
    Json(req): Json<EndpointSettings>,
) -> Result<Json<StateResponse>, ApiError> {
    let mut cfg = state.config();
    let Some(ep) = cfg.endpoint_mut(&req.endpoint.as_str().into()) else {
        return Err(bad_request(format!("no endpoint called '{}'", req.endpoint)));
    };
    if let Some(db) = req.input_gain_db {
        ep.input_gain_db = db;
    }
    if let Some(muted) = req.input_muted {
        ep.input_muted = muted;
    }
    if let Some(kind) = req.kind {
        ep.kind = kind;
    }
    commit(&state, cfg)
}

#[derive(Deserialize)]
pub struct TalkRequest {
    pub endpoint: String,
    pub slot: u8,
    pub on: bool,
}

/// Talk is runtime state, not config: it goes straight to the engine and is never
/// written to disk. Nobody wants yesterday's latched keys restored on boot.
async fn set_talk(
    State(state): State<AppState>,
    Json(req): Json<TalkRequest>,
) -> impl IntoResponse {
    state.host.set_talk(&req.endpoint, req.slot, req.on);
    StatusCode::NO_CONTENT
}

async fn clear_talk(State(state): State<AppState>) -> impl IntoResponse {
    state.host.clear_all_talk();
    StatusCode::NO_CONTENT
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| push_meters(socket, state))
}

/// Push a meter snapshot at the host's publish rate. Read-only — every mutation goes
/// through the REST API, so there is no command parsing here to get wrong.
async fn push_meters(mut socket: WebSocket, state: AppState) {
    let mut ticker = tokio::time::interval(Duration::from_millis(50));
    loop {
        ticker.tick().await;
        let snapshot = state.host.snapshot();
        let Ok(text) = serde_json::to_string(&snapshot) else {
            continue;
        };
        if socket.send(Message::Text(text)).await.is_err() {
            break;
        }
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/state", get(get_state))
        .route("/api/meters", get(get_meters))
        .route("/api/partylines", post(add_partyline))
        .route("/api/partylines/:id", axum::routing::delete(delete_partyline))
        .route("/api/endpoints", post(add_endpoint))
        .route("/api/endpoints/:id", axum::routing::delete(delete_endpoint))
        .route("/api/endpoint-settings", post(update_endpoint))
        .route("/api/assign", post(assign))
        .route("/api/unassign", post(unassign))
        .route("/api/key", post(update_key))
        .route("/api/talk", post(set_talk))
        .route("/api/talk/clear", post(clear_talk))
        .route("/ws", get(ws_handler))
        .with_state(state)
}
