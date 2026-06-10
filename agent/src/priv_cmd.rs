//! Privilege-aware, timeout-protected subprocess helpers.
//!
//! All agent subprocess calls go through `run_cmd` or `priv_command` so that:
//!   1. Non-root agents transparently get `sudo -n` prepended for disk tools.
//!   2. Every call has a hard wall-clock timeout — a hung drive, frozen NIC
//!      firmware, or stalled vendor tool can never block a monitoring task forever.
//!
//! # Sudoers
//! When running as non-root, add /etc/sudoers.d/monitor-agent:
//!   monitor-agent ALL=(ALL) NOPASSWD: /usr/sbin/smartctl, /usr/bin/nvme, /usr/sbin/nvme

use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::warn;

/// Default timeouts per tool category.
pub const TIMEOUT_STORAGE: Duration = Duration::from_secs(20); // ssacli, perccli64, storcli64
pub const TIMEOUT_IPMI:    Duration = Duration::from_secs(15); // ipmitool
pub const TIMEOUT_NET:     Duration = Duration::from_secs(10); // ethtool
pub const TIMEOUT_GPU:     Duration = Duration::from_secs(15); // nvidia-smi, rocm-smi
pub const TIMEOUT_GENERAL: Duration = Duration::from_secs(15); // lspci, sensors, etc.

/// Returns true if the current process is running as UID 0 (root).
#[inline]
pub fn is_root() -> bool {
    unsafe { libc::getuid() == 0 }
}

/// Build a command, prepending `sudo -n` when not running as root.
/// Used for tools that require CAP_SYS_RAWIO (smartctl, nvme).
pub fn priv_command(bin: &str) -> Command {
    if is_root() {
        Command::new(bin)
    } else {
        let mut cmd = Command::new("sudo");
        cmd.args(["-n", bin]);
        cmd
    }
}

/// Output of a timed subprocess run.
pub struct CmdOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: std::process::ExitStatus,
}

/// Run a command with a hard timeout. Returns an error if:
/// - The binary cannot be spawned
/// - The timeout elapses (child is killed via kill_on_drop)
/// - The process wait fails
///
/// On timeout, logs a warning with the command name so operators can
/// correlate stalls with specific hardware probes.
pub async fn run_cmd(mut cmd: Command, label: &str, t: Duration) -> anyhow::Result<CmdOutput> {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn `{label}`: {e}"))?;

    let started = std::time::Instant::now();
    match timeout(t, child.wait_with_output()).await {
        Ok(Ok(out)) => {
            let elapsed_ms = started.elapsed().as_millis();
            let code = out.status.code().unwrap_or(-1);
            // All exit codes logged at DEBUG — individual modules decide
            // whether a non-zero result is noteworthy enough for INFO/WARN.
            let stderr_hint = String::from_utf8_lossy(&out.stderr);
            let hint = stderr_hint.lines().next().unwrap_or("").trim();
            tracing::debug!(cmd = label, exit_code = code, elapsed_ms, stderr = %hint, "cmd done");
            Ok(CmdOutput {
                stdout: out.stdout,
                stderr: out.stderr,
                status: out.status,
            })
        }
        Ok(Err(e)) => Err(anyhow::anyhow!("`{label}` wait error: {e}")),
        Err(_) => {
            warn!(
                cmd = label,
                timeout_s = t.as_secs(),
                "command timed out — process killed"
            );
            Err(anyhow::anyhow!(
                "`{label}` timed out after {}s — hardware may be unresponsive",
                t.as_secs()
            ))
        }
    }
}

/// Convenience: run a regular (non-privileged) command with timeout.
pub async fn run(bin: &str, args: &[&str], t: Duration) -> anyhow::Result<CmdOutput> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    run_cmd(cmd, bin, t).await
}
