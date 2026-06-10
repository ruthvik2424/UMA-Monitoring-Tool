//! UMA agent entry point.
//!
//!   - Loads config (TOML).
//!   - Detects vendor + builds host identity.
//!   - Spawns `kmsg` tailer (broadcast channel for all kmsg-listening modules).
//!   - Spawns each module as an independent task.
//!   - Spawns the heartbeat task.
//!   - Spawns the WSS transport with a critical-bypass mpsc lane.
//!   - Installs SIGTERM/SIGINT handler that does one final flush before exit.
//!
//! Maintenance: set `[agent] maintenance = true` or `log_level = "maintenance"` (also accepts the
//! typo `maintainance`) to suppress all outbound alerts while heartbeats + hello still run.

#[cfg(not(target_os = "linux"))]
compile_error!("uma-agent is Linux-only. Build with --target x86_64-unknown-linux-musl.");

use anyhow::Result;
use clap::Parser;
use regex::RegexSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};
use uma_shared::{AlertBuilder, Envelope, Heartbeat, Hello, Severity};

const VERSION: &str = env!("CARGO_PKG_VERSION");

mod bus;
mod config;
mod logging;
mod priv_cmd;
#[cfg(feature = "debug-http")]
mod debug_http;
mod host;
mod journal;
mod kmsg;
mod modules;
mod state;
mod transport;
mod vendor_detect;

use crate::bus::{AlertSink, Outbound};

#[derive(Debug, Parser)]
#[command(name = "monitor-agent", version = VERSION, about = "UMA monitoring agent - Ultra-low latency bare-metal hardware monitoring")]
struct Cli {
    #[arg(long, env = "UMA_CONFIG", default_value = "/etc/monitor-agent/config.toml")]
    config: PathBuf,
    #[arg(long, default_value_t = false)]
    print_default_config: bool,
    /// Print a JSON snapshot of thresholds, syslog rules, and probe command outputs, then exit.
    #[arg(long, default_value_t = false)]
    debug_snapshot: bool,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.print_default_config {
        println!("{}", include_str!("../config.example.toml"));
        return Ok(());
    }

    #[cfg(feature = "debug-http")]
    if cli.debug_snapshot {
        let cfg = config::AgentConfig::load(&cli.config)
            .unwrap_or_else(|e| {
                eprintln!("warning: cannot load config ({e}); using defaults");
                config::AgentConfig::default_fallback()
            });
        let log_filter = cfg.tracing_log_filter();
        init_tracing(&log_filter);
        let host = host::HostId::detect()?;
        let snapshot = debug_http::build_debug_snapshot(&cfg, &host).await;
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }
    
    #[cfg(not(feature = "debug-http"))]
    if cli.debug_snapshot {
        eprintln!("Error: --debug-snapshot requires debug-http feature (available in v3.0.0+)");
        std::process::exit(1);
    }

    let cfg = config::AgentConfig::load(&cli.config)
        .unwrap_or_else(|e| {
            eprintln!("warning: cannot load config ({e}); using defaults");
            config::AgentConfig::default_fallback()
        });

    let log_filter = cfg.tracing_log_filter();
    // _log_guards must stay alive for the whole process — dropping them flushes logs.
    let _log_guards = logging::init(&log_filter);
    info!(
        execution_log = %logging::execution_log_path().display(),
        error_log     = %logging::error_log_path().display(),
        "log files initialised"
    );
    enforce_oom_score_adjust();
    install_default_crypto_provider();
    let host = host::HostId::detect()?;
    let vendor = vendor_detect::VendorProfile::detect();
    info!(
        host = %host.host(), host_id = %host.host_id(),
        bmc = %vendor.info.bmc_vendor, gpus = ?vendor.info.gpu_vendors,
        "uma agent starting"
    );

    let (norm_tx, norm_rx) = mpsc::channel::<Outbound>(cfg.collector.send_queue_size);
    let (crit_tx, crit_rx) = mpsc::channel::<Outbound>(cfg.collector.critical_queue_size);
    let (fan_tx, _) = broadcast::channel::<uma_shared::Alert>(1024);
    #[cfg(feature = "maintenance")]
    let maintenance = cfg.maintenance_mode();
    #[cfg(not(feature = "maintenance"))]
    let maintenance = false;
    
    if maintenance {
        info!("maintenance mode: outbound alerts suppressed (collector will not receive alerts)");
    }
    let sink = AlertSink::new(norm_tx, crit_tx, fan_tx.clone(), maintenance);

    let kmsg_rx = match kmsg::spawn(2048) {
        Ok(rx) => Some(rx),
        Err(e) => {
            warn!(error = %e, "kmsg unavailable — kmsg-driven modules will be inert");
            None
        }
    };

    let journal_rx = match journal::spawn(4096) {
        Ok(rx) => Some(rx),
        Err(e) => {
            warn!(error = %e, "journald unavailable — syslog_rules will be inert");
            None
        }
    };

    // ---- Hello ----
    let hello = Hello {
        host: host.host().to_string(),
        host_id: host.host_id().to_string(),
        primary_ip: host.primary_ip().to_string(),
        agent_version: env!("CARGO_PKG_VERSION").into(),
        started_at: chrono::Utc::now(),
        vendor: vendor.info.clone(),
        tags: cfg.tags.clone(),
        maintenance,
    };
    sink.send(Outbound::Hello(hello));

    // ---- Module spawn ----
    let make_tracker = || state::AlertTracker::new(cfg.re_notify(), cfg.recovery());
    let syslog_nic_ignore =
        RegexSet::new(&cfg.modules.network_nic.ignore_regex).unwrap_or_else(|e| {
            warn!(
                error = %e,
                "network_nic.ignore_regex compile failed — syslog NIC ignore-list empty"
            );
            RegexSet::empty()
        });

    modules::nvme::spawn(host.clone(), cfg.modules.nvme.clone(), sink.clone(), make_tracker());
    modules::disk_smart::spawn(host.clone(), cfg.modules.disk_smart.clone(), sink.clone(), make_tracker(), vendor.clone());
    modules::thermal::spawn(host.clone(), cfg.modules.thermal.clone(), sink.clone(), make_tracker(), vendor.clone());
    modules::network_nic::spawn(host.clone(), cfg.modules.network_nic.clone(), sink.clone(), make_tracker());
    modules::storage_controller::spawn(
        host.clone(), cfg.modules.storage_controller.clone(), sink.clone(),
        make_tracker(), vendor.clone(),
        kmsg_rx.as_ref().map(|r| r.resubscribe()).unwrap_or_else(dummy_kmsg),
    );
    modules::storage_io::spawn(
        host.clone(), cfg.modules.storage_io.clone(), sink.clone(),
        make_tracker(),
        kmsg_rx.as_ref().map(|r| r.resubscribe()).unwrap_or_else(dummy_kmsg),
        sink.subscribe(),
    );
    modules::mempressure::spawn(
        host.clone(), cfg.modules.mempressure.clone(), sink.clone(),
        make_tracker(),
        kmsg_rx.as_ref().map(|r| r.resubscribe()).unwrap_or_else(dummy_kmsg),
    );
    modules::oshang::spawn(
        host.clone(), cfg.modules.oshang.clone(), sink.clone(),
        make_tracker(),
        kmsg_rx.as_ref().map(|r| r.resubscribe()).unwrap_or_else(dummy_kmsg),
    );
    modules::memory_ecc::spawn(
        host.clone(), cfg.modules.memory_ecc.clone(), sink.clone(),
        make_tracker(),
    );
    modules::cpu_mce::spawn(
        host.clone(), cfg.modules.cpu_mce.clone(), sink.clone(),
        make_tracker(),
        kmsg_rx.as_ref().map(|r| r.resubscribe()).unwrap_or_else(dummy_kmsg),
    );
    modules::pcie_aer::spawn(
        host.clone(), cfg.modules.pcie_aer.clone(), sink.clone(),
        make_tracker(),
        kmsg_rx.as_ref().map(|r| r.resubscribe()).unwrap_or_else(dummy_kmsg),
    );
    modules::syslog_rules::spawn(
        host.clone(),
        cfg.modules.syslog_rules.clone(),
        syslog_nic_ignore,
        sink.clone(),
        make_tracker(),
        journal_rx.as_ref().map(|r| r.resubscribe()).unwrap_or_else(dummy_journal),
    );
    modules::gpu_nvidia::spawn(host.clone(), cfg.modules.gpu_nvidia.clone(), sink.clone(), make_tracker(), vendor.clone());
    modules::gpu_amd::spawn(host.clone(), cfg.modules.gpu_amd.clone(), sink.clone(), make_tracker(), vendor.clone());
    modules::bmc_redfish::spawn(host.clone(), cfg.modules.bmc_redfish.clone(), sink.clone(), make_tracker());
    modules::bmc_eventlog::spawn(host.clone(), cfg.modules.bmc_eventlog.clone(), sink.clone(), make_tracker());
    modules::lifecycle::spawn(host.clone(), cfg.modules.lifecycle.clone(), sink.clone(), make_tracker());
    modules::boot::spawn(host.clone(), cfg.modules.boot.clone(), sink.clone());

    // ---- Heartbeat ----
    spawn_heartbeat(host.clone(), cfg.heartbeat.interval_s, sink.clone());

    // ---- Transport ----
    let dbg_listen = cfg
        .agent
        .debug_listen
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let dbg_cfg = Arc::new(cfg.clone());
    let dbg_host = Arc::new(host.clone());
    tokio::spawn(transport::run(cfg, norm_rx, crit_rx));
    #[cfg(feature = "debug-http")]
    if let Some(addr) = dbg_listen {
        debug_http::spawn(addr, dbg_cfg, dbg_host);
    }

    // ---- Signal handling: best-effort agent_stopping alert ----
    let stop_sink = sink.clone();
    let stop_host = host.clone();
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("signal");
        tokio::select! {
            _ = sigterm.recv() => info!("SIGTERM"),
            _ = sigint.recv() => info!("SIGINT"),
        }
        let b = AlertBuilder::new(stop_host.host(), stop_host.host_id(),
            uma_shared::cat::AGENT, "agent_stopping", Severity::Info)
            .title("Agent stopping").message("monitor-agent received a termination signal.");
        stop_sink.send(Outbound::Alert(b.build_firing()));
        // Give the transport ~250ms to drain.
        tokio::time::sleep(Duration::from_millis(250)).await;
        std::process::exit(0);
    });

    futures_util::future::pending::<()>().await;
    Ok(())
}

fn dummy_kmsg() -> broadcast::Receiver<crate::kmsg::KmsgLine> {
    let (tx, rx) = broadcast::channel(1);
    drop(tx);
    rx
}

fn dummy_journal() -> broadcast::Receiver<crate::journal::JournalLine> {
    let (tx, rx) = broadcast::channel(1);
    drop(tx);
    rx
}

fn spawn_heartbeat(host: host::HostId, interval_s: u64, sink: AlertSink) {
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(interval_s.max(1)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            sink.send(Outbound::Heartbeat(Heartbeat {
                host: host.host().to_string(),
                host_id: host.host_id().to_string(),
                ts: chrono::Utc::now(),
                uptime_s: started.elapsed().as_secs(),
                firing_count: 0, // collector tracks this; agent leaves it 0
                agent_version: env!("CARGO_PKG_VERSION").into(),
            }));
        }
    });
    let _ = Envelope::Heartbeat;
}

fn init_tracing(level: &str) {
    // Kept for the debug-snapshot path only (no file logging needed there).
    let _ = level;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Belt-and-braces: even if the systemd unit forgets OOMScoreAdjust, write
/// the canonical value to /proc/self/oom_score_adj so the agent is the LAST
/// thing the kernel kills under pressure.
fn enforce_oom_score_adjust() {
    let _ = std::fs::write("/proc/self/oom_score_adj", "-1000");
}

fn install_default_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
