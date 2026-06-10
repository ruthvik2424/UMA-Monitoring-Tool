//! Memory-pressure & OOM detection.
//!
//! - PSI (`/proc/pressure/memory`): early-warning when `some avg10` stays
//!   above the configured threshold for `sustain_s` seconds.
//! - kmsg: parses every "Out of memory: Killed process" line and emits a
//!   per-victim alert with PID/comm/RSS/oom_score.
//! - Storm detection: ≥ N OOM kills in a rolling window → escalate.
//!
//! The agent itself sets `OOMScoreAdjust=-1000` in its systemd unit so that
//! it survives long enough to actually report the event.

use anyhow::Result;
use regex::Regex;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use uma_shared::{cat, AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::MemPressureCfg;
use crate::host::HostId;
use crate::kmsg::KmsgLine;
use crate::state::{AlertTracker, Decision};

pub fn spawn(
    host: HostId,
    cfg: MemPressureCfg,
    sink: AlertSink,
    tracker: AlertTracker,
    kmsg: broadcast::Receiver<KmsgLine>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("mempressure disabled");
            return;
        }
        let h_psi = tokio::spawn(psi_loop(host.clone(), cfg.clone(), sink.clone(), tracker.clone()));
        let h_oom = tokio::spawn(oom_loop(host, cfg, sink, tracker, kmsg));
        let _ = tokio::join!(h_psi, h_oom);
    })
}

async fn psi_loop(host: HostId, cfg: MemPressureCfg, sink: AlertSink, tracker: AlertTracker) {
    if !std::path::Path::new("/proc/pressure/memory").exists() {
        warn!("mempressure: PSI not available on this kernel; skipping");
        return;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.poll_s.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut breach_since: Option<Instant> = None;

    loop {
        tick.tick().await;
        let some_avg10 = match read_psi() {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, "mempressure: PSI read failed");
                continue;
            }
        };
        if some_avg10 >= cfg.psi_some_avg10_warn {
            let t0 = breach_since.get_or_insert_with(Instant::now);
            if t0.elapsed() >= Duration::from_secs(cfg.sustain_s) {
                let b = AlertBuilder::new(
                    host.host(),
                    host.host_id(),
                    cat::MEMORY_PRESSURE,
                    "psi_high",
                    Severity::Warning,
                )
                .value(format!("{some_avg10:.1}%"))
                .threshold(format!("{:.1}% for {}s", cfg.psi_some_avg10_warn, cfg.sustain_s))
                .title("Sustained memory pressure")
                .message(format!(
                    "PSI memory `some avg10` has been {:.1}% for at least {}s. \
                     The kernel is repeatedly stalling tasks waiting on memory; \
                     the OOM killer is likely to fire soon.",
                    some_avg10, cfg.sustain_s
                ));
                if let Decision::Emit(a) = tracker.observe(b) {
                    sink.send(Outbound::Alert(a));
                }
            }
        } else {
            breach_since = None;
            // Try to mark the matching alert resolved.
            let fp = format!("{}|{}|-|psi_high", host.host_id(), cat::MEMORY_PRESSURE);
            if let Some(resolved) = tracker.observe_ok(&fp) {
                sink.send(Outbound::Alert(resolved));
            }
        }
    }
}

fn read_psi() -> Result<f32> {
    let s = std::fs::read_to_string("/proc/pressure/memory")?;
    // Format:
    //   some avg10=0.00 avg60=0.00 avg300=0.00 total=...
    //   full avg10=0.00 ...
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("some ") {
            for tok in rest.split_whitespace() {
                if let Some(v) = tok.strip_prefix("avg10=") {
                    return Ok(v.parse().unwrap_or(0.0));
                }
            }
        }
    }
    Ok(0.0)
}

async fn oom_loop(
    host: HostId,
    cfg: MemPressureCfg,
    sink: AlertSink,
    tracker: AlertTracker,
    mut rx: broadcast::Receiver<KmsgLine>,
) {
    // Examples of lines we care about:
    //   "Out of memory: Killed process 12345 (postgres) total-vm:..."
    //   "invoked oom-killer: gfp_mask=..."
    let killed_re = Regex::new(
        r"Out of memory: Killed process (\d+) \(([^)]+)\)(.*total-vm:(\d+)kB)?"
    ).unwrap();
    let mut storm: VecDeque<Instant> = VecDeque::new();

    loop {
        match rx.recv().await {
            Ok(line) => {
                if let Some(c) = killed_re.captures(&line.text) {
                    let pid = c.get(1).map(|m| m.as_str()).unwrap_or("?").to_string();
                    let comm = c.get(2).map(|m| m.as_str()).unwrap_or("?").to_string();
                    let total_vm_kb = c.get(4).and_then(|m| m.as_str().parse::<u64>().ok());

                    let b = AlertBuilder::new(
                        host.host(),
                        host.host_id(),
                        cat::MEMORY_PRESSURE,
                        "oom_kill",
                        Severity::Critical,
                    )
                    .device(&comm)
                    .value(format!("PID {pid}"))
                    .title("OOM killer fired")
                    .message(format!(
                        "Kernel OOM killer terminated process {pid} ({comm}){}. \
                         The host is under heavy memory pressure; investigate \
                         the workload that grew unbounded.",
                        match total_vm_kb {
                            Some(kb) => format!(" with total-vm={}MB", kb / 1024),
                            None => "".into(),
                        }
                    ))
                    .raw_kv("pid", serde_json::json!(pid))
                    .raw_kv("comm", serde_json::json!(comm))
                    .raw_kv("kmsg", serde_json::json!(line.text));
                    if let Decision::Emit(a) = tracker.observe(b) {
                        sink.send(Outbound::Alert(a));
                    }

                    let now = Instant::now();
                    let win = Duration::from_secs(cfg.oom_storm_window_s);
                    storm.push_back(now);
                    while let Some(&t) = storm.front() {
                        if now.duration_since(t) > win {
                            storm.pop_front();
                        } else {
                            break;
                        }
                    }
                    if storm.len() as u32 >= cfg.oom_storm_count {
                        let b = AlertBuilder::new(
                            host.host(),
                            host.host_id(),
                            cat::MEMORY_PRESSURE,
                            "oom_storm",
                            Severity::Critical,
                        )
                        .value(format!("{} kills / {}s", storm.len(), cfg.oom_storm_window_s))
                        .threshold(format!("{} / {}s", cfg.oom_storm_count, cfg.oom_storm_window_s))
                        .title("OOM kill storm")
                        .message(format!(
                            "{} processes killed by OOM in the last {}s — the \
                             host is in a memory-thrashing loop. Cordoning the \
                             node from the orchestrator is recommended.",
                            storm.len(),
                            cfg.oom_storm_window_s
                        ));
                        if let Decision::Emit(a) = tracker.observe(b) {
                            sink.send(Outbound::Alert(a));
                        }
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("mempressure: kmsg lag {n}");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}
