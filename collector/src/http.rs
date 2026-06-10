//! HTTP routes for the GUI port.
//!
//!   GET  /             → serves the SPA (index.html)
//!   GET  /app.js, /style.css → static SPA assets
//!   GET  /api/snapshot → JSON snapshot of current state
//!   GET  /api/alerts?limit=N → recent alert history
//!   GET  /api/hosts    → host status list (heatmap data)
//!   POST /api/resolve  → mark an active alert as resolved (operator action)
//!   GET  /api/config   → read the live collector config.toml (v4)
//!   POST /api/config   → overwrite the live collector config.toml (v4)
//!   GET  /api/webhooks → list configured webhook entries summary (v4)
//!   GET  /ws/subscribe → WSS feed (handled in this module so it shares state)

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::warn;
use uma_shared::{Alert, AlertState, Envelope, HostState, Snapshot};

use crate::state::AppState;
use crate::store::Store;
use crate::ws_subscribe;

#[derive(Clone)]
pub struct GuiState {
    pub app: AppState,
    pub store: Store,
    pub writer: mpsc::Sender<Alert>,
    /// Path to the collector's own config.toml, used by the config-editor API.
    pub config_path: PathBuf,
}

pub fn router(state: GuiState) -> Router {
    let st = Arc::new(state);
    Router::new()
        .route("/", get(serve_index))
        .route("/app.js", get(serve_appjs))
        .route("/style.css", get(serve_css))
        .route("/api/snapshot", get(api_snapshot))
        .route("/api/alerts", get(api_alerts))
        .route("/api/hosts", get(api_hosts))
        .route("/api/resolve", post(api_resolve))
        .route("/api/history/clear", post(api_clear_history))
        .route("/api/config", get(api_get_config).post(api_post_config))
        .route("/api/webhooks", get(api_webhooks))
        .route("/ws/subscribe", get(ws_subscribe::handler))
        .with_state(st)
}

async fn serve_index() -> impl IntoResponse {
    static BODY: &str = include_str!("../../gui/index.html");
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], BODY)
}
async fn serve_appjs() -> impl IntoResponse {
    static BODY: &str = include_str!("../../gui/app.js");
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8")], BODY)
}
async fn serve_css() -> impl IntoResponse {
    static BODY: &str = include_str!("../../gui/style.css");
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], BODY)
}

async fn api_snapshot(State(s): State<Arc<GuiState>>) -> Json<Snapshot> {
    Json(s.app.snapshot())
}

#[derive(Debug, Deserialize)]
struct AlertsQuery {
    #[serde(default = "default_limit")]
    limit: i64,
}
fn default_limit() -> i64 { 500 }

async fn api_alerts(
    State(s): State<Arc<GuiState>>,
    Query(q): Query<AlertsQuery>,
) -> Result<Json<Vec<Alert>>, (StatusCode, String)> {
    s.store
        .list_recent(q.limit.clamp(1, 5000))
        .await
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn api_hosts(State(s): State<Arc<GuiState>>) -> Json<Vec<HostState>> {
    Json(s.app.snapshot().hosts)
}

#[derive(Debug, Deserialize)]
struct ResolveReq {
    fingerprint: String,
    /// Optional operator name / note. Captured into the resolved record's
    /// `raw` map so it surfaces in History detail.
    #[serde(default)]
    by: String,
}

/// Manual operator resolve. Drops the alert from the firing map,
/// synthesises a `state=resolved` event with the same fingerprint,
/// broadcasts to all GUI subscribers (so every browser updates), and
/// persists to SQLite history.
async fn api_resolve(
    State(s): State<Arc<GuiState>>,
    Json(req): Json<ResolveReq>,
) -> Response {
    let mut g = s.app.firing.write();
    let alert = match g.remove(&req.fingerprint) {
        Some(a) => a,
        None => return (StatusCode::NOT_FOUND, "fingerprint not in firing map").into_response(),
    };
    drop(g);

    // Decrement host firing count.
    {
        let mut hosts = s.app.hosts.write();
        if let Some(h) = hosts.get_mut(&alert.host_id) {
            if h.firing_count > 0 { h.firing_count -= 1; }
        }
    }

    let now = Utc::now();
    let mut resolved = alert.clone();
    resolved.id = ulid::Ulid::new().to_string();
    resolved.state = AlertState::Resolved;
    resolved.last_seen = now;
    resolved.ts = now;
    if !resolved.title.starts_with("Resolved:") {
        resolved.title = format!("Resolved: {}", resolved.title);
    }
    resolved.message = format!(
        "Manually resolved by operator{}. Original alert had {} occurrence(s) since {}.",
        if req.by.is_empty() { String::new() } else { format!(" '{}'", req.by) },
        alert.occurrences,
        alert.first_seen.to_rfc3339(),
    );
    resolved.raw.insert(
        "resolved_by".into(),
        serde_json::json!(if req.by.is_empty() { "operator" } else { &req.by }),
    );
    resolved.raw.insert("resolved_via".into(), serde_json::json!("gui"));

    // Persist + broadcast.
    if s.writer.try_send(resolved.clone()).is_err() {
        warn!("api_resolve: store writer queue full — resolve not persisted");
    }
    s.app.broadcast(Envelope::Alert(resolved));

    StatusCode::NO_CONTENT.into_response()
}

#[derive(Debug, serde::Serialize)]
struct ClearHistoryResp { deleted: u64 }

/// Operator action: wipe the persisted alert history. Active firing alerts
/// are unaffected. The GUI re-renders an empty History tab on the broadcast
/// signal we send below (every connected browser re-fetches /api/alerts).
async fn api_clear_history(State(s): State<Arc<GuiState>>) -> Response {
    match s.store.clear_all().await {
        Ok(n) => {
            // Tell every connected GUI to refresh history. We piggyback on
            // the existing Snapshot envelope so we don't need a new type.
            let snap = s.app.snapshot();
            s.app.broadcast(Envelope::Snapshot(snap));
            (StatusCode::OK, Json(ClearHistoryResp { deleted: n })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---- v4: Config editor API ----

/// Return the current collector config.toml contents as plain text.
/// The GUI settings page presents this in an editable textarea.
async fn api_get_config(State(s): State<Arc<GuiState>>) -> Response {
    match tokio::fs::read_to_string(&s.config_path).await {
        Ok(text) => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            text,
        ).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Overwrite the collector config.toml with the posted body (plain text).
/// A collector restart is still required for changes to take effect.
async fn api_post_config(
    State(s): State<Arc<GuiState>>,
    body: Bytes,
) -> Response {
    let text = match std::str::from_utf8(&body) {
        Ok(t) => t,
        Err(_) => return (StatusCode::BAD_REQUEST, "Body must be valid UTF-8").into_response(),
    };
    // Basic sanity: must contain at least one TOML-like key.
    if !text.contains('=') {
        return (StatusCode::BAD_REQUEST, "Does not look like a valid TOML config").into_response();
    }
    match tokio::fs::write(&s.config_path, text).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---- v4: Webhooks summary API ----

#[derive(Debug, serde::Serialize)]
struct WebhookSummary {
    name:    String,
    kind:    String,
    sev_min: String,
    active:  bool,
}

/// Return a summary list of configured webhooks for the settings UI.
async fn api_webhooks(State(s): State<Arc<GuiState>>) -> impl IntoResponse {
    #[cfg(feature = "webhooks")]
    {
        use crate::config::CollectorConfig;
        // Re-read the live config to surface current webhook definitions.
        let summaries: Vec<WebhookSummary> = match tokio::fs::read_to_string(&s.config_path).await {
            Ok(text) => match toml::from_str::<CollectorConfig>(&text) {
                Ok(cfg) => cfg.webhooks.iter().map(|w| WebhookSummary {
                    name:    w.name.clone(),
                    kind:    w.kind.clone(),
                    sev_min: w.severity_min.clone(),
                    active:  true,
                }).collect(),
                Err(_) => vec![],
            },
            Err(_) => vec![],
        };
        Json(summaries).into_response()
    }
    #[cfg(not(feature = "webhooks"))]
    {
        let _ = s; // suppress unused warning
        Json(Vec::<WebhookSummary>::new()).into_response()
    }
}
