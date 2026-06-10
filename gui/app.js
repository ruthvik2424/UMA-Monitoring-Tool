// UMA v4.1 GUI — vanilla SPA. No bundler, no framework.
//
// Views: Alerts | History | Connectivity | Settings
// v4 additions: settings tab, host tooltip, formatted modals, password-protected clear
// v4.1 fixes: grey dark mode, IP stamped on alerts, stable tooltip timestamp
(() => {
  // ============================================================
  // State
  // ============================================================
  const state = {
    alerts:    new Map(),   // fingerprint → alert
    hosts:     new Map(),   // host_id → host
    history:   [],          // newest-first, capped at HISTORY_CAP

    activeTab: "alerts",
    expanded:  new Set(),

    filterText:    "",
    showCritical:  true,
    showWarning:   true,
    showInfo:      false,

    histFilter:      "",
    histShowFiring:  true,
    histShowResolved: true,

    heatmapFilter: "",

    theme: "light",
  };

  const HISTORY_CAP = 5000;

  // ============================================================
  // Desktop Notifications
  // ============================================================
  const notify = {
    enabled:  false,
    sound:    true,
    severity: "critical",
    notified: new Set(),
    audio:    null,
    initial_load_done: false,
  };

  function loadNotifySettings() {
    try {
      const s = JSON.parse(localStorage.getItem("uma-notify") || "{}");
      notify.enabled  = !!s.enabled;
      notify.sound    = s.sound !== false;
      notify.severity = s.severity || "critical";
    } catch {}
  }
  function saveNotifySettings() {
    try {
      localStorage.setItem("uma-notify", JSON.stringify({
        enabled: notify.enabled, sound: notify.sound, severity: notify.severity,
      }));
    } catch {}
  }

  function notifyPermLabel() {
    if (!("Notification" in window)) return "unsupported";
    return Notification.permission;
  }

  function shouldNotify(env) {
    if (!notify.enabled) return false;
    if (env.type !== "alert" || env.state !== "firing") return false;
    if (notify.notified.has(env.fingerprint)) return false;
    if (notify.severity === "critical" && env.severity !== "critical") return false;
    if (notify.severity === "warning"  && !["critical","warning"].includes(env.severity)) return false;
    if (!notify.initial_load_done) return false;
    return true;
  }

  function showDesktopNotification(env) {
    if (!("Notification" in window)) return;
    if (Notification.permission !== "granted") return;
    const icon = env.severity === "critical" ? "⚠️" : env.severity === "warning" ? "⚡" : "ℹ️";
    const host = env.host || "?";
    const ip   = (state.hosts.get(env.host_id) || {}).primary_ip || "";
    try {
      const n = new Notification(`${icon} ${env.title}`, {
        body: `${host}${ip ? " ("+ip+")" : ""}\n${env.message || env.metric || ""}`,
        tag:  env.fingerprint,
        requireInteraction: env.severity === "critical",
      });
      n.onclick = () => {
        window.focus();
        switchTab("alerts");
        state.expanded.add(env.fingerprint);
        rerenderAlerts();
        n.close();
      };
    } catch(e) { console.warn("notification failed:", e); }
    if (notify.sound) playBeep(env.severity);
    notify.notified.add(env.fingerprint);
  }

  function playBeep(severity) {
    try {
      if (!notify.audio) {
        const Ctx = window.AudioContext || window.webkitAudioContext;
        if (!Ctx) return;
        notify.audio = new Ctx();
      }
      if (notify.audio.state === "suspended") notify.audio.resume();
      const ctx = notify.audio;
      const now = ctx.currentTime;
      const beeps = severity === "critical" ? [[880,.25],[880,.25]]
                  : severity === "warning"  ? [[660,.25]]
                  :                           [[440,.15]];
      let t = now;
      for (const [freq,dur] of beeps) {
        const osc = ctx.createOscillator();
        const gn  = ctx.createGain();
        osc.connect(gn); gn.connect(ctx.destination);
        osc.frequency.value = freq;
        osc.type = "sine";
        gn.gain.setValueAtTime(0.001, t);
        gn.gain.exponentialRampToValueAtTime(0.25, t+0.02);
        gn.gain.exponentialRampToValueAtTime(0.001, t+dur);
        osc.start(t); osc.stop(t+dur+0.05);
        t += dur+0.10;
      }
    } catch(e) { console.warn("beep failed:", e); }
  }

  function syncNotifyUI() {
    if (!("Notification" in window)) return;
    const enableEl = $("#np-enable");
    if (enableEl) enableEl.checked = notify.enabled;
    const soundEl  = $("#np-sound");
    if (soundEl)  soundEl.checked  = notify.sound;
    const sevEl    = $("#np-severity");
    if (sevEl)    sevEl.value       = notify.severity;
    const lbl = $("#np-perm");
    if (lbl) {
      const perm = notifyPermLabel();
      lbl.textContent = perm;
      lbl.className = "np-perm " + (perm === "granted" ? "granted" : perm === "denied" ? "denied" : "");
    }
  }

  async function setNotifyEnabled(want) {
    if (want && "Notification" in window && Notification.permission !== "granted") {
      const result = await Notification.requestPermission();
      if (result !== "granted") {
        notify.enabled = false;
        alert("Browser notification permission was denied. Enable it in your browser settings.");
        syncNotifyUI();
        return;
      }
    }
    notify.enabled = want;
    saveNotifySettings();
    syncNotifyUI();
  }

  // ============================================================
  // Theme
  // ============================================================
  function applyTheme(t) {
    state.theme = t;
    document.documentElement.setAttribute("data-theme", t);
    try { localStorage.setItem("uma-theme", t); } catch {}
    // Sync theme buttons in settings
    const btnLight = $("#theme-light");
    const btnDark  = $("#theme-dark");
    if (btnLight) btnLight.classList.toggle("active", t === "light");
    if (btnDark)  btnDark.classList.toggle("active",  t === "dark");
  }
  function initTheme() {
    let t = "light";
    try { t = localStorage.getItem("uma-theme") || "light"; } catch {}
    applyTheme(t);
  }

  // ============================================================
  // Password management (history clear — hidden, not user-settable from UI)
  // Default is set once on first load. Change via browser console only:
  //   localStorage.setItem('uma-clear-pwd', 'yourNewPassword')
  // ============================================================
  const PWD_KEY     = "uma-clear-pwd";
  const DEFAULT_PWD = "UMA@Clr#4821";   // shipped default — change via console

  function initClearPassword() {
    try {
      if (!localStorage.getItem(PWD_KEY)) {
        localStorage.setItem(PWD_KEY, DEFAULT_PWD);
      }
    } catch {}
  }
  function getClearPassword() {
    try { return localStorage.getItem(PWD_KEY) || DEFAULT_PWD; } catch { return DEFAULT_PWD; }
  }

  // ============================================================
  // WebSocket
  // ============================================================
  let ws = null, backoff = 250;

  function connect() {
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    ws = new WebSocket(`${proto}//${location.host}/ws/subscribe`);
    ws.onopen  = () => { backoff = 250; setStatus("online", "live"); };
    ws.onclose = () => {
      setStatus("offline", "disconnected — retrying…");
      setTimeout(connect, backoff);
      backoff = Math.min(backoff * 2, 8000);
    };
    ws.onerror  = () => ws.close();
    ws.onmessage = (e) => { try { handle(JSON.parse(e.data)); } catch {} };
  }

  function handle(env) {
    switch (env.type) {
      case "snapshot":
        state.alerts.clear();
        env.alerts.forEach(a => state.alerts.set(a.fingerprint, a));
        state.hosts.clear();
        env.hosts.forEach(h => state.hosts.set(h.host_id, h));
        state.history = [];
        env.alerts.forEach(a => notify.notified.add(a.fingerprint));
        setTimeout(() => { notify.initial_load_done = true; }, 500);
        rerenderAll();
        fetchHistory();
        break;

      case "alert": {
        pushHistory(env);
        if (env.state === "resolved") {
          state.alerts.delete(env.fingerprint);
          notify.notified.delete(env.fingerprint);
        } else {
          const existing = state.alerts.get(env.fingerprint);
          if (existing) {
            env.first_seen   = existing.first_seen || env.first_seen;
            env.occurrences  = Math.max(env.occurrences || 1, existing.occurrences || 1);
          }
          state.alerts.set(env.fingerprint, env);
          if (shouldNotify(env)) showDesktopNotification(env);
        }
        rerenderAlerts(); rerenderHistory(); rerenderHeatmap();
        break;
      }

      case "heartbeat": {
        let h = state.hosts.get(env.host_id);
        if (!h) {
          h = { host: env.host, host_id: env.host_id, primary_ip: "",
                online: true, last_seen: env.ts, firing_count: 0, tags: {}, maintenance: false };
          state.hosts.set(env.host_id, h);
        }
        h.last_seen    = env.ts;
        h.firing_count = env.firing_count ?? h.firing_count;
        rerenderHeatmap();
        break;
      }

      case "host_status": {
        let hs = state.hosts.get(env.host_id);
        if (!hs) {
          hs = { host: env.host, host_id: env.host_id, primary_ip: "",
                 online: env.online, last_seen: env.last_seen, firing_count: env.firing_count,
                 tags: {}, maintenance: false };
          state.hosts.set(env.host_id, hs);
        } else {
          hs.online      = env.online;
          hs.last_seen   = env.last_seen;
          hs.firing_count = env.firing_count;
        }
        rerenderHeatmap();
        break;
      }

      case "hello": {
        let h = state.hosts.get(env.host_id);
        if (!h) {
          h = { host: env.host, host_id: env.host_id, primary_ip: env.primary_ip || "",
                online: true, last_seen: env.started_at, firing_count: 0,
                tags: env.tags || {}, maintenance: !!env.maintenance };
          state.hosts.set(env.host_id, h);
        } else {
          if (env.primary_ip)   h.primary_ip  = env.primary_ip;
          if (env.tags)         h.tags        = env.tags;
          if (env.host)         h.host        = env.host;
          if ("maintenance" in env) h.maintenance = !!env.maintenance;
        }
        rerenderHeatmap();
        break;
      }
    }
  }

  function pushHistory(env) {
    state.history.unshift(env);
    if (state.history.length > HISTORY_CAP) state.history.length = HISTORY_CAP;
  }

  async function fetchHistory() {
    try {
      const r = await fetch("/api/alerts?limit=2000");
      if (!r.ok) return;
      state.history = await r.json();
      rerenderHistory();
    } catch {}
  }

  // ============================================================
  // DOM helpers
  // ============================================================
  const $ = (s) => document.querySelector(s);

  function setStatus(cls, text) {
    $("#conn-dot").className = "dot " + cls;
    $("#conn-text").textContent = text;
  }

  function esc(s) {
    if (s == null) return "";
    return String(s).replaceAll("&","&amp;").replaceAll("<","&lt;")
      .replaceAll(">","&gt;").replaceAll('"',"&quot;");
  }

  function fmtTime(ts) {
    if (!ts) return "";
    return new Date(ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false });
  }
  function fmtFull(ts) {
    if (!ts) return "";
    return new Date(ts).toISOString().replace('T',' ').replace(/\.\d+Z$/,' UTC');
  }
  function fmtAgo(ts) {
    if (!ts) return "";
    const sec = Math.max(0, (Date.now() - new Date(ts).getTime()) / 1000);
    if (sec < 1)    return "just now";
    if (sec < 60)   return Math.floor(sec) + "s ago";
    if (sec < 3600) return Math.floor(sec/60) + "m ago";
    if (sec < 86400) return Math.floor(sec/3600) + "h ago";
    return Math.floor(sec/86400) + "d ago";
  }
  function hostInfo(host_id) { return state.hosts.get(host_id) || null; }

  /** Resolve the best IP for an alert: prefer the stamped field on the alert
   *  (available for all new alerts since v4.1), fallback to the live host map. */
  function alertIp(a) {
    if (a.primary_ip) return a.primary_ip;
    const h = hostInfo(a.host_id);
    return (h && h.primary_ip) ? h.primary_ip : "";
  }

  // ============================================================
  // Re-render all views
  // ============================================================
  function rerenderAll() { rerenderAlerts(); rerenderHistory(); rerenderHeatmap(); }

  // ============================================================
  // Active Alerts
  // ============================================================
  function passesAlert(a) {
    if (state.filterText) {
      const t = state.filterText.toLowerCase();
      const h = hostInfo(a.host_id);
      const ip = h ? (h.primary_ip || "") : "";
      if (!`${a.host} ${ip} ${a.title} ${a.message} ${a.fingerprint} ${a.category}`.toLowerCase().includes(t)) return false;
    }
    if (a.severity === "critical" && !state.showCritical) return false;
    if (a.severity === "warning"  && !state.showWarning)  return false;
    if (a.severity === "info"     && !state.showInfo)     return false;
    return true;
  }
  const sevRank = { critical: 0, warning: 1, info: 2 };
  function sortAlerts(a, b) {
    const sr = (sevRank[a.severity]||2) - (sevRank[b.severity]||2);
    if (sr) return sr;
    return new Date(b.first_seen||b.ts) - new Date(a.first_seen||a.ts);
  }

  function rerenderAlerts() {
    const list  = $("#alerts-list");
    const empty = $("#alerts-empty");
    const visible = [...state.alerts.values()].filter(passesAlert).sort(sortAlerts);
    list.innerHTML = "";
    visible.forEach(a => list.appendChild(renderAlert(a)));
    $("#alert-count").textContent = visible.length;
    empty.style.display = visible.length === 0 ? "flex" : "none";
  }

  function renderAlert(a) {
    const h  = hostInfo(a.host_id);
    const ip = alertIp(a);
    const div = document.createElement("div");
    const expanded = state.expanded.has(a.fingerprint);
    div.className = `alert ${a.severity}${expanded ? " expanded" : ""}`;

    div.innerHTML = `
      <div class="stripe"></div>
      <div class="alert-inner">
        <div class="when">
          <div class="ago">${fmtAgo(a.first_seen || a.ts)}</div>
          <div title="${esc(fmtFull(a.first_seen || a.ts))}">${fmtTime(a.first_seen || a.ts)}</div>
          <div class="cat">${esc(a.category)}</div>
        </div>
        <div class="body">
          <div class="title">${esc(a.title)}</div>
          <div class="msg">${esc(a.message)}</div>
          <div class="host-row">
            <span class="host-name">${esc(a.host)}</span>
            ${ip ? `<span class="host-ip">${esc(ip)}</span>` : ""}
            ${a.device ? `<span class="device">· ${esc(a.device)}</span>` : ""}
          </div>
        </div>
        <div class="right">
          <span class="sev-badge sev-${a.severity}">${a.severity}</span>
          ${a.occurrences > 1 ? `<div class="occ">×${a.occurrences}</div>` : ""}
          <button class="resolve-btn" data-fp="${esc(a.fingerprint)}">Resolve</button>
        </div>
      </div>
    `;

    if (expanded) {
      const det = document.createElement("div");
      det.className = "detail";
      det.appendChild(renderAlertDetailInline(a, h, ip));
      div.appendChild(det);
    }

    div.querySelector(".resolve-btn").onclick = (ev) => {
      ev.stopPropagation();
      resolveAlert(a.fingerprint, ev.currentTarget);
    };
    div.onclick = (ev) => {
      if (ev.target.tagName === "BUTTON" || ev.target.tagName === "A") return;
      if (state.expanded.has(a.fingerprint)) state.expanded.delete(a.fingerprint);
      else state.expanded.add(a.fingerprint);
      rerenderAlerts();
    };
    return div;
  }

  function renderAlertDetailInline(a, h, ip) {  // ip already resolved by caller
    const tags = h && h.tags ? Object.entries(h.tags).map(([k,v]) => `${k}=${v}`).join(", ") : "";
    const inner = document.createElement("div");
    inner.className = "detail-inner";
    inner.innerHTML = `
      <dl>
        <dt>Host</dt>           <dd><strong>${esc(a.host)}</strong>${ip ? ` <span style="color:var(--muted);font-family:monospace;font-size:12px;">(${esc(ip)})</span>` : ""}</dd>
        <dt>Category</dt>       <dd>${esc(a.category)}${a.subsystem ? " / " + esc(a.subsystem) : ""}</dd>
        <dt>What fired</dt>     <dd>${esc(a.metric)}${a.device ? " on <code>" + esc(a.device) + "</code>" : ""}</dd>
        ${a.value ? `<dt>Reading</dt><dd>${esc(a.value)}${a.threshold ? ` <span style="color:var(--muted);">(threshold ${esc(a.threshold)})</span>` : ""}</dd>` : ""}
        <dt>First seen</dt>     <dd>${esc(fmtFull(a.first_seen || a.ts))} <span style="color:var(--muted);">(${fmtAgo(a.first_seen || a.ts)})</span></dd>
        <dt>Occurrences</dt>    <dd>${a.occurrences || 1}</dd>
        ${tags ? `<dt>Host tags</dt><dd>${esc(tags)}</dd>` : ""}
        <dt>Explanation</dt>    <dd>${esc(a.message)}</dd>
      </dl>
    `;
    return inner;
  }

  async function resolveAlert(fingerprint, btn) {
    if (btn) { btn.disabled = true; btn.textContent = "Resolving…"; }
    try {
      const r = await fetch("/api/resolve", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ fingerprint, by: "" }),
      });
      if (!r.ok) {
        if (btn) { btn.disabled = false; btn.textContent = "Resolve"; }
        alert(`Resolve failed (${r.status}): ${await r.text()}`);
      }
    } catch(e) {
      if (btn) { btn.disabled = false; btn.textContent = "Resolve"; }
      alert(`Resolve failed: ${e}`);
    }
  }

  // ============================================================
  // History
  // ============================================================
  function passesHistory(a) {
    if (a.state === "firing"   && !state.histShowFiring)   return false;
    if (a.state === "resolved" && !state.histShowResolved) return false;
    if (state.histFilter) {
      const t = state.histFilter.toLowerCase();
      const h = hostInfo(a.host_id);
      const ip = h ? (h.primary_ip || "") : "";
      if (!`${a.host} ${ip} ${a.title} ${a.message} ${a.category} ${a.severity} ${a.fingerprint}`.toLowerCase().includes(t)) return false;
    }
    return true;
  }

  function rerenderHistory() {
    const rows  = $("#history-rows");
    const empty = $("#history-empty");
    const visible = state.history.filter(passesHistory);
    $("#history-count").textContent = `${visible.length} event${visible.length === 1 ? "" : "s"}`;
    rows.innerHTML = "";
    const LIMIT = 1000;
    for (let i = 0; i < Math.min(visible.length, LIMIT); i++) {
      rows.appendChild(renderHistoryRow(visible[i]));
    }
    if (visible.length > LIMIT) {
      const more = document.createElement("div");
      more.className = "hist-row";
      more.innerHTML = `<div class="muted small" style="grid-column:1/-1;text-align:center;padding:8px 0;">+ ${visible.length - LIMIT} more — refine filter to see all</div>`;
      rows.appendChild(more);
    }
    empty.style.display = visible.length === 0 ? "flex" : "none";
  }

  function renderHistoryRow(a) {
    const h  = hostInfo(a.host_id);
    const ip = alertIp(a);
    const full = fmtFull(a.ts);
    const div = document.createElement("div");
    div.className = `hist-row ${a.severity}${a.state === "resolved" ? " resolved" : ""}`;
    div.innerHTML = `
      <div class="h-time" title="${esc(full)}">
        ${esc(full.slice(11,19))}
        <span class="h-date">${esc(full.slice(0,10))}</span>
      </div>
      <div><span class="h-sev">${a.severity}</span></div>
      <div class="h-state">${a.state}</div>
      <div class="h-host">
        ${esc(a.host)}
        ${ip ? `<span class="h-ip">${esc(ip)}</span>` : ""}
      </div>
      <div class="h-cat">${esc(a.category)}</div>
      <div class="h-title" title="${esc(a.title)}">${esc(a.title)}</div>
    `;
    div.onclick = () => openDetailModal(a);
    return div;
  }

  // ============================================================
  // Alert Detail Modal — FORMATTED (not raw JSON)
  // ============================================================
  function openDetailModal(a) {
    const h  = hostInfo(a.host_id);
    const ip = alertIp(a);
    const tags = h && h.tags ? Object.entries(h.tags).map(([k,v]) => `${k}=${v}`).join(", ") : "";

    // Severity badge
    const badge = $("#modal-sev-badge");
    badge.className = `sev-badge sev-${a.severity}`;
    badge.textContent = a.severity.toUpperCase();

    $("#modal-title").textContent = a.title || "Alert detail";
    $("#modal-subtitle").textContent = `${a.host || ""}${ip ? "  ·  " + ip : ""}  ·  ${a.category || ""}`;

    const body = $("#modal-body");
    body.innerHTML = `
      <div class="modal-detail">

        <div class="detail-section">
          <div class="detail-section-title">Host</div>
          <dl class="detail-grid">
            <dt>Hostname</dt>     <dd class="detail-value-highlight">${esc(a.host || "—")}</dd>
            <dt>IP Address</dt>   <dd><span class="detail-fingerprint" style="font-size:12.5px;">${esc(ip || "—")}</span></dd>
            ${tags ? `<dt>Tags</dt><dd>${esc(tags)}</dd>` : ""}
            ${(h && h.maintenance) ? `<dt>Status</dt><dd><span style="color:var(--info);font-weight:600;">In maintenance</span></dd>` : ""}
          </dl>
        </div>

        <div class="detail-section">
          <div class="detail-section-title">Alert Details</div>
          <dl class="detail-grid">
            <dt>Category</dt>     <dd>${esc(a.category || "—")}${a.subsystem ? " <span style='color:var(--muted)'>/ " + esc(a.subsystem) + "</span>" : ""}</dd>
            <dt>What fired</dt>   <dd><strong>${esc(a.metric || "—")}</strong>${a.device ? " on <code>" + esc(a.device) + "</code>" : ""}</dd>
            ${a.value ? `<dt>Reading</dt><dd>${esc(a.value)}${a.threshold ? ` <span style="color:var(--muted)">(threshold ${esc(a.threshold)})</span>` : ""}</dd>` : ""}
            <dt>Explanation</dt>  <dd style="line-height:1.5;">${esc(a.message || "—")}</dd>
          </dl>
        </div>

        <div class="detail-section">
          <div class="detail-section-title">Timeline</div>
          <dl class="detail-grid">
            <dt>First seen</dt>   <dd>${esc(fmtFull(a.first_seen || a.ts))} <span style="color:var(--muted)">(${fmtAgo(a.first_seen || a.ts)})</span></dd>
            <dt>Last seen</dt>    <dd>${esc(fmtFull(a.last_seen || a.ts))} <span style="color:var(--muted)">(${fmtAgo(a.last_seen || a.ts)})</span></dd>
            <dt>State</dt>        <dd><span style="color:${a.state === 'resolved' ? 'var(--resolved)' : 'var(--critical)'};font-weight:600;">${esc(a.state || "firing")}</span></dd>
            <dt>Occurrences</dt>  <dd>${a.occurrences || 1}</dd>
          </dl>
        </div>

        <div class="detail-section">
          <div class="detail-section-title">Internal</div>
          <dl class="detail-grid">
            <dt>Fingerprint</dt>  <dd><span class="detail-fingerprint">${esc(a.fingerprint || "—")}</span></dd>
            ${a.host_id ? `<dt>Host ID</dt><dd><span class="detail-fingerprint">${esc(a.host_id)}</span></dd>` : ""}
          </dl>
        </div>

      </div>
    `;

    $("#modal").classList.remove("hidden");
  }

  function closeModal() { $("#modal").classList.add("hidden"); }

  // ============================================================
  // Connectivity (Heatmap)
  // ============================================================
  function classifyHost(h) {
    const ageS = (Date.now() - new Date(h.last_seen).getTime()) / 1000;
    if (ageS > 30)        return { cls: "offline",  label: "Offline"  };
    if (h.firing_count>0) return { cls: "critical", label: "Alerting" };
    if (ageS > 10)        return { cls: "stale",    label: "Stale"    };
    if (h.maintenance)    return { cls: "maintenance", label: "Maintenance" };
    return                       { cls: "online",   label: "Healthy"  };
  }

  // Tooltip element
  const tooltip = $("#host-tooltip");
  let tooltipTarget = null;

  function showTooltip(h, c, ev) {
    if (!tooltip) return;
    // Show the actual heartbeat timestamp (stable), not a live countdown.
    // The countdown fluctuates every second during rerenderHeatmap; timestamp
    // only changes when a real heartbeat arrives.
    const lastSeenTs = h.last_seen
      ? new Date(h.last_seen).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false })
      : "—";
    const stateColor = c.cls === 'online' ? 'var(--resolved)' :
                       c.cls === 'critical' ? 'var(--critical)' :
                       c.cls === 'stale'    ? 'var(--warning)'  :
                       c.cls === 'maintenance' ? 'var(--info)'  : 'var(--muted)';
    tooltip.innerHTML = `
      <div class="tooltip-hostname">${esc(h.host || "Unknown")}</div>
      <div class="tooltip-row">
        <span class="tooltip-label">IP Address</span>
        <span class="tooltip-value mono">${esc(h.primary_ip || "—")}</span>
      </div>
      <div class="tooltip-row">
        <span class="tooltip-label">Last heartbeat</span>
        <span class="tooltip-value mono">${esc(lastSeenTs)}</span>
      </div>
      <div class="tooltip-row">
        <span class="tooltip-label">State</span>
        <span class="tooltip-value" style="color:${stateColor};font-weight:600;">${esc(c.label)}</span>
      </div>
      <div class="tooltip-row">
        <span class="tooltip-label">Active alerts</span>
        <span class="tooltip-value">${h.firing_count || 0}</span>
      </div>
      ${h.maintenance ? `<div class="tooltip-row"><span class="tooltip-label">Maintenance</span><span class="tooltip-value" style="color:var(--info);">Active</span></div>` : ""}
      <div class="tooltip-click-hint">Click to filter alerts for this host</div>
    `;
    tooltip.classList.remove("hidden");
    positionTooltip(ev);
  }
  function hideTooltip() {
    if (tooltip) tooltip.classList.add("hidden");
    tooltipTarget = null;
  }
  function positionTooltip(ev) {
    if (!tooltip || tooltip.classList.contains("hidden")) return;
    const margin  = 14;
    const tw = tooltip.offsetWidth  || 240;
    const th = tooltip.offsetHeight || 180;
    let x = ev.clientX + margin;
    let y = ev.clientY + margin;
    if (x + tw > window.innerWidth)  x = ev.clientX - tw - margin;
    if (y + th > window.innerHeight) y = ev.clientY - th - margin;
    tooltip.style.left = x + "px";
    tooltip.style.top  = y + "px";
  }
  document.addEventListener("mousemove", (ev) => {
    if (!tooltip.classList.contains("hidden")) positionTooltip(ev);
  });

  function rerenderHeatmap() {
    const grid  = $("#heatmap-grid");
    const all = [...state.hosts.values()]
      .filter(h => {
        if (!state.heatmapFilter) return true;
        const t = state.heatmapFilter.toLowerCase();
        return (h.host + " " + (h.primary_ip||"")).toLowerCase().includes(t);
      })
      .sort((a,b) => a.host.localeCompare(b.host));

    grid.innerHTML = "";
    const counts = { online:0, stale:0, offline:0, critical:0 };
    let online = 0, total = state.hosts.size;

    for (const h of all) {
      const c = classifyHost(h);
      counts[c.cls] = (counts[c.cls]||0) + 1;
      if (c.cls === "online") online++;

      const cell = document.createElement("div");
      cell.className = `cell-host ${c.cls}`;
      const initials = (h.host || "?").slice(0, 2).toUpperCase();
      cell.innerHTML = initials + (h.firing_count > 0 ? `<span class="num">${h.firing_count}</span>` : "");

      cell.addEventListener("mouseenter", (ev) => { tooltipTarget = h; showTooltip(h, c, ev); });
      cell.addEventListener("mouseleave", hideTooltip);

      cell.onclick = () => {
        state.filterText = h.host;
        $("#filter-text").value = h.host;
        switchTab("alerts");
        rerenderAlerts();
      };
      grid.appendChild(cell);
    }

    $("#online-count").textContent = `${online}/${total}`;
    drawDonut(total, counts);
  }

  function drawDonut(total, counts) {
    const svg = $("#donut");
    svg.innerHTML = "";
    $("#donut-total").textContent = total;

    const cx = 21, cy = 21, r = 15.9;
    const style = getComputedStyle(document.documentElement);
    const colors = {
      online:   style.getPropertyValue("--resolved").trim()  || "#16a34a",
      stale:    style.getPropertyValue("--warning").trim()   || "#d97706",
      offline:  style.getPropertyValue("--offline").trim()   || "#9ca3af",
      critical: style.getPropertyValue("--critical").trim()  || "#dc2626",
    };

    const bg = document.createElementNS("http://www.w3.org/2000/svg","circle");
    bg.setAttribute("cx",cx); bg.setAttribute("cy",cy); bg.setAttribute("r",r);
    bg.setAttribute("fill","none"); bg.setAttribute("stroke","var(--line-soft)"); bg.setAttribute("stroke-width","5");
    svg.appendChild(bg);

    if (total === 0) { renderLegend(counts); return; }

    let offset = 25;
    for (const k of ["critical","offline","stale","online"]) {
      const v = counts[k]||0; if (!v) continue;
      const pct = (v/total)*100;
      const seg = document.createElementNS("http://www.w3.org/2000/svg","circle");
      seg.setAttribute("cx",cx); seg.setAttribute("cy",cy); seg.setAttribute("r",r);
      seg.setAttribute("fill","none"); seg.setAttribute("stroke",colors[k]); seg.setAttribute("stroke-width","5");
      seg.setAttribute("stroke-dasharray",`${pct} ${100-pct}`);
      seg.setAttribute("stroke-dashoffset",offset);
      seg.setAttribute("transform",`rotate(-90 ${cx} ${cy})`);
      svg.appendChild(seg);
      offset = (offset-pct+100)%100;
    }
    renderLegend(counts);
  }

  function renderLegend(counts) {
    const lg = $("#legend");
    lg.innerHTML = [
      ["critical","Alerting","var(--critical)"],
      ["offline", "Offline", "var(--offline)"],
      ["stale",   "Stale",   "var(--warning)"],
      ["online",  "Healthy", "var(--resolved)"],
    ].map(([k,label,color]) => `
      <div class="lg-row">
        <div class="lg-name">
          <span class="lg-swatch" style="background:${color}"></span>
          <span>${label}</span>
        </div>
        <div class="lg-count">${counts[k]||0}</div>
      </div>
    `).join("");
  }

  // ============================================================
  // Settings — Config Editor
  // ============================================================
  let configOriginal = "";

  async function loadConfig() {
    const area = $("#config-editor-area");
    const actions = $("#config-editor-actions");
    if (!area) return;

    try {
      const r = await fetch("/api/config");
      if (r.status === 404) {
        area.innerHTML = `<div class="settings-info-banner" style="margin:14px 18px;">
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="12" y1="8" x2="12" y2="12"/><line x1="12" y1="16" x2="12.01" y2="16"/></svg>
          Config editor API not available on this collector version. Edit <code>/etc/monitor-collector/config.toml</code> directly on the server.
        </div>`;
        return;
      }
      if (!r.ok) throw new Error(`HTTP ${r.status}`);
      configOriginal = await r.text();
      area.innerHTML = `<div style="padding:4px 18px 4px;"><textarea id="config-textarea" spellcheck="false">${esc(configOriginal)}</textarea></div>`;
      if (actions) actions.classList.remove("hidden");
    } catch(e) {
      area.innerHTML = `<div class="settings-warn-banner" style="margin:14px 18px;">
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>
        Could not load config: ${esc(e.message)}
      </div>`;
    }
  }

  async function saveConfig() {
    const ta = $("#config-textarea");
    const status = $("#config-save-status");
    if (!ta) return;
    const content = ta.value;
    try {
      if (status) { status.textContent = "Saving…"; status.style.color = "var(--muted)"; }
      const r = await fetch("/api/config", {
        method: "POST",
        headers: { "Content-Type": "text/plain" },
        body: content,
      });
      if (!r.ok) throw new Error(`HTTP ${r.status}: ${await r.text()}`);
      configOriginal = content;
      if (status) { status.textContent = "Saved ✓ (restart collector to apply)"; status.style.color = "var(--resolved)"; }
    } catch(e) {
      if (status) { status.textContent = `Error: ${e.message}`; status.style.color = "var(--critical)"; }
    }
  }

  // ============================================================
  // Settings — Webhooks list
  // ============================================================
  async function loadWebhooks() {
    const list = $("#webhooks-list");
    if (!list) return;
    try {
      const r = await fetch("/api/webhooks");
      if (r.status === 404) {
        list.innerHTML = `<div class="muted small" style="padding:8px 0;">Webhook status API not available on this version.</div>`;
        return;
      }
      if (!r.ok) throw new Error(`HTTP ${r.status}`);
      const webhooks = await r.json();
      if (!webhooks.length) {
        list.innerHTML = `<div class="muted small" style="padding:8px 0;">No webhooks configured. Add them in the config editor below.</div>`;
        return;
      }
      list.innerHTML = webhooks.map(w => `
        <div class="webhook-item">
          <span class="webhook-kind ${w.kind === 'slack' ? 'slack' : w.kind.includes('chat') ? 'gchat' : 'generic'}">${esc(w.kind)}</span>
          <span class="webhook-name">${esc(w.name || "Unnamed")}</span>
          <span class="webhook-sev">min: ${esc(w.sev_min || "info")}</span>
          <span class="webhook-status ${w.active !== false ? 'active' : 'inactive'}">${w.active !== false ? "Active" : "Inactive"}</span>
        </div>
      `).join("");
    } catch(e) {
      list.innerHTML = `<div class="muted small" style="padding:8px 0;">Could not load webhooks: ${esc(e.message)}</div>`;
    }
  }

  // ============================================================
  // Tab switching
  // ============================================================
  function switchTab(name) {
    state.activeTab = name;
    document.querySelectorAll(".tab").forEach(t => t.classList.toggle("active", t.dataset.tab === name));
    document.querySelectorAll(".view").forEach(v => v.classList.toggle("active", v.id === "view-" + name));
    if (name === "heatmap")  rerenderHeatmap();
    if (name === "history")  rerenderHistory();
    if (name === "alerts")   rerenderAlerts();
    if (name === "settings") { loadConfig(); loadWebhooks(); }
  }

  // ============================================================
  // Password-protected history clear
  // ============================================================
  function promptClearHistory() {
    const stored = getClearPassword();
    const countEl = $("#history-count");
    const count = state.history.length;
    if (count === 0) return;

    if (!stored) {
      // No password set — confirm with a plain dialog
      if (!confirm(`Permanently delete ${count} historical alert(s)? Set a password in Settings to require one.`)) return;
      doClearHistory();
      return;
    }

    // Show password modal
    const msgEl = $("#pwd-modal-msg");
    if (msgEl) msgEl.textContent = `Enter the password to permanently wipe ${count} alert record(s). This cannot be undone.`;
    const input = $("#pwd-modal-input");
    if (input) input.value = "";
    const errEl = $("#pwd-modal-error");
    if (errEl) errEl.classList.add("hidden");
    $("#pwd-modal").classList.remove("hidden");
    if (input) setTimeout(() => input.focus(), 80);
  }

  async function doClearHistory() {
    const btn = $("#clear-history");
    if (btn) { btn.disabled = true; }
    try {
      const r = await fetch("/api/history/clear", { method: "POST" });
      if (!r.ok) alert(`Clear failed (${r.status}): ${await r.text()}`);
    } catch(e) {
      alert(`Clear failed: ${e}`);
    } finally {
      if (btn) btn.disabled = false;
    }
  }

  // ============================================================
  // Wire up all event handlers
  // ============================================================
  document.querySelectorAll(".tab").forEach(t => t.onclick = () => switchTab(t.dataset.tab));

  // Alerts filters
  $("#filter-text").oninput     = (e) => { state.filterText  = e.target.value; rerenderAlerts(); };
  $("#filter-critical").onchange = (e) => { state.showCritical = e.target.checked; rerenderAlerts(); };
  $("#filter-warning").onchange  = (e) => { state.showWarning  = e.target.checked; rerenderAlerts(); };
  $("#filter-info").onchange     = (e) => { state.showInfo     = e.target.checked; rerenderAlerts(); };

  // History filters
  $("#history-filter").oninput  = (e) => { state.histFilter       = e.target.value;   rerenderHistory(); };
  $("#hist-firing").onchange    = (e) => { state.histShowFiring    = e.target.checked; rerenderHistory(); };
  $("#hist-resolved").onchange  = (e) => { state.histShowResolved = e.target.checked; rerenderHistory(); };

  // History clear (password-protected)
  $("#clear-history").onclick = promptClearHistory;

  // Heatmap filter
  $("#heatmap-filter").oninput = (e) => { state.heatmapFilter = e.target.value; rerenderHeatmap(); };

  // Modal close
  $("#modal-close").onclick = closeModal;
  $("#modal").onclick = (e) => { if (e.target.id === "modal") closeModal(); };

  // Password modal
  $("#pwd-modal-close").onclick  = () => { $("#pwd-modal").classList.add("hidden"); };
  $("#pwd-modal-cancel").onclick = () => { $("#pwd-modal").classList.add("hidden"); };
  $("#pwd-modal-confirm").onclick = () => {
    const input = $("#pwd-modal-input");
    const errEl = $("#pwd-modal-error");
    const entered = input ? input.value : "";
    const stored  = getClearPassword();
    if (entered !== stored) {
      if (errEl) errEl.classList.remove("hidden");
      if (input) { input.value = ""; input.focus(); }
      return;
    }
    $("#pwd-modal").classList.add("hidden");
    doClearHistory();
  };
  const pwdInput = $("#pwd-modal-input");
  if (pwdInput) pwdInput.onkeydown = (e) => { if (e.key === "Enter") $("#pwd-modal-confirm").click(); };

  // Settings — theme buttons
  $("#theme-light").onclick = () => applyTheme("light");
  $("#theme-dark").onclick  = () => applyTheme("dark");

  // History clear password — no UI to change it; initialized from DEFAULT_PWD.
  // To change: open browser console and run:
  //   localStorage.setItem('uma-clear-pwd', 'yourNewPassword')

  // Settings — notifications
  const npEnable = $("#np-enable");
  if (npEnable) {
    npEnable.onchange = (e) => setNotifyEnabled(e.target.checked);
    $("#np-sound").onchange    = (e) => { notify.sound    = e.target.checked; saveNotifySettings(); };
    $("#np-severity").onchange = (e) => { notify.severity = e.target.value;   saveNotifySettings(); };
    $("#np-test").onclick = async () => {
      if (!notify.enabled) await setNotifyEnabled(true);
      if (Notification.permission !== "granted") return;
      showDesktopNotification({
        type: "alert", state: "firing", severity: "critical",
        fingerprint: "uma|test|" + Date.now(), title: "UMA test alert",
        host: "test-host", host_id: "x",
        message: "If you can see and hear this, desktop notifications are working.",
      });
    };
  }

  // Settings — config editor
  const configReload = $("#config-reload");
  if (configReload) configReload.onclick = loadConfig;
  const configSave = $("#config-save");
  if (configSave) configSave.onclick = saveConfig;

  // Global keyboard shortcuts
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      closeModal();
      $("#pwd-modal").classList.add("hidden");
    }
  });

  // ============================================================
  // Live refresh intervals
  // ============================================================
  setInterval(rerenderHeatmap, 1000);  // freshness colors stay live
  setInterval(rerenderAlerts,  5000);  // "Xs ago" stays fresh

  // ============================================================
  // Boot
  // ============================================================
  initTheme();
  initClearPassword();   // set default clear-history password on first load
  loadNotifySettings();
  syncNotifyUI();
  connect();
})();
