//! Host identity. Cached at startup; cheap to clone.

use anyhow::{Context, Result};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct HostId {
    pub hostname: Arc<str>,
    pub machine_id: Arc<str>,
    pub primary_ip: Arc<str>,
}

impl HostId {
    pub fn detect() -> Result<Self> {
        let hostname = hostname::get()
            .context("reading hostname")?
            .to_string_lossy()
            .into_owned();

        // /etc/machine-id is the canonical identifier on systemd hosts.
        let machine_id = std::fs::read_to_string("/etc/machine-id")
            .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                use std::hash::{Hash, Hasher};
                hostname.hash(&mut h);
                format!("hn-{:016x}", h.finish())
            });

        let primary_ip = detect_primary_ip().unwrap_or_default();

        Ok(Self {
            hostname: Arc::from(hostname.as_str()),
            machine_id: Arc::from(machine_id.as_str()),
            primary_ip: Arc::from(primary_ip.as_str()),
        })
    }

    pub fn host(&self) -> &str { &self.hostname }
    pub fn host_id(&self) -> &str { &self.machine_id }
    pub fn primary_ip(&self) -> &str { &self.primary_ip }
}

/// Best-effort primary IPv4 detection. Tries two methods in order:
///   1. `ip -4 -j addr show` — parses JSON, skips virtual interfaces
///   2. `hostname -I` — simpler fallback available on all systemd distros
/// Returns empty string if both fail (agent still starts normally).
fn detect_primary_ip() -> Option<String> {
    detect_ip_via_ip_cmd().or_else(detect_ip_via_hostname)
}

fn detect_ip_via_ip_cmd() -> Option<String> {
    use std::process::Command;
    let out = Command::new("ip")
        .args(["-4", "-j", "addr", "show"])
        .output()
        .ok()?;
    if !out.status.success() { return None; }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let arr = v.as_array()?;
    let skip = ["lo", "docker", "cni", "veth", "br-", "tun", "tap", "virbr", "uma-test"];
    for iface in arr {
        let name = iface.get("ifname").and_then(|n| n.as_str()).unwrap_or("");
        if skip.iter().any(|p| name.starts_with(p)) { continue; }
        let oper = iface.get("operstate").and_then(|n| n.as_str()).unwrap_or("");
        if oper != "UP" && oper != "UNKNOWN" { continue; }
        if let Some(addrs) = iface.get("addr_info").and_then(|a| a.as_array()) {
            for a in addrs {
                if a.get("family").and_then(|f| f.as_str()) == Some("inet") {
                    if let Some(ip) = a.get("local").and_then(|l| l.as_str()) {
                        return Some(ip.to_string());
                    }
                }
            }
        }
    }
    None
}

fn detect_ip_via_hostname() -> Option<String> {
    use std::process::Command;
    // `hostname -I` prints space-separated IPs; pick first non-loopback IPv4.
    let out = Command::new("hostname").arg("-I").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.split_whitespace()
        .find(|ip| {
            !ip.starts_with("127.")
                && !ip.starts_with("169.254.")
                && !ip.contains(':') // skip IPv6
        })
        .map(|s| s.to_string())
}
