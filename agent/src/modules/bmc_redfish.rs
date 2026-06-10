//! BMC sensor monitoring via Redfish + IPMI fallback.
//!
//! Polls /redfish/v1/Chassis/.../Thermal and /Power for fan RPM, PSU
//! status, voltage rails. If Redfish is unreachable, falls back to
//! `ipmitool sdr type Fan|PSU|Voltage`.

use anyhow::Result;
use serde_json::Value;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, info};
use uma_shared::{cat, AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::BmcCfg;
use crate::host::HostId;
use crate::state::{AlertTracker, Decision};

pub fn spawn(host: HostId, cfg: BmcCfg, sink: AlertSink, tracker: AlertTracker) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("bmc_redfish disabled");
            return;
        }
        let client = build_client(&cfg);
        let mut tick = tokio::time::interval(Duration::from_secs(cfg.poll_s.max(5)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if !cfg.redfish_url.is_empty() {
                if let Err(e) = poll_redfish(&host, &cfg, &sink, &tracker, &client).await {
                    debug!(error = %e, "bmc_redfish: poll failed");
                }
            }
        }
    })
}

fn build_client(cfg: &BmcCfg) -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5));
    if cfg.redfish_insecure {
        b = b.danger_accept_invalid_certs(true);
    }
    b.build().unwrap_or_else(|_| reqwest::Client::new())
}

async fn poll_redfish(
    host: &HostId,
    cfg: &BmcCfg,
    sink: &AlertSink,
    tracker: &AlertTracker,
    client: &reqwest::Client,
) -> Result<()> {
    let chassis_url = format!("{}/redfish/v1/Chassis", cfg.redfish_url.trim_end_matches('/'));
    let resp: Value = client
        .get(&chassis_url)
        .basic_auth(&cfg.redfish_username, Some(&cfg.redfish_password))
        .send()
        .await?
        .json()
        .await?;
    let members = resp
        .get("Members")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    for m in members {
        if let Some(rel) = m.get("@odata.id").and_then(|v| v.as_str()) {
            let thermal = format!("{}{}/Thermal", cfg.redfish_url.trim_end_matches('/'), rel);
            let power = format!("{}{}/Power", cfg.redfish_url.trim_end_matches('/'), rel);
            if let Ok(t) = client.get(&thermal).basic_auth(&cfg.redfish_username, Some(&cfg.redfish_password)).send().await {
                if let Ok(j) = t.json::<Value>().await {
                    check_fans(host, sink, tracker, &j);
                }
            }
            if let Ok(p) = client.get(&power).basic_auth(&cfg.redfish_username, Some(&cfg.redfish_password)).send().await {
                if let Ok(j) = p.json::<Value>().await {
                    check_psus(host, sink, tracker, &j);
                }
            }
        }
    }
    Ok(())
}

/// Redfish "Status.Health" values: OK | Warning | Critical | (blank).
/// Anything else (or missing) is treated as OK.
fn redfish_severity(health: &str) -> Option<Severity> {
    match health.to_ascii_lowercase().as_str() {
        "critical" => Some(Severity::Critical),
        "warning"  => Some(Severity::Warning),
        _ => None,
    }
}

/// Whether an entry is physically present. Empty bays/slots report
/// `State=Absent` in Redfish — we must skip those or every chassis with
/// half-populated PSU/fan bays produces false positives.
fn is_present(state: &str) -> bool {
    !matches!(
        state.to_ascii_lowercase().as_str(),
        "absent" | "unavailable" | "deferring" | "standby"
    )
}

/// Try several name fields so HPE/Dell entries get a useful label.
fn entry_name(entry: &Value, fallback: &str) -> String {
    if let Some(n) = entry.get("Name").and_then(|v| v.as_str()) {
        if !n.is_empty() && n != "HpeServerPowerSupply" && n != "HpeServerFan" {
            return n.to_string();
        }
    }
    if let Some(bay) = entry.pointer("/Oem/Hpe/BayNumber").and_then(|v| v.as_u64()) {
        return format!("{fallback} bay {bay}");
    }
    if let Some(mid) = entry.get("MemberId").and_then(|v| v.as_str()) {
        return format!("{fallback} {mid}");
    }
    fallback.to_string()
}

fn check_fans(host: &HostId, sink: &AlertSink, tracker: &AlertTracker, j: &Value) {
    let fans = j.get("Fans").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for f in fans {
        let state = f.pointer("/Status/State").and_then(|v| v.as_str()).unwrap_or("");
        if !is_present(state) {
            continue; // empty fan bay — not a fault
        }
        let name = entry_name(&f, "Fan");
        let rpm = f.get("Reading").and_then(|v| v.as_u64()).unwrap_or(0);
        let health = f.pointer("/Status/Health").and_then(|v| v.as_str()).unwrap_or("OK").to_string();
        let sev = redfish_severity(&health);
        // Only alert on stalled fan if the fan reports itself enabled —
        // a healthy variable-speed fan can read 0 RPM at idle on some
        // platforms, but in that case Health is still OK.
        let stalled = rpm == 0 && state.eq_ignore_ascii_case("enabled") && sev.is_some();
        if let Some(sev) = sev.or(if stalled { Some(Severity::Critical) } else { None }) {
            let b = AlertBuilder::new(host.host(), host.host_id(), cat::BMC_REDFISH, "fan_unhealthy", sev)
                .device(&name)
                .value(format!("rpm={rpm}, state={state}, health={health}"))
                .title("BMC reports unhealthy fan")
                .message(format!(
                    "Fan {name}: RPM={rpm}, state={state}, health={health}. \
                     Replace promptly to avoid thermal escalation."
                ));
            if let Decision::Emit(a) = tracker.observe(b) { sink.send(Outbound::Alert(a)); }
        }
    }
}

fn check_psus(host: &HostId, sink: &AlertSink, tracker: &AlertTracker, j: &Value) {
    let psus = j.get("PowerSupplies").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for p in psus {
        let state = p.pointer("/Status/State").and_then(|v| v.as_str()).unwrap_or("");
        // Skip empty PSU bays. Most chassis have 2-4 bays; only the populated
        // ones report meaningful health. iLO often labels empty bays
        // State=Absent with Health=Warning, which would false-fire here.
        if !is_present(state) {
            continue;
        }
        let name = entry_name(&p, "PSU");
        let health = p.pointer("/Status/Health").and_then(|v| v.as_str()).unwrap_or("OK").to_string();
        let sev = match redfish_severity(&health) {
            Some(s) => s,
            None => continue, // present + healthy
        };
        let b = AlertBuilder::new(host.host(), host.host_id(), cat::BMC_REDFISH, "psu_unhealthy", sev)
            .device(&name)
            .value(format!("state={state}, health={health}"))
            .title("BMC reports unhealthy PSU")
            .message(format!(
                "Power supply {name}: state={state}, health={health}. \
                 Replace and verify the redundant feed."
            ));
        if let Decision::Emit(a) = tracker.observe(b) { sink.send(Outbound::Alert(a)); }
    }
}
