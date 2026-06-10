//! File-based logging for the UMA agent.
//!
//! Two log files, both under /var/log/monitor-agent/:
//!
//!   error.log      — ERROR + WARN only. 7-day retention. For incidents and
//!                    on-call triage: shows every failure, timeout, and config
//!                    problem without the noise of routine poll output.
//!
//!   execution.log  — INFO + above. 3-day retention. Full operational trace:
//!                    agent start, module activation, poll cycles, alerts fired,
//!                    config values loaded, connectivity events. Lets you replay
//!                    exactly what the agent did and why.
//!
//! Log format (both files):
//!   2026-06-08 10:44:27.114 IST  INFO  [disk_smart] Poll /dev/sdb: temp=35°C wear=4% media_errors=0
//!   2026-06-08 10:44:30.001 IST  ERROR [disk_smart] smartctl timed out (30s) on /dev/sdc — drive may be unresponsive
//!
//! Both files are also written in addition to systemd journal (stdout).
//! Log rotation and deletion are handled by both this module (on startup) and
//! the logrotate config deployed alongside the agent.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{filter::LevelFilter, fmt, EnvFilter, Layer};

pub const LOG_DIR: &str = "/var/log/monitor-agent";

/// Error log: WARN + ERROR, kept 7 days.
const ERROR_LOG_FILE:      &str = "error.log";
const ERROR_LOG_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);

/// Execution log: INFO + above, kept 3 days.
const EXEC_LOG_FILE:       &str = "execution.log";
const EXEC_LOG_RETENTION:  Duration = Duration::from_secs(3 * 24 * 3600);

/// Initialise file-based logging.
///
/// Returns two `WorkerGuard` handles that must be kept alive for the duration
/// of the process — dropping them flushes and closes the background log writers.
/// Call this once, before spawning any tracing-instrumented tasks.
pub fn init(env_filter_str: &str) -> (WorkerGuard, WorkerGuard) {
    let log_dir = Path::new(LOG_DIR);

    // Best-effort: create log directory.  If this fails (e.g. in tests or
    // CI that doesn't run as root), fall back gracefully to stdout-only.
    if std::fs::create_dir_all(log_dir).is_err() {
        init_stdout_only(env_filter_str);
        // Return dummy guards — stdout writer has no background thread.
        let (_, g1) = tracing_appender::non_blocking(std::io::sink());
        let (_, g2) = tracing_appender::non_blocking(std::io::sink());
        return (g1, g2);
    }

    // Purge old log files before opening new appenders.
    purge_old_logs(log_dir, ERROR_LOG_FILE, ERROR_LOG_RETENTION);
    purge_old_logs(log_dir, EXEC_LOG_FILE,  EXEC_LOG_RETENTION);

    // ---- Error log appender (WARN + ERROR) ----
    let error_file = open_log_file(log_dir, ERROR_LOG_FILE);
    let (error_writer, error_guard) = tracing_appender::non_blocking(error_file);
    let error_layer = fmt::layer()
        .with_writer(error_writer)
        .with_timer(ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f %Z".into()))
        .with_ansi(false)
        .with_level(true)
        .with_target(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .compact()
        .with_filter(LevelFilter::WARN);

    // ---- Execution log appender (INFO + above) ----
    let exec_file = open_log_file(log_dir, EXEC_LOG_FILE);
    let (exec_writer, exec_guard) = tracing_appender::non_blocking(exec_file);
    let exec_layer = fmt::layer()
        .with_writer(exec_writer)
        .with_timer(ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f %Z".into()))
        .with_ansi(false)
        .with_level(true)
        .with_target(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .compact()
        .with_filter(LevelFilter::INFO);

    // ---- Stdout / journald layer (original behaviour, coloured) ----
    let stdout_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(env_filter_str));
    let stdout_layer = fmt::layer()
        .with_timer(ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f %Z".into()))
        .with_ansi(false)
        .compact()
        .with_filter(stdout_filter);

    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(exec_layer)
        .with(error_layer)
        .init();

    (error_guard, exec_guard)
}

/// Fallback: stdout only (used when log dir cannot be created).
fn init_stdout_only(env_filter_str: &str) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(env_filter_str));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f %Z".into()))
        .with_ansi(false)
        .compact()
        .init();
}

/// Open (or append to) a log file, writing a startup separator line.
fn open_log_file(dir: &Path, filename: &str) -> std::fs::File {
    let path = dir.join(filename);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap_or_else(|_| {
            // If we can't open the file, fall back to /dev/null silently.
            std::fs::OpenOptions::new()
                .write(true).open("/dev/null").expect("/dev/null")
        });
    // Write a separator so restarts are visible in the log.
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");
    let _ = writeln!(f,
        "\n──────────────────────────────────────────────────────────────────\
         \n  UMA agent started  {}  (PID {})\
         \n──────────────────────────────────────────────────────────────────",
        ts,
        std::process::id(),
    );
    f
}

/// Delete rotated/old log files older than `max_age`.
/// Files matching `<basename>.*` (tracing-appender rotation suffixes like
/// `execution.log.2026-06-05`) are also cleaned.
fn purge_old_logs(dir: &Path, basename: &str, max_age: Duration) {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(max_age)
        .unwrap_or(std::time::UNIX_EPOCH);

    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(basename) { continue; }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() { continue };
        if let Ok(modified) = meta.modified() {
            if modified < cutoff {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Returns the absolute path to the execution log (for --version / startup banner).
pub fn execution_log_path() -> PathBuf {
    PathBuf::from(LOG_DIR).join(EXEC_LOG_FILE)
}

/// Returns the absolute path to the error log.
pub fn error_log_path() -> PathBuf {
    PathBuf::from(LOG_DIR).join(ERROR_LOG_FILE)
}
