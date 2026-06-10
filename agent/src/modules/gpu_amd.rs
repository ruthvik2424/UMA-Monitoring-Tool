//! AMD GPU monitoring via `rocm-smi --json`.
//!
//! ROCm is the standard tooling on AMD Instinct/Radeon Pro nodes. If
//! rocm-smi isn't installed we fall back to /sys/class/drm/cardN/device/hwmon
//! for at least temperature.

use anyhow::Result;
use serde_json::Value;
use std::time::Duration;
use tokio::task::JoinHandle;

use crate::priv_cmd::{run, TIMEOUT_GPU};
use tracing::{debug, info};
use uma_shared::{cat, AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::SimpleEnable;
use crate::host::HostId;
use crate::state::{AlertTracker, Decision};
use crate::vendor_detect::VendorProfile;

pub fn spawn(
    host: HostId,
    cfg: SimpleEnable,
    sink: AlertSink,
    tracker: AlertTracker,
    vendor: VendorProfile,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled || !vendor.info.gpu_vendors.iter().any(|v| v == "amd") {
            info!("gpu_amd disabled (cfg or no AMD GPU)");
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if vendor.has_rocm_smi {
                if let Err(e) = poll_rocm(&host, &sink, &tracker).await {
                    debug!(error = %e, "gpu_amd: rocm-smi failed");
                }
            } else {
                poll_sysfs(&host, &sink, &tracker);
            }
        }
    })
}

async fn poll_rocm(host: &HostId, sink: &AlertSink, tracker: &AlertTracker) -> Result<()> {
    let o = run("rocm-smi", &["-a", "--json"], TIMEOUT_GPU).await?;
    if !o.status.success() {
        return Err(anyhow::anyhow!("rocm-smi failed"));
    }
    let v: Value = serde_json::from_slice(&o.stdout)?;
    let map = v.as_object().ok_or_else(|| anyhow::anyhow!("rocm-smi non-object"))?;
    for (card, body) in map {
        if !card.starts_with("card") { continue; }
        let temp = body
            .get("Temperature (Sensor edge) (C)")
            .and_then(|v| v.as_str().and_then(|s| s.parse::<f32>().ok()));
        let throttle = body
            .get("Performance Level")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let Some(t) = temp {
            if t >= 95.0 {
                let b = AlertBuilder::new(
                    host.host(),
                    host.host_id(),
                    cat::GPU_AMD,
                    "temperature",
                    Severity::Critical,
                )
                .device(card)
                .value(format!("{t:.1}°C"))
                .threshold("95°C")
                .title("AMD GPU critical temperature")
                .message(format!("AMD {card} edge temp {t:.1}°C — investigate cooling."));
                if let Decision::Emit(a) = tracker.observe(b) { sink.send(Outbound::Alert(a)); }
            }
        }
        let _ = throttle;
    }
    Ok(())
}

fn poll_sysfs(host: &HostId, sink: &AlertSink, tracker: &AlertTracker) {
    if let Ok(rd) = std::fs::read_dir("/sys/class/drm") {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if !n.starts_with("card") || n.contains('-') { continue; }
            let hwmon_glob = format!("/sys/class/drm/{n}/device/hwmon/hwmon*/temp1_input");
            if let Ok(matches) = glob::glob(&hwmon_glob) {
                for p in matches.flatten() {
                    if let Ok(s) = std::fs::read_to_string(&p) {
                        if let Ok(milli_c) = s.trim().parse::<i32>() {
                            let c = milli_c as f32 / 1000.0;
                            if c >= 95.0 {
                                let b = AlertBuilder::new(
                                    host.host(), host.host_id(),
                                    cat::GPU_AMD, "temperature", Severity::Critical,
                                )
                                .device(&n)
                                .value(format!("{c:.1}°C"))
                                .title("AMD GPU critical temperature (sysfs)")
                                .message(format!("{n} edge temp {c:.1}°C."));
                                if let Decision::Emit(a) = tracker.observe(b) { sink.send(Outbound::Alert(a)); }
                            }
                        }
                    }
                }
            }
        }
    }
}
