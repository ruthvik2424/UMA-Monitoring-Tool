//! Pre-shutdown / pre-reboot alerting via systemd-logind D-Bus.
//!
//! Mechanism:
//!   1. Take a "delay"-type inhibitor lock on logind. Logind will pause shutdown
//!      for up to InhibitDelayMaxSec (default ~5s) once we hold this lock.
//!   2. Subscribe to the `PrepareForShutdown(bool start)` D-Bus signal.
//!   3. When the signal fires with `start=true`, push a `host_rebooting` /
//!      `host_shutting_down` alert on the open WSS, then release the lock so
//!      logind proceeds.
//!
//! A secondary **polling** path watches `/run/systemd/shutdown/scheduled` because some
//! sites see unreliable / late `PrepareForShutdown` deliveries under load —
//! duplicates are suppressed via `AlertTracker` (fingerprint excludes `raw` payload).

use anyhow::{anyhow, Context, Result};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use uma_shared::{cat, AlertBuilder, Severity};
use zbus::{proxy, zvariant::OwnedFd as ZOwnedFd, Connection};
use futures_util::stream::StreamExt;

use crate::bus::{AlertSink, Outbound};
use crate::config::SimpleEnable;
use crate::host::HostId;
use crate::state::{AlertTracker, Decision};

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait LoginManager {
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<ZOwnedFd>;

    #[zbus(signal)]
    fn prepare_for_shutdown(&self, start: bool) -> zbus::Result<()>;
}

pub fn spawn(
    host: HostId,
    cfg: SimpleEnable,
    sink: AlertSink,
    tracker: AlertTracker,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("lifecycle module disabled");
            return;
        }
        tokio::spawn(scheduled_file_poll(host.clone(), sink.clone(), tracker.clone()));

        loop {
            match logind_shutdown_loop(&host, &sink, &tracker).await {
                Ok(()) => {
                    info!("lifecycle: logind handshake complete — re-arming after brief pause");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(e) => {
                    warn!(error = %e, "lifecycle: D-Bus loop exited; retrying in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    })
}

/// Poll `shutdown/scheduled`; complements logind signals when dbus delivery is flaky.
async fn scheduled_file_poll(host: HostId, sink: AlertSink, tracker: AlertTracker) {
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut alerted_for_presence = false;

    loop {
        tick.tick().await;
        let path = Path::new("/run/systemd/shutdown/scheduled");
        if path.exists() {
            if !alerted_for_presence {
                alerted_for_presence = true;
                let (metric, title) = guess_kind_from_scheduled_path();
                let b = lifecycle_alert_builder(&host, metric, title, "shutdown.scheduled_file");
                if let Decision::Emit(a) = tracker.observe(b) {
                    sink.send(Outbound::Alert(a));
                }
            }
        } else {
            alerted_for_presence = false;
        }
    }
}

async fn logind_shutdown_loop(host: &HostId, sink: &AlertSink, tracker: &AlertTracker) -> Result<()> {
    let conn = Connection::system()
        .await
        .context("connecting to system D-Bus")?;
    let mgr = LoginManagerProxy::new(&conn).await.context("logind proxy")?;

    let _inhibit_lock: OwnedFd = mgr
        .inhibit(
            "shutdown",
            "monitor-agent",
            "uma: notify collector before host stops",
            "delay",
        )
        .await
        .context("taking shutdown delay inhibitor")?
        .into();
    info!("lifecycle: shutdown inhibitor held");

    let mut signals = mgr
        .receive_prepare_for_shutdown()
        .await
        .context("subscribing to PrepareForShutdown")?;

    while let Some(msg) = signals.next().await {
        let args = match msg.args() {
            Ok(a) => a,
            Err(e) => {
                warn!(error = %e, "lifecycle: bad PrepareForShutdown payload");
                continue;
            }
        };
        if *args.start() {
            let (metric, title) = guess_kind_from_scheduled_path();
            let b = lifecycle_alert_builder(host, metric, title, "logind.PrepareForShutdown");
            if let Decision::Emit(a) = tracker.observe(b) {
                sink.send(Outbound::Alert(a));
            }
            info!("lifecycle: shutdown notification sent via logind — releasing inhibitor");
            return Ok(());
        }
    }
    Err(anyhow!("PrepareForShutdown stream ended unexpectedly"))
}

fn lifecycle_alert_builder(
    host: &HostId,
    metric: &'static str,
    title: &'static str,
    source: &'static str,
) -> AlertBuilder {
    let init = match source {
        "logind.PrepareForShutdown" => "by systemd-logind",
        "shutdown.scheduled_file" => {
            "from /run/systemd/shutdown/scheduled (PrepareForShutdown may lag)"
        }
        _ => "by lifecycle monitor",
    };
    let motion = if metric == "host_rebooting" {
        "a controlled reboot sequence"
    } else {
        "a controlled shutdown sequence"
    };
    let msg = format!(
        "Host {} is entering {} ({init}). Agent will stop in ~5 s.",
        host.host(),
        motion,
        init = init,
    );
    AlertBuilder::new(host.host(), host.host_id(), cat::LIFECYCLE, metric, Severity::Warning)
        .title(title)
        .message(msg)
        .raw_kv("source", serde_json::json!(source))
}

/// Best-effort read of `/run/systemd/shutdown/scheduled` (+ cheap default.target hint).
fn guess_kind_from_scheduled_path() -> (&'static str, &'static str) {
    if let Ok(s) = std::fs::read_to_string("/run/systemd/shutdown/scheduled") {
        if s.contains("MODE=reboot") {
            return ("host_rebooting", "Host reboot initiated");
        }
        if s.contains("MODE=poweroff") {
            return ("host_shutting_down", "Host shutdown initiated");
        }
    }

    let target = std::fs::read_link("/run/systemd/system/default.target")
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    if target.contains("reboot") {
        ("host_rebooting", "Host reboot initiated")
    } else if target.contains("poweroff") || target.contains("halt") {
        ("host_shutting_down", "Host shutdown initiated")
    } else {
        ("host_rebooting", "Host reboot initiated")
    }
}
