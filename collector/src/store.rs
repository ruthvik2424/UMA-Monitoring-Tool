//! SQLite ring-buffer for alert history.
//!
//! Schema is intentionally tiny: alerts(id, ts, host_id, host, severity,
//! state, category, fingerprint, body_json). A background task periodically
//! deletes rows older than `retention_days`.
//!
//! Writes go on a separate task so they never block the broadcast fanout.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uma_shared::Alert;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    pub async fn open(path: &Path, retention_days: u32) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let url = format!("sqlite://{}", path.display());
        let opts = SqliteConnectOptions::from_str(&url)
            .context("parsing sqlite URL")?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .context("opening sqlite")?;
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS alerts (
                id          TEXT PRIMARY KEY,
                ts          TEXT NOT NULL,
                host_id     TEXT NOT NULL,
                host        TEXT NOT NULL,
                severity    TEXT NOT NULL,
                state       TEXT NOT NULL,
                category    TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                body        TEXT NOT NULL
            );"#,
        )
        .execute(&pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS alerts_ts ON alerts(ts);")
            .execute(&pool).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS alerts_fp ON alerts(fingerprint);")
            .execute(&pool).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS alerts_host ON alerts(host_id);")
            .execute(&pool).await?;

        info!("collector store at {}", path.display());

        let store = Self { pool };
        store.spawn_retention(retention_days);
        Ok(store)
    }

    pub fn spawn_writer(&self) -> mpsc::Sender<Alert> {
        let (tx, mut rx) = mpsc::channel::<Alert>(8192);
        let pool = self.pool.clone();
        tokio::spawn(async move {
            while let Some(a) = rx.recv().await {
                if let Err(e) = insert(&pool, &a).await {
                    warn!(error = %e, "store: insert failed");
                }
            }
        });
        tx
    }

    pub async fn list_recent(&self, limit: i64) -> Result<Vec<Alert>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT body FROM alerts ORDER BY ts DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (body,) in rows {
            if let Ok(a) = serde_json::from_str::<Alert>(&body) {
                out.push(a);
            }
        }
        Ok(out)
    }

    /// Wipe the alert history table. Operator action triggered by the GUI
    /// "Clear history" button. Does NOT touch the in-memory firing map.
    pub async fn clear_all(&self) -> Result<u64> {
        let r = sqlx::query("DELETE FROM alerts").execute(&self.pool).await?;
        // Reclaim space — VACUUM is heavy but okay because we expect this to
        // be infrequent and intentional.
        let _ = sqlx::query("VACUUM").execute(&self.pool).await;
        Ok(r.rows_affected())
    }

    fn spawn_retention(&self, days: u32) {
        let pool = self.pool.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                tick.tick().await;
                let cutoff: DateTime<Utc> = Utc::now() - ChronoDuration::days(days as i64);
                if let Err(e) = sqlx::query("DELETE FROM alerts WHERE ts < ?")
                    .bind(cutoff.to_rfc3339())
                    .execute(&pool)
                    .await
                {
                    warn!(error = %e, "store: retention prune failed");
                }
                debug!("store: pruned alerts older than {} days", days);
            }
        });
    }
}

async fn insert(pool: &SqlitePool, a: &Alert) -> Result<()> {
    let body = serde_json::to_string(a)?;
    sqlx::query(
        "INSERT OR REPLACE INTO alerts (id, ts, host_id, host, severity, state, category, fingerprint, body)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&a.id)
    .bind(a.ts.to_rfc3339())
    .bind(&a.host_id)
    .bind(&a.host)
    .bind(a.severity.as_str())
    .bind(format!("{:?}", a.state).to_lowercase())
    .bind(&a.category)
    .bind(&a.fingerprint)
    .bind(&body)
    .execute(pool)
    .await?;
    Ok(())
}
