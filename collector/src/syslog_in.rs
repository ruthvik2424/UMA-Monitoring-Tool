//! BMC syslog ingest (UDP and TCP+TLS).
//!
//! iLO and iDRAC can be configured to forward their event log via the
//! standard syslog protocol. We accept on :514/udp (legacy) and :6514/tcp+tls
//! (RFC 5425), parse the message, normalize into our Alert schema, and feed
//! it through the same fanout as agent-originated alerts.

use chrono::Utc;
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, UdpSocket};
use tracing::{debug, info, warn};
use uma_shared::{cat, Alert, AlertBuilder, Envelope, Severity};

use crate::state::AppState;

pub fn spawn_udp(addr: SocketAddr, app: AppState) {
    tokio::spawn(async move {
        let sock = match UdpSocket::bind(addr).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, %addr, "syslog/udp: bind failed");
                return;
            }
        };
        info!(%addr, "syslog/udp listening");
        let mut buf = vec![0u8; 8192];
        loop {
            let (n, src) = match sock.recv_from(&mut buf).await {
                Ok(x) => x,
                Err(e) => { warn!(error = %e, "udp recv"); continue; }
            };
            handle_line(&app, &buf[..n], src.ip().to_string()).await;
        }
    });
}

pub fn spawn_tcp_plain(addr: SocketAddr, app: AppState) {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => { warn!(error = %e, %addr, "syslog/tcp: bind failed"); return; }
        };
        info!(%addr, "syslog/tcp listening (plain — for testing only)");
        loop {
            let (sock, peer) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => { warn!(error = %e, "syslog/tcp accept"); continue; }
            };
            let app = app.clone();
            tokio::spawn(async move {
                let mut r = BufReader::new(sock);
                let mut line = String::new();
                while let Ok(n) = r.read_line(&mut line).await {
                    if n == 0 { break; }
                    handle_line(&app, line.as_bytes(), peer.ip().to_string()).await;
                    line.clear();
                }
            });
        }
    });
}

async fn handle_line(app: &AppState, bytes: &[u8], peer: String) {
    let s = String::from_utf8_lossy(bytes).trim().to_string();
    if s.is_empty() { return; }
    let alert = parse_to_alert(&s, &peer);
    let env = Envelope::Alert(alert.clone());
    app.apply_alert(&alert);
    app.broadcast(env);
    debug!(peer = %peer, "syslog: ingested 1 message");
}

/// Best-effort conversion: pull severity from the syslog PRI, map
/// well-known iLO/iDRAC tokens to a category, fall back to bmc_eventlog.
fn parse_to_alert(line: &str, peer: &str) -> Alert {
    // Syslog format: <PRI>VERSION TIMESTAMP HOSTNAME APP-NAME PROCID MSGID STRUCTURED-DATA MSG
    // PRI = facility*8 + severity. We only care about severity.
    let (pri, body) = if let Some(stripped) = line.strip_prefix('<') {
        if let Some(end) = stripped.find('>') {
            let prival: u8 = stripped[..end].parse().unwrap_or(13);
            (prival % 8, &stripped[end + 1..])
        } else {
            (6, line)
        }
    } else {
        (6, line)
    };
    let severity = match pri {
        0..=2 => Severity::Critical,
        3..=4 => Severity::Warning,
        _ => Severity::Info,
    };

    let lower = body.to_lowercase();
    let category = if lower.contains("ilo") || lower.contains("iml") {
        cat::BMC_EVENTLOG
    } else if lower.contains("idrac") || lower.contains("dell") {
        cat::BMC_EVENTLOG
    } else {
        cat::SYSLOG
    };

    let mut tags = HashMap::new();
    tags.insert("source", "bmc_syslog");

    let b = AlertBuilder::new(
        peer,           // hostname unknown; use peer IP as host
        peer,           // host_id ditto
        category,
        "syslog_event",
        severity,
    )
    .title("BMC syslog event")
    .message(body.to_string())
    .raw_kv("peer", serde_json::json!(peer))
    .raw_kv("raw", serde_json::json!(line));
    let mut a = b.build_firing();
    a.ts = Utc::now();
    a
}
