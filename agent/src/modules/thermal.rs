//! Thermal monitoring.
//!
//! Three sources combined:
//!   - /sys/class/thermal/thermal_zone*/temp  (CPU/PCH zones)
//!   - /sys/devices/system/cpu/cpu*/thermal_throttle/*  (per-core throttle counters)
//!   - IPMI `ipmitool sdr type Temperature`  for the **inlet ambient** sensor
//!     (vendor-named "01-Inlet Ambient" / "Inlet Temp")
//!
//! The user has been hitting vendor auto-shutdown at ~42°C inlet. We fire
//! tiered alerts at 32 / 36 / 38 °C with a rate-of-rise estimate so an
//! operator can evict workloads before the BMC pulls the plug.

use anyhow::Result;
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tracing::{debug, info};
use uma_shared::{cat, AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::ThermalCfg;
use crate::host::HostId;
use crate::priv_cmd::{run, TIMEOUT_IPMI};
use crate::state::{AlertTracker, Decision};
use crate::vendor_detect::VendorProfile;

pub fn spawn(
    host: HostId,
    cfg: ThermalCfg,
    sink: AlertSink,
    tracker: AlertTracker,
    vendor: VendorProfile,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("thermal disabled");
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_secs(cfg.poll_s.max(2)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_throttle: HashMap<String, u64> = HashMap::new();
        let mut last_inlet: Option<(Instant, f32)> = None;

        // Check once at startup whether IPMI device is present.
        // If not, skip ipmitool on every poll — no point retrying every 5s.
        let ipmi_available = vendor.has_ipmitool
            && (std::path::Path::new("/dev/ipmi0").exists()
                || std::path::Path::new("/dev/ipmi/0").exists()
                || std::path::Path::new("/dev/ipmidev/0").exists());
        if vendor.has_ipmitool && !ipmi_available {
            info!("thermal: ipmitool present but no /dev/ipmi0 device — inlet temp via IPMI disabled (VM or no BMC)");
        }
        loop {
            tick.tick().await;

            // 1. Per-CPU throttle counters
            for (cpu, count) in read_throttle_counters() {
                let prev = last_throttle.insert(cpu.clone(), count).unwrap_or(count);
                if count > prev {
                    let b = AlertBuilder::new(
                        host.host(),
                        host.host_id(),
                        cat::THERMAL,
                        "cpu_throttle",
                        Severity::Warning,
                    )
                    .device(&cpu)
                    .value((count - prev).to_string())
                    .title("CPU thermal throttling")
                    .message(format!(
                        "{cpu} throttled {} more time(s) since the last poll \
                         (cumulative {count}). Check airflow and inlet temperature.",
                        count - prev
                    ));
                    if let Decision::Emit(a) = tracker.observe(b) {
                        sink.send(Outbound::Alert(a));
                    }
                }
            }

            // 2. Inlet ambient (BMC).
            if ipmi_available {
                match read_inlet_temp().await {
                    Ok(Some(t)) => {
                        let now = Instant::now();
                        let rate = last_inlet
                            .map(|(t0, v0)| (t - v0) / now.duration_since(t0).as_secs_f32().max(1.0) * 60.0)
                            .unwrap_or(0.0);
                        last_inlet = Some((now, t));

                        let (sev, label) = if t >= cfg.inlet_emergency_c {
                            (Some(Severity::Critical), "EMERGENCY")
                        } else if t >= cfg.inlet_critical_c {
                            (Some(Severity::Critical), "critical")
                        } else if t >= cfg.inlet_warn_c {
                            (Some(Severity::Warning), "warning")
                        } else {
                            (None, "ok")
                        };
                        info!(
                            inlet_c = %format!("{t:.1}"),
                            rate_c_per_min = %format!("{rate:.2}"),
                            status = %label,
                            warn_threshold = %cfg.inlet_warn_c,
                            crit_threshold = %cfg.inlet_critical_c,
                            "thermal poll"
                        );
                        let label = if label == "ok" { "" } else { label };
                        if let Some(sev) = sev {
                            let metric = if t >= cfg.inlet_emergency_c {
                                "inlet_temp_emergency"
                            } else if t >= cfg.inlet_critical_c {
                                "inlet_temp_critical"
                            } else {
                                "inlet_temp_warning"
                            };
                            let est_to_shutdown = if rate > 0.0 {
                                let to = ((42.0 - t) / rate * 60.0).max(0.0);
                                format!("ETA to vendor shutdown (~42°C): {:.0}s at +{:.2}°C/min", to, rate)
                            } else {
                                "Stable or cooling.".into()
                            };
                            let b = AlertBuilder::new(
                                host.host(),
                                host.host_id(),
                                cat::THERMAL,
                                metric,
                                sev,
                            )
                            .device("inlet")
                            .value(format!("{t:.1}°C"))
                            .threshold(format!(
                                "warn={}, crit={}, emerg={}",
                                cfg.inlet_warn_c, cfg.inlet_critical_c, cfg.inlet_emergency_c
                            ))
                            .title(format!("Inlet temperature {label}"))
                            .message(format!(
                                "BMC inlet ambient sensor reads {t:.1}°C. {est_to_shutdown}. \
                                 Consider migrating workloads off this host before the BMC \
                                 forces an emergency shutdown."
                            ))
                            .raw_kv("rate_c_per_min", serde_json::json!(rate));
                            if let Decision::Emit(a) = tracker.observe(b) {
                                sink.send(Outbound::Alert(a));
                            }
                        }
                    }
                    Ok(None) => {} // sensor not present
                    Err(e) => debug!(error = %e, "thermal: inlet read failed"),
                }
            }
        }
    })
}

fn read_throttle_counters() -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let pattern = "/sys/devices/system/cpu/cpu*/thermal_throttle/core_throttle_count";
    if let Ok(paths) = glob::glob(pattern) {
        for entry in paths.flatten() {
            let cpu = entry
                .ancestors()
                .nth(2)
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Ok(t) = std::fs::read_to_string(&entry) {
                if let Ok(n) = t.trim().parse::<u64>() {
                    out.push((cpu, n));
                }
            }
        }
    }
    out
}

/// Parse `'NN[.NN] degrees C'` values from an `ipmitool sdr type Temperature`
/// row. Prefer the canonical reading column — never treat middle columns
/// (`55.1` sensor-id style noise on HPE, `7.1` priority on Dell) as °C.
static TEMP_DEG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*degrees?\s*c\b").expect("TEMP_DEG_RE")
});
static RE_DIG_INLET_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*\d+-inlet\b").expect("RE_DIG_INLET_PREFIX"));
/// PSU-related inlet rows (`P/S 1 Inlet`, etc.) — not chassis ambient.
static RE_PSU_INLET: LazyLock<Regex> = LazyLock::new(|| {
    // Rust's `regex` crate does not support look-around; `\b` catches `psu`
    // whole-word without false positives like `capsule`.
    Regex::new(r"(?i)p\s*/\s*s|power\s+supply|\bpsu\b").expect("RE_PSU_INLET")
});

fn parse_degrees_columns(line: &str) -> Vec<f32> {
    TEMP_DEG_RE
        .captures_iter(line)
        .filter_map(|c| c[1].parse::<f32>().ok())
        .filter(|&t| (-5.0..125.0).contains(&t))
        .collect()
}

/// Prefer chassis **inlet ambient** (HPE `01-Inlet Ambient`) over **PSU inlet**
/// (`P/S N Inlet`, ~37°C) which falsely trips shutdown-threshold alerting.
fn inlet_sensor_priority(name: &str) -> Option<(i32, bool)> {
    let n = name.to_lowercase();
    let n = n.trim();
    if n.contains("ambient") || n.contains("ambient temp") || n.contains("ambient temperature") {
        return Some((1000, false));
    }
    if n.contains("room") || n.contains("air inlet") {
        return Some((900, false));
    }
    if (n.contains("inlet temp")
        || n.contains("-inlet")
        || RE_DIG_INLET_PREFIX.is_match(n))
        && !is_psu_inlet_like(n)
    {
        return Some((850, false));
    }
    if n.contains("inlet") && is_psu_inlet_like(n) {
        return Some((100, true)); // PSU inlet — skipped if chassis ambient exists
    }
    if n.contains("inlet") {
        return Some((300, false));
    }
    None
}

fn is_psu_inlet_like(n: &str) -> bool {
    RE_PSU_INLET.is_match(n)
}

async fn read_inlet_temp() -> Result<Option<f32>> {
    let o = run("ipmitool", &["sdr", "type", "Temperature"], TIMEOUT_IPMI).await?;
    if !o.status.success() {
        return Ok(None);
    }
    let s = String::from_utf8_lossy(&o.stdout);
    let mut rows: Vec<(i32, bool, f32)> = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        let Some(sensor_name) = line.split('|').next().map(|c| c.trim()) else {
            continue;
        };
        let sensor_name_lc = sensor_name.to_lowercase();
        let Some((prio, psu_fallback)) = inlet_sensor_priority(sensor_name_lc.as_str()) else {
            continue;
        };
        let temps = parse_degrees_columns(line);
        let Some(t) = temps.last().copied() else {
            continue;
        };
        rows.push((prio, psu_fallback, t));
    }
    rows.sort_by_key(|&(p, _, _)| std::cmp::Reverse(p));

    let has_true_ambient = rows.iter().any(|(prio, _, _)| *prio >= 850);
    for (prio, psu_fallback, t) in &rows {
        if *prio >= 850 {
            debug!(degrees = %t, "thermal: inlet from preferred ambient-ish sensor row");
            return Ok(Some(*t));
        }
        if has_true_ambient && *psu_fallback {
            continue;
        }
        if *prio >= 300 {
            debug!(degrees = %t, "thermal: inlet from inlet-labelled sensor row");
            return Ok(Some(*t));
        }
    }
    Ok(rows.first().map(|(_, _, t)| *t))
}
