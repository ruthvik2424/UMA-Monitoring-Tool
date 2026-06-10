//! SATA / SAS SMART monitoring via `smartctl -a -j`.
//!
//! Lighter-touch than NVMe (smartctl is slower and more expensive). We
//! enumerate via /sys/block/sd* and run smartctl every cfg.poll_s.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Stdio;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// Maximum time to wait for one smartctl invocation.
/// A hung or misbehaving drive can cause smartctl to stall;
/// this cap ensures the poll loop always makes progress.
const SMARTCTL_TIMEOUT: Duration = Duration::from_secs(30);
use uma_shared::{cat, AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::DiskSmartCfg;
use crate::host::HostId;
use crate::priv_cmd::priv_command;
use crate::state::{AlertTracker, Decision};
use crate::vendor_detect::VendorProfile;

pub fn spawn(
    host: HostId,
    cfg: DiskSmartCfg,
    sink: AlertSink,
    tracker: AlertTracker,
    vendor: VendorProfile,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("disk_smart disabled");
            return;
        }
        if !vendor.has_smartctl {
            warn!("disk_smart: smartctl not present — disabling");
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_secs(cfg.poll_s.max(60)));
        // Never accumulate missed ticks — if smartctl stalls on a bad drive,
        // wait the full poll_s after it finishes before trying again.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            for dev in enumerate() {
                if let Err(e) = poll_one(&host, &sink, &tracker, &dev).await {
                    debug!(device = %dev, error = %e, "disk_smart: poll failed");
                }
            }
        }
    })
}

fn enumerate() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir("/sys/block")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| (n.starts_with("sd") || n.starts_with("hd")) && !n.contains("boot"))
        .collect();
    names.sort(); // deterministic order: sda, sdb, sdc ...

    // Only include devices whose /dev node actually exists.
    // /sys/block may contain stale entries for unplugged or iSCSI-detached
    // drives — running smartctl on a missing device node is harmless but noisy.
    names
        .into_iter()
        .map(|n| format!("/dev/{n}"))
        .filter(|dev| std::path::Path::new(dev).exists())
        .collect()
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SmartReport {
    smart_status: Option<SmartStatus>,
    temperature: Option<TempBlock>,
    ata_smart_attributes: Option<AtaAttrs>,
    model_name: Option<String>,
    device: Option<DeviceBlock>,
}
#[derive(Debug, Deserialize)] struct SmartStatus { passed: Option<bool> }
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TempBlock   { current: Option<i32> }
#[derive(Debug, Deserialize)] struct AtaAttrs    { table: Option<Vec<AtaAttr>> }
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AtaAttr {
    id: u32,
    name: String,
    raw: AttrRaw,
    when_failed: Option<String>,
}
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AttrRaw {
    value: u64,
    #[serde(default)]
    string: String,
}
#[derive(Debug, Deserialize)] struct DeviceBlock { #[serde(rename = "type")] _type: Option<String> }

async fn poll_one(host: &HostId, sink: &AlertSink, tracker: &AlertTracker, dev: &str) -> Result<()> {
    let child = priv_command("smartctl")
        .args(["-a", "-j", dev])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawning smartctl")?;
    let o = timeout(SMARTCTL_TIMEOUT, child.wait_with_output())
        .await
        .context("smartctl timed out (30s) — drive may be unresponsive")?
        .context("smartctl wait failed")?;
    // smartctl exits non-zero on any health concern; still parse stdout.
    let report: SmartReport = serde_json::from_slice(&o.stdout)
        .context("parsing smartctl JSON")?;

    let model = report.model_name.clone().unwrap_or_else(|| "unknown".into());
    if let Some(s) = &report.smart_status {
        if matches!(s.passed, Some(false)) {
            let b = AlertBuilder::new(
                host.host(),
                host.host_id(),
                cat::DISK_SMART,
                "smart_overall",
                Severity::Critical,
            )
            .device(dev)
            .device_model(&model)
            .value("FAILED")
            .threshold("PASSED")
            .title("SMART overall health FAILED")
            .message(format!(
                "Drive {dev} ({model}) reports SMART overall health FAILED. \
                 Replace this drive ASAP."
            ));
            if let Decision::Emit(a) = tracker.observe(b) { sink.send(Outbound::Alert(a)); }
        }
    }
    let mut bad_attrs: Vec<String> = Vec::new();
    if let Some(attrs) = &report.ata_smart_attributes {
        if let Some(table) = &attrs.table {
            for a in table {
                let interesting = matches!(
                    a.id,
                    5    // reallocated_sector_ct
                    | 197 // current_pending_sector
                    | 198 // offline_uncorrectable
                    | 187 // reported_uncorrect
                );
                let failing = a.when_failed.as_deref().unwrap_or("-") != "-";
                if (interesting && a.raw.value > 0) || failing {
                    bad_attrs.push(format!("{}={}", a.name, a.raw.value));
                    let b = AlertBuilder::new(
                        host.host(),
                        host.host_id(),
                        cat::DISK_SMART,
                        format!("attr_{}", a.name.to_lowercase()),
                        if failing { Severity::Critical } else { Severity::Warning },
                    )
                    .device(dev)
                    .device_model(&model)
                    .value(a.raw.value.to_string())
                    .title(format!("SMART attribute {} non-zero", a.name))
                    .message(format!(
                        "Drive {dev} ({model}) attribute {} = {}{}.",
                        a.name,
                        a.raw.value,
                        if failing { " (FAILING_NOW)" } else { "" }
                    ));
                    if let Decision::Emit(a) = tracker.observe(b) { sink.send(Outbound::Alert(a)); }
                }
            }
        }
    }

    let smart_status = report.smart_status
        .and_then(|s| s.passed)
        .map(|p| if p { "passed" } else { "FAILED" })
        .unwrap_or("unknown");
    let temp_c = report.temperature.and_then(|t| t.current);
    info!(
        device  = %dev,
        model   = %model,
        smart   = %smart_status,
        temp_c  = ?temp_c,
        bad_attrs = %if bad_attrs.is_empty() { "none".to_string() } else { bad_attrs.join(", ") },
        "disk_smart poll"
    );
    Ok(())
}
