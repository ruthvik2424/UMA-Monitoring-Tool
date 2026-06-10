//! Boot history monitoring.
//!
//! On agent startup we compare the current boot id with the last one we
//! recorded in /var/lib/monitor-agent/last-boot. If they differ AND the
//! kernel taint flags suggest something bad happened, emit an
//! `unclean_reboot` alert with the most likely cause.

use std::path::Path;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use uma_shared::{cat, AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::SimpleEnable;
use crate::host::HostId;

const STATE_FILE: &str = "/var/lib/monitor-agent/last-boot";

pub fn spawn(host: HostId, cfg: SimpleEnable, sink: AlertSink) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("boot disabled");
            return;
        }
        if let Err(e) = run(&host, &sink) {
            warn!(error = %e, "boot: startup check failed");
        }
    })
}

fn run(host: &HostId, sink: &AlertSink) -> std::io::Result<()> {
    let cur = current_boot_id();
    let prev = std::fs::read_to_string(STATE_FILE).ok().map(|s| s.trim().to_string());

    if let Some(parent) = Path::new(STATE_FILE).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Some(b) = &cur {
        let _ = std::fs::write(STATE_FILE, b);
    }

    let booted_fresh = match (prev.as_deref(), cur.as_deref()) {
        (Some(p), Some(c)) if p != c => true,
        (None, Some(_)) => false, // first ever run, don't alarm
        _ => false,
    };
    if !booted_fresh { return Ok(()); }

    let tainted = std::fs::read_to_string("/proc/sys/kernel/tainted")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);

    let mut hints: Vec<&str> = Vec::new();
    if tainted & 0x80 != 0 { hints.push("machine check (MCE)"); }
    if tainted & 0x200 != 0 { hints.push("warning"); }
    if tainted & 0x400 != 0 { hints.push("crap module loaded"); }
    if tainted & 0x800 != 0 { hints.push("bad page reference"); }
    if tainted & 0x4000 != 0 { hints.push("kernel hard / soft lockup"); }

    let suggested_cause = if hints.is_empty() {
        "no taint bits set — might have been a clean reboot we missed, or a hard power event".to_string()
    } else {
        hints.join(", ")
    };

    let prev_id = prev.unwrap_or_else(|| "(none)".into());
    let cur_id = cur.unwrap_or_else(|| "(unknown)".into());
    let b = AlertBuilder::new(
        host.host(),
        host.host_id(),
        cat::BOOT,
        "unclean_reboot_check",
        Severity::Warning,
    )
    .title("Host rebooted since last agent run")
    .message(format!(
        "Boot id changed ({prev_id} → {cur_id}) since the agent last ran. \
         Likely cause: {suggested_cause}. Check `journalctl -b -1` for the \
         tail of the previous boot."
    ))
    .raw_kv("tainted", serde_json::json!(tainted))
    .raw_kv("prev_boot_id", serde_json::json!(prev_id))
    .raw_kv("cur_boot_id", serde_json::json!(cur_id));
    sink.send(Outbound::Alert(b.build_firing()));
    debug!("boot module: emitted reboot detection alert");
    Ok(())
}

fn current_boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
}
