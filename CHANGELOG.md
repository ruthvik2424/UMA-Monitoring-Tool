# UMA Changelog

All notable changes to the UMA monitoring system will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [4.3.0] - Agent Hardening: Universal Timeouts + File Logging

### Added

#### Agent Log Files
Two structured log files now written by the agent (in addition to systemd journal):

- **`/var/log/monitor-agent/error.log`** — WARN + ERROR only. 7-day retention.
  For on-call triage: every failure, timeout, permission error, and config problem
  without polling noise. Survives restarts — each restart appends a separator line.

- **`/var/log/monitor-agent/execution.log`** — INFO + above. 3-day retention.
  Full operational trace: agent startup, module activation, config values loaded,
  alert fired events, connectivity events. Format:
  ```
  2026-06-08 10:44:27.114 IST  INFO  uma_agent: agent starting host=server-01
  2026-06-08 10:44:27.116 IST  INFO  nvme: Module starting (poll=60s)
  2026-06-08 10:44:31.505 IST  INFO  uma_agent::bus: ALERT FIRED severity=critical category=nvme metric=media_errors host=server-01
  2026-06-08 10:44:32.001 IST  ERROR disk_smart: smartctl timed out (30s) on /dev/sdc — drive may be unresponsive
  ```

- **Log rotation**: logrotate config deployed to `/etc/logrotate.d/monitor-agent`
  handles daily rotation with configurable retain counts (3 and 7 days respectively).
  Agent also purges old files on startup.

- **Alert events in execution log**: every `ALERT FIRED` call from any module is
  now logged at INFO with severity, category, metric, host, device, state, and title.
  Config-level maintenance suppression is also logged.

#### Universal Subprocess Timeouts
All 10 modules that spawn external commands now have hard wall-clock timeouts.
Previously only `smartctl` and `nvme` had timeouts; a hung vendor tool (e.g. a
stalled HPE ssacli, frozen ethtool firmware, unresponsive ipmitool) could block
a module task indefinitely.

| Module | Command | Timeout |
|--------|---------|---------|
| `disk_smart` | smartctl | 30s |
| `nvme` | nvme smart-log | 20s |
| `thermal` | ipmitool sdr | 15s |
| `bmc_eventlog` | ipmitool sel | 15s |
| `memory_ecc` | ipmitool sdr (IPMI path) | 15s (via spawn_blocking) |
| `storage_controller` | ssacli, perccli64 | 20s each |
| `network_nic` | ethtool -S | 10s |
| `gpu_nvidia` | nvidia-smi | 15s |
| `gpu_amd` | rocm-smi | 15s |
| `pcie_aer` | lspci -vmm | 15s |

All timeout constants are defined in `priv_cmd.rs` and can be tuned per-deployment.

#### `priv_cmd.rs` — Universal Subprocess API
New `run()`, `run_priv()`, and `run_cmd()` helpers replace scattered
`Command::new().output().await` calls. Every module now uses the same pattern:
- `sudo -n` prefix when not running as root (for disk tools)
- Hard timeout with `kill_on_drop(true)` cleanup
- Structured warning log on timeout naming the command and wall time

#### `disk_smart` — Ghost device filtering
`enumerate()` now sorts `/sys/block/sd*` alphabetically and skips devices where
`/dev/<name>` does not exist. Prevents wasted smartctl attempts on detached
iSCSI volumes, disappeared HBAs, or QEMU stub devices.

### Changed
- `MissedTickBehavior::Delay` set on disk_smart poll loop (was default Burst)
- Systemd unit: added `/var/log/monitor-agent` to `ReadWritePaths`
- deploy.py: creates `/var/log/monitor-agent`, deploys logrotate config

---

## [4.2.0] - EDAC Removal / BMC-Safe ECC Monitoring

### Changed (Breaking Default Behavior)

- **`memory_ecc` module: EDAC sysfs polling now DISABLED by default.**
  HPE (ProLiant/iLO), Dell (iDRAC), and Supermicro explicitly flag the Linux `edac_core`
  kernel module as incompatible with their BMC firmware. Loading EDAC while the BMC is active
  causes both sides to contend over the same memory controller registers, leading to:
  missed ECC events, duplicate events in iLO IML / iDRAC SEL, or incorrect DIMM identification.

- **Removed `edac-utils` from recommended apt packages** in README and Ansible roles.
  Installing `edac-utils` automatically loads `edac_core` on boot — exactly what vendors say
  not to do on managed servers.

### Added

- **IPMI SDR memory sensor polling** (`ipmi_sdr_enable = true`, default ON):
  New primary ECC detection path for BMC-equipped servers. Runs
  `ipmitool sdr type Memory` on a configurable interval and parses CE/UE sensor counts
  directly from the BMC's own side-channel. No kernel module loaded; no register conflict.
  Automatically disabled if `/dev/ipmi0` is absent or `ipmitool` is not installed.

### ECC Coverage Matrix (post-v4.2.0)

| Signal | Source | Module | BMC-safe |
|--------|--------|--------|----------|
| DIMM correctable/uncorrectable error count | BMC IPMI SDR | `memory_ecc` (ipmi_sdr_enable) | ✅ Yes |
| MCE / [Hardware Error] kernel events | `/dev/kmsg` MCA | `cpu_mce` | ✅ Yes |
| BMC event log (IML/SEL) ECC entries | Redfish + ipmitool sel | `bmc_eventlog` | ✅ Yes |
| DIMM health status | Redfish `/Systems/1/Memory` | `bmc_redfish` | ✅ Yes |
| EDAC sysfs (`/sys/devices/system/edac/mc/`) | EDAC kernel driver | `memory_ecc` (edac_sysfs_enable, OFF by default) | ⚠️ Conflicts with BMC |

### Config

```toml
[modules.memory_ecc]
enabled = true
poll_s = 30
ce_per_hour_warn = 50
ipmi_sdr_enable = true       # default: on — BMC side-channel, safe with iLO/iDRAC
edac_sysfs_enable = false    # default: off — only enable on hosts WITHOUT a BMC
```

---

## [4.1.0] - Bug Fixes & Polish

### Fixed

- **Dark mode — black + grey theme**: Removed all blue tints from dark mode. Accent color changed from `#3b82f6` (blue) to `#9ca3af` (grey). Active tab, primary buttons, focus rings, and badges now use neutral grey tones. Severity alert colors (critical red, warning amber, info, resolved green) remain distinct for readability.

- **IP addresses now visible everywhere**: `primary_ip` was only stored on `HostState` and not included in persisted alert records, so history showed no IPs for offline/reconnected hosts.
  - Added `primary_ip` field (with `#[serde(default)]`) to the `Alert` struct in `shared/lib.rs`
  - Collector now stamps the IP from its `HostInfo` cache onto every incoming alert in `ws_ingest.rs` before broadcast and SQLite storage
  - GUI uses a two-stage fallback: `a.primary_ip` (always available for new alerts) then `state.hosts` map (for in-session live events)
  - Added `hostname -I` as a second fallback for IP detection in `agent/src/host.rs` when `ip -4 -j addr show` fails or returns no suitable interface

- **Connectivity tooltip last-seen no longer fluctuates**: Tooltip previously showed a live relative timestamp ("80ms ago", "1.1s ago") recalculated every second during the heatmap re-render loop. Now shows the fixed HH:MM:SS time of the last received heartbeat, which only changes when a real heartbeat arrives.

- **History clear password is now hidden**: Removed the "History Clear Password" field from the Settings page UI. The password is initialized automatically on first load and is only changeable via the browser console (`localStorage.setItem('uma-clear-pwd', 'new')`). No longer visible or settable by dashboard viewers.

---

## [4.0.0] - Collector UI Upgrade

### Added — GUI (monitor-collector)

- **Modern UI Overhaul**: Complete redesign of the web interface
  - Inter font (via Google Fonts) for clean, professional typography
  - Lucide-style inline SVG icons throughout the interface
  - Smooth CSS transitions and hover states
  - Refined color system with improved light and dark themes
  - `v4` version badge in the topbar

- **Settings Tab**: New dedicated settings page consolidating all configuration
  - *General*: Light/Dark theme switcher with visual toggle buttons
  - *History Clear Password*: Set a password required to wipe alert history (protects against accidental or unauthorized wipes)
  - *Desktop Notifications*: Toggle, sound, severity filter, and test button
  - *Webhook Integrations*: Live status view of configured Slack/Google Chat/generic webhooks (via `/api/webhooks`)
  - *Collector Config Editor*: Edit `config.toml` directly from the browser (via `/api/config` GET/POST), with Reload and Save buttons

- **Connectivity Tab — Rich Host Tooltips**: Hovering over any host cell now shows a floating tooltip with:
  - Full hostname, IP address
  - Last-seen latency (live updating)
  - Current state (Healthy / Alerting / Stale / Offline / Maintenance) with color coding
  - Active alert count
  - Click-to-filter hint

- **History Tab — Formatted Alert Detail Modal**: Clicking an alert no longer shows raw JSON; instead shows a clean structured panel with sections:
  - *Host*: hostname, IP, tags, maintenance status
  - *Alert Details*: category, what fired, device, reading vs threshold, full explanation
  - *Timeline*: first seen, last seen, state, occurrence count
  - *Internal*: fingerprint, host ID

- **History Clear — Password Protection**: The "Clear history" button now prompts for a password (configured in Settings → General). Without a password set, falls back to a plain confirm dialog.

### Added — Collector Backend (new API endpoints)

- `GET /api/config` — Returns live `config.toml` content as plain text for the in-browser editor
- `POST /api/config` — Overwrites `config.toml` with the submitted body (restart required to apply)
- `GET /api/webhooks` — Returns a summary list of configured webhooks (name, kind, severity, active flag) for the Settings UI

### Changed

- `GuiState` struct now includes `config_path: PathBuf` to support the config editor API
- Notification panel and theme toggle removed from topbar; moved entirely into the Settings tab

---

## [3.0.0] - Current Release

### Added
- **Maintenance Mode**: Agent can suppress alerts while keeping heartbeats active
  - `[agent] maintenance = true` in config.toml
  - Alternative: `log_level = "maintenance"` (or typo `maintainance`)
  - Collector respects maintenance flag from agent Hello messages
  - GUI shows maintenance badge for hosts in maintenance mode

- **Debug HTTP Endpoint**: Metrics export functionality (similar to node_exporter)
  - `GET /v1/debug/snapshot` returns JSON with:
    - Effective module thresholds and syslog rules
    - List of probe commands with exit codes and output
    - Current monitoring parameter values and states
  - Configurable via `agent.debug_listen = "127.0.0.1:19100"`
  - Localhost-only binding for security
  - Timeout protection for command execution

- **Enhanced Configuration Validation**
  - Better error messages for invalid collector URLs
  - Improved TOML parsing with detailed diagnostics

### Changed
- **Agent Hello Protocol**: Added `maintenance` field to Hello messages
- **Collector State**: Tracks per-host maintenance status
- **Alert Processing**: Alerts from maintenance hosts are logged but not processed

### Technical Details
- Added `debug_http.rs` module with comprehensive system introspection
- Extended `AgentConfig` with `maintenance_mode()` helper method  
- Enhanced `collector/state.rs` to track maintenance flags
- Added maintenance filtering in `ws_ingest.rs`

---

## [2.0.0] - Notification & Integration Release

### Added
- **Desktop Notifications**: Browser-based real-time alerting
  - Native browser notification API integration
  - Severity-based icons (⚠️ critical, ⚡ warning, ℹ️ info)
  - Click-to-focus functionality
  - Automatic deduplication by alert fingerprint
  - Critical alerts require user interaction to dismiss

- **Webhook Integrations**: Multi-platform alert delivery
  - **Slack Integration**: Rich formatting with severity colors and action buttons
  - **Google Chat Integration**: Card-based messages with host details
  - **Generic JSON Webhooks**: Customizable HTTP POST endpoints
  - Per-webhook configuration:
    - Severity filtering (`info`, `warning`, `critical`)
    - Rate limiting (configurable alerts per minute)
    - Delivery state filtering (`firing`, `resolved`)
    - Custom retry logic with exponential backoff

- **BMC Fixes and Enhancements**
  - Improved BMC event log parsing
  - Enhanced Redfish API integration
  - Better handling of vendor-specific BMC implementations
  - Added support for Dell iDRAC and HPE iLO remote syslog

### Changed
- **GUI Notification System**: Complete overhaul of client-side alerting
  - Added notification permission request flow
  - Configurable notification preferences
  - Sound alerts for critical events
  - Notification test functionality

- **Collector Webhook System**: New outbound fanout architecture
  - Broadcast channel for efficient multi-destination delivery
  - Per-webhook async tasks to prevent backpressure
  - HTTP client pooling and connection reuse

### Technical Details
- Added `webhooks.rs` module with comprehensive delivery system
- Enhanced `gui/app.js` with notification management
- Extended collector configuration schema for webhook definitions
- Added webhook health monitoring and error reporting

---

## [1.0.0] - Initial Release

### Added
- **Core Monitoring System**: Complete bare-metal hardware monitoring
  - **Agent Architecture**: Rust-based monitoring daemon (~5MB static binary)
  - **Collector Architecture**: Central aggregation and web interface
  - **mTLS Transport**: Secure WebSocket communication with mutual TLS

- **17 Monitoring Modules**: Comprehensive hardware fault detection
  - `nvme`: NVMe SSD health, wear leveling, temperature monitoring
  - `disk_smart`: SMART attribute monitoring for traditional drives  
  - `memory_ecc`: ECC error detection and reporting
  - `cpu_mce`: Machine Check Exception monitoring
  - `pcie_aer`: PCIe Advanced Error Reporting
  - `thermal`: Temperature monitoring with rate-of-rise detection
  - `network_nic`: Network interface health and error tracking
  - `storage_controller`: HPE/Dell/LSI controller monitoring
  - `storage_io`: Disk disappearance and Ceph OSD correlation
  - `mempressure`: Memory pressure and OOM detection
  - `oshang`: OS hang and soft/hard lockup detection
  - `gpu_nvidia`: NVIDIA GPU health via NVML
  - `gpu_amd`: AMD GPU monitoring via ROCm
  - `bmc_redfish`: BMC health via Redfish API
  - `bmc_eventlog`: BMC event log monitoring
  - `syslog_rules`: Custom journal-driven rule engine
  - `boot`: Unclean reboot detection
  - `lifecycle`: Pre-shutdown/reboot alerting via systemd

- **Web GUI**: Real-time monitoring interface
  - Live alert dashboard with expand/collapse details
  - Historical alert timeline with filtering
  - Connectivity heatmap with donut chart visualization
  - Host grid with color-coded status indicators
  - Alert resolution and history management

- **Production Deployment**: Enterprise-ready deployment system
  - **Ansible Playbooks**: Complete infrastructure automation
  - **Certificate Management**: Automated mTLS PKI generation
  - **Systemd Integration**: Hardened service configurations
  - **Multi-host Support**: Fleet-scale deployment capabilities

- **Ultra-Low Latency Design**: <5ms p99 kernel-to-GUI latency
  - Critical alert bypass lane to prevent queue blocking
  - TCP_NODELAY optimization for immediate delivery
  - Kernel message (`/dev/kmsg`) monitoring for instant detection
  - Pre-shutdown alerts via systemd D-Bus integration

### Technical Foundation
- **Rust Ecosystem**: Built on Tokio async runtime
- **Transport Layer**: WebSocket over TLS with rustls
- **Data Storage**: SQLite ring buffer with 30-day retention
- **Configuration**: TOML-based with hierarchical overrides
- **Cross-compilation**: Static musl binaries for maximum compatibility
- **Security**: mTLS mutual authentication with CA management

### Deployment Options
- **Ansible**: Full automation with role-based deployment
- **Manual**: Step-by-step installation guide
- **Python Deploy Script**: Single-file deployment alternative

---

## Version Numbering System

Starting with v4.0.0, UMA follows semantic versioning:

- **Major version** (X.0.0): Breaking changes, major feature additions
- **Minor version** (X.Y.0): New features, backward compatible
- **Patch version** (X.Y.Z): Bug fixes, security updates

### Binary Versioning
- Binaries are tagged with full version: `uma-agent-v4.0.0`, `uma-collector-v4.0.0`
- Artifacts stored in `artifacts/vX.Y.Z/` directories
- Deploy script supports version selection: `./deploy.py build --version v4.0.0`

### Release Branches
- `main`: Latest stable release
- `develop`: Development branch for next version
- `v1.x`, `v2.x`, `v3.x`: Historical version branches for reference
- Tags: `v1.0.0`, `v2.0.0`, `v3.0.0` mark specific releases