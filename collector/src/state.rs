//! Collector in-memory state.
//!
//! Holds:
//!   - Per-host last_seen timestamp + tags + firing alert count (drives heatmap).
//!   - Currently-firing alerts keyed by fingerprint (drives the GUI's
//!     initial Snapshot on connect and dedup safety net).
//!   - Broadcast channel for live fanout to GUI subscribers.

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::broadcast;
use uma_shared::{Alert, AlertState, Envelope, HostState, HostStatus, Snapshot, VendorInfo};

#[derive(Clone)]
pub struct AppState {
    pub firing: Arc<RwLock<HashMap<String, Alert>>>, // fingerprint -> alert
    pub hosts: Arc<RwLock<HashMap<String, HostInfo>>>, // host_id -> HostInfo
    pub fanout: broadcast::Sender<Envelope>,
    pub offline_after_s: u64,
}

#[derive(Clone, Debug)]
pub struct HostInfo {
    pub host: String,
    pub host_id: String,
    pub primary_ip: String,
    pub last_seen: DateTime<Utc>,
    pub tags: BTreeMap<String, String>,
    pub vendor: VendorInfo,
    pub firing_count: u32,
    pub agent_version: String,
    pub maintenance: bool,
}

impl AppState {
    pub fn new(offline_after_s: u64) -> Self {
        let (fanout, _) = broadcast::channel::<Envelope>(8192);
        Self {
            firing: Arc::default(),
            hosts: Arc::default(),
            fanout,
            offline_after_s,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let alerts: Vec<Alert> = self.firing.read().values().cloned().collect();
        let hosts: Vec<HostState> = self.hosts.read().values().map(|h| HostState {
            host: h.host.clone(),
            host_id: h.host_id.clone(),
            primary_ip: h.primary_ip.clone(),
            online: (Utc::now() - h.last_seen).num_seconds() <= self.offline_after_s as i64,
            last_seen: h.last_seen,
            firing_count: h.firing_count,
            tags: h.tags.clone(),
            agent_version: h.agent_version.clone(),
            maintenance: h.maintenance,
        }).collect();
        Snapshot { generated_at: Utc::now(), alerts, hosts }
    }

    /// Apply an incoming alert and decide whether to broadcast it.
    /// Returns true if the alert was new or a state change (worth broadcasting).
    pub fn apply_alert(&self, a: &Alert) -> bool {
        let mut g = self.firing.write();
        match a.state {
            AlertState::Firing => {
                let was_new = !g.contains_key(&a.fingerprint);
                g.insert(a.fingerprint.clone(), a.clone());
                if was_new {
                    self.bump_host_count(&a.host_id, 1);
                }
                true
            }
            AlertState::Resolved => {
                if g.remove(&a.fingerprint).is_some() {
                    self.bump_host_count(&a.host_id, -1);
                    true
                } else {
                    false
                }
            }
        }
    }

    fn bump_host_count(&self, host_id: &str, delta: i32) {
        let mut g = self.hosts.write();
        if let Some(h) = g.get_mut(host_id) {
            let n = h.firing_count as i32 + delta;
            h.firing_count = n.max(0) as u32;
        }
    }

    pub fn touch_host(&self, host: &str, host_id: &str, ts: DateTime<Utc>, agent_version: &str) {
        let mut g = self.hosts.write();
        let entry = g.entry(host_id.to_string()).or_insert_with(|| HostInfo {
            host: host.to_string(),
            host_id: host_id.to_string(),
            primary_ip: String::new(),
            last_seen: ts,
            tags: BTreeMap::new(),
            vendor: VendorInfo::default(),
            firing_count: 0,
            agent_version: agent_version.to_string(),
            maintenance: false,
        });
        entry.last_seen = ts;
        entry.host = host.to_string();
        if !agent_version.is_empty() {
            entry.agent_version = agent_version.to_string();
        }
    }

    pub fn register_hello(&self, h: &uma_shared::Hello) {
        let mut g = self.hosts.write();
        let entry = g.entry(h.host_id.clone()).or_insert_with(|| HostInfo {
            host: h.host.clone(),
            host_id: h.host_id.clone(),
            primary_ip: h.primary_ip.clone(),
            last_seen: h.started_at,
            tags: h.tags.clone(),
            vendor: h.vendor.clone(),
            firing_count: 0,
            agent_version: h.agent_version.clone(),
            maintenance: h.maintenance,
        });
        entry.tags = h.tags.clone();
        entry.vendor = h.vendor.clone();
        entry.agent_version = h.agent_version.clone();
        entry.host = h.host.clone();
        entry.maintenance = h.maintenance;
        if !h.primary_ip.is_empty() {
            entry.primary_ip = h.primary_ip.clone();
        }
    }

    /// Return the last known primary IP for a host (empty string if unknown).
    pub fn host_primary_ip(&self, host_id: &str) -> String {
        self.hosts
            .read()
            .get(host_id)
            .map(|h| h.primary_ip.clone())
            .unwrap_or_default()
    }

    /// Last known maintenance flag from the agent Hello.
    pub fn host_in_maintenance(&self, host_id: &str) -> bool {
        self.hosts
            .read()
            .get(host_id)
            .map(|h| h.maintenance)
            .unwrap_or(false)
    }

    pub fn host_status(&self, host_id: &str) -> Option<HostStatus> {
        let g = self.hosts.read();
        let h = g.get(host_id)?;
        Some(HostStatus {
            host: h.host.clone(),
            host_id: h.host_id.clone(),
            online: (Utc::now() - h.last_seen).num_seconds() <= self.offline_after_s as i64,
            last_seen: h.last_seen,
            firing_count: h.firing_count,
        })
    }

    pub fn broadcast(&self, env: Envelope) {
        let _ = self.fanout.send(env);
    }
}
