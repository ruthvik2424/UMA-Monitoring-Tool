//! Collector configuration.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[cfg(feature = "webhooks")]
use crate::webhooks::WebhookCfg;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CollectorConfig {
    #[serde(default = "default_bind_ingest")]
    pub bind_ingest: SocketAddr,
    #[serde(default = "default_bind_gui")]
    pub bind_gui: SocketAddr,
    #[serde(default)]
    pub bind_syslog_tls: Option<SocketAddr>,
    #[serde(default)]
    pub bind_syslog_udp: Option<SocketAddr>,
    #[serde(default)]
    pub tls: TlsBlock,
    #[serde(default = "default_db")]
    pub db_path: PathBuf,
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    #[serde(default = "default_offline_after_s")]
    pub offline_after_s: u64,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Outbound webhook fan-out (Slack, Google Chat, generic JSON POST).
    /// Each entry is delivered independently; failures don't affect the
    /// agent ingest path.
    #[cfg(feature = "webhooks")]
    #[serde(default)]
    pub webhooks: Vec<WebhookCfg>,
    #[cfg(not(feature = "webhooks"))]
    #[serde(default)]
    pub webhooks: Vec<()>,  // Empty type when webhooks disabled
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TlsBlock {
    pub ca_cert: PathBuf,
    pub server_cert: PathBuf,
    pub server_key: PathBuf,
    /// Require client certs (mTLS) for the agent ingest port.
    #[serde(default = "yes")]
    pub require_client_cert: bool,
}

fn yes() -> bool { true }

fn default_bind_ingest() -> SocketAddr { "0.0.0.0:9443".parse().unwrap() }
fn default_bind_gui() -> SocketAddr { "0.0.0.0:8443".parse().unwrap() }
fn default_db() -> PathBuf { PathBuf::from("/var/lib/monitor-collector/collector.sqlite") }
fn default_retention_days() -> u32 { 30 }
fn default_offline_after_s() -> u64 { 10 }
fn default_log_level() -> String { "info".into() }

impl CollectorConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let c: Self = toml::from_str(&s)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(c)
    }
}
