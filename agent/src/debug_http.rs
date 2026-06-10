//! Local-only HTTP debug server — node-exporter-style browser interface.
//!
//! Routes:
//!   GET /                       → HTML dashboard (auto-refreshes, renders all data below)
//!   GET /v1/debug/snapshot      → Raw JSON (used by the UI and curl)
//!   GET /debug/snapshot         → Alias for the above

use crate::config::AgentConfig;
use crate::host::HostId;
use crate::priv_cmd::is_root;
use regex::RegexSet;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

const MAX_BODY_CHARS: usize = 48 * 1024;
const CMD_TIMEOUT: Duration = Duration::from_secs(12);

pub fn spawn(bind: String, cfg: Arc<AgentConfig>, host: Arc<HostId>) {
    tokio::spawn(async move {
        let bind = bind.trim();
        if bind.is_empty() {
            return;
        }
        let addr: Result<std::net::SocketAddr, _> = bind.parse();
        let addr = match addr {
            Ok(a) => a,
            Err(e) => {
                warn!(%bind, error = %e, "agent.debug_listen: invalid bind address");
                return;
            }
        };
        if !addr.ip().is_loopback() {
            warn!(
                %bind,
                "agent.debug_listen: refusing to bind non-loopback address \
                 (use 127.0.0.1:PORT for validation endpoint)"
            );
            return;
        }
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                warn!(%bind, error = %e, "agent.debug_listen: bind failed");
                return;
            }
        };
        info!(address = %addr, "debug snapshot HTTP listening");
        loop {
            let Ok((stream, peer)) = listener.accept().await else {
                continue;
            };
            if !peer.ip().is_loopback() {
                continue;
            }
            let cfg = cfg.clone();
            let host = host.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_connection(stream, &cfg, &host).await {
                    tracing::debug!(error = %e, "debug snapshot conn error");
                }
            });
        }
    });
}

static DEBUG_UI_HTML: &str = include_str!("debug_ui.html");

async fn serve_connection(mut stream: TcpStream, cfg: &AgentConfig, host: &HostId) -> std::io::Result<()> {
    let mut buf = [0_u8; 4096];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let first = req.lines().next().unwrap_or("");

    // Parse just the path from "GET /path HTTP/1.1"
    let path = first
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/");

    let (status, content_type, body): (&str, &str, String) = match path {
        "/" | "/v1/debug" | "/v1/debug/" => {
            ("200 OK", "text/html; charset=utf-8", DEBUG_UI_HTML.to_string())
        }
        "/v1/debug/snapshot" | "/debug/snapshot" => {
            let snapshot = build_debug_snapshot(cfg, host).await.to_string();
            ("200 OK", "application/json; charset=utf-8", snapshot)
        }
        _ => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            format!("Not found: {path}\n\nAvailable:\n  GET /\n  GET /v1/debug/snapshot"),
        ),
    };

    let resp = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Cache-Control: no-cache\r\n\
         \r\n\
         {}",
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes()).await?;
    Ok(())
}

pub async fn build_debug_snapshot(cfg: &AgentConfig, host: &HostId) -> Value {
    let mut probes = Vec::<Value>::new();

    // For commands that need raw device access (smartctl, nvme, ipmitool),
    // prefix with `sudo -n` when the agent is not running as root.
    // A sudoers file at /etc/sudoers.d/monitor-agent provides NOPASSWD for these.
    let sudo: &[&str] = if is_root() { &[] } else { &["sudo", "-n"] };

    probes.push(run_cmd_probe("uname", &[("argv", json!(["/bin/uname", "-a"]))]).await);
    probes.push(
        run_cmd_probe(
            "proc_pressure_memory",
            &[("argv", json!(["/bin/cat", "/proc/pressure/memory"]))],
        )
        .await,
    );

    if Path::new("/usr/bin/nvme").exists() || Path::new("/bin/nvme").exists() {
        let bin = if Path::new("/usr/bin/nvme").exists() { "/usr/bin/nvme" } else { "/bin/nvme" };
        let nvme_list: Vec<&str> = sudo.iter().chain([bin, "list"].iter()).copied().collect();
        probes.push(run_cmd_probe("nvme_list", &[("argv", json!(nvme_list))]).await);
        if let Some(dev) = first_glob_dev("/dev/nvme*n1") {
            let nvme_smart: Vec<&str> = sudo.iter()
                .chain([bin, "smart-log", "-o", "json"].iter())
                .copied().collect();
            probes.push(
                run_cmd_probe("nvme_smart_log_json",
                    &[("argv", json!([nvme_smart, vec![dev.as_str()]].concat()))]).await,
            );
        }
    }

    if let Some(sd) = first_block_dev("sd") {
        if Path::new("/usr/sbin/smartctl").exists() {
            let dev = format!("/dev/{sd}");
            // -i = device identity only (no SG_IO passthrough) — works on VMs too
            let mut info_argv: Vec<serde_json::Value> = sudo.iter()
                .chain(["/usr/sbin/smartctl", "-i", "-j"].iter())
                .map(|s| json!(s)).collect();
            info_argv.push(json!(&dev));
            probes.push(run_cmd_probe("smartctl_info", &[("argv", json!(info_argv))]).await);
            // Full SMART data — may show EPERM on QEMU VMs (SG_IO passthrough blocked by hypervisor)
            // On real HPE/Dell/Supermicro hardware this returns full SMART attributes
            let mut full_argv: Vec<serde_json::Value> = sudo.iter()
                .chain(["/usr/sbin/smartctl", "-a", "-j"].iter())
                .map(|s| json!(s)).collect();
            full_argv.push(json!(&dev));
            probes.push(run_cmd_probe("smartctl_full", &[("argv", json!(full_argv))]).await);
        }
    }

    if Path::new("/usr/bin/ipmitool").exists() {
        let ipmitool = "/usr/bin/ipmitool";
        let make_ipmi = |args: &[&str]| -> serde_json::Value {
            let v: Vec<&str> = sudo.iter().chain(std::iter::once(&ipmitool)).chain(args.iter()).copied().collect();
            json!(v)
        };
        probes.push(run_cmd_probe("ipmitool_sdr_temperature",
            &[("argv", make_ipmi(&["sdr", "type", "Temperature"]))]).await);
        probes.push(run_cmd_probe("ipmitool_sdr_memory",
            &[("argv", make_ipmi(&["sdr", "type", "Memory"]))]).await);
        probes.push(run_cmd_probe("ipmitool_sel_last20",
            &[("argv", make_ipmi(&["sel", "elist", "last", "20"]))]).await);
    }

    if Path::new("/usr/bin/lspci").exists() {
        probes.push(
            run_shell_probe(
                "lspci_nn_head",
                "/usr/bin/sh",
                &["-c", "LC_ALL=C /usr/bin/lspci -nn 2>/dev/null | head -n 30"],
            )
            .await,
        );
    }

    if let Ok(iface) = first_monitorable_iface(&cfg.modules.network_nic.ignore_regex) {
        if Path::new("/usr/sbin/ethtool").exists() {
            probes.push(
                run_cmd_probe(
                    "ethtool_S",
                    &[(
                        "argv",
                        json!(["/usr/sbin/ethtool", "-S", iface.as_str()]),
                    )],
                )
                .await,
            );
        }
    }

    if Path::new("/usr/bin/journalctl").exists() {
        probes.push(
            run_cmd_probe(
                "journalctl_kernel_tail",
                &[(
                    "argv",
                    json!([
                        "/usr/bin/journalctl",
                        "-k",
                        "-n",
                        "8",
                        "--no-pager",
                        "-o",
                        "short-precise"
                    ]),
                )],
            )
            .await,
        );
    }

    // EDAC CE count (if present)
    for mc in 0..4u8 {
        let p = format!("/sys/devices/system/edac/mc/mc{mc}/ce_count");
        if Path::new(&p).exists() {
            probes.push(
                run_cmd_probe(
                    &format!("edac_mc{mc}_ce_count"),
                    &[("argv", json!(["/bin/cat", p]))],
                )
                .await,
            );
        }
    }

    let mut modules_val = serde_json::to_value(&cfg.modules).unwrap_or(json!({}));
    redact_secrets(&mut modules_val);

    let syslog_rules = serde_json::to_value(&cfg.modules.syslog_rules).unwrap_or(json!({}));

    json!({
        "schema_version": 1,
        "generated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "maintenance_mode": cfg.maintenance_mode(),
        "host": {
            "hostname": host.host(),
            "host_id": host.host_id(),
            "primary_ip": host.primary_ip(),
        },
        "tags": cfg.tags,
        "modules_effective": modules_val,
        "network_nic_ignore_regex": cfg.modules.network_nic.ignore_regex,
        "syslog_rules": syslog_rules,
        "probes": probes,
        "note": "Each probe shows what the OS returned for common agent inputs. Truncated; bind debug_listen to 127.0.0.1 only.",
    })
}

fn redact_secrets(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if k == "redfish_password" {
                    *val = Value::String("***".into());
                } else {
                    redact_secrets(val);
                }
            }
        }
        Value::Array(a) => {
            for x in a.iter_mut() {
                redact_secrets(x);
            }
        }
        _ => {}
    }
}

async fn run_cmd_probe(name: &str, extra_fields: &[(&str, Value)]) -> Value {
    let argv = extra_fields.iter().find(|(k, _)| *k == "argv").and_then(|(_, v)| v.as_array());
    let Some(av) = argv else {
        return json!({ "name": name, "error": "missing argv" });
    };
    let strs: Vec<String> = av
        .iter()
        .filter_map(|x| x.as_str().map(|s| s.to_string()))
        .collect();
    if strs.is_empty() {
        return json!({ "name": name, "error": "empty argv" });
    }
    let exe = strs[0].clone();
    let args: Vec<&str> = strs.iter().skip(1).map(|s| s.as_str()).collect();
    let child = match tokio::process::Command::new(&exe)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)   // auto-kill if the timeout cancels the future
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let mut base = json!({
                "name": name,
                "what": format!("Ran `{exe}` {:?}", args),
                "error": format!("spawn: {e}"),
            });
            for (k, v) in extra_fields {
                base[k] = v.clone();
            }
            return base;
        }
    };
    match tokio::time::timeout(CMD_TIMEOUT, child.wait_with_output()).await {
        Err(_) => {
            // child is killed automatically via kill_on_drop when the future is dropped
            let mut base = json!({
                "name": name,
                "what": format!("Ran `{}` {:?}", exe, args),
                "exit_code": null,
                "error": "timeout",
            });
            for (k, v) in extra_fields {
                base[k] = v.clone();
            }
            base
        }
        Ok(Err(e)) => {
            let mut base = json!({
                "name": name,
                "what": format!("Ran `{}` {:?}", exe, args),
                "error": format!("wait: {e}"),
            });
            for (k, v) in extra_fields {
                base[k] = v.clone();
            }
            base
        }
        Ok(Ok(output)) => {
            let stdout = truncate(String::from_utf8_lossy(&output.stdout).as_ref());
            let stderr = truncate(String::from_utf8_lossy(&output.stderr).as_ref());
            let mut base = json!({
                "name": name,
                "what": format!("Ran `{}` {:?}", exe, args),
                "exit_code": output.status.code(),
                "stdout": stdout,
                "stderr": stderr,
            });
            for (k, v) in extra_fields {
                base[k] = v.clone();
            }
            base
        }
    }
}

async fn run_shell_probe(name: &str, shell: &str, shell_argv: &[&str]) -> Value {
    let child = match tokio::process::Command::new(shell)
        .args(shell_argv)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)   // auto-kill if the timeout cancels the future
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "name": name,
                "what": format!("Ran `{shell}` {:?}", shell_argv),
                "error": format!("spawn: {e}")
            })
        }
    };
    match tokio::time::timeout(CMD_TIMEOUT, child.wait_with_output()).await {
        Err(_) => {
            // child is killed automatically via kill_on_drop when the future is dropped
            json!({
                "name": name,
                "what": format!("Ran `{shell}` {:?}", shell_argv),
                "error": "timeout"
            })
        }
        Ok(Err(e)) => json!({
            "name": name,
            "what": format!("Ran `{shell}` {:?}", shell_argv),
            "error": format!("wait: {e}")
        }),
        Ok(Ok(output)) => json!({
            "name": name,
            "what": format!("Ran `{shell}` {:?}", shell_argv),
            "exit_code": output.status.code(),
            "stdout": truncate(String::from_utf8_lossy(&output.stdout).as_ref()),
            "stderr": truncate(String::from_utf8_lossy(&output.stderr).as_ref()),
        }),
    }
}

fn truncate(s: &str) -> String {
    if s.len() <= MAX_BODY_CHARS {
        s.to_string()
    } else {
        let mut end = MAX_BODY_CHARS;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…[truncated {} bytes]", &s[..end], s.len() - end)
    }
}

fn first_glob_dev(pattern_path: &str) -> Option<String> {
    let expanded = if pattern_path.starts_with("/dev/") {
        pattern_path.to_string()
    } else {
        format!("/dev/{pattern_path}")
    };
    // Sort results so we always pick the lowest-numbered device (nvme0n1 before nvme1n1 etc.)
    let mut matches: Vec<String> = glob::glob(&expanded)
        .ok()?
        .filter_map(Result::ok)
        .filter(|p| p.exists())
        .filter_map(|p| p.into_os_string().into_string().ok())
        .collect();
    matches.sort();
    matches.into_iter().next()
}

fn first_block_dev(kind_prefix: &str) -> Option<String> {
    let dir = Path::new("/sys/block");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    // Collect and sort so we get sda, sdb... in order (readdir order is undefined).
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(kind_prefix) && !n.contains("boot"))
        .collect();
    names.sort();
    // Return the first name whose /dev/<name> device file actually exists.
    names.into_iter().find(|n| Path::new(&format!("/dev/{n}")).exists())
}

fn first_monitorable_iface(ignore_patterns: &[String]) -> Result<String, ()> {
    let set =
        RegexSet::new(ignore_patterns).map_err(|_| ())?;
    let dir = Path::new("/sys/class/net");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Err(());
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "lo")
        .collect();
    names.sort();
    for n in names {
        if !set.is_match(&n) {
            return Ok(n);
        }
    }
    Err(())
}
