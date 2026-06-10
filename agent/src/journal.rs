//! Async tail of the systemd journal.
//!
//! `/dev/kmsg` is kernel-only. To catch userspace daemon messages
//! (systemd "Watchdog timeout ...", pacemaker "no heartbeat from peer ...",
//! corosync, drbd, multipathd, keepalived, etc.) we need journald.
//!
//! Implementation: spawn `journalctl -f -o cat --since now` and read its
//! stdout line by line, broadcasting each line. Cheap (one subprocess),
//! works on every systemd-based distro, no new system dependency.
//!
//! `--since now` ensures we don't replay history on agent restart.
//! `-o cat` strips metadata and gives us just the message body.
//! `--follow` streams new entries forever; if journalctl exits we restart.

use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct JournalLine {
    pub text: String,
}

pub fn spawn(buffer: usize) -> Result<broadcast::Receiver<JournalLine>> {
    let (tx, rx) = broadcast::channel(buffer);

    // Probe for journalctl up front — if it's not present we'd loop forever.
    let probe = std::process::Command::new("journalctl")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("probing for journalctl")?;
    if !probe.success() {
        return Err(anyhow::anyhow!("journalctl probe failed"));
    }

    let tx_clone = tx.clone();
    tokio::spawn(async move {
        loop {
            let mut child = match Command::new("journalctl")
                .args(["-f", "-o", "cat", "--no-pager", "--since", "now"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "journal: spawn failed; retrying in 2s");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    let _ = child.kill().await;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            info!("journal: tailing journalctl -f");
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        warn!("journal: journalctl EOF; restarting");
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
                        if !trimmed.is_empty() {
                            let _ = tx_clone.send(JournalLine { text: trimmed });
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "journal: read error; restarting");
                        break;
                    }
                }
            }
            let _ = child.kill().await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    Ok(rx)
}
