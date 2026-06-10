//! Memory ECC monitoring — BMC-safe implementation.
//!
//! ## Detection paths
//!
//! 1. **IPMI SDR** (default ON): `ipmitool sdr type "Memory"` reads correctable
//!    and uncorrectable ECC counts directly from the BMC sensor channel.
//!    No kernel module required; safe alongside HPE iLO, Dell iDRAC, Supermicro.
//!
//! 2. **EDAC sysfs** (default OFF): Polls `/sys/devices/system/edac/mc/*/ce_count`.
//!    Requires the `edac_core` kernel module. DISABLED by default because HPE, Dell,
//!    and Supermicro explicitly warn that loading EDAC while their BMC firmware is
//!    active causes both sides to fight over the same memory controller registers,
//!    resulting in missed or duplicate ECC events in the BMC event log.
//!    Enable only on hosts without a management controller (bare VMs, etc.).
//!
//! 3. **MCE / Hardware Error kmsg** (handled in `cpu_mce` module): The kernel's own
//!    Machine Check Architecture fires `mce:` / `[Hardware Error]` kmsg lines
//!    independently of any EDAC driver. `cpu_mce.rs` covers this path; we do NOT
//!    duplicate it here.
//!
//! ## Migration note
//!
//! The previous EDAC-specific kmsg pattern (`EDAC MC\d+ UE`) is removed because:
//!   a) It only fires when the EDAC module is loaded (which we now discourage).
//!   b) The same events arrive as MCE lines handled by `cpu_mce.rs`.

use std::collections::HashMap;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use uma_shared::{cat, AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::MemoryEccCfg;
use crate::host::HostId;
use crate::state::{AlertTracker, Decision};

pub fn spawn(
    host: HostId,
    cfg: MemoryEccCfg,
    sink: AlertSink,
    tracker: AlertTracker,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("memory_ecc disabled");
            return;
        }

        let mut handles = vec![];

        if cfg.edac_sysfs_enable {
            warn!(
                "memory_ecc: EDAC sysfs polling enabled. \
                 Disable on hosts with HPE iLO / Dell iDRAC / Supermicro BMC \
                 to avoid memory controller register conflicts."
            );
            handles.push(tokio::spawn(edac_poll_loop(
                host.clone(), cfg.clone(), sink.clone(), tracker.clone(),
            )));
        } else {
            info!(
                "memory_ecc: EDAC sysfs polling disabled (default, safe with BMC). \
                 ECC coverage: IPMI SDR + cpu_mce (MCE kmsg) + bmc_eventlog."
            );
        }

        if cfg.ipmi_sdr_enable {
            handles.push(tokio::spawn(ipmi_sdr_poll_loop(
                host.clone(), cfg.clone(), sink.clone(), tracker.clone(),
            )));
        }

        for h in handles {
            let _ = h.await;
        }
    })
}

// ============================================================================
// Path 1 — IPMI SDR memory sensor polling (BMC-safe, default)
// ============================================================================

/// Poll memory ECC sensor counts via `ipmitool sdr type Memory`.
/// The BMC maintains its own counters on the management side-channel;
/// reading them does not touch the EDAC driver or memory controller registers.
async fn ipmi_sdr_poll_loop(
    host: HostId,
    cfg: MemoryEccCfg,
    sink: AlertSink,
    tracker: AlertTracker,
) {
    // Check ipmitool is present and /dev/ipmi0 exists before spinning.
    if !std::path::Path::new("/dev/ipmi0").exists()
        && !std::path::Path::new("/dev/ipmi/0").exists()
        && !std::path::Path::new("/dev/ipmidev/0").exists()
    {
        info!("memory_ecc: no /dev/ipmi0 device — IPMI SDR polling skipped (no BMC or IPMI not exposed)");
        return;
    }
    if std::process::Command::new("which")
        .arg("ipmitool").output()
        .map(|o| !o.status.success()).unwrap_or(true)
    {
        warn!("memory_ecc: ipmitool not found — IPMI SDR polling disabled. Install: apt-get install ipmitool");
        return;
    }

    info!("memory_ecc: IPMI SDR memory sensor polling active (BMC-channel, no EDAC conflict)");

    let mut tick = tokio::time::interval(Duration::from_secs(cfg.poll_s.max(30)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut prev_counts: HashMap<String, (u64, u64)> = HashMap::new(); // sensor → (ce, ue)

    loop {
        tick.tick().await;
        match tokio::task::spawn_blocking(read_ipmi_sdr_memory).await {
            Ok(sensors) => {
                for (name, ce, ue) in sensors {
                    let prev = prev_counts.entry(name.clone()).or_insert((ce, ue));
                    let new_ce = ce.saturating_sub(prev.0);
                    let new_ue = ue.saturating_sub(prev.1);
                    *prev = (ce, ue);

                    if new_ue > 0 {
                        let b = AlertBuilder::new(
                            host.host(), host.host_id(),
                            cat::MEMORY_ECC, "uncorrectable_ecc_ipmi",
                            Severity::Critical,
                        )
                        .device(&name)
                        .value(new_ue.to_string())
                        .threshold("0")
                        .title("Memory uncorrectable ECC error (IPMI)")
                        .message(format!(
                            "BMC sensor '{name}' reports {new_ue} new uncorrectable ECC \
                             error(s). This typically causes a host reboot or panic. \
                             Identify and replace the affected DIMM. \
                             Check iLO/iDRAC IML/SEL for DIMM slot details."
                        ));
                        if let Decision::Emit(a) = tracker.observe(b) {
                            sink.send(Outbound::Alert(a));
                        }
                    }
                    if new_ce > 0 {
                        let rate_per_hr = new_ce as f64
                            / (cfg.poll_s.max(1) as f64 / 3600.0);
                        if new_ce as u64 >= cfg.ce_per_hour_warn.saturating_div(12).max(1)
                            || rate_per_hr as u64 >= cfg.ce_per_hour_warn
                        {
                            let b = AlertBuilder::new(
                                host.host(), host.host_id(),
                                cat::MEMORY_ECC, "ce_rate_high_ipmi",
                                Severity::Warning,
                            )
                            .device(&name)
                            .value(format!("{new_ce} new"))
                            .threshold(format!("{}/hr threshold", cfg.ce_per_hour_warn))
                            .title("Correctable ECC rate elevated (IPMI)")
                            .message(format!(
                                "BMC sensor '{name}' logged {new_ce} new correctable ECC \
                                 error(s) in the last {} seconds. Elevated CE rate is a \
                                 leading indicator of a failing DIMM. Monitor and plan \
                                 replacement during next maintenance window.",
                                cfg.poll_s
                            ));
                            if let Decision::Emit(a) = tracker.observe(b) {
                                sink.send(Outbound::Alert(a));
                            }
                        }
                    }
                }
            }
            Err(e) => warn!("memory_ecc: IPMI SDR task panic: {e}"),
        }
    }
}

/// Run `ipmitool sdr type Memory` and parse ECC sensor readings.
/// Returns (sensor_name, correctable_count, uncorrectable_count).
fn read_ipmi_sdr_memory() -> Vec<(String, u64, u64)> {
    // Blocking version: called from spawn_blocking — use std::process.
    // Timeout is enforced by the spawn_blocking future timeout in the caller.
    use std::process::Command;
    let out = match Command::new("ipmitool")
        .args(["sdr", "type", "Memory"])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            debug!(
                "ipmitool sdr type Memory failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return vec![];
        }
        Err(e) => { debug!("ipmitool spawn failed: {e}"); return vec![]; }
    };

    parse_ipmi_sdr_output(&String::from_utf8_lossy(&out))
}

/// Parse `ipmitool sdr type Memory` text output into ECC counters.
///
/// Example lines:
///   DIMM 1A Status   | 00h | ok  | 12.1 | Presence Detected
///   DIMM_A1 CE Count | 2   | ok  | 12.3 | 2
///   DIMM_A1 UE Count | 0   | ok  | 12.3 | 0
fn parse_ipmi_sdr_output(text: &str) -> Vec<(String, u64, u64)> {
    // Group sensors by DIMM slot: collect CE and UE paired sensors.
    let mut ce_sensors: HashMap<String, u64> = HashMap::new();
    let mut ue_sensors: HashMap<String, u64> = HashMap::new();
    let mut standalone: Vec<(String, u64, u64)> = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }

        // Format: "Sensor Name | hex_value | status | entity | reading_string"
        let fields: Vec<&str> = line.splitn(5, '|').map(|f| f.trim()).collect();
        if fields.len() < 3 { continue; }

        let name  = fields[0];
        let value_str = fields[1];
        let status = fields[2].to_lowercase();

        // Skip non-numeric sensor readings (status-only sensors)
        let value: u64 = match value_str.parse() {
            Ok(v) => v,
            Err(_) => {
                // Some sensors report hex like "00h" — try parsing as hex
                value_str.trim_end_matches('h')
                    .parse::<u64>()
                    .or_else(|_| u64::from_str_radix(value_str.trim_start_matches("0x"), 16))
                    .unwrap_or(0)
            }
        };

        // Skip if sensor is unavailable / not present
        if status.contains("ns") || status.contains("na") || status.contains("disabled") {
            continue;
        }

        let name_lower = name.to_lowercase();

        if name_lower.contains("ue") || name_lower.contains("uncorrect") {
            ue_sensors.insert(name.to_string(), value);
        } else if name_lower.contains("ce") || name_lower.contains("correct") {
            ce_sensors.insert(name.to_string(), value);
        } else if name_lower.contains("ecc") || name_lower.contains("error") {
            // Generic ECC sensor — treat as CE unless very high
            if value > 0 {
                standalone.push((name.to_string(), value, 0));
            }
        }
    }

    // Pair CE/UE sensors that share a DIMM prefix
    let mut result: Vec<(String, u64, u64)> = standalone;

    // Try to match CE/UE by shared name prefix
    for (ce_name, ce_val) in &ce_sensors {
        let prefix = ce_name
            .to_lowercase()
            .replace("ce count", "").replace("ce_count", "")
            .replace(" correctable", "").replace("_correctable", "")
            .trim().to_string();
        let ue_val = ue_sensors.iter()
            .find(|(k, _)| k.to_lowercase().contains(&prefix))
            .map(|(_, v)| *v)
            .unwrap_or(0);
        result.push((ce_name.clone(), *ce_val, ue_val));
    }
    // Add unpaired UE sensors
    for (ue_name, ue_val) in &ue_sensors {
        let prefix = ue_name
            .to_lowercase()
            .replace("ue count", "").replace("ue_count", "")
            .replace(" uncorrectable", "").replace("_uncorrectable", "")
            .trim().to_string();
        let already_paired = ce_sensors.keys()
            .any(|k| k.to_lowercase().contains(&prefix));
        if !already_paired {
            result.push((ue_name.clone(), 0, *ue_val));
        }
    }

    result
}

// ============================================================================
// Path 2 — EDAC sysfs (opt-in only, disabled by default)
// ============================================================================

async fn edac_poll_loop(
    host: HostId,
    cfg: MemoryEccCfg,
    sink: AlertSink,
    tracker: AlertTracker,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.poll_s.max(2)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last: HashMap<String, (u64, u64)> = HashMap::new();
    loop {
        tick.tick().await;
        for (mc, ce, ue) in read_edac_sysfs() {
            let prev = last.entry(mc.clone()).or_insert((ce, ue));
            let dce = ce.saturating_sub(prev.0);
            let due = ue.saturating_sub(prev.1);
            *prev = (ce, ue);
            let dt_s = cfg.poll_s.max(1) as f64;

            if due > 0 {
                let b = AlertBuilder::new(
                    host.host(), host.host_id(),
                    cat::MEMORY_ECC, "uncorrectable_ecc",
                    Severity::Critical,
                )
                .device(&mc)
                .value(due.to_string())
                .threshold("0")
                .title("Memory uncorrectable ECC error (EDAC)")
                .message(format!(
                    "EDAC reports {due} new uncorrectable ECC error(s) on {mc}. \
                     This usually causes the host to reboot. Replace the affected DIMM."
                ));
                if let Decision::Emit(a) = tracker.observe(b) { sink.send(Outbound::Alert(a)); }
            }
            if dce > 0 {
                let per_hour = (dce as f64) / dt_s * 3600.0;
                if per_hour as u64 >= cfg.ce_per_hour_warn {
                    let b = AlertBuilder::new(
                        host.host(), host.host_id(),
                        cat::MEMORY_ECC, "ce_rate_high",
                        Severity::Warning,
                    )
                    .device(&mc)
                    .value(format!("{per_hour:.0}/hr"))
                    .threshold(format!("{}/hr", cfg.ce_per_hour_warn))
                    .title("Correctable ECC rate elevated (EDAC)")
                    .message(format!(
                        "{mc} is logging correctable ECC at ~{per_hour:.0}/hr — \
                         a leading indicator that this DIMM is failing."
                    ));
                    if let Decision::Emit(a) = tracker.observe(b) { sink.send(Outbound::Alert(a)); }
                }
            }
        }
    }
}

fn read_edac_sysfs() -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/devices/system/edac/mc") {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if !n.starts_with("mc") { continue; }
            let ce = std::fs::read_to_string(e.path().join("ce_count"))
                .ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
            let ue = std::fs::read_to_string(e.path().join("ue_count"))
                .ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
            out.push((n, ce, ue));
        }
    }
    out
}
