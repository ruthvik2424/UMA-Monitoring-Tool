//! CPU Machine Check Exception (MCE) detection.
//!
//! Source: kmsg lines starting with "mce:" or "[Hardware Error]".
//! Lightly decoded — bank, status, address — to give the operator something
//! actionable without the full mcelog dependency.

use regex::Regex;
use std::sync::LazyLock;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use uma_shared::{cat, AlertBuilder, Severity};

use crate::bus::{AlertSink, Outbound};
use crate::config::SimpleEnable;
use crate::host::HostId;
use crate::kmsg::KmsgLine;
use crate::state::{AlertTracker, Decision};

pub fn spawn(
    host: HostId,
    cfg: SimpleEnable,
    sink: AlertSink,
    tracker: AlertTracker,
    mut rx: broadcast::Receiver<KmsgLine>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("cpu_mce disabled");
            return;
        }
        let mce_re = Regex::new(r"(?i)\b(mce|machine check|hardware error)\b").unwrap();
        let bank_re = Regex::new(r"(?i)bank[: ]+(\d+)").unwrap();
        let cpu_re = Regex::new(r"(?i)CPU[: ]?(\d+)").unwrap();
        let status_re = Regex::new(r"(?i)STATUS[: ]+([0-9A-Fa-fx]+)").unwrap();
        loop {
            match rx.recv().await {
                Ok(line) => {
                    let t = &line.text;
                    if !mce_re.is_match(t) {
                        continue;
                    }
                    let bank = bank_re.captures(t).map(|c| c[1].to_string());
                    let cpu = cpu_re.captures(t).map(|c| c[1].to_string());
                    let status = status_re.captures(t).map(|c| c[1].to_string());
                    let reason = decode(t);
                    let (severity, title, metric) = mce_alert_severity_and_copy(t);
                    let advice = advice_for(metric);
                    let b = AlertBuilder::new(host.host(), host.host_id(), cat::CPU_MCE, metric, severity)
                    .device(format!(
                        "cpu={}, bank={}",
                        cpu.as_deref().unwrap_or("?"),
                        bank.as_deref().unwrap_or("?")
                    ))
                    .value(status.clone().unwrap_or_default())
                    .title(title)
                    .message(format!(
                        "CPU {} hardware check on bank {} (decoded hint: {}). \
                         Raw: \"{}\". {advice}",
                        cpu.as_deref().unwrap_or("?"),
                        bank.as_deref().unwrap_or("?"),
                        reason,
                        t,
                    ))
                    .raw_kv("kmsg", serde_json::json!(t))
                    .raw_kv("status", serde_json::json!(status));
                    if let Decision::Emit(a) = tracker.observe(b) {
                        sink.send(Outbound::Alert(a));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("cpu_mce: kmsg lag {n}"),
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

static CE_PIPE_MARKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\|CE\|").unwrap());
/// Same semantics as memory_ecc: **`|UE|`/`|UC|`-style uncorrect markers or keywords**.
static EXPLICIT_UNCORR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\buncorrect(ed|able)\b|\|UCE\||\|UC(?=[\|\]])|\bseverity:\s*fatal\b|\bmca:\s*fatal\b")
        .unwrap()
});
static MEMORY_GEO_HINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(rank|bank|row|column|slice|dram)\b|\bnode:\s*\d+|\bcard:\s*\d+\s+.*\bmodule:\s*\d+")
        .unwrap()
});

/// Classify MCA / `[Hardware Error]` lines so **`|CE|` CMCI summaries are not Sev1 pages**.
fn mce_alert_severity_and_copy(text: &str) -> (Severity, &'static str, &'static str) {
    let tl = text.to_lowercase();
    let corr_only_ce = CE_PIPE_MARKER.is_match(text) && !EXPLICIT_UNCORR.is_match(text);
    if corr_only_ce {
        return (
            Severity::Warning,
            "Corrected machine check / CMCI",
            "corrected_cmci_kmsg",
        );
    }

    let apei_generic = tl.contains("apei generic hardware error source");

    let fragment_without_status = MEMORY_GEO_HINT.is_match(text)
        && tl.contains("[hardware error]")
        && !EXPLICIT_UNCORR.is_match(text)
        && !tl.contains("mce:")
        ;

    let vague_summary = (apei_generic || fragment_without_status) && !EXPLICIT_UNCORR.is_match(text);

    if vague_summary {
        (
            Severity::Warning,
            "APEI / hardware RAS summary logged",
            "hardware_error_summary",
        )
    } else {
        (Severity::Critical, "CPU Machine Check Exception", "mce_event")
    }
}

fn advice_for(metric: &str) -> &'static str {
    match metric {
        "corrected_cmci_kmsg" => {
            "`|CE|` in the MCA status means the hardware reported a corrected error — do not panic-page as UE; \
             watch for escalating uncorrected MCA lines or DIMM swaps."
        }
        "hardware_error_summary" => {
            "APEI generic summaries omit per-bank MCA detail; correlate with SEL/IML and nearby MCA printks for UE."
        }
        _ => "If this is uncorrected, replace the CPU/socket if it recurs; single bursts can be transient.",
    }
}

/// Tiny mcelog-style decoder. We only handle the common cases — anything
/// unknown gets a fall-through label rather than an empty alert.
fn decode(text: &str) -> &'static str {
    let t = text.to_lowercase();
    if t.contains("dram") || t.contains("memory controller") {
        "DRAM ECC / memory controller error"
    } else if t.contains("l2") {
        "L2 cache error"
    } else if t.contains("l3") {
        "L3 cache / LLC error"
    } else if t.contains("bus") {
        "Bus / interconnect error"
    } else if t.contains("tlb") {
        "TLB error"
    } else if t.contains("uncorrected") {
        "Uncorrected error"
    } else if t.contains("corrected") {
        "Corrected error"
    } else {
        "see raw"
    }
}
