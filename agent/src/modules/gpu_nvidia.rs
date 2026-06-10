//! NVIDIA GPU monitoring via `nvidia-smi --query-gpu=...`.
//!
//! NVML would be lower latency but it requires linking against the NVIDIA
//! driver headers. nvidia-smi is universally available wherever the driver
//! is installed, has a stable CSV output mode, and works on every Ubuntu
//! version we care about.

use anyhow::Result;
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
        if !cfg.enabled || !vendor.info.gpu_vendors.iter().any(|v| v == "nvidia") {
            info!("gpu_nvidia disabled (cfg or no nvidia GPU)");
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match read_gpus().await {
                Ok(rows) => {
                    for r in rows {
                        check(&host, &sink, &tracker, &r);
                    }
                }
                Err(e) => debug!(error = %e, "gpu_nvidia: query failed"),
            }
        }
    })
}

#[derive(Debug)]
struct GpuRow {
    index: String,
    name: String,
    temp_c: i32,
    power_w: f32,
    power_limit_w: f32,
    util: u32,
    ecc_uncorr: u64,
    throttle_reasons: String,
}

async fn read_gpus() -> Result<Vec<GpuRow>> {
    let o = run("nvidia-smi", &[
        "--query-gpu=index,name,temperature.gpu,power.draw,power.limit,utilization.gpu,ecc.errors.uncorrected.volatile.total,clocks_throttle_reasons.active",
        "--format=csv,noheader,nounits",
    ], TIMEOUT_GPU).await?;
    if !o.status.success() {
        return Err(anyhow::anyhow!("nvidia-smi failed: {}", String::from_utf8_lossy(&o.stderr)));
    }
    let s = String::from_utf8_lossy(&o.stdout);
    let mut out = Vec::new();
    for line in s.lines() {
        let cols: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if cols.len() < 8 { continue; }
        out.push(GpuRow {
            index: cols[0].into(),
            name: cols[1].into(),
            temp_c: cols[2].parse().unwrap_or(0),
            power_w: cols[3].parse().unwrap_or(0.0),
            power_limit_w: cols[4].parse().unwrap_or(0.0),
            util: cols[5].parse().unwrap_or(0),
            ecc_uncorr: cols[6].parse().unwrap_or(0),
            throttle_reasons: cols[7].into(),
        });
    }
    Ok(out)
}

fn check(host: &HostId, sink: &AlertSink, tracker: &AlertTracker, r: &GpuRow) {
    let dev = format!("gpu{}", r.index);
    if r.ecc_uncorr > 0 {
        let b = AlertBuilder::new(host.host(), host.host_id(), cat::GPU_NVIDIA, "ecc_uncorrected", Severity::Critical)
            .device(&dev).device_model(&r.name)
            .value(r.ecc_uncorr.to_string())
            .title("NVIDIA GPU uncorrectable ECC")
            .message(format!("GPU {} ({}) has {} uncorrectable ECC errors. Replace this GPU.", r.index, r.name, r.ecc_uncorr));
        if let Decision::Emit(a) = tracker.observe(b) { sink.send(Outbound::Alert(a)); }
    }
    if r.temp_c >= 85 {
        let b = AlertBuilder::new(host.host(), host.host_id(), cat::GPU_NVIDIA, "temperature", Severity::Critical)
            .device(&dev).device_model(&r.name)
            .value(format!("{}°C", r.temp_c)).threshold("85°C")
            .title("NVIDIA GPU critical temperature")
            .message(format!("GPU {} ({}) at {}°C — at or above thermal slowdown threshold.", r.index, r.name, r.temp_c));
        if let Decision::Emit(a) = tracker.observe(b) { sink.send(Outbound::Alert(a)); }
    }
    let parsed_throttle: u64 = u64::from_str_radix(r.throttle_reasons.trim_start_matches("0x"), 16).unwrap_or(0);
    // Bit 0 (GPU idle) doesn't matter; anything else does.
    if parsed_throttle & !1 != 0 {
        let b = AlertBuilder::new(host.host(), host.host_id(), cat::GPU_NVIDIA, "throttling", Severity::Warning)
            .device(&dev).device_model(&r.name)
            .value(r.throttle_reasons.clone())
            .title("NVIDIA GPU throttling")
            .message(format!("GPU {} ({}) actively throttling: clocks_throttle_reasons.active = {}.", r.index, r.name, r.throttle_reasons));
        if let Decision::Emit(a) = tracker.observe(b) { sink.send(Outbound::Alert(a)); }
    }
    if r.power_limit_w > 0.0 && r.power_w > r.power_limit_w * 0.98 {
        let b = AlertBuilder::new(host.host(), host.host_id(), cat::GPU_NVIDIA, "power_at_limit", Severity::Info)
            .device(&dev).device_model(&r.name)
            .value(format!("{:.0}W / {:.0}W", r.power_w, r.power_limit_w))
            .title("NVIDIA GPU at power limit")
            .message(format!("GPU {} drawing {:.0}W (98% of {:.0}W cap). Util {}%.", r.index, r.power_w, r.power_limit_w, r.util));
        if let Decision::Emit(a) = tracker.observe(b) { sink.send(Outbound::Alert(a)); }
    }
}
