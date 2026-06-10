//! OS hang & lockup detection from kmsg.
//!
//! Critical patterns (push immediately so the alert is on the wire before
//! the kernel completes its panic path):
//!   - "INFO: task X:PID blocked for more than N seconds"  → task_hang (warn)
//!   - "watchdog: BUG: soft lockup - CPU#X stuck for"      → cpu_softlockup (crit)
//!   - "NMI watchdog: BUG: hard LOCKUP"                    → cpu_hardlockup (crit)
//!   - "BUG:" / "Oops:"  / "general protection fault"      → kernel_bug (crit)

use regex::Regex;
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
            info!("oshang disabled");
            return;
        }
        let task_re = Regex::new(r"INFO: task (\S+):(\d+) blocked for more than (\d+) seconds").unwrap();
        let soft_re = Regex::new(r"watchdog: BUG: soft lockup - CPU#(\d+) stuck for (\d+s)").unwrap();
        let hard_re = Regex::new(r"NMI watchdog: BUG: hard LOCKUP on CPU (\d+)").unwrap();
        let bug_re  = Regex::new(r"^(BUG:|Oops:|general protection fault|kernel BUG at)").unwrap();

        loop {
            match rx.recv().await {
                Ok(line) => {
                    let t = &line.text;
                    if let Some(c) = task_re.captures(t) {
                        let comm = &c[1];
                        let pid = &c[2];
                        let secs = &c[3];
                        let b = AlertBuilder::new(
                            host.host(),
                            host.host_id(),
                            cat::OS_HANG,
                            "task_hang",
                            Severity::Warning,
                        )
                        .device(format!("pid={pid}"))
                        .value(format!("{secs}s"))
                        .title("Hung task detected")
                        .message(format!(
                            "Task {comm} (pid {pid}) has been blocked for over {secs} seconds. \
                             Common causes: stuck I/O on a wedged storage controller, deadlock, \
                             paging on a swap-starved host."
                        ))
                        .raw_kv("kmsg", serde_json::json!(t));
                        emit(&sink, &tracker, b);
                    } else if let Some(c) = soft_re.captures(t) {
                        let cpu = &c[1];
                        let stuck = &c[2];
                        let b = AlertBuilder::new(
                            host.host(),
                            host.host_id(),
                            cat::OS_HANG,
                            "cpu_softlockup",
                            Severity::Critical,
                        )
                        .device(format!("cpu{cpu}"))
                        .value(stuck.to_string())
                        .title("CPU soft lockup")
                        .message(format!(
                            "watchdog reported a soft lockup on CPU {cpu} for {stuck}. The \
                             kernel may panic shortly. Investigate via `kdump` if enabled."
                        ))
                        .raw_kv("kmsg", serde_json::json!(t));
                        emit(&sink, &tracker, b);
                    } else if let Some(c) = hard_re.captures(t) {
                        let cpu = &c[1];
                        let b = AlertBuilder::new(
                            host.host(),
                            host.host_id(),
                            cat::OS_HANG,
                            "cpu_hardlockup",
                            Severity::Critical,
                        )
                        .device(format!("cpu{cpu}"))
                        .title("CPU hard lockup (NMI watchdog)")
                        .message(format!(
                            "NMI watchdog fired for CPU {cpu} — hard lockup. Host is almost \
                             certainly going to panic."
                        ))
                        .raw_kv("kmsg", serde_json::json!(t));
                        emit(&sink, &tracker, b);
                    } else if bug_re.is_match(t) {
                        let b = AlertBuilder::new(
                            host.host(),
                            host.host_id(),
                            cat::OS_HANG,
                            "kernel_bug",
                            Severity::Critical,
                        )
                        .title("Kernel BUG / oops")
                        .message(format!("Kernel logged: \"{}\". Capture vmcore via kdump.", t))
                        .raw_kv("kmsg", serde_json::json!(t));
                        emit(&sink, &tracker, b);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("oshang: kmsg lag {n}");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

fn emit(sink: &AlertSink, tracker: &AlertTracker, b: AlertBuilder) {
    if let Decision::Emit(a) = tracker.observe(b) {
        sink.send(Outbound::Alert(a));
    }
}
