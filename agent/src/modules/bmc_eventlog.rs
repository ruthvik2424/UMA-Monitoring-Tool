//! BMC SEL + HPE iLO IML monitoring.
//!
//! Polls Redfish:
//!   - /redfish/v1/Systems/{id}/LogServices/SEL/Entries     (IPMI SEL)
//!   - /redfish/v1/Managers/{id}/LogServices/IML/Entries    (HPE Integrated Mgmt Log)
//!
//! Tracks the highest entry id we've seen per log service, and emits an
//! alert for any new entry of severity Caution/Warning or Critical/Severe.
//! Falls back to `ipmitool sel elist` if Redfish is unreachable.

use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::task::JoinHandle;

use crate::priv_cmd::{run, TIMEOUT_IPMI};
use tracing::{debug, info};
use uma_shared::{cat, AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::BmcCfg;
use crate::host::HostId;
use crate::state::{AlertTracker, Decision};

pub fn spawn(host: HostId, cfg: BmcCfg, sink: AlertSink, tracker: AlertTracker) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("bmc_eventlog disabled");
            return;
        }
        let client = build_client(&cfg);
        let mut last_seen: HashMap<String, String> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_secs(cfg.poll_s.max(5)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if !cfg.redfish_url.is_empty() {
                if let Err(e) = poll(&host, &cfg, &sink, &tracker, &client, &mut last_seen).await {
                    debug!(error = %e, "bmc_eventlog: redfish poll failed; trying ipmitool fallback");
                    let _ = poll_ipmi(&host, &sink, &tracker).await;
                }
            } else {
                let _ = poll_ipmi(&host, &sink, &tracker).await;
            }
        }
    })
}

fn build_client(cfg: &BmcCfg) -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5));
    if cfg.redfish_insecure { b = b.danger_accept_invalid_certs(true); }
    b.build().unwrap_or_else(|_| reqwest::Client::new())
}

async fn poll(
    host: &HostId,
    cfg: &BmcCfg,
    sink: &AlertSink,
    tracker: &AlertTracker,
    client: &reqwest::Client,
    last_seen: &mut HashMap<String, String>,
) -> Result<()> {
    let base = cfg.redfish_url.trim_end_matches('/');
    for log in [
        format!("{base}/redfish/v1/Systems/1/LogServices/IEL/Entries"), // HPE
        format!("{base}/redfish/v1/Systems/1/LogServices/SEL/Entries"), // IPMI std
        format!("{base}/redfish/v1/Managers/1/LogServices/IML/Entries"), // HPE IML
    ] {
        let resp = match client
            .get(&log)
            .basic_auth(&cfg.redfish_username, Some(&cfg.redfish_password))
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !resp.status().is_success() { continue; }
        let j: Value = match resp.json().await { Ok(j) => j, Err(_) => continue };
        let members = j.get("Members").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let prev = last_seen.get(&log).cloned().unwrap_or_default();
        let mut newest = prev.clone();
        for m in members {
            let id = m.get("Id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if id <= prev { continue; }
            if id > newest { newest = id.clone(); }
            let sev_str = m.get("Severity").and_then(|v| v.as_str()).unwrap_or("OK").to_string();
            let msg = m.get("Message").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let created = m.get("Created").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let sev = match sev_str.to_lowercase().as_str() {
                "critical" | "severe" | "fatal" => Severity::Critical,
                "warning" | "caution" => Severity::Warning,
                _ => continue, // ignore informational
            };
            let b = AlertBuilder::new(host.host(), host.host_id(), cat::BMC_EVENTLOG, "bmc_log_entry", sev)
                .device(log.split('/').last().unwrap_or("log").to_string())
                .value(sev_str.clone())
                .title("BMC event log entry")
                .message(format!("BMC logged {sev_str} event at {created}: {msg}"))
                .raw_kv("entry_id", serde_json::json!(id))
                .raw_kv("log", serde_json::json!(log));
            if let Decision::Emit(a) = tracker.observe(b) { sink.send(Outbound::Alert(a)); }
        }
        last_seen.insert(log, newest);
    }
    Ok(())
}

async fn poll_ipmi(host: &HostId, sink: &AlertSink, tracker: &AlertTracker) -> Result<()> {
    let o = run("ipmitool", &["sel", "elist"], TIMEOUT_IPMI).await?;
    if !o.status.success() { return Ok(()); }
    let s = String::from_utf8_lossy(&o.stdout);
    // Just emit an aggregate count alert if SEL is non-trivially populated;
    // detailed parsing is left to ipmitool/Redfish path. The fallback exists
    // so even bare-IPMI nodes get *something*.
    let count = s.lines().count();
    if count > 0 {
        let b = AlertBuilder::new(host.host(), host.host_id(), cat::BMC_EVENTLOG, "ipmi_sel_count", Severity::Info)
            .value(count.to_string())
            .title("IPMI SEL has entries")
            .message(format!("ipmitool sel elist returned {count} entries. Configure Redfish for full parsing."));
        if let Decision::Emit(a) = tracker.observe(b) { sink.send(Outbound::Alert(a)); }
    }
    Ok(())
}
