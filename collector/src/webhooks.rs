//! Outbound webhook sink — fans alerts out to Slack / Google Chat / generic
//! HTTP POST endpoints. One async task per configured webhook; each is a
//! broadcast::Receiver subscriber so the existing fanout to the GUI is
//! unaffected (no extra latency on the WSS path).
//!
//! Per-webhook config supports:
//!   - kind:           "slack" | "google_chat" | "generic_json"
//!   - severity_min:   "info" | "warning" | "critical"
//!   - states / notify_on_resolved: default `["firing"]` only — opt-in resolves
//!   - host_filter:    optional regex against alert.host
//!   - category_filter: optional regex against alert.category
//!   - rate_limit_per_min: drop messages above this rate to avoid alert storms
//!     hammering the upstream channel
//!
//! Failures are warned (not retried for now). Slack/GChat rate-limit at
//! ~1/sec per webhook; we honour their 429 responses by backing off.

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio::time::sleep;
use tracing::{debug, info, warn};
use uma_shared::{Alert, AlertState, Envelope, Severity};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebhookCfg {
    pub name: String,
    /// "slack" | "google_chat" | "generic_json"
    pub kind: String,
    pub url: String,
    #[serde(default = "default_sev_min")]
    pub severity_min: String,
    #[serde(default = "default_states")]
    pub states: Vec<String>,
    /// Append `resolved` deliveries (Slack noise control). Default OFF — use
    /// `states = ["firing","resolved"]` **or** set this flag when you want
    /// clears mirrored to chat.
    #[serde(default)]
    pub notify_on_resolved: bool,
    #[serde(default)]
    pub host_filter: Option<String>,
    #[serde(default)]
    pub category_filter: Option<String>,
    #[serde(default = "default_rate")]
    pub rate_limit_per_min: u32,
    /// Optional URL prefix used in the "View in UMA" button for Slack/GChat
    /// (e.g. "https://collector.dc1:8443"). If empty, no link button is added.
    #[serde(default)]
    pub gui_url: String,
    /// Skip TLS verification for the webhook endpoint (rarely needed —
    /// only for internal corp HTTPS proxies with self-signed certs).
    #[serde(default)]
    pub insecure_tls: bool,
}

fn default_sev_min() -> String { "warning".into() }
fn default_states() -> Vec<String> { vec!["firing".into()] }
fn default_rate() -> u32 { 60 }

fn effective_delivery_states(cfg: &WebhookCfg) -> Vec<String> {
    let mut s = cfg.states.clone();
    if cfg.notify_on_resolved && !s.iter().any(|x| x.eq_ignore_ascii_case("resolved")) {
        s.push("resolved".into());
    }
    if s.is_empty() {
        s.push("firing".into());
    }
    s
}

/// Spawn one task per webhook. Each subscribes to its own copy of the
/// broadcast channel so a slow webhook can't backpressure the GUI fanout.
pub fn spawn_all(cfgs: Vec<WebhookCfg>, fanout: &broadcast::Sender<Envelope>) {
    for cfg in cfgs {
        let rx = fanout.subscribe();
        tokio::spawn(run_one(cfg, rx));
    }
}

async fn run_one(cfg: WebhookCfg, mut rx: broadcast::Receiver<Envelope>) {
    let client = match build_client(&cfg) {
        Ok(c) => c,
        Err(e) => { warn!("webhook '{}': cannot build client: {}", cfg.name, e); return; }
    };
    let sev_min = parse_sev(&cfg.severity_min);
    let states_eff = effective_delivery_states(&cfg);
    let host_re = cfg.host_filter.as_ref().and_then(|s| Regex::new(s).ok());
    let cat_re = cfg.category_filter.as_ref().and_then(|s| Regex::new(s).ok());

    info!(
        "webhook '{}' active (kind={}, sev_min={:?}, delivery_states={:?}, rate={}/min)",
        cfg.name, cfg.kind, sev_min, states_eff, cfg.rate_limit_per_min
    );

    let mut window: Vec<Instant> = Vec::new();
    let window_dur = Duration::from_secs(60);

    loop {
        let env = match rx.recv().await {
            Ok(e) => e,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("webhook '{}': lagged by {} envelopes", cfg.name, n);
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let alert = match env {
            Envelope::Alert(a) => a,
            _ => continue,    // only alerts go to webhooks
        };
        if !passes_filters(
            &alert,
            sev_min,
            states_eff.as_slice(),
            host_re.as_ref(),
            cat_re.as_ref(),
        ) {
            continue;
        }

        // Sliding-window rate limit per webhook.
        let now = Instant::now();
        window.retain(|t| now.duration_since(*t) <= window_dur);
        if window.len() as u32 >= cfg.rate_limit_per_min {
            debug!("webhook '{}': rate-limited; dropping {}", cfg.name, alert.fingerprint);
            continue;
        }
        window.push(now);

        let body = match cfg.kind.as_str() {
            "slack"                       => format_slack(&alert, &cfg.gui_url),
            "google_chat" | "gchat"       => format_gchat(&alert, &cfg.gui_url),
            "generic_json"                => format_generic(&alert),
            other => { warn!("webhook '{}': unknown kind {:?}", cfg.name, other); continue; }
        };

        deliver(&client, &cfg, &alert, body).await;
    }
}

fn build_client(cfg: &WebhookCfg) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5));
    if cfg.insecure_tls { b = b.danger_accept_invalid_certs(true); }
    Ok(b.build()?)
}

fn parse_sev(s: &str) -> Severity {
    match s.to_lowercase().as_str() {
        "critical" => Severity::Critical,
        "warning"  => Severity::Warning,
        _          => Severity::Info,
    }
}

fn passes_filters(a: &Alert, sev_min: Severity, states: &[String],
                  host_re: Option<&Regex>, cat_re: Option<&Regex>) -> bool {
    // Severity ordering: Info < Warning < Critical.
    if a.severity < sev_min { return false; }
    let state = match a.state { AlertState::Firing => "firing", AlertState::Resolved => "resolved" };
    if !states.iter().any(|s| s.eq_ignore_ascii_case(state)) { return false; }
    if let Some(re) = host_re { if !re.is_match(&a.host) { return false; } }
    if let Some(re) = cat_re  { if !re.is_match(&a.category) { return false; } }
    true
}

async fn deliver(client: &reqwest::Client, cfg: &WebhookCfg, alert: &Alert, body: serde_json::Value) {
    // One simple retry on 5xx / network error, then give up. Slack/GChat
    // have their own redundancy.
    for attempt in 1..=2 {
        let res = client.post(&cfg.url).json(&body).send().await;
        match res {
            Ok(r) if r.status().is_success() => {
                debug!("webhook '{}': delivered {} ({})", cfg.name, alert.fingerprint, alert.severity.as_str());
                return;
            }
            Ok(r) if r.status().as_u16() == 429 => {
                let retry_after_s: u64 = r.headers()
                    .get("retry-after").and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse().ok()).unwrap_or(2);
                warn!("webhook '{}': rate-limited by upstream; sleeping {}s", cfg.name, retry_after_s);
                sleep(Duration::from_secs(retry_after_s)).await;
            }
            Ok(r) => {
                let code = r.status();
                let txt = r.text().await.unwrap_or_default();
                warn!("webhook '{}': HTTP {} attempt {}/{}: {}", cfg.name, code, attempt, 2, txt);
                if !code.is_server_error() { return; }   // don't retry 4xx
                sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                warn!("webhook '{}': transport err attempt {}/{}: {}", cfg.name, attempt, 2, e);
                sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

// ============================================================================
// Format builders
// ============================================================================

fn format_slack(a: &Alert, gui_url: &str) -> serde_json::Value {
    let color = match a.severity {
        Severity::Critical => "#e5484d",
        Severity::Warning  => "#ffba18",
        Severity::Info     => "#3e63dd",
    };
    let icon = match a.severity {
        Severity::Critical => ":rotating_light:",
        Severity::Warning  => ":warning:",
        Severity::Info     => ":information_source:",
    };
    let state_emoji = match a.state {
        AlertState::Firing   => ":fire:",
        AlertState::Resolved => ":white_check_mark:",
    };
    let state_word = match a.state {
        AlertState::Firing   => "FIRING",
        AlertState::Resolved => "RESOLVED",
    };
    let mut blocks = vec![
        serde_json::json!({
            "type": "header",
            "text": { "type": "plain_text",
                      "text": format!("{} {} {}", state_emoji, icon, a.title) }
        }),
        serde_json::json!({
            "type": "section",
            "text": { "type": "mrkdwn", "text": format!("*{}*\n{}", a.host, a.message) }
        }),
        serde_json::json!({
            "type": "section",
            "fields": [
                {"type": "mrkdwn", "text": format!("*Severity:*\n{}", a.severity.as_str().to_uppercase())},
                {"type": "mrkdwn", "text": format!("*State:*\n{}", state_word)},
                {"type": "mrkdwn", "text": format!("*Category:*\n{}", a.category)},
                {"type": "mrkdwn", "text": format!("*Device:*\n{}", a.device.as_deref().unwrap_or("-"))},
                {"type": "mrkdwn", "text": format!("*First seen:*\n{}", a.first_seen.to_rfc3339())},
                {"type": "mrkdwn", "text": format!("*Occurrences:*\n{}", a.occurrences)},
            ]
        }),
    ];
    if !gui_url.is_empty() {
        blocks.push(serde_json::json!({
            "type": "actions",
            "elements": [{
                "type": "button",
                "text": {"type": "plain_text", "text": "View in UMA"},
                "url": gui_url,
                "style": if a.severity == Severity::Critical { "danger" } else { "primary" },
            }]
        }));
    }
    blocks.push(serde_json::json!({
        "type": "context",
        "elements": [{
            "type": "mrkdwn",
            "text": format!("Fingerprint: `{}`", a.fingerprint),
        }]
    }));

    serde_json::json!({
        "attachments": [{
            "color": color,
            "blocks": blocks,
            "fallback": format!("{} {} on {}: {}", state_word, a.severity.as_str(), a.host, a.title),
        }]
    })
}

fn format_gchat(a: &Alert, gui_url: &str) -> serde_json::Value {
    let icon = match a.severity {
        Severity::Critical => "🔴",
        Severity::Warning  => "🟡",
        Severity::Info     => "🔵",
    };
    let state_word = match a.state {
        AlertState::Firing   => "FIRING",
        AlertState::Resolved => "RESOLVED",
    };
    let mut widgets = vec![
        serde_json::json!({"textParagraph": {"text": &a.message}}),
        serde_json::json!({"decoratedText": {
            "topLabel": "Host",
            "text": &a.host,
        }}),
        serde_json::json!({"decoratedText": {
            "topLabel": "Category / metric",
            "text": format!("{} / {}", a.category, a.metric),
        }}),
        serde_json::json!({"decoratedText": {
            "topLabel": "Severity / State",
            "text": format!("{} / {}", a.severity.as_str().to_uppercase(), state_word),
        }}),
        serde_json::json!({"decoratedText": {
            "topLabel": "First seen",
            "text": a.first_seen.to_rfc3339(),
        }}),
    ];
    if a.occurrences > 1 {
        widgets.push(serde_json::json!({"decoratedText": {
            "topLabel": "Occurrences",
            "text": a.occurrences.to_string(),
        }}));
    }
    if !gui_url.is_empty() {
        widgets.push(serde_json::json!({
            "buttonList": {"buttons": [{
                "text": "View in UMA",
                "onClick": {"openLink": {"url": gui_url}},
            }]}
        }));
    }
    serde_json::json!({
        "cardsV2": [{
            "cardId": a.fingerprint,
            "card": {
                "header": {
                    "title": format!("{} {}", icon, a.title),
                    "subtitle": format!("{} · {}", a.host, a.severity.as_str().to_uppercase()),
                    "imageType": "CIRCLE"
                },
                "sections": [{ "widgets": widgets }]
            }
        }]
    })
}

fn format_generic(a: &Alert) -> serde_json::Value {
    // Just dump the alert as-is. Useful for piping into custom collectors,
    // PagerDuty Events API v2, etc. (the user defines their own consumer).
    serde_json::json!(a)
}
