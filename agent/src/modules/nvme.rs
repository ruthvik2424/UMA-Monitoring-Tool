//! NVMe SMART monitoring.
//!
//! Polls every drive in /sys/class/nvme/nvmeN with `nvme smart-log -o json`
//! once per `poll_s`. Detects:
//!   - critical_warning bitmap (any bit set is a vendor-defined fault)
//!   - percentage_used (wear) crossing warn/critical thresholds
//!   - media_errors > 0 (delta-only after first sample)
//!   - num_err_log_entries delta (something new in the controller log)
//!   - composite_temperature outside warn/critical
//!
//! `nvme-cli` is the standard tool on every modern Ubuntu host; we shell
//! out to it because the in-kernel ioctl is unstable across kernel versions.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

const NVME_CMD_TIMEOUT: Duration = Duration::from_secs(20);
use tokio::task::JoinHandle;

use crate::priv_cmd::priv_command;
use tracing::{debug, info, warn};
use uma_shared::{cat, AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::NvmeCfg;
use crate::host::HostId;
use crate::state::{AlertTracker, Decision};

pub fn spawn(host: HostId, cfg: NvmeCfg, sink: AlertSink, tracker: AlertTracker) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("nvme module disabled");
            return;
        }
        if !cmd_exists("nvme").await {
            warn!("nvme module: `nvme` CLI not present — disabling");
            return;
        }
        info!("nvme module starting (poll = {}s)", cfg.poll_s);
        let mut tick = tokio::time::interval(Duration::from_secs(cfg.poll_s.max(5)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_err_log: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();

        loop {
            tick.tick().await;
            let drives = match enumerate_drives() {
                Ok(d) => d,
                Err(e) => {
                    debug!(error = %e, "nvme: enumerate failed");
                    continue;
                }
            };
            for dev in drives {
                if let Err(e) =
                    poll_one(&host, &cfg, &sink, &tracker, &mut last_err_log, &dev).await
                {
                    debug!(device = %dev.display(), error = %e, "nvme: poll failed");
                }
            }
        }
    })
}

fn enumerate_drives() -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/dev")? {
        let e = entry?;
        let name = e.file_name();
        let s = name.to_string_lossy();
        // /dev/nvme0n1, /dev/nvme1n1, etc. (skip namespace partitions)
        if s.starts_with("nvme")
            && s.contains('n')
            && !s.contains('p')
            && s.chars().filter(|c| c.is_ascii_digit()).count() >= 2
        {
            out.push(e.path());
        }
    }
    Ok(out)
}

async fn poll_one(
    host: &HostId,
    cfg: &NvmeCfg,
    sink: &AlertSink,
    tracker: &AlertTracker,
    last_err_log: &mut std::collections::HashMap<String, u64>,
    dev: &std::path::Path,
) -> Result<()> {
    let dev_str = dev.to_string_lossy().to_string();
    let smart = run_smart_log(&dev_str).await?;
    let model = read_model(dev).unwrap_or_else(|| "unknown".into());

    // 1. critical_warning bitmap.
    if smart.critical_warning != 0 {
        let bits = decode_critical_warning(smart.critical_warning);
        let b = AlertBuilder::new(
            host.host(),
            host.host_id(),
            cat::NVME,
            "critical_warning",
            Severity::Critical,
        )
        .device(&dev_str)
        .device_model(&model)
        .value(format!("0x{:02x}", smart.critical_warning))
        .threshold("0x00")
        .title("NVMe drive reporting reliability degradation")
        .message(format!(
            "NVMe drive {dev_str} ({model}) raised critical_warning 0x{:02x} ({}). \
             Wear level is {}%. {} media errors logged. Recommend replacement \
             during the next maintenance window.",
            smart.critical_warning, bits, smart.percentage_used, smart.media_errors
        ))
        .raw_kv("critical_warning", serde_json::json!(smart.critical_warning))
        .raw_kv("percentage_used", serde_json::json!(smart.percentage_used))
        .raw_kv("media_errors", serde_json::json!(smart.media_errors));

        emit(sink, tracker, b);
    } else {
        // Recovery path.
        let fp = format!(
            "{}|{}|{}|critical_warning",
            host.host_id(),
            cat::NVME,
            dev_str
        );
        if let Some(resolved) = tracker.observe_ok(&fp) {
            sink.send(Outbound::Alert(resolved));
        }
    }

    // 2. wear.
    let wear = smart.percentage_used as u8;
    let (sev, do_alert) = if wear >= cfg.wear_critical_pct {
        (Some(Severity::Critical), true)
    } else if wear >= cfg.wear_warn_pct {
        (Some(Severity::Warning), true)
    } else {
        (None, false)
    };
    if do_alert {
        let b = AlertBuilder::new(
            host.host(),
            host.host_id(),
            cat::NVME,
            "percentage_used",
            sev.unwrap(),
        )
        .device(&dev_str)
        .device_model(&model)
        .value(wear.to_string())
        .threshold(format!(
            "warn={}%, critical={}%",
            cfg.wear_warn_pct, cfg.wear_critical_pct
        ))
        .title("NVMe drive wear approaching end-of-life")
        .message(format!(
            "NVMe drive {dev_str} ({model}) reports {wear}% of rated write \
             endurance consumed. Plan replacement before it crosses 100%."
        ));
        emit(sink, tracker, b);
    }

    // 3. media_errors (any non-zero is concerning; treat first observation
    //    as the baseline, then alert on any new error).
    if smart.media_errors > 0 {
        let b = AlertBuilder::new(
            host.host(),
            host.host_id(),
            cat::NVME,
            "media_errors",
            Severity::Warning,
        )
        .device(&dev_str)
        .device_model(&model)
        .value(smart.media_errors.to_string())
        .threshold("0")
        .title("NVMe drive reporting media errors")
        .message(format!(
            "NVMe drive {dev_str} ({model}) has {} cumulative media errors. \
             Watch for further growth — this is a leading indicator of failure.",
            smart.media_errors
        ));
        emit(sink, tracker, b);
    }

    // 4. error log delta.
    let prev = last_err_log.get(&dev_str).copied().unwrap_or(smart.num_err_log_entries);
    if smart.num_err_log_entries > prev {
        let new = smart.num_err_log_entries - prev;
        let b = AlertBuilder::new(
            host.host(),
            host.host_id(),
            cat::NVME,
            "err_log_entries",
            Severity::Warning,
        )
        .device(&dev_str)
        .device_model(&model)
        .value(smart.num_err_log_entries.to_string())
        .title("NVMe controller logged new error entries")
        .message(format!(
            "NVMe drive {dev_str} ({model}) added {new} new error log entry(ies); \
             total now {}. Run `nvme error-log {dev_str}` for details.",
            smart.num_err_log_entries
        ));
        emit(sink, tracker, b);
    }
    last_err_log.insert(dev_str.clone(), smart.num_err_log_entries);

    // 5. composite temperature (Kelvin in nvme-cli output).
    let temp_c = (smart.composite_temperature as i32).saturating_sub(273);
    if temp_c >= cfg.temp_critical_c {
        let b = AlertBuilder::new(
            host.host(),
            host.host_id(),
            cat::NVME,
            "temperature",
            Severity::Critical,
        )
        .device(&dev_str)
        .device_model(&model)
        .value(format!("{temp_c}°C"))
        .threshold(format!("{}°C", cfg.temp_critical_c))
        .title("NVMe drive critically hot")
        .message(format!(
            "NVMe drive {dev_str} ({model}) reports composite temperature {temp_c}°C. \
             Verify chassis airflow and inlet temperature."
        ));
        emit(sink, tracker, b);
    } else if temp_c >= cfg.temp_warn_c {
        let b = AlertBuilder::new(
            host.host(),
            host.host_id(),
            cat::NVME,
            "temperature",
            Severity::Warning,
        )
        .device(&dev_str)
        .device_model(&model)
        .value(format!("{temp_c}°C"))
        .threshold(format!("{}°C", cfg.temp_warn_c))
        .title("NVMe drive running hot")
        .message(format!(
            "NVMe drive {dev_str} ({model}) is at {temp_c}°C — above the \
             configured warning threshold."
        ));
        emit(sink, tracker, b);
    }

    // Execution log: one INFO line per poll so operators can see live values
    // without needing to parse the collector GUI or run nvme-cli manually.
    info!(
        device  = %dev_str,
        model   = %model,
        temp_c,
        wear_pct = smart.percentage_used,
        media_errors = smart.media_errors,
        err_log = smart.num_err_log_entries,
        spare_pct = smart.available_spare,
        "nvme poll"
    );

    Ok(())
}

fn emit(sink: &AlertSink, tracker: &AlertTracker, b: AlertBuilder) {
    if let Decision::Emit(a) = tracker.observe(b) {
        sink.send(Outbound::Alert(a));
    }
}

fn decode_critical_warning(b: u8) -> String {
    let mut bits = Vec::new();
    if b & 0x01 != 0 { bits.push("spare-below-threshold"); }
    if b & 0x02 != 0 { bits.push("temperature-threshold-exceeded"); }
    if b & 0x04 != 0 { bits.push("reliability-degraded"); }
    if b & 0x08 != 0 { bits.push("read-only"); }
    if b & 0x10 != 0 { bits.push("volatile-backup-failed"); }
    if b & 0x20 != 0 { bits.push("persistent-memory-region-readonly"); }
    if bits.is_empty() { "unknown".into() } else { bits.join(", ") }
}

#[derive(Debug, Deserialize)]
#[allow(non_snake_case, dead_code)]
struct NvmeSmartLog {
    #[serde(default)]
    critical_warning: u8,
    #[serde(default)]
    composite_temperature: u32, // Kelvin
    #[serde(default)]
    percentage_used: u32,
    #[serde(default)]
    media_errors: u64,
    #[serde(default)]
    num_err_log_entries: u64,
    #[serde(default)]
    available_spare: u32,
    #[serde(default)]
    available_spare_threshold: u32,
}

async fn run_smart_log(dev: &str) -> Result<NvmeSmartLog> {
    let child = priv_command("nvme")
        .args(["smart-log", dev, "-o", "json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawning nvme smart-log")?;
    let out = timeout(NVME_CMD_TIMEOUT, child.wait_with_output())
        .await
        .context("nvme smart-log timed out (20s) — drive may be unresponsive")?
        .context("nvme smart-log wait failed")?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "nvme smart-log failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let s: NvmeSmartLog = serde_json::from_slice(&out.stdout)
        .context("parsing nvme smart-log JSON")?;
    Ok(s)
}

fn read_model(dev: &std::path::Path) -> Option<String> {
    // /sys/block/nvme0n1/device/model
    let name = dev.file_name()?.to_string_lossy().into_owned();
    let p = format!("/sys/block/{name}/device/model");
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

async fn cmd_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}
