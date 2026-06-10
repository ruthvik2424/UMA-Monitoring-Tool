//! NIC link-flap + counter monitoring.
//!
//! Two data sources:
//!   - rtnetlink RTM_NEWLINK events → instant up/down notification (no polling)
//!   - `ethtool -S <iface>` polled every cfg.counter_poll_s for CRC/FEC counters
//!
//! Flap detection: maintains a per-interface ring of recent transition
//! timestamps. ≥ flap_threshold transitions inside flap_window_s → flap alert
//! with the full timeline. Resolves automatically when the link is stable
//! for one full window.

use anyhow::Result;
use futures_util::stream::StreamExt;
use netlink_packet_route::link::LinkAttribute;
use netlink_sys::{AsyncSocket, SocketAddr};
use regex::RegexSet;
use rtnetlink::new_connection;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

use crate::priv_cmd::{run, TIMEOUT_NET};
use tracing::{debug, info, warn};
use uma_shared::{cat, AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::NetworkNicCfg;
use crate::host::HostId;
use crate::state::{AlertTracker, Decision};

#[derive(Debug, Default, Clone)]
struct CounterSnapshot {
    crc_errors: u64,
    fec_corrected: u64,
    fec_uncorrected: u64,
    rx_errors: u64,
    tx_errors: u64,
}

pub fn spawn(
    host: HostId,
    cfg: NetworkNicCfg,
    sink: AlertSink,
    tracker: AlertTracker,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("network_nic disabled");
            return;
        }
        let ignore = match RegexSet::new(&cfg.ignore_regex) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "network_nic: bad ignore regex; using empty set");
                RegexSet::empty()
            }
        };
        // Two parallel tasks: rtnetlink event loop + counter poller.
        let h1 = tokio::spawn(rtnetlink_loop(
            host.clone(),
            cfg.clone(),
            sink.clone(),
            tracker.clone(),
            ignore.clone(),
        ));
        let h2 = tokio::spawn(counter_loop(
            host.clone(),
            cfg.clone(),
            sink.clone(),
            tracker.clone(),
            ignore,
        ));
        let _ = tokio::join!(h1, h2);
    })
}

async fn rtnetlink_loop(
    host: HostId,
    cfg: NetworkNicCfg,
    sink: AlertSink,
    tracker: AlertTracker,
    ignore: RegexSet,
) {
    loop {
        if let Err(e) = run_rtnetlink(&host, &cfg, &sink, &tracker, &ignore).await {
            warn!(error = %e, "network_nic: rtnetlink loop crashed; restarting");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

async fn run_rtnetlink(
    host: &HostId,
    cfg: &NetworkNicCfg,
    sink: &AlertSink,
    tracker: &AlertTracker,
    ignore: &RegexSet,
) -> Result<()> {
    use netlink_packet_route::link::LinkMessage;
    use netlink_packet_route::RouteNetlinkMessage as Rtnl;

    let (mut conn, _handle, mut messages) = new_connection()?;
    // Subscribe to the LINK group only — we don't need address/route updates.
    let groups = nix::libc::RTMGRP_LINK as u32;
    let addr = SocketAddr::new(0, groups);
    conn.socket_mut().socket_mut().bind(&addr)?;
    tokio::spawn(conn);

    info!("network_nic: rtnetlink listening for link state changes");

    // ifindex -> sliding window of (ts, up:bool)
    let mut history: HashMap<u32, VecDeque<(Instant, bool)>> = HashMap::new();
    // ifindex -> name (cached from message attrs)
    let mut names: HashMap<u32, String> = HashMap::new();
    // ifindex -> last operstate string (so we only react to *transitions*)
    let mut last_state: HashMap<u32, String> = HashMap::new();

    while let Some((msg, _)) = messages.next().await {
        let payload = msg.payload;
        let nm: Rtnl = match payload {
            netlink_packet_core::NetlinkPayload::InnerMessage(m) => m,
            _ => continue,
        };
        let link: LinkMessage = match nm {
            Rtnl::NewLink(l) => l,
            _ => continue,
        };
        let ifindex = link.header.index;
        let mut name: Option<String> = None;
        let mut operstate: Option<String> = None;
        for attr in &link.attributes {
            match attr {
                LinkAttribute::IfName(n) => name = Some(n.clone()),
                LinkAttribute::OperState(s) => operstate = Some(format!("{s:?}").to_lowercase()),
                _ => {}
            }
        }
        if let Some(n) = name {
            names.insert(ifindex, n);
        }
        let iface = match names.get(&ifindex) {
            Some(n) => n.clone(),
            None => continue,
        };
        if ignore.is_match(&iface) {
            continue;
        }
        let state = match operstate {
            Some(s) => s,
            None => continue,
        };
        let prev = last_state.insert(ifindex, state.clone());
        let prev_state = prev.as_deref().map(|s| s.to_string());
        if prev_state.as_deref() == Some(state.as_str()) {
            continue; // not a transition
        }
        let up = state == "up";
        debug!(iface = %iface, prev = ?prev_state, new = %state, "rtnetlink link change");

        // Per-transition link_down alert (independent of flap detection).
        // FIRING when we go DOWN, RESOLVED (force) when we come back UP.
        let down_fp = format!("{}|{}|{}|link_down", host.host_id(), cat::NETWORK_NIC, iface);
        if !up {
            let b = AlertBuilder::new(
                host.host(),
                host.host_id(),
                cat::NETWORK_NIC,
                "link_down",
                Severity::Warning,
            )
            .device(&iface)
            .value("down")
            .threshold("up")
            .title(format!("Network interface '{iface}' went DOWN"))
            .message(format!(
                "Interface {iface} transitioned to operstate=down (was {}). \
                 Check cable, SFP, switch port, LACP partner, or driver state.",
                prev_state.as_deref().unwrap_or("(unknown)")
            ));
            if let Decision::Emit(a) = tracker.observe(b) {
                sink.send(Outbound::Alert(a));
            }
        } else if prev_state.is_some() {
            // Coming back up — immediately resolve any pending link_down alert.
            if let Some(resolved) = tracker.force_resolve(&down_fp) {
                sink.send(Outbound::Alert(resolved));
            }
        }

        // Sliding-window flap detection (escalates if rapid up/down churn).
        let win = history.entry(ifindex).or_default();
        let now = Instant::now();
        win.push_back((now, up));
        let cutoff = now - Duration::from_secs(cfg.flap_window_s);
        while let Some(&(t, _)) = win.front() {
            if t < cutoff {
                win.pop_front();
            } else {
                break;
            }
        }

        let transitions = win.len();
        if transitions as u32 >= cfg.flap_threshold {
            let timeline = win
                .iter()
                .map(|(t, u)| {
                    let dt = now.duration_since(*t);
                    format!("{}@T-{}.{:03}s", if *u { "up" } else { "down" }, dt.as_secs(), dt.subsec_millis())
                })
                .collect::<Vec<_>>()
                .join(", ");

            let b = AlertBuilder::new(
                host.host(),
                host.host_id(),
                cat::NETWORK_NIC,
                "link_flapping",
                Severity::Warning,
            )
            .device(&iface)
            .value(format!("{transitions} transitions"))
            .threshold(format!(
                "{} per {}s",
                cfg.flap_threshold, cfg.flap_window_s
            ))
            .title("Network interface flapping")
            .message(format!(
                "Interface {iface} flapped {transitions} times in the last \
                 {}s window. Recent transitions: {timeline}. Check cable, \
                 SFP, switch port, or LACP partner.",
                cfg.flap_window_s
            ))
            .raw_kv("transitions", serde_json::json!(transitions))
            .raw_kv("timeline", serde_json::json!(timeline));
            if let Decision::Emit(a) = tracker.observe(b) {
                sink.send(Outbound::Alert(a));
            }
        }
    }
    Ok(())
}

async fn counter_loop(
    host: HostId,
    cfg: NetworkNicCfg,
    sink: AlertSink,
    tracker: AlertTracker,
    ignore: RegexSet,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.counter_poll_s.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last: HashMap<String, CounterSnapshot> = HashMap::new();
    // Interfaces where ethtool -S returned EOPNOTSUPP (exit 94) or "no stats available".
    // These are virtual/tunnel NICs that will never support ethtool stats.
    // We log once and never retry — no point polluting the log every 5 seconds.
    let mut ethtool_unsupported: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        tick.tick().await;
        let ifaces = match list_interfaces() {
            Ok(i) => i,
            Err(e) => {
                debug!(error = %e, "network_nic: list_interfaces failed");
                continue;
            }
        };
        for iface in ifaces {
            if ignore.is_match(&iface) { continue; }
            if ethtool_unsupported.contains(&iface) { continue; }
            let snap = match read_counters(&iface).await {
                Ok(s) => s,
                Err(e) => {
                    // Detect permanent "no stats" (EOPNOTSUPP, exit 94).
                    // Mark the interface so we never retry — log once at INFO.
                    let msg = e.to_string();
                    if msg.contains("no stats") || msg.contains("94") || msg.contains("Operation not supported") {
                        info!(
                            iface = %iface,
                            "ethtool -S not supported on this interface (virtual/tunnel NIC) — skipping permanently"
                        );
                        ethtool_unsupported.insert(iface.clone());
                    }
                    continue;
                }
            };
            let prev = last.insert(iface.clone(), snap.clone()).unwrap_or_default();
            check_delta(&host, &sink, &tracker, &iface, &prev, &snap);
        }
    }
}

fn check_delta(
    host: &HostId,
    sink: &AlertSink,
    tracker: &AlertTracker,
    iface: &str,
    prev: &CounterSnapshot,
    cur: &CounterSnapshot,
) {
    let crc_d = cur.crc_errors.saturating_sub(prev.crc_errors);
    let fec_u_d = cur.fec_uncorrected.saturating_sub(prev.fec_uncorrected);
    if crc_d > 0 {
        let b = AlertBuilder::new(
            host.host(),
            host.host_id(),
            cat::NETWORK_NIC,
            "crc_errors",
            Severity::Warning,
        )
        .device(iface)
        .value(crc_d.to_string())
        .title("NIC reporting new CRC errors")
        .message(format!(
            "Interface {iface} accumulated {crc_d} new CRC errors since the \
             last poll. Likely cable, transceiver, or upstream port issue."
        ));
        emit(sink, tracker, b);
    }
    if fec_u_d > 0 {
        let b = AlertBuilder::new(
            host.host(),
            host.host_id(),
            cat::NETWORK_NIC,
            "fec_uncorrected",
            Severity::Critical,
        )
        .device(iface)
        .value(fec_u_d.to_string())
        .title("NIC FEC uncorrected errors")
        .message(format!(
            "Interface {iface} reported {fec_u_d} new uncorrectable FEC \
             errors. Link integrity is degraded; expect packet loss."
        ));
        emit(sink, tracker, b);
    }
}

fn emit(sink: &AlertSink, tracker: &AlertTracker, b: AlertBuilder) {
    if let Decision::Emit(a) = tracker.observe(b) {
        sink.send(Outbound::Alert(a));
    }
}

fn list_interfaces() -> Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/sys/class/net")? {
        out.push(entry?.file_name().to_string_lossy().into_owned());
    }
    Ok(out)
}

async fn read_counters(iface: &str) -> Result<CounterSnapshot> {
    // Cheap path: /proc/net/dev for rx/tx errors. CRC and FEC need ethtool -S.
    let mut snap = CounterSnapshot::default();

    if let Ok(t) = std::fs::read_to_string("/proc/net/dev") {
        for line in t.lines() {
            let line = line.trim_start();
            if let Some(rest) = line.strip_prefix(&format!("{iface}:")) {
                let cols: Vec<&str> = rest.split_whitespace().collect();
                if cols.len() >= 16 {
                    snap.rx_errors = cols[2].parse().unwrap_or(0);
                    snap.tx_errors = cols[10].parse().unwrap_or(0);
                }
            }
        }
    }

    let out = run("ethtool", &["-S", iface], TIMEOUT_NET).await;
    if let Ok(ref out) = out {
        // Exit 94 = EOPNOTSUPP — virtual/tunnel NIC, will never support stats.
        if out.status.code() == Some(94)
            || String::from_utf8_lossy(&out.stderr).contains("no stats available")
        {
            return Err(anyhow::anyhow!("ethtool: no stats (exit 94)"));
        }
    }
    if let Ok(out) = out {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let line = line.trim();
                if let Some((k, v)) = line.split_once(':') {
                    let k = k.trim();
                    let v: u64 = v.trim().parse().unwrap_or(0);
                    match k {
                        "rx_crc_errors" | "crc_errors" => snap.crc_errors = v,
                        "fec_corrected_blocks" | "rx_fec_corrected" => snap.fec_corrected = v,
                        "fec_uncorrectable_blocks" | "rx_fec_uncorrectable" => {
                            snap.fec_uncorrected = v
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(snap)
}
