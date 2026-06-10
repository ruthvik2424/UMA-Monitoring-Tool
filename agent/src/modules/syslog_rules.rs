//! Configurable regex rules over the systemd journal stream.
//!
//! Reads from journald (which is a SUPERSET of /dev/kmsg — kernel printk
//! lines also appear in journald). This lets rules match BOTH kernel
//! events ("hpsa: controller lockup detected") AND userspace daemon
//! events ("systemd: Watchdog timeout for foo.service") — the latter
//! never appearing in /dev/kmsg.
//!
//! Compile each rule's regex once at startup; reject the whole config
//! if any regex fails (config error, not silent runtime breakage).

use anyhow::{Context, Result};
use regex::{Regex, RegexSet};
use std::sync::LazyLock;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use uma_shared::{AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::SyslogRulesCfg;
use crate::host::HostId;
use crate::journal::JournalLine;
use crate::state::{AlertTracker, Decision};

struct Compiled {
    name: String,
    re: Regex,
    severity: Severity,
    category: String,
    title: String,
    message: String,
}

/// Kernel / driver link lines surfaced through journalctl (`iface: Link DOWN`, etc.).
/// Must stay aligned with `modules.network_nic.ignore_regex`: same taps/veths are suppressed.
static JOURNAL_LINK_IFACE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)([\w.@+:~/-]{2,80}):\s+(?:Link\s+(?:DOWN|UP)|lost\s+carrier|NO-CARRIER)\b")
        .expect("JOURNAL_LINK_IFACE regex")
});

fn iface_kernel_link_journal(text: &str) -> Option<String> {
    JOURNAL_LINK_IFACE.captures(text).map(|c| c.get(1).unwrap().as_str().to_owned())
}

pub fn spawn(
    host: HostId,
    cfg: SyslogRulesCfg,
    nic_ignore: RegexSet,
    sink: AlertSink,
    tracker: AlertTracker,
    mut rx: broadcast::Receiver<JournalLine>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("syslog_rules disabled");
            return;
        }
        let rules = match compile_rules(&cfg) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "syslog_rules: compile failed; module disabled");
                return;
            }
        };
        info!("syslog_rules: {} rules loaded", rules.len());

        loop {
            match rx.recv().await {
                Ok(line) => {
                    for r in &rules {
                        if !r.re.is_match(&line.text) {
                            continue;
                        }
                        if r.category.eq_ignore_ascii_case("network_nic") {
                            if let Some(ref iface) = iface_kernel_link_journal(&line.text) {
                                if nic_ignore.is_match(iface) {
                                    continue;
                                }
                            }
                        }
                        let msg = r.message.replace("{match}", &line.text);
                        let b = AlertBuilder::new(
                            host.host(),
                            host.host_id(),
                            r.category.as_str(),
                            r.name.as_str(),
                            r.severity,
                        )
                        .title(r.title.as_str())
                        .message(msg)
                        .raw_kv("journal", serde_json::json!(line.text));
                        if let Decision::Emit(a) = tracker.observe(b) {
                            sink.send(Outbound::Alert(a));
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("syslog_rules: lag {n}"),
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

fn compile_rules(cfg: &SyslogRulesCfg) -> Result<Vec<Compiled>> {
    let mut out = Vec::with_capacity(cfg.rules.len());
    for r in &cfg.rules {
        let re = Regex::new(&r.regex)
            .with_context(|| format!("compiling rule '{}'", r.name))?;
        let severity = match r.severity.to_lowercase().as_str() {
            "critical" => Severity::Critical,
            "warning" => Severity::Warning,
            "info" | _ => Severity::Info,
        };
        out.push(Compiled {
            name: r.name.clone(),
            re,
            severity,
            category: r.category.clone(),
            title: r.title.clone(),
            message: r.message.clone(),
        });
    }
    Ok(out)
}
