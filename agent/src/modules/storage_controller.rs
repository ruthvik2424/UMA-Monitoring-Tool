//! HPE/Dell/LSI storage controller monitoring.
//!
//! Two detection paths run in parallel:
//!
//! 1. KMSG TAILER — instant signal for the "controller wedged" case the user
//!    keeps hitting on slot 3/6. Patterns:
//!      hpsa, smartpqi: "controller lockup detected", "Acknowledging event",
//!                      "scsi N: resetting host", "controller is offline"
//!
//! 2. PERIODIC POLL — vendor CLI enumeration. If a controller that was
//!    previously listed disappears (or its status flips to non-OK) we emit
//!    a `controller_lockup` critical alert with the slot number and a list
//!    of disks that were attached to it.
//!
//! Alerts are deduplicated with a stable fingerprint
//! (host_id|storage_controller|slot|metric) so repeats during the same
//! lockup don't generate noise.

use anyhow::Result;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::broadcast;

use crate::priv_cmd::{run, TIMEOUT_STORAGE};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use uma_shared::{cat, AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::StorageControllerCfg;
use crate::host::HostId;
use crate::kmsg::KmsgLine;
use crate::state::{AlertTracker, Decision};
use crate::vendor_detect::VendorProfile;

#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // slot is retained for the raw_kv payload; pci_bdf/attached_disks populated by future enrichers
struct ControllerView {
    slot: String,                  // "3" or "PCIe Slot 3"
    model: String,
    status: String,                // OK | Failed | Cache Module Failed | …
    pci_bdf: String,
    attached_disks: Vec<String>,   // /dev/sdX
}

pub fn spawn(
    host: HostId,
    cfg: StorageControllerCfg,
    sink: AlertSink,
    tracker: AlertTracker,
    vendor: VendorProfile,
    kmsg: broadcast::Receiver<KmsgLine>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("storage_controller disabled");
            return;
        }
        let h1 = tokio::spawn(kmsg_tailer(host.clone(), sink.clone(), tracker.clone(), kmsg));
        let h2 = tokio::spawn(poll_loop(host, cfg, sink, tracker, vendor));
        let _ = tokio::join!(h1, h2);
    })
}

async fn kmsg_tailer(
    host: HostId,
    sink: AlertSink,
    tracker: AlertTracker,
    mut rx: broadcast::Receiver<KmsgLine>,
) {
    // HPE SmartArray drivers: hpsa (older), smartpqi (Gen10+).
    // Real-world patterns observed in production (do NOT narrow without testing):
    //   hpsa:    "controller lockup detected"
    //   smartpqi: "controller offline: reason code 0x4 (no controller heartbeat detected)"
    //   smartpqi: "controller is offline"          <- some firmware variants
    //   Both:    "hard reset", "fatal error"
    let lockup_re = Regex::new(
        r"(?i)(hpsa|smartpqi).*(controller\s+(lockup\s+detected|(?:is\s+)?offline)|no\s+controller\s+heartbeat|hard\s+reset|fatal\s+error|reason\s+code\s+0x)"
    ).unwrap();
    let reset_re = Regex::new(
        r"(?i)(scsi|hpsa|smartpqi).*(resetting host|abort)\b"
    ).unwrap();

    loop {
        match rx.recv().await {
            Ok(line) => {
                if lockup_re.is_match(&line.text) {
                    let b = AlertBuilder::new(
                        host.host(),
                        host.host_id(),
                        cat::STORAGE_CONTROLLER,
                        "controller_lockup_kmsg",
                        Severity::Critical,
                    )
                    .title("Storage controller lockup (kernel)")
                    .message(format!(
                        "Kernel logged a SmartArray controller lockup: \"{}\". Disks behind this controller will be unreachable; Ceph OSDs on those disks will go down. Issue a hot-reset (`echo 1 > /sys/class/scsi_host/hostX/eh_deadline`) or schedule a power cycle.",
                        line.text
                    ))
                    .raw_kv("kmsg", serde_json::json!(line.text));
                    if let Decision::Emit(a) = tracker.observe(b) {
                        sink.send(Outbound::Alert(a));
                    }
                } else if reset_re.is_match(&line.text) {
                    debug!(line = %line.text, "storage_controller: scsi reset noted");
                    // We don't alert on every reset — only on lockup. The
                    // user can correlate via raw kmsg view in the GUI.
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("storage_controller: kmsg receiver lagged by {n}");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn poll_loop(
    host: HostId,
    cfg: StorageControllerCfg,
    sink: AlertSink,
    tracker: AlertTracker,
    vendor: VendorProfile,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.poll_s.max(5)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut last: HashMap<String, ControllerView> = HashMap::new();
    loop {
        tick.tick().await;
        let view = enumerate(&vendor).await;
        if view.is_empty() {
            continue;
        }
        let now: HashSet<String> = view.keys().cloned().collect();
        let prev: HashSet<String> = last.keys().cloned().collect();

        // Disappeared controller = LOCKUP.
        for slot in prev.difference(&now) {
            let prev_view = last.get(slot).cloned().unwrap_or_default();
            let disks = if prev_view.attached_disks.is_empty() {
                "unknown".to_string()
            } else {
                prev_view.attached_disks.join(", ")
            };
            let b = AlertBuilder::new(
                host.host(),
                host.host_id(),
                cat::STORAGE_CONTROLLER,
                "controller_lockup",
                Severity::Critical,
            )
            .device(format!("slot={slot}"))
            .device_model(&prev_view.model)
            .title("Storage controller no longer enumerable")
            .message(format!(
                "Controller in slot {slot} ({}) disappeared from enumeration. \
                 Disks lost: {disks}. This typically means the controller is \
                 wedged. Disks will return only after a controller reset or \
                 power cycle.",
                prev_view.model
            ))
            .raw_kv("slot", serde_json::json!(slot))
            .raw_kv("attached_disks", serde_json::json!(prev_view.attached_disks))
            .raw_kv("pci_bdf", serde_json::json!(prev_view.pci_bdf));
            if let Decision::Emit(a) = tracker.observe(b) {
                sink.send(Outbound::Alert(a));
            }
        }

        // Bad status on a present controller = also a lockup-class alert.
        for (slot, ctrl) in &view {
            let ok = ctrl.status.eq_ignore_ascii_case("ok") || ctrl.status.is_empty();
            if !ok {
                let b = AlertBuilder::new(
                    host.host(),
                    host.host_id(),
                    cat::STORAGE_CONTROLLER,
                    "controller_status",
                    Severity::Critical,
                )
                .device(format!("slot={slot}"))
                .device_model(&ctrl.model)
                .value(&ctrl.status)
                .threshold("OK")
                .title("Storage controller reporting unhealthy status")
                .message(format!(
                    "Controller in slot {slot} ({}) status = '{}'. Inspect \
                     `ssacli ctrl all show config` (HPE) or `perccli64 /c0 show all` \
                     (Dell) for details.",
                    ctrl.model, ctrl.status
                ));
                if let Decision::Emit(a) = tracker.observe(b) {
                    sink.send(Outbound::Alert(a));
                }
            }
        }

        last = view;
    }
}

async fn enumerate(vendor: &VendorProfile) -> HashMap<String, ControllerView> {
    let mut out = HashMap::new();
    if vendor.has_ssacli {
        if let Ok(map) = enumerate_hpe().await {
            out.extend(map);
        }
    }
    if vendor.has_perccli {
        if let Ok(map) = enumerate_perccli().await {
            out.extend(map);
        }
    }
    if vendor.has_storcli && out.is_empty() {
        if let Ok(map) = enumerate_storcli().await {
            out.extend(map);
        }
    }
    out
}

async fn enumerate_hpe() -> Result<HashMap<String, ControllerView>> {
    let mut out = HashMap::new();
    let o = run("ssacli", &["ctrl", "all", "show", "status"], TIMEOUT_STORAGE).await?;
    if !o.status.success() {
        return Ok(out);
    }
    let s = String::from_utf8_lossy(&o.stdout);
    // Output looks like:
    //   Smart Array P408i-a in Slot 0 (Embedded)
    //      Controller Status: OK
    //      Cache Status: OK
    let slot_re = Regex::new(r"in Slot ([\dA-Za-z\-]+)").unwrap();
    let model_re = Regex::new(r"^(.*?)\s+in Slot").unwrap();
    let mut cur_slot: Option<String> = None;
    let mut cur_model = String::new();
    let mut cur_status = String::new();
    for raw in s.lines() {
        let line = raw.trim();
        if line.is_empty() { continue; }
        if line.contains("in Slot") {
            if let Some(slot) = cur_slot.take() {
                out.insert(
                    slot.clone(),
                    ControllerView {
                        slot,
                        model: std::mem::take(&mut cur_model),
                        status: std::mem::take(&mut cur_status),
                        pci_bdf: String::new(),
                        attached_disks: Vec::new(),
                    },
                );
            }
            if let Some(c) = slot_re.captures(line) {
                cur_slot = Some(c[1].to_string());
            }
            if let Some(c) = model_re.captures(line) {
                cur_model = c[1].to_string();
            }
        } else if let Some(rest) = line.strip_prefix("Controller Status:") {
            cur_status = rest.trim().to_string();
        }
    }
    if let Some(slot) = cur_slot {
        out.insert(
            slot.clone(),
            ControllerView {
                slot,
                model: cur_model,
                status: cur_status,
                pci_bdf: String::new(),
                attached_disks: Vec::new(),
            },
        );
    }
    Ok(out)
}

async fn enumerate_perccli() -> Result<HashMap<String, ControllerView>> {
    let mut out = HashMap::new();
    let o = match run("perccli64", &["show", "ctrlcount"], TIMEOUT_STORAGE).await {
        Ok(o) => o,
        Err(_) => return Ok(out), // perccli64 not present or failed — no Dell controllers
    };
    if !o.status.success() {
        return Ok(out);
    }
    // For each /cN, fetch status.
    let count_re = Regex::new(r"Controller Count\s*=\s*(\d+)").unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    let count: usize = count_re
        .captures(&s)
        .and_then(|c| c[1].parse().ok())
        .unwrap_or(0);
    for i in 0..count {
        let cn = format!("/c{i}");
        let oo = run("perccli64", &[cn.as_str(), "show", "all"], TIMEOUT_STORAGE).await?;
        if !oo.status.success() { continue; }
        let body = String::from_utf8_lossy(&oo.stdout);
        let model = body
            .lines()
            .find_map(|l| l.strip_prefix("Product Name = "))
            .unwrap_or("PERC")
            .to_string();
        let status = body
            .lines()
            .find_map(|l| l.strip_prefix("Controller Status = "))
            .unwrap_or("Unknown")
            .to_string();
        out.insert(
            i.to_string(),
            ControllerView {
                slot: i.to_string(),
                model,
                status,
                pci_bdf: String::new(),
                attached_disks: Vec::new(),
            },
        );
    }
    Ok(out)
}

async fn enumerate_storcli() -> Result<HashMap<String, ControllerView>> {
    enumerate_perccli().await
}
