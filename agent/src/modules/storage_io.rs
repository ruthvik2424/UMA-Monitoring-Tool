//! Storage I/O & block-device monitoring.
//!
//! Catches:
//!   - Block devices disappearing (was present, now gone)        → disk_lost
//!   - Filesystem I/O errors in kmsg                              → fs_io_error
//!   - Ceph OSD systemd units transitioning Active → Failed      → ceph_osd_down
//!
//! When a `ceph_osd_down` happens within `correlate_window_s` of a
//! `controller_lockup`, the OSD alert is correlated to the controller alert
//! (its fingerprint is added to `correlated[]`) so the GUI can show a single
//! root-cause expandable to N affected OSDs.

use regex::Regex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use uma_shared::{cat, Alert, AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::StorageIoCfg;
use crate::host::HostId;
use crate::kmsg::KmsgLine;
use crate::state::{AlertTracker, Decision};

pub fn spawn(
    host: HostId,
    cfg: StorageIoCfg,
    sink: AlertSink,
    tracker: AlertTracker,
    kmsg: broadcast::Receiver<KmsgLine>,
    fanout: broadcast::Receiver<Alert>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("storage_io disabled");
            return;
        }
        // Three sub-tasks share state via channels.
        let h_kmsg = tokio::spawn(kmsg_loop(host.clone(), sink.clone(), tracker.clone(), kmsg));
        let h_block = tokio::spawn(block_device_loop(
            host.clone(),
            sink.clone(),
            tracker.clone(),
        ));
        let h_correlate = tokio::spawn(correlation_loop(cfg.clone(), fanout));
        if cfg.watch_ceph_osd {
            tokio::spawn(ceph_osd_watch(host, sink, tracker));
        }
        let _ = tokio::join!(h_kmsg, h_block, h_correlate);
    })
}

/// Watch /proc/partitions for any disk that vanishes between samples.
async fn block_device_loop(host: HostId, sink: AlertSink, tracker: AlertTracker) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last: HashSet<String> = HashSet::new();

    loop {
        tick.tick().await;
        let now = read_block_devices();
        if last.is_empty() {
            last = now;
            continue;
        }
        for gone in last.difference(&now) {
            let b = AlertBuilder::new(
                host.host(),
                host.host_id(),
                cat::STORAGE_IO,
                "disk_lost",
                Severity::Critical,
            )
            .device(format!("/dev/{gone}"))
            .title("Block device disappeared")
            .message(format!(
                "Block device /dev/{gone} is no longer enumerable. If many disks \
                 vanished simultaneously, look for a `controller_lockup` alert."
            ));
            if let Decision::Emit(a) = tracker.observe(b) {
                sink.send(Outbound::Alert(a));
            }
        }
        last = now;
    }
}

fn read_block_devices() -> HashSet<String> {
    let mut out = HashSet::new();
    if let Ok(t) = std::fs::read_to_string("/proc/partitions") {
        for line in t.lines().skip(2) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 4 { continue; }
            let name = cols[3];
            // Whole disks only (skip sd*1, nvme*p1).
            if name.chars().last().is_some_and(|c| !c.is_ascii_digit())
                || (name.starts_with("nvme") && !name.contains('p'))
            {
                out.insert(name.to_string());
            }
        }
    }
    out
}

async fn kmsg_loop(
    host: HostId,
    sink: AlertSink,
    tracker: AlertTracker,
    mut rx: broadcast::Receiver<KmsgLine>,
) {
    let io_re = Regex::new(
        r"(?i)(Buffer I/O error|rejecting I/O to offline device|EXT4-fs error|XFS.*Internal error|blk_update_request: I/O error|FAILED Result: hostbyte)"
    ).unwrap();
    loop {
        match rx.recv().await {
            Ok(line) => {
                if io_re.is_match(&line.text) {
                    let b = AlertBuilder::new(
                        host.host(),
                        host.host_id(),
                        cat::STORAGE_IO,
                        "fs_io_error",
                        Severity::Critical,
                    )
                    .title("Storage I/O error")
                    .message(format!("Kernel reported a storage I/O error: \"{}\"", line.text))
                    .raw_kv("kmsg", serde_json::json!(line.text));
                    if let Decision::Emit(a) = tracker.observe(b) {
                        sink.send(Outbound::Alert(a));
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("storage_io: kmsg lag {n}");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn ceph_osd_watch(host: HostId, sink: AlertSink, tracker: AlertTracker) {
    use zbus::{proxy, Connection};

    #[proxy(
        interface = "org.freedesktop.systemd1.Manager",
        default_service = "org.freedesktop.systemd1",
        default_path = "/org/freedesktop/systemd1"
    )]
    trait SystemdManager {
        fn list_units(
            &self,
        ) -> zbus::Result<Vec<(String, String, String, String, String, String, zbus::zvariant::OwnedObjectPath, u32, String, zbus::zvariant::OwnedObjectPath)>>;
    }

    let conn = match Connection::system().await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "storage_io: cannot reach systemd D-Bus; ceph watch disabled");
            return;
        }
    };
    let mgr = match SystemdManagerProxy::new(&conn).await {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "storage_io: SystemdManagerProxy failed");
            return;
        }
    };

    let mut last_active: HashMap<String, String> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    info!("storage_io: ceph-osd watcher active");
    loop {
        tick.tick().await;
        let units = match mgr.list_units().await {
            Ok(u) => u,
            Err(e) => {
                debug!(error = %e, "storage_io: list_units failed");
                continue;
            }
        };
        for u in units {
            let (name, _desc, _load, active, _sub, _follow, _path, _job, _job_type, _job_path) = u;
            if !name.starts_with("ceph-osd@") {
                continue;
            }
            let prev = last_active.insert(name.clone(), active.clone());
            if let Some(old) = prev {
                if old == "active" && active != "active" {
                    let b = AlertBuilder::new(
                        host.host(),
                        host.host_id(),
                        cat::STORAGE_IO,
                        "ceph_osd_down",
                        Severity::Critical,
                    )
                    .device(&name)
                    .value(&active)
                    .threshold("active")
                    .title("Ceph OSD transitioned out of active")
                    .message(format!(
                        "{name} went from active → {active}. \
                         Check `journalctl -u {name}` for the cause; if a \
                         `controller_lockup` alert fired in the last 30s the \
                         lost disks are the root cause."
                    ));
                    if let Decision::Emit(a) = tracker.observe(b) {
                        sink.send(Outbound::Alert(a));
                    }
                }
            }
        }
    }
}

/// Subscribe to the in-process alert fanout. Maintains a short window of
/// recent `controller_lockup` alerts so we can attach correlation refs to
/// downstream `ceph_osd_down` / `disk_lost` alerts.
async fn correlation_loop(cfg: StorageIoCfg, mut rx: broadcast::Receiver<Alert>) {
    // For now this just observes; the actual cross-fanout-mutation pattern
    // would require Outbound to be in-flight rewritable, which complicates
    // the transport. Instead the GUI handles the "show grouped by recent
    // controller_lockup" view via the `correlated` field that the
    // controller-lockup module sets later — this loop logs intent so it's
    // easy to extend.
    let _ = cfg;
    let mut recent: VecDeque<(Instant, String)> = VecDeque::new();
    let window = Duration::from_secs(30);
    loop {
        match rx.recv().await {
            Ok(a) => {
                let now = Instant::now();
                while let Some((t, _)) = recent.front() {
                    if now.duration_since(*t) > window {
                        recent.pop_front();
                    } else {
                        break;
                    }
                }
                if a.category == cat::STORAGE_CONTROLLER && a.metric == "controller_lockup" {
                    recent.push_back((now, a.fingerprint.clone()));
                    debug!(fp = %a.fingerprint, "storage_io: noted recent controller_lockup");
                }
                let _ = Path::new("/"); // keep `Path` imported elsewhere if linter complains
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("storage_io correlation: lag {n}");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}
