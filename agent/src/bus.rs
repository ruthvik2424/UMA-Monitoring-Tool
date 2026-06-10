//! In-process broadcast bus + alert sink.
//!
//! Modules push `Outbound` messages here. The transport task drains and ships
//! them on the WebSocket. Critical-severity alerts go on a dedicated MPSC
//! channel so a flooded normal queue can't delay a fatal-event alert.

use tokio::sync::{broadcast, mpsc};
use uma_shared::{Alert, Envelope, Heartbeat, Hello, Severity};

#[derive(Debug, Clone)]
pub enum Outbound {
    Alert(Alert),
    Heartbeat(Heartbeat),
    Hello(Hello),
}

impl Outbound {
    pub fn into_envelope(self) -> Envelope {
        match self {
            Outbound::Alert(a) => Envelope::Alert(a),
            Outbound::Heartbeat(h) => Envelope::Heartbeat(h),
            Outbound::Hello(h) => Envelope::Hello(h),
        }
    }
}

/// Shared sender given to every module. Cheap to clone.
#[derive(Debug, Clone)]
pub struct AlertSink {
    normal: mpsc::Sender<Outbound>,
    critical: mpsc::Sender<Outbound>,
    // For modules that want to react to other modules' alerts (e.g.
    // storage_io correlating ceph_osd_down with controller_lockup).
    fanout: broadcast::Sender<Alert>,
    /// When true, `Outbound::Alert` is dropped (maintenance mode).
    suppress_alerts: bool,
}

impl AlertSink {
    pub fn new(
        normal: mpsc::Sender<Outbound>,
        critical: mpsc::Sender<Outbound>,
        fanout: broadcast::Sender<Alert>,
        suppress_alerts: bool,
    ) -> Self {
        Self {
            normal,
            critical,
            fanout,
            suppress_alerts,
        }
    }

    /// Non-blocking send. Critical alerts use the dedicated lane.
    /// On overflow we drop the oldest from the local queue rather than the
    /// new alert — fresh signal beats stale signal.
    pub fn send(&self, msg: Outbound) {
        if self.suppress_alerts {
            if let Outbound::Alert(ref a) = msg {
                tracing::debug!(
                    category = %a.category,
                    metric = %a.metric,
                    "maintenance: alert suppressed (not sent to collector)"
                );
                return;
            }
        }
        // Log every outbound alert to the execution log so operators can see
        // what the agent fired and when, without opening the collector GUI.
        if let Outbound::Alert(a) = &msg {
            tracing::info!(
                severity  = %a.severity.as_str(),
                category  = %a.category,
                metric    = %a.metric,
                state     = ?a.state,
                host      = %a.host,
                device    = ?a.device,
                title     = %a.title,
                "ALERT FIRED"
            );
            let _ = self.fanout.send(a.clone());
        }
        let lane = match &msg {
            Outbound::Alert(a) if a.severity == Severity::Critical => &self.critical,
            _ => &self.normal,
        };
        if let Err(e) = lane.try_send(msg) {
            // Try to make room by discarding one stale entry, then retry.
            tracing::warn!(error = %e, "alert sink full — dropping one to make room");
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Alert> {
        self.fanout.subscribe()
    }
}
