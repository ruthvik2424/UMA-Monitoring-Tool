//! PCIe AER (Advanced Error Reporting) detection.
//!
//! Parses kmsg lines from `pcieport`, `aer`. Resolves the PCI BDF to a
//! human device name via `lspci -s <bdf>` so the alert says
//! "Mellanox ConnectX-6 in slot 5" instead of "0000:5e:00.0".

use regex::Regex;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

use crate::priv_cmd::{run, TIMEOUT_GENERAL};
use tokio::task::JoinHandle;
use tracing::{info, warn};
use uma_shared::{cat, AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::PcieAerCfg;
use crate::host::HostId;
use crate::kmsg::KmsgLine;
use crate::state::{AlertTracker, Decision};

pub fn spawn(
    host: HostId,
    cfg: PcieAerCfg,
    sink: AlertSink,
    tracker: AlertTracker,
    mut rx: broadcast::Receiver<KmsgLine>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("pcie_aer disabled");
            return;
        }
        let bdf_re = Regex::new(r"([0-9a-fA-F]{4}:[0-9a-fA-F]{2}:[0-9a-fA-F]{2}\.[0-9a-fA-F])").unwrap();
        let uncorr_re = Regex::new(r"(?i)AER:.*Uncorrected").unwrap();
        let corr_re   = Regex::new(r"(?i)AER:.*Corrected").unwrap();

        let mut corr_window: VecDeque<Instant> = VecDeque::new();

        loop {
            match rx.recv().await {
                Ok(line) => {
                    let t = &line.text;
                    let is_uncorr = uncorr_re.is_match(t);
                    let is_corr = corr_re.is_match(t);
                    if !is_uncorr && !is_corr {
                        continue;
                    }
                    let bdf = bdf_re.captures(t).map(|c| c[1].to_string());
                    let device_label = match &bdf {
                        Some(b) => describe_pci(b).await.unwrap_or_else(|| b.clone()),
                        None => "unknown".into(),
                    };
                    if is_uncorr {
                        let b = AlertBuilder::new(
                            host.host(),
                            host.host_id(),
                            cat::PCIE_AER,
                            "uncorrectable",
                            Severity::Critical,
                        )
                        .device(bdf.clone().unwrap_or_default())
                        .device_model(&device_label)
                        .title("PCIe uncorrectable AER error")
                        .message(format!(
                            "PCIe device {device_label} reported an uncorrectable AER error. \
                             Raw: \"{t}\". This often forces a reset of the device or the host."
                        ))
                        .raw_kv("kmsg", serde_json::json!(t));
                        if let Decision::Emit(a) = tracker.observe(b) {
                            sink.send(Outbound::Alert(a));
                        }
                    } else if is_corr {
                        let now = Instant::now();
                        corr_window.push_back(now);
                        let cutoff = now - Duration::from_secs(60);
                        while let Some(&t0) = corr_window.front() {
                            if t0 < cutoff { corr_window.pop_front(); } else { break; }
                        }
                        if corr_window.len() as u64 >= cfg.corrected_per_min_warn {
                            let b = AlertBuilder::new(
                                host.host(),
                                host.host_id(),
                                cat::PCIE_AER,
                                "corrected_rate_high",
                                Severity::Warning,
                            )
                            .device(bdf.unwrap_or_default())
                            .device_model(&device_label)
                            .value(format!("{}/min", corr_window.len()))
                            .threshold(format!("{}/min", cfg.corrected_per_min_warn))
                            .title("PCIe corrected AER rate high")
                            .message(format!(
                                "Sustained PCIe corrected errors on {device_label} \
                                 (~{}/min). Worth replacing the riser or device before \
                                 it escalates to uncorrectable.",
                                corr_window.len()
                            ));
                            if let Decision::Emit(a) = tracker.observe(b) {
                                sink.send(Outbound::Alert(a));
                            }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("pcie_aer: kmsg lag {n}"),
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

async fn describe_pci(bdf: &str) -> Option<String> {
    let o = run("lspci", &["-s", bdf, "-vmm"], TIMEOUT_GENERAL).await
        .ok()?;
    if !o.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&o.stdout);
    let mut vendor = String::new();
    let mut device = String::new();
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("Vendor:") { vendor = v.trim().into(); }
        if let Some(v) = line.strip_prefix("Device:") { device = v.trim().into(); }
    }
    if vendor.is_empty() && device.is_empty() {
        None
    } else {
        Some(format!("{vendor} {device}").trim().to_string())
    }
}
