//! Agent configuration. TOML on disk, loaded once at startup.
//!
//! Every module-specific block is optional and falls back to sane defaults
//! so a fresh node only needs the collector endpoint and certs to be useful.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub agent: AgentBlock,
    #[serde(default)]
    pub collector: CollectorBlock,
    #[serde(default)]
    pub tls: TlsBlock,
    #[serde(default)]
    pub heartbeat: HeartbeatBlock,
    #[serde(default)]
    pub dedup: DedupBlock,
    #[serde(default)]
    pub modules: ModulesBlock,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentBlock {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Explicit maintenance flag (alerts suppressed). Can also set
    /// `log_level = "maintenance"` or `log_level = "maintainance"` (typo).
    #[serde(default)]
    pub maintenance: bool,
    /// HTTP `GET /v1/debug/snapshot` on this address, e.g. `127.0.0.1:19100`.
    /// Bind to loopback only; responses may include sensitive paths. Omitted or empty disables.
    #[serde(default)]
    pub debug_listen: Option<String>,
}
impl Default for AgentBlock {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            maintenance: false,
            debug_listen: None,
        }
    }
}
fn default_log_level() -> String { "info".into() }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CollectorBlock {
    #[serde(default = "default_collector_url")]
    pub url: String,
    #[serde(default = "default_send_queue")]
    pub send_queue_size: usize,
    #[serde(default = "default_critical_queue")]
    pub critical_queue_size: usize,
    #[serde(default = "default_reconnect_min_ms")]
    pub reconnect_min_ms: u64,
    #[serde(default = "default_reconnect_max_ms")]
    pub reconnect_max_ms: u64,
    /// SO_KEEPALIVE timer (seconds) on the agent->collector socket.
    #[serde(default = "default_keepalive_s")]
    pub keepalive_s: u64,
}
impl Default for CollectorBlock {
    fn default() -> Self {
        Self {
            url: default_collector_url(),
            send_queue_size: default_send_queue(),
            critical_queue_size: default_critical_queue(),
            reconnect_min_ms: default_reconnect_min_ms(),
            reconnect_max_ms: default_reconnect_max_ms(),
            keepalive_s: default_keepalive_s(),
        }
    }
}
fn default_collector_url() -> String { "wss://collector.local:9443/v1/ingest".into() }
fn default_send_queue() -> usize { 4096 }
fn default_critical_queue() -> usize { 1024 }
fn default_reconnect_min_ms() -> u64 { 250 }
fn default_reconnect_max_ms() -> u64 { 30_000 }
fn default_keepalive_s() -> u64 { 5 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TlsBlock {
    #[serde(default = "default_ca")]
    pub ca_cert: PathBuf,
    #[serde(default = "default_client_cert")]
    pub client_cert: PathBuf,
    #[serde(default = "default_client_key")]
    pub client_key: PathBuf,
    /// Disable certificate verification — DO NOT use in production.
    #[serde(default)]
    pub insecure_skip_verify: bool,
}
impl Default for TlsBlock {
    fn default() -> Self {
        Self {
            ca_cert: default_ca(),
            client_cert: default_client_cert(),
            client_key: default_client_key(),
            insecure_skip_verify: false,
        }
    }
}
fn default_ca() -> PathBuf { PathBuf::from("/etc/monitor-agent/ca.crt") }
fn default_client_cert() -> PathBuf { PathBuf::from("/etc/monitor-agent/client.crt") }
fn default_client_key() -> PathBuf { PathBuf::from("/etc/monitor-agent/client.key") }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HeartbeatBlock {
    #[serde(default = "default_heartbeat_s")]
    pub interval_s: u64,
}
impl Default for HeartbeatBlock {
    fn default() -> Self { Self { interval_s: default_heartbeat_s() } }
}
fn default_heartbeat_s() -> u64 { 5 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DedupBlock {
    /// How long an alert must remain firing before we re-notify.
    #[serde(default = "default_renotify_s")]
    pub re_notify_after_s: u64,
    /// Metric must be OK for this long before emitting `resolved`.
    #[serde(default = "default_recovery_s")]
    pub recovery_window_s: u64,
}
impl Default for DedupBlock {
    fn default() -> Self {
        Self {
            re_notify_after_s: default_renotify_s(),
            recovery_window_s: default_recovery_s(),
        }
    }
}
fn default_renotify_s() -> u64 { 30 * 60 }
fn default_recovery_s() -> u64 { 60 }

/// Per-module enable flags + thresholds. Each is optional so the
/// config can be tiny in the common case.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ModulesBlock {
    #[serde(default)]
    pub nvme: NvmeCfg,
    #[serde(default)]
    pub disk_smart: DiskSmartCfg,
    #[serde(default)]
    pub memory_ecc: MemoryEccCfg,
    #[serde(default)]
    pub cpu_mce: SimpleEnable,
    #[serde(default)]
    pub pcie_aer: PcieAerCfg,
    #[serde(default)]
    pub thermal: ThermalCfg,
    #[serde(default)]
    pub gpu_nvidia: SimpleEnable,
    #[serde(default)]
    pub gpu_amd: SimpleEnable,
    #[serde(default)]
    pub network_nic: NetworkNicCfg,
    #[serde(default)]
    pub storage_controller: StorageControllerCfg,
    #[serde(default)]
    pub storage_io: StorageIoCfg,
    #[serde(default)]
    pub mempressure: MemPressureCfg,
    #[serde(default)]
    pub oshang: SimpleEnable,
    #[serde(default)]
    pub bmc_redfish: BmcCfg,
    #[serde(default)]
    pub bmc_eventlog: BmcCfg,
    #[serde(default)]
    pub syslog_rules: SyslogRulesCfg,
    #[serde(default)]
    pub boot: SimpleEnable,
    #[serde(default)]
    pub lifecycle: SimpleEnable,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SimpleEnable {
    #[serde(default = "yes")]
    pub enabled: bool,
}
impl Default for SimpleEnable {
    fn default() -> Self { Self { enabled: true } }
}
fn yes() -> bool { true }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NvmeCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_nvme_poll_s")]
    pub poll_s: u64,
    #[serde(default = "default_wear_warn")]
    pub wear_warn_pct: u8,
    #[serde(default = "default_wear_crit")]
    pub wear_critical_pct: u8,
    #[serde(default = "default_temp_warn")]
    pub temp_warn_c: i32,
    #[serde(default = "default_temp_crit")]
    pub temp_critical_c: i32,
}
impl Default for NvmeCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_s: default_nvme_poll_s(),
            wear_warn_pct: default_wear_warn(),
            wear_critical_pct: default_wear_crit(),
            temp_warn_c: default_temp_warn(),
            temp_critical_c: default_temp_crit(),
        }
    }
}
fn default_nvme_poll_s() -> u64 { 60 }
fn default_wear_warn() -> u8 { 80 }
fn default_wear_crit() -> u8 { 90 }
fn default_temp_warn() -> i32 { 70 }
fn default_temp_crit() -> i32 { 80 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiskSmartCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_smart_poll_s")]
    pub poll_s: u64,
}
impl Default for DiskSmartCfg {
    fn default() -> Self { Self { enabled: true, poll_s: default_smart_poll_s() } }
}
fn default_smart_poll_s() -> u64 { 300 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemoryEccCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_ecc_poll_s")]
    pub poll_s: u64,
    #[serde(default = "default_ce_per_hour_warn")]
    pub ce_per_hour_warn: u64,
    /// Poll the EDAC kernel sysfs (/sys/devices/system/edac/mc/).
    /// DISABLED by default because the EDAC kernel module conflicts with HPE iLO,
    /// Dell iDRAC, and Supermicro BMC firmware — both try to own the memory controller
    /// hardware registers, causing the BMC to log false or missed ECC events.
    /// Only enable on hosts WITHOUT a management controller (bare-metal VMs, etc.).
    #[serde(default)]
    pub edac_sysfs_enable: bool,
    /// Poll memory ECC sensor counts from the BMC via `ipmitool sdr type Memory`.
    /// This reads from the BMC's own counter channel — no kernel driver conflict.
    /// Requires ipmitool to be installed and IPMI/KCS accessible (/dev/ipmi0).
    #[serde(default = "yes")]
    pub ipmi_sdr_enable: bool,
}
impl Default for MemoryEccCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_s: default_ecc_poll_s(),
            ce_per_hour_warn: default_ce_per_hour_warn(),
            edac_sysfs_enable: false,  // EDAC conflicts with iLO/iDRAC/BMC by default
            ipmi_sdr_enable: true,
        }
    }
}
fn default_ecc_poll_s() -> u64 { 5 }
fn default_ce_per_hour_warn() -> u64 { 50 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PcieAerCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_aer_corr_per_min_warn")]
    pub corrected_per_min_warn: u64,
}
impl Default for PcieAerCfg {
    fn default() -> Self {
        Self { enabled: true, corrected_per_min_warn: default_aer_corr_per_min_warn() }
    }
}
fn default_aer_corr_per_min_warn() -> u64 { 100 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThermalCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_thermal_poll_s")]
    pub poll_s: u64,
    #[serde(default = "default_inlet_warn")]
    pub inlet_warn_c: f32,
    #[serde(default = "default_inlet_crit")]
    pub inlet_critical_c: f32,
    #[serde(default = "default_inlet_emerg")]
    pub inlet_emergency_c: f32,
}
impl Default for ThermalCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_s: default_thermal_poll_s(),
            inlet_warn_c: default_inlet_warn(),
            inlet_critical_c: default_inlet_crit(),
            inlet_emergency_c: default_inlet_emerg(),
        }
    }
}
fn default_thermal_poll_s() -> u64 { 5 }
fn default_inlet_warn() -> f32 { 32.0 }
fn default_inlet_crit() -> f32 { 36.0 }
fn default_inlet_emerg() -> f32 { 38.0 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkNicCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Sliding window for flap detection.
    #[serde(default = "default_flap_window_s")]
    pub flap_window_s: u64,
    /// Number of carrier transitions inside the window to trigger a flap alert.
    #[serde(default = "default_flap_threshold")]
    pub flap_threshold: u32,
    /// Counter polling interval (CRC, FEC, etc).
    #[serde(default = "default_nic_counter_poll_s")]
    pub counter_poll_s: u64,
    /// Skip these interfaces (regex matches).
    #[serde(default = "default_nic_ignore")]
    pub ignore_regex: Vec<String>,
}
impl Default for NetworkNicCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            flap_window_s: default_flap_window_s(),
            flap_threshold: default_flap_threshold(),
            counter_poll_s: default_nic_counter_poll_s(),
            ignore_regex: default_nic_ignore(),
        }
    }
}
fn default_flap_window_s() -> u64 { 10 }
fn default_flap_threshold() -> u32 { 2 }
fn default_nic_counter_poll_s() -> u64 { 5 }
fn default_nic_ignore() -> Vec<String> {
    vec!["^lo$".into(), "^docker.*".into(), "^veth.*".into(), "^br-.*".into(), "^cali.*".into(), "^cni.*".into(), "^tun.*".into(), "^tap.*".into(), "^virbr.*".into()]
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageControllerCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_ctrl_poll_s")]
    pub poll_s: u64,
}
impl Default for StorageControllerCfg {
    fn default() -> Self { Self { enabled: true, poll_s: default_ctrl_poll_s() } }
}
fn default_ctrl_poll_s() -> u64 { 15 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageIoCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Look for ceph-osd@N.service units.
    #[serde(default = "yes")]
    pub watch_ceph_osd: bool,
    /// Window during which a ceph_osd_down is correlated with a controller_lockup.
    #[serde(default = "default_ceph_correlate_s")]
    pub correlate_window_s: u64,
}
impl Default for StorageIoCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            watch_ceph_osd: true,
            correlate_window_s: default_ceph_correlate_s(),
        }
    }
}
fn default_ceph_correlate_s() -> u64 { 30 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemPressureCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_psi_poll_s")]
    pub poll_s: u64,
    /// PSI `some avg10` percentage that triggers a warning.
    #[serde(default = "default_psi_warn")]
    pub psi_some_avg10_warn: f32,
    /// Required sustained duration (seconds) above the threshold.
    #[serde(default = "default_psi_sustain_s")]
    pub sustain_s: u64,
    /// Number of OOM kills inside `oom_storm_window_s` to escalate to oom_storm.
    #[serde(default = "default_oom_storm_count")]
    pub oom_storm_count: u32,
    #[serde(default = "default_oom_storm_window_s")]
    pub oom_storm_window_s: u64,
}
impl Default for MemPressureCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_s: default_psi_poll_s(),
            psi_some_avg10_warn: default_psi_warn(),
            sustain_s: default_psi_sustain_s(),
            oom_storm_count: default_oom_storm_count(),
            oom_storm_window_s: default_oom_storm_window_s(),
        }
    }
}
fn default_psi_poll_s() -> u64 { 2 }
fn default_psi_warn() -> f32 { 50.0 }
fn default_psi_sustain_s() -> u64 { 30 }
fn default_oom_storm_count() -> u32 { 3 }
fn default_oom_storm_window_s() -> u64 { 300 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BmcCfg {
    #[serde(default)]
    pub enabled: bool, // off until configured
    #[serde(default = "default_bmc_poll_s")]
    pub poll_s: u64,
    /// Local IPMI device, used as fallback when Redfish is unreachable.
    #[serde(default = "default_ipmi_dev")]
    pub ipmi_device: PathBuf,
    /// Optional Redfish endpoint. If empty, Redfish is skipped entirely.
    #[serde(default)]
    pub redfish_url: String,
    #[serde(default)]
    pub redfish_username: String,
    #[serde(default)]
    pub redfish_password: String,
    #[serde(default)]
    pub redfish_insecure: bool,
}
impl Default for BmcCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_s: default_bmc_poll_s(),
            ipmi_device: default_ipmi_dev(),
            redfish_url: String::new(),
            redfish_username: String::new(),
            redfish_password: String::new(),
            redfish_insecure: false,
        }
    }
}
fn default_bmc_poll_s() -> u64 { 10 }
fn default_ipmi_dev() -> PathBuf { PathBuf::from("/dev/ipmi0") }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SyslogRulesCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// If unspecified in TOML, serde populates this with `default_syslog_rules()`.
    /// Specify an empty list `rules = []` in config.toml to opt out of all rules.
    #[serde(default = "default_syslog_rules")]
    pub rules: Vec<SyslogRule>,
}
impl Default for SyslogRulesCfg {
    fn default() -> Self {
        Self { enabled: true, rules: default_syslog_rules() }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SyslogRule {
    pub name: String,
    pub regex: String,
    pub severity: String,    // info|warning|critical
    pub category: String,
    pub title: String,
    pub message: String,     // may include {match} placeholder
}

/// Built-in rules covering common kernel events that the dedicated
/// modules don't already match. Override or extend via host_vars in
/// Ansible (`uma_modules_overrides.syslog_rules.rules`).
fn default_syslog_rules() -> Vec<SyslogRule> {
    use SyslogRule as R;
    vec![
        // SCSI/SATA command timeouts — drive about to disappear.
        R {
            name: "scsi_command_timeout".into(),
            regex: r"sd \d+:\d+:\d+:\d+:.*timing out|ata\d+\.\d+: exception Emask".into(),
            severity: "warning".into(),
            category: "storage_io".into(),
            title: "Storage controller command timeout".into(),
            message: "Kernel reported a SCSI/SATA command timeout. Drive is unresponsive; if it persists the disk will be marked offline. Trace: {match}".into(),
        },
        // Generic PCIe link recovery (often precedes AER).
        R {
            name: "pcie_link_recovery".into(),
            regex: r"PCIe Bus Error|pciehp.*Link (Down|Up)|pci_bus.*recovery".into(),
            severity: "warning".into(),
            category: "pcie_aer".into(),
            title: "PCIe link instability".into(),
            message: "Kernel logged a PCIe link/bus event. Often precedes an uncorrectable AER. Trace: {match}".into(),
        },
        // Ethernet/IB driver resets that link-flap detection may miss.
        R {
            name: "nic_driver_reset".into(),
            regex: r"(ixgbe|i40e|ice|mlx5_core|bnxt_en|ena).*(reset|recovery)|NIC Reset".into(),
            severity: "warning".into(),
            category: "network_nic".into(),
            title: "NIC driver reset".into(),
            message: "Network driver issued a reset/recovery event. Brief packet loss expected; investigate if recurring. Trace: {match}".into(),
        },
        // Watchdog warnings (NOT lockup — oshang catches the lockup itself).
        R {
            name: "watchdog_warning".into(),
            regex: r"hardware watchdog|iTCO_wdt|softdog: WDIOC".into(),
            severity: "info".into(),
            category: "syslog".into(),
            title: "Hardware watchdog message".into(),
            message: "Kernel logged a hardware-watchdog event. Trace: {match}".into(),
        },
        // PSU / voltage / chassis intrusion in kernel ACPI events.
        R {
            name: "acpi_power_event".into(),
            regex: r"ACPI.*power|ACPI.*thermal|ACPI: Critical|chassis intrusion".into(),
            severity: "warning".into(),
            category: "thermal".into(),
            title: "ACPI power/thermal event".into(),
            message: "ACPI subsystem reported a power or thermal event. Trace: {match}".into(),
        },
    ]
}

impl AgentConfig {
    /// Used when the config file is missing or invalid (same as historical inline default in `main`).
    pub fn default_fallback() -> Self {
        Self {
            agent: AgentBlock::default(),
            collector: CollectorBlock::default(),
            tls: TlsBlock::default(),
            heartbeat: HeartbeatBlock::default(),
            dedup: DedupBlock::default(),
            modules: ModulesBlock::default(),
            tags: BTreeMap::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: AgentConfig = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    /// True when alerts must not be sent to the collector (maintenance window).
    pub fn maintenance_mode(&self) -> bool {
        if self.agent.maintenance {
            return true;
        }
        match self.agent.log_level.to_lowercase().as_str() {
            "maintainance" | "maintenance" => true,
            _ => false,
        }
    }

    /// Tracing filter string: maintenance log_level values map to `info` for tracing.
    pub fn tracing_log_filter(&self) -> String {
        match self.agent.log_level.to_lowercase().as_str() {
            "maintainance" | "maintenance" => "info".into(),
            _ => self.agent.log_level.clone(),
        }
    }

    pub fn re_notify(&self) -> Duration { Duration::from_secs(self.dedup.re_notify_after_s) }
    pub fn recovery(&self) -> Duration { Duration::from_secs(self.dedup.recovery_window_s) }
}
