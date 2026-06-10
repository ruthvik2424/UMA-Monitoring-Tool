//! Async tail of /dev/kmsg.
//!
//! `/dev/kmsg` is the kernel's structured-printk interface. Each read
//! returns exactly one record. Opening with `O_NONBLOCK` lets us treat it
//! like a stream: read until EAGAIN, await readability, repeat.
//!
//! Records look like:
//!   `LEVEL,SEQNUM,TIMESTAMP,FLAG;TEXT`
//!
//! We parse the prefix, keep the text, and broadcast to all subscribers.
//! Every kernel-log-driven module listens here instead of polling journalctl.

use anyhow::{Context, Result};
use std::os::unix::io::AsRawFd;
use std::os::unix::prelude::OpenOptionsExt;
use tokio::io::{unix::AsyncFd, Interest};
use tokio::sync::broadcast;
use tracing::{debug, warn};

#[derive(Debug, Clone)]
#[allow(dead_code)] // level/seq/ts_us are diagnostics surfaced via raw_kv on alerts
pub struct KmsgLine {
    pub level: u8,
    pub seq: u64,
    pub ts_us: u64,
    pub text: String,
}

impl KmsgLine {
    /// Approximate severity bucket.
    #[allow(dead_code)] // exposed for future syslog_rules severity mapping
    pub fn severity_str(&self) -> &'static str {
        match self.level {
            0..=2 => "critical", // EMERG/ALERT/CRIT
            3 => "error",
            4 => "warning",
            5 => "notice",
            6 => "info",
            _ => "debug",
        }
    }
}

/// Open /dev/kmsg, seek to "tail" (skip historical records so we don't
/// re-fire alerts at startup), and broadcast new lines.
pub fn spawn(buffer: usize) -> Result<broadcast::Receiver<KmsgLine>> {
    let (tx, rx) = broadcast::channel(buffer);
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc_flags())
        .open("/dev/kmsg")
        .context("opening /dev/kmsg (need CAP_SYSLOG or root)")?;

    // Skip every record currently in the kernel ring buffer; subsequent
    // reads block until a NEW message arrives. SEEK_END (2) does this
    // correctly. SEEK_DATA (3) is only useful in conjunction with
    // SYSLOG_ACTION_CLEAR — without a recent clear it returns position 0
    // (= oldest message), which causes every agent restart to replay the
    // entire kmsg history and falsely re-fire alerts.
    unsafe {
        let _ = nix::libc::lseek(f.as_raw_fd(), 0, nix::libc::SEEK_END);
    }

    let async_fd = AsyncFd::with_interest(f, Interest::READABLE)
        .context("registering /dev/kmsg with tokio")?;

    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            let mut guard = match async_fd.readable().await {
                Ok(g) => g,
                Err(e) => {
                    warn!(error = %e, "kmsg readable() failed");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
            };
            // Drain everything currently available before yielding.
            loop {
                let n = unsafe {
                    nix::libc::read(
                        async_fd.get_ref().as_raw_fd(),
                        buf.as_mut_ptr() as *mut _,
                        buf.len(),
                    )
                };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    match err.raw_os_error() {
                        Some(e) if e == nix::libc::EAGAIN || e == nix::libc::EWOULDBLOCK => {
                            guard.clear_ready();
                            break;
                        }
                        Some(e) if e == nix::libc::EPIPE => {
                            // Reader fell behind the kernel ring; resync and continue.
                            debug!("kmsg EPIPE — resynchronizing");
                            continue;
                        }
                        _ => {
                            warn!(error = %err, "kmsg read failed");
                            guard.clear_ready();
                            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                            break;
                        }
                    }
                }
                if n == 0 {
                    guard.clear_ready();
                    break;
                }
                let bytes = &buf[..n as usize];
                if let Some(line) = parse(bytes) {
                    let _ = tx.send(line);
                }
            }
        }
    });
    Ok(rx)
}

fn parse(bytes: &[u8]) -> Option<KmsgLine> {
    let s = std::str::from_utf8(bytes).ok()?;
    let (prefix, rest) = s.split_once(';')?;
    let mut parts = prefix.split(',');
    let level: u8 = parts.next()?.parse().ok()?;
    let seq: u64 = parts.next()?.parse().ok()?;
    let ts_us: u64 = parts.next()?.parse().ok()?;
    // The fourth field (flag) and any continuation key/value pairs are
    // ignored for now.
    let text_end = rest.find('\n').unwrap_or(rest.len());
    let text = rest[..text_end].to_string();
    Some(KmsgLine { level, seq, ts_us, text })
}

fn libc_flags() -> i32 {
    nix::libc::O_NONBLOCK
}
