//! Collector->GUI WebSocket subscription.
//!
//! On connect we:
//!   1. Send a full Snapshot (currently-firing alerts + host states).
//!   2. Subscribe to the broadcast channel and forward every Envelope.

use axum::extract::{ws::{Message, WebSocket, WebSocketUpgrade}, State};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tracing::{debug, warn};
use uma_shared::Envelope;

use crate::http::GuiState;

pub async fn handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<GuiState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |sock| handle_socket(sock, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<GuiState>) {
    let (mut tx, mut rx) = socket.split();
    let snapshot = state.app.snapshot();
    let initial = match serde_json::to_string(&Envelope::Snapshot(snapshot)) {
        Ok(s) => s,
        Err(e) => { warn!(error = %e, "snapshot serialize"); return; }
    };
    if tx.send(Message::Text(initial.into())).await.is_err() { return; }

    let mut feed = state.app.fanout.subscribe();
    loop {
        tokio::select! {
            evt = feed.recv() => {
                match evt {
                    Ok(env) => {
                        match serde_json::to_string(&env) {
                            Ok(s) => {
                                if tx.send(Message::Text(s.into())).await.is_err() { return; }
                            }
                            Err(e) => warn!(error = %e, "subscribe: serialize failed"),
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("subscribe: GUI client lagged by {n}; dropping");
                        return;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
            msg = rx.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => { debug!(error = %e, "subscribe: rx err"); return; }
                }
            }
        }
    }
}
