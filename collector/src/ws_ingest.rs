//! Agent->Collector WebSocket ingest.
//!
//! Each agent maintains one persistent WSS connection here. We:
//!   - Validate the client cert (mTLS done by axum-server-rustls).
//!   - Decode each frame as an Envelope.
//!   - Update collector AppState (firing map, host last_seen, host firing_count).
//!   - Persist alerts to SQLite via the writer mpsc.
//!   - Re-broadcast to all GUI subscribers.

use axum::extract::{ws::{Message, WebSocket, WebSocketUpgrade}, State};
use axum::response::IntoResponse;
use chrono::Utc;
use futures_util::stream::StreamExt;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uma_shared::{Alert, Envelope, HostStatus};

use crate::state::AppState;

#[derive(Clone)]
pub struct IngestState {
    pub app: AppState,
    pub writer: mpsc::Sender<Alert>,
}

pub async fn handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<IngestState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |sock| handle_socket(sock, state))
}

async fn handle_socket(mut sock: WebSocket, state: Arc<IngestState>) {
    let mut current_host: Option<(String, String)> = None; // (host, host_id)
    while let Some(msg) = sock.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => { warn!(error = %e, "ingest: ws recv error"); break; }
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => break,
        };
        let env: Envelope = match serde_json::from_str(&text) {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, payload = %text, "ingest: bad envelope");
                continue;
            }
        };
        match env {
            Envelope::Hello(h) => {
                info!(host = %h.host, ver = %h.agent_version, "ingest: hello");
                state.app.register_hello(&h);
                current_host = Some((h.host.clone(), h.host_id.clone()));
                state.app.broadcast(Envelope::Hello(h));
            }
            Envelope::Heartbeat(hb) => {
                state.app.touch_host(&hb.host, &hb.host_id, hb.ts, &hb.agent_version);
                state.app.broadcast(Envelope::Heartbeat(hb.clone()));
                if let Some(s) = state.app.host_status(&hb.host_id) {
                    if !s.online {
                        // shouldn't really happen — heartbeat is recent — but just in case
                        debug!("ingest: heartbeat from host marked offline");
                    }
                }
            }
            Envelope::Alert(mut a) => {
                if state.app.host_in_maintenance(&a.host_id) {
                    debug!(
                        host_id = %a.host_id,
                        fp = %a.fingerprint,
                        "ingest: alert ignored (host in maintenance)"
                    );
                    continue;
                }
                state.app.touch_host(&a.host, &a.host_id, a.ts, "");
                // Stamp primary_ip from collector's host state so it travels
                // with every alert record persisted to SQLite. This makes the
                // IP visible in History even for offline or reconnected hosts.
                if a.primary_ip.is_empty() {
                    a.primary_ip = state.app.host_primary_ip(&a.host_id);
                }
                let changed = state.app.apply_alert(&a);
                if state.writer.try_send(a.clone()).is_err() {
                    warn!("ingest: store writer queue full — alert not persisted");
                }
                if changed {
                    state.app.broadcast(Envelope::Alert(a));
                }
            }
            Envelope::Snapshot(_) | Envelope::HostStatus(_) => {
                // Agents don't send these; ignore.
            }
        }
    }
    if let Some((host, host_id)) = current_host {
        info!(%host, "ingest: agent disconnected");
        state.app.broadcast(Envelope::HostStatus(HostStatus {
            host,
            host_id,
            online: false,
            last_seen: Utc::now(),
            firing_count: 0,
        }));
    }
}
