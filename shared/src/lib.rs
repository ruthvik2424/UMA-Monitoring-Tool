//! Shared types between the UMA agent, collector and GUI.
//!
//! Anything that travels on the wire MUST live here so both the producer
//! and consumer agree on the schema. Backwards compatibility is achieved
//! via `#[serde(default)]` on every optional field.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Top-level message envelope. Every WebSocket frame is one of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Envelope {
    /// Agent->collector: a state-changing alert (firing or resolved).
    Alert(Alert),
    /// Agent->collector: keep-alive that drives the connectivity heatmap.
    Heartbeat(Heartbeat),
    /// Agent->collector: one-shot identification after WSS handshake.
    Hello(Hello),
    /// Collector->GUI: full snapshot of currently-firing alerts.
    Snapshot(Snapshot),
    /// Collector->GUI: a host transitioned online/offline.
    HostStatus(HostStatus),
}

/// Severity ordering matters: Info < Warning < Critical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertState {
    Firing,
    Resolved,
}

/// The canonical alert record.
///
/// `fingerprint` is the dedup key. Anything keyed with the same fingerprint is
/// considered the same logical alert — repeats increment `occurrences` rather
/// than emitting new alerts (unless `re_notify_after` has elapsed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub id: String, // ULID generated on agent
    pub ts: DateTime<Utc>,

    pub host: String,
    pub host_id: String, // /etc/machine-id
    /// Stamped by the collector from its HostInfo when the alert is first received.
    /// Present on every alert in SQLite so history shows IPs even for offline hosts.
    #[serde(default)]
    pub primary_ip: String,

    pub category: String,        // e.g. "nvme", "lifecycle", "memory_ecc"
    #[serde(default)]
    pub subsystem: Option<String>,
    pub severity: Severity,
    pub state: AlertState,

    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub device_model: Option<String>,

    pub metric: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub threshold: Option<String>,

    pub title: String,
    pub message: String,

    pub fingerprint: String,

    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    #[serde(default = "one")]
    pub occurrences: u64,

    /// Free-form structured data captured at detection time. Useful for
    /// debugging without changing the schema.
    #[serde(default)]
    pub raw: BTreeMap<String, serde_json::Value>,

    /// Optional list of correlated fingerprints (e.g. controller_lockup
    /// alert listing all the ceph_osd_down alerts it caused).
    #[serde(default)]
    pub correlated: Vec<String>,
}

fn one() -> u64 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub host: String,
    pub host_id: String,
    pub ts: DateTime<Utc>,
    pub uptime_s: u64,
    /// Number of currently-firing alerts on this host.
    #[serde(default)]
    pub firing_count: u32,
    /// Agent version, useful for rollout tracking.
    #[serde(default)]
    pub agent_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub host: String,
    pub host_id: String,
    #[serde(default)]
    pub primary_ip: String,
    pub agent_version: String,
    pub started_at: DateTime<Utc>,
    pub vendor: VendorInfo,
    /// Tag set used by the GUI for grouping, e.g. `dc=dc1`, `rack=R12`,
    /// `role=ceph-osd`. Pulled straight from agent config.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    /// When true, the agent suppresses outbound alerts (maintenance window).
    #[serde(default)]
    pub maintenance: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VendorInfo {
    #[serde(default)]
    pub system_manufacturer: String,
    #[serde(default)]
    pub system_product_name: String,
    #[serde(default)]
    pub bios_version: String,
    #[serde(default)]
    pub bmc_vendor: String, // hpe | dell | supermicro | unknown
    #[serde(default)]
    pub gpu_vendors: Vec<String>, // nvidia | amd | intel
    #[serde(default)]
    pub kernel: String,
    #[serde(default)]
    pub os_release: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub generated_at: DateTime<Utc>,
    pub alerts: Vec<Alert>,
    pub hosts: Vec<HostState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostStatus {
    pub host: String,
    pub host_id: String,
    pub online: bool,
    pub last_seen: DateTime<Utc>,
    pub firing_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostState {
    pub host: String,
    pub host_id: String,
    #[serde(default)]
    pub primary_ip: String,
    pub online: bool,
    pub last_seen: DateTime<Utc>,
    pub firing_count: u32,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    #[serde(default)]
    pub agent_version: String,
    /// Mirrored from the agent Hello; used by the GUI for a maintenance badge.
    #[serde(default)]
    pub maintenance: bool,
}

/// Builder for constructing alerts ergonomically inside modules.
#[derive(Debug)]
pub struct AlertBuilder {
    host: String,
    host_id: String,
    category: String,
    subsystem: Option<String>,
    severity: Severity,
    metric: String,
    device: Option<String>,
    device_model: Option<String>,
    value: Option<String>,
    threshold: Option<String>,
    title: String,
    message: String,
    raw: BTreeMap<String, serde_json::Value>,
    correlated: Vec<String>,
}

impl AlertBuilder {
    pub fn new(
        host: impl Into<String>,
        host_id: impl Into<String>,
        category: impl Into<String>,
        metric: impl Into<String>,
        severity: Severity,
    ) -> Self {
        Self {
            host: host.into(),
            host_id: host_id.into(),
            category: category.into(),
            subsystem: None,
            severity,
            metric: metric.into(),
            device: None,
            device_model: None,
            value: None,
            threshold: None,
            title: String::new(),
            message: String::new(),
            raw: BTreeMap::new(),
            correlated: Vec::new(),
        }
    }

    pub fn subsystem(mut self, s: impl Into<String>) -> Self {
        self.subsystem = Some(s.into());
        self
    }
    pub fn device(mut self, d: impl Into<String>) -> Self {
        self.device = Some(d.into());
        self
    }
    pub fn device_model(mut self, m: impl Into<String>) -> Self {
        self.device_model = Some(m.into());
        self
    }
    pub fn value(mut self, v: impl Into<String>) -> Self {
        self.value = Some(v.into());
        self
    }
    pub fn threshold(mut self, t: impl Into<String>) -> Self {
        self.threshold = Some(t.into());
        self
    }
    pub fn title(mut self, t: impl Into<String>) -> Self {
        self.title = t.into();
        self
    }
    pub fn message(mut self, m: impl Into<String>) -> Self {
        self.message = m.into();
        self
    }
    pub fn raw_kv(mut self, k: impl Into<String>, v: serde_json::Value) -> Self {
        self.raw.insert(k.into(), v);
        self
    }
    pub fn correlate(mut self, fp: impl Into<String>) -> Self {
        self.correlated.push(fp.into());
        self
    }

    /// Compose a stable fingerprint. Anything that should be treated as the
    /// "same" alert across recurrences MUST end up with the same fingerprint.
    pub fn fingerprint(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.host_id,
            self.category,
            self.device.as_deref().unwrap_or("-"),
            self.metric
        )
    }

    pub fn build_firing(self) -> Alert {
        let now = Utc::now();
        let fp = self.fingerprint();
        Alert {
            id: ulid::Ulid::new().to_string(),
            ts: now,
            host: self.host,
            host_id: self.host_id,
            // primary_ip is left empty here; the collector stamps it from
            // its HostInfo cache (ws_ingest.rs) before broadcast and storage.
            primary_ip: String::new(),
            category: self.category,
            subsystem: self.subsystem,
            severity: self.severity,
            state: AlertState::Firing,
            device: self.device,
            device_model: self.device_model,
            metric: self.metric,
            value: self.value,
            threshold: self.threshold,
            title: self.title,
            message: self.message,
            fingerprint: fp,
            first_seen: now,
            last_seen: now,
            occurrences: 1,
            raw: self.raw,
            correlated: self.correlated,
        }
    }
}

/// Constants — categories. Keep these in lockstep with the GUI.
pub mod cat {
    pub const NVME: &str = "nvme";
    pub const DISK_SMART: &str = "disk_smart";
    pub const MEMORY_ECC: &str = "memory_ecc";
    pub const CPU_MCE: &str = "cpu_mce";
    pub const PCIE_AER: &str = "pcie_aer";
    pub const THERMAL: &str = "thermal";
    pub const GPU_NVIDIA: &str = "gpu_nvidia";
    pub const GPU_AMD: &str = "gpu_amd";
    pub const NETWORK_NIC: &str = "network_nic";
    pub const STORAGE_CONTROLLER: &str = "storage_controller";
    pub const STORAGE_IO: &str = "storage_io";
    pub const MEMORY_PRESSURE: &str = "memory_pressure";
    pub const OS_HANG: &str = "os_hang";
    pub const BMC_REDFISH: &str = "bmc_redfish";
    pub const BMC_EVENTLOG: &str = "bmc_eventlog";
    pub const SYSLOG: &str = "syslog";
    pub const BOOT: &str = "boot";
    pub const LIFECYCLE: &str = "lifecycle";
    pub const AGENT: &str = "agent";
    pub const COLLECTOR: &str = "collector";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_round_trips_through_json() {
        let a = AlertBuilder::new("h1", "hid", "nvme", "critical_warning", Severity::Critical)
            .device("/dev/nvme0n1")
            .device_model("Samsung MZ1L21T9HCLS")
            .value("0x04")
            .threshold("0x00")
            .title("NVMe drive reporting reliability degradation")
            .message("Replace within next maintenance window.")
            .build_firing();
        let s = serde_json::to_string(&a).unwrap();
        let b: Alert = serde_json::from_str(&s).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.severity, b.severity);
    }

    #[test]
    fn fingerprint_is_stable() {
        let a = AlertBuilder::new("h1", "hid", "nvme", "media_errors", Severity::Warning)
            .device("/dev/nvme0n1");
        let b = AlertBuilder::new("h2-different-host", "hid", "nvme", "media_errors", Severity::Warning)
            .device("/dev/nvme0n1");
        assert_eq!(a.fingerprint(), b.fingerprint(),
            "fingerprint must be host_id-based, not hostname-based");
    }
}
