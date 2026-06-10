//! Per-module dedup state machine.
//!
//! Each module owns one `AlertTracker`. It maps `fingerprint -> AlertState`
//! and decides whether a new sample should be emitted as a fresh alert,
//! suppressed (counter++), re-notified, or transitioned to resolved.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uma_shared::{Alert, AlertBuilder, AlertState, Severity};

#[derive(Debug, Clone)]
struct Entry {
    last_emitted: Instant,
    last_seen: Instant,
    occurrences: u64,
    severity: Severity,
    template: AlertSnapshot, // metadata for resolved-emission
    ok_since: Option<Instant>,
}

/// Minimal copy of the alert needed to emit a `resolved` later
/// without keeping the full Alert (which has a unique ULID per emission).
#[derive(Debug, Clone)]
struct AlertSnapshot {
    host: String,
    host_id: String,
    category: String,
    subsystem: Option<String>,
    metric: String,
    device: Option<String>,
    device_model: Option<String>,
    title: String,
}

#[derive(Debug, Clone)]
pub struct AlertTracker {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
    re_notify: Duration,
    recovery: Duration,
}

#[derive(Debug)]
pub enum Decision {
    /// First time we've seen this fingerprint or re_notify_after elapsed: emit.
    Emit(Alert),
    /// Already firing, not yet time to re-notify: suppressed.
    Suppress,
}

impl AlertTracker {
    pub fn new(re_notify: Duration, recovery: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            re_notify,
            recovery,
        }
    }

    /// Record a positive detection. Builds an Alert via the supplied
    /// builder. Returns `Emit(_)` only when the alert should be sent.
    pub fn observe(&self, b: AlertBuilder) -> Decision {
        let fp = b.fingerprint();
        let now = Instant::now();
        let mut g = self.inner.lock();
        match g.get_mut(&fp) {
            Some(e) => {
                e.last_seen = now;
                e.occurrences += 1;
                e.ok_since = None;
                if now.duration_since(e.last_emitted) >= self.re_notify {
                    let mut alert = b.build_firing();
                    alert.occurrences = e.occurrences;
                    e.last_emitted = now;
                    Decision::Emit(alert)
                } else {
                    Decision::Suppress
                }
            }
            None => {
                let alert = b.build_firing();
                let snap = AlertSnapshot {
                    host: alert.host.clone(),
                    host_id: alert.host_id.clone(),
                    category: alert.category.clone(),
                    subsystem: alert.subsystem.clone(),
                    metric: alert.metric.clone(),
                    device: alert.device.clone(),
                    device_model: alert.device_model.clone(),
                    title: alert.title.clone(),
                };
                g.insert(
                    fp,
                    Entry {
                        last_emitted: now,
                        last_seen: now,
                        occurrences: 1,
                        severity: alert.severity,
                        template: snap,
                        ok_since: None,
                    },
                );
                Decision::Emit(alert)
            }
        }
    }

    /// Record that this fingerprint is currently "OK" (metric back to normal).
    /// Returns a resolved alert once the metric has been OK for `recovery_window`.
    pub fn observe_ok(&self, fingerprint: &str) -> Option<Alert> {
        let now = Instant::now();
        let mut g = self.inner.lock();
        let e = g.get_mut(fingerprint)?;
        match e.ok_since {
            None => {
                e.ok_since = Some(now);
                None
            }
            Some(t0) if now.duration_since(t0) >= self.recovery => {
                let snap = e.template.clone();
                let occ = e.occurrences;
                let sev = e.severity;
                g.remove(fingerprint);
                let resolved = build_resolved(snap, sev, occ);
                Some(resolved)
            }
            _ => None,
        }
    }

    /// Force-resolve regardless of recovery window — used when the underlying
    /// resource disappears entirely (NIC removed, disk pulled, etc).
    #[allow(dead_code)] // wired in by storage_io when udev REMOVE arrives
    pub fn force_resolve(&self, fingerprint: &str) -> Option<Alert> {
        let mut g = self.inner.lock();
        let e = g.remove(fingerprint)?;
        Some(build_resolved(e.template, e.severity, e.occurrences))
    }
}

fn build_resolved(snap: AlertSnapshot, severity: Severity, occ: u64) -> Alert {
    let now = chrono::Utc::now();
    let fp = format!(
        "{}|{}|{}|{}",
        snap.host_id,
        snap.category,
        snap.device.as_deref().unwrap_or("-"),
        snap.metric,
    );
    Alert {
        id: ulid::Ulid::new().to_string(),
        ts: now,
        host: snap.host,
        host_id: snap.host_id,
        primary_ip: String::new(), // stamped by collector on ingest
        category: snap.category,
        subsystem: snap.subsystem,
        severity,
        state: AlertState::Resolved,
        device: snap.device,
        device_model: snap.device_model,
        metric: snap.metric,
        value: None,
        threshold: None,
        title: format!("Resolved: {}", snap.title),
        message: format!(
            "Condition recovered after {} occurrence(s).",
            occ
        ),
        fingerprint: fp,
        first_seen: now,
        last_seen: now,
        occurrences: occ,
        raw: Default::default(),
        correlated: Vec::new(),
    }
}
