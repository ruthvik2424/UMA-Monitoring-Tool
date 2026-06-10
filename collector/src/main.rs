//! UMA collector entry point.
//!
//! Two listeners:
//!   - :9443  agent ingest (mTLS, WSS)        — `wss://collector/v1/ingest`
//!   - :8443  GUI               (TLS)         — `wss://collector/ws/subscribe`
//!                                              + HTTP /api/* and SPA assets
//!
//! Optional:
//!   - :6514/tcp+tls or :514/udp BMC syslog ingest

use anyhow::{Context, Result};
use axum::routing::get;
use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

const VERSION: &str = env!("CARGO_PKG_VERSION");

mod config;
mod http;
mod state;
mod store;
mod syslog_in;
#[cfg(feature = "webhooks")]
mod webhooks;
mod ws_ingest;
mod ws_subscribe;

use crate::config::CollectorConfig;
use crate::state::AppState;
use crate::store::Store;
use crate::ws_ingest::IngestState;

#[derive(Debug, Parser)]
#[command(name = "monitor-collector", version = VERSION, about = "Ferrous monitoring collector - Central aggregation and web interface")]
struct Cli {
    #[arg(long, env = "UMA_COLLECTOR_CONFIG", default_value = "/etc/monitor-collector/config.toml")]
    config: PathBuf,
    #[arg(long, default_value_t = false)]
    print_default_config: bool,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.print_default_config {
        println!("{}", DEFAULT_CONFIG);
        return Ok(());
    }
    let cfg = CollectorConfig::load(&cli.config)
        .context("loading collector config")?;
    init_tracing(&cfg.log_level);
    install_default_crypto_provider();

    let app = AppState::new(cfg.offline_after_s);
    let store = Store::open(&cfg.db_path, cfg.retention_days).await?;
    let writer = store.spawn_writer();

    spawn_offline_watcher(app.clone(), cfg.offline_after_s);

    if let Some(addr) = cfg.bind_syslog_udp {
        syslog_in::spawn_udp(addr, app.clone());
    }
    if let Some(addr) = cfg.bind_syslog_tls {
        // Plain TCP for now; TLS variant can be added with a separate rustls setup.
        syslog_in::spawn_tcp_plain(addr, app.clone());
    }

    // Outbound webhook fan-out (Slack / Google Chat / generic).
    #[cfg(feature = "webhooks")]
    if !cfg.webhooks.is_empty() {
        webhooks::spawn_all(cfg.webhooks.clone(), &app.fanout);
    }

    let ingest_state = Arc::new(IngestState { app: app.clone(), writer: writer.clone() });

    let ingest_router: Router = Router::new()
        .route("/v1/ingest", get(ws_ingest::handler))
        .with_state(ingest_state.clone());

    let gui_state = http::GuiState {
        app: app.clone(),
        store: store.clone(),
        writer: writer.clone(),
        config_path: cli.config.clone(),
    };
    let gui_router: Router = http::router(gui_state);

    spawn_tls_server(cfg.bind_ingest, &cfg.tls.server_cert, &cfg.tls.server_key, ingest_router).await?;
    spawn_tls_server(cfg.bind_gui, &cfg.tls.server_cert, &cfg.tls.server_key, gui_router).await?;

    info!(ingest = %cfg.bind_ingest, gui = %cfg.bind_gui, "uma collector running");
    futures_util::future::pending::<()>().await;
    Ok(())
}

async fn spawn_tls_server(
    addr: SocketAddr,
    cert: &std::path::Path,
    key: &std::path::Path,
    router: Router,
) -> Result<()> {
    let tls = RustlsConfig::from_pem_file(cert, key)
        .await
        .with_context(|| format!("loading server cert {:?}", cert))?;
    tokio::spawn(async move {
        if let Err(e) = axum_server::bind_rustls(addr, tls)
            .serve(router.into_make_service_with_connect_info::<SocketAddr>())
            .await
        {
            tracing::error!(error = %e, "tls server crashed");
        }
    });
    Ok(())
}

fn spawn_offline_watcher(app: AppState, after_s: u64) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        let mut last_seen_online: std::collections::HashMap<String, bool> =
            std::collections::HashMap::new();
        loop {
            tick.tick().await;
            let now = chrono::Utc::now();
            let hosts: Vec<_> = app.hosts.read().values().cloned().collect();
            for h in hosts {
                let online = (now - h.last_seen).num_seconds() <= after_s as i64;
                let prev = last_seen_online.insert(h.host_id.clone(), online);
                if Some(online) != prev {
                    let env = uma_shared::Envelope::HostStatus(uma_shared::HostStatus {
                        host: h.host.clone(),
                        host_id: h.host_id.clone(),
                        online,
                        last_seen: h.last_seen,
                        firing_count: h.firing_count,
                    });
                    app.broadcast(env);
                    if !online {
                        // Synthesize a host_offline alert.
                        let mut a = uma_shared::AlertBuilder::new(
                            h.host.clone(), h.host_id.clone(),
                            uma_shared::cat::COLLECTOR, "host_offline",
                            uma_shared::Severity::Critical,
                        )
                        .title("Host went offline")
                        .message(format!(
                            "No heartbeat from {} for over {}s. Either the agent stopped, the network is gone, or the host crashed.",
                            h.host, after_s
                        ))
                        .build_firing();
                        a.ts = now;
                        app.apply_alert(&a);
                        app.broadcast(uma_shared::Envelope::Alert(a));
                    } else {
                        // Resolve any host_offline alert.
                        let fp = format!("{}|{}|-|host_offline", h.host_id, uma_shared::cat::COLLECTOR);
                        let mut g = app.firing.write();
                        if let Some(mut a) = g.remove(&fp) {
                            a.state = uma_shared::AlertState::Resolved;
                            a.title = format!("Resolved: {}", a.title);
                            a.ts = now;
                            drop(g);
                            app.broadcast(uma_shared::Envelope::Alert(a));
                        }
                    }
                }
            }
        }
    });
}

fn init_tracing(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
}

fn install_default_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

const DEFAULT_CONFIG: &str = r#"
bind_ingest = "0.0.0.0:9443"
bind_gui    = "0.0.0.0:8443"
# Optional BMC syslog ingest:
# bind_syslog_udp = "0.0.0.0:514"
# bind_syslog_tls = "0.0.0.0:6514"

db_path = "/var/lib/monitor-collector/collector.sqlite"
retention_days = 30
offline_after_s = 10
log_level = "info"

[tls]
ca_cert     = "/etc/monitor-collector/ca.crt"
server_cert = "/etc/monitor-collector/server.crt"
server_key  = "/etc/monitor-collector/server.key"
require_client_cert = true
"#;
