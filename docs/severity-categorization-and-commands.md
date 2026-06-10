# Severity categorization & agent commands reference

This document describes how **severity** (`info` | `warning` | `critical`) is assigned across the **`monitor-agent`**, what you **can customize without code**, and **which commands or kernel interfaces** produce the underlying signals (with illustrative examples).

**Schema:** Alerts are JSON with `severity`, `category`, `metric`, `title`, `message`, etc.; see [`alert-schema.md`](./alert-schema.md).  
**Default numeric knobs:** [`thresholds.md`](./thresholds.md).

---

## 1. Severity model (three levels)

Defined in **`shared`** as an ordered enum (**`Info` < `Warning` < `Critical`**). Anything that consumes alerts (GUI, Slack webhooks, automation) may filter on this ordering.

| Level | Operational meaning (UMA convention) |
|-------|--------------------------------------|
| **info** | State that is worth recording but rarely needs immediate paging (e.g. NIC link-up journal rule, NVIDIA GPU riding at power limit, some BMC SEL deltas). |
| **warning** | Degraded capacity or early warning — act soon (PSI memory pressure sustained, inlet above warn tier, PCIe *corrected* AER storm, NIC link down / flap, SMART attr non-zero without FAILING_NOW, CPU CMCI-only / APEI summary lines, orchestrated reboot/shutdown heads-up). |
| **critical** | Service-impacting or imminent failure — page (NVMe critical-warning / high wear thresholds, ECC UE, uncorrectable AER/MCE-class signals, controller lockup, disk vanished, FEC uncorrected, OOM kill, hung/soft/hard lockup, inlet critical/emergency tiers, GPU UE / high temp ≥85 °C where coded). |

**Note:** Threshold **numbers** live in **`/etc/monitor-agent/config.toml`**. Severity for a given scenario is implemented in Rust per **`metric`**; changing a threshold may change *whether* something fires but not always *which tier* (e.g. SMART “overall FAILED” stays critical).

---

## 2. Categories and metrics

Every alert has:

- **`category`** — broad bucket (`nvme`, `network_nic`, `thermal`, `lifecycle`, …; see **`shared/src/lib.rs`** → `uma_shared::cat`).
- **`metric`** — specific detector (`link_down`, `critical_warning`, `uncorrectable`, …).

**Fingerprint** (dedup key) for agent-built alerts follows  
`host_id | category | device_or_- | metric` (excluding `severity`), so recurrence bumps **`occurrences`** instead of spawning endless duplicate cards.

---

## 3. Can you customize severity per requirement?

Yes — **partially**, depending on signal path:

### 3.1 No agent code change — **recommended**

| Mechanism | What you control |
|-----------|-------------------|
| **`agent/config.toml` (`[modules.*]`)** | Numeric poll intervals and thresholds (NVMe wear/temp, thermal inlet tiers, PSI, OOM storm counts, PCIe corrected rate/min, flap window/threshold, memory CE rate warning, BMC poll interval…). Often changes **frequency** / **presence** of alerts; some metrics always map to a fixed tier (see §4). |
| **`[[modules.syslog_rules.rules]]`** | **`severity`** per rule (`"info"` \| `"warning"` \| `"critical"`). Use for **journal-driven** alerts only for those rules (not for NVMe poll, netlink NIC, etc.). |
| **`deploy.py` → per-host `config_append`** | Append arbitrary TOML (e.g. extra **`[[modules.syslog_rules.rules]]`** or override **`[modules.thermal]`**). |
| **Collector webhook `severity_min`** | Filters **delivery** (Slack/GChat/post); **does not change** alert severity stored in UMA/GUI. |
| **`notify_on_resolved` / `states`** (collector webhooks)** | Controls whether **resolved** transitions are mirrored to chat — not severity. |

### 3.2 Requires Rust code change + rebuild

Anything where severity is hard-coded in a module (examples):

- **`lifecycle`** → always **`warning`** for planned reboot/shutdown notifications.
- **NVIDIA** → uncorrect ECC / temp **`≥ 85°C`** (**`critical`**), throttle bitmap (**`warning`**), ~98% of power limit (**`info`**).
- **Various kmsg parsers** → “uncorrected” vs “corrected” paths map to fixed tiers.
- **`main.rs`** lifecycle of **agent SIGTERM**: **`severity::Info`** for “agent stopping”.

If your policy wants different tiers (e.g. lifecycle → info), patch the **`AlertBuilder::new(..., Severity::*)`** for that **`metric`** and rebuild the agent binary.

---

## 4. Module → typical severities → source & example commands

**Legend:**

- **Kmsg** — async read **`/dev/kmsg`** inside the agent (not a CLI you run manually).
- **Journal** — long-running **`journalctl -f`** child process (exact args below).

### 4.1 Core event streams

| Mechanism | How the agent obtains data |
|-----------|-------------------------------|
| **Kernel ring buffer | Open **`/dev/kmsg`**, seek to tail, parse lines (**`modules::kmsg`**). |
| **systemd journal | **`journalctl -f --no-pager -o cat --since now`** — stdin/stdout spawned by agent (**`journal.rs`**). Probe: **`journalctl --version`**. |
| **DBus (system)** | **`zbus`** to **`org.freedesktop.login1`** (lifecycle), systemd manager (**`ceph-osd@*`** watches in **`storage_io`**), etc. |

Example — journal follower (effective command line):

```bash
journalctl -f --no-pager -o cat --since now
```

Example line downstream (hypothetical):

```text
ens1f0: Link DOWN
```

---

### 4.2 By module (severity summary + CLI / sysfs)

The table lists **principal** externally visible commands or paths. Internal netlink/socket code has no standalone shell equivalent.

| Module (`category`) | Typical severities & metrics | Commands / interfaces | Example invocation | Illustrative output / fields used |
|---------------------|-----------------------------|------------------------|---------------------|----------------------------------|
| **nvme** | **Critical**: `critical_warning`, high wear (`wear_critical_pct`), some media paths; **Warning**: wear warn, temp warn, incremental error log deltas | **`nvme smart-log <dev> -o json`**; model from **`/sys/block/<disk>/device/model`** | `sudo nvme smart-log /dev/nvme0 -o json` | JSON keys: **`critical_warning`**, **`percentage_used`**, **`media_errors`**, **`composite_temperature`** (Kelvin in spec; code converts where needed). |
| **disk_smart** | **Critical**: `smart_overall`, critical failing attrs; **Warning**: realloc/pending/offline UNC attrs | **`smartctl -a -j <dev>`**; enumerate **`/sys/block`** `sd*` / `hd*` | `sudo smartctl -a -j /dev/sda` | JSON: **`smart_status.passed`**, ATA **`attributes.table`** (**`when_failed`**, **`raw.value`** for attrs 5, 187, 197, 198). |
| **memory_ecc** | **Critical**: `uncorrectable_ecc`, `uncorrectable_ecc_kmsg` (explicit UE markers); **Warning**: **`ce_rate_high`** | sysfs **`/sys/devices/system/edac/mc/mc*/{ce_count,ue_count}`**; patterns on **kmsg** | `cat /sys/devices/system/edac/mc/mc0/ce_count` | Counter deltas drive CE/hour heuristic; UE increment → critical. |
| **cpu_mce** | **Warning**: CMCI / `|CE|`, vague APEI summary; **Critical**: otherwise **kmsg** | **Kmsg** | *(kernel log sample)* `[Hardware Error]: CPU:0 ... MC255_STATUS[...|CE|...]` | Classification in **`cpu_mce.rs`** maps CE-only vs UE vs summaries. |
| **pcie_aer** | **Critical**: **`uncorrectable`**; **Warning**: **`corrected_rate_high`** (≥ **`corrected_per_min_warn`** per rolling minute) | **Kmsg**; enrich with **`lspci -s <BDF> -vmm`** | `lspci -s 0000:3d:00.0 -vmm` | kmsg strings like **`AER: ... Uncorrected`** vs **`Corrected`**; **`lspci`** provides human-readable **Device**/vendor strings. |
| **thermal** | **Warning**: CPU throttle deltas, inlet **warn** tier; **Critical**: inlet **critical** / **emergency** tiers | sysfs **`thermal_throttle/core_throttle_count`** globs; **`ipmitool sdr type Temperature`** | `sudo ipmitool sdr type Temperature` | Parses rows ending in **`NN degrees C`**, prefers chassis **inlet ambient** over PSU inlet (see **`thermal.rs`**). Thresholds **`inlet_warn_c`/`inlet_critical_c`/`inlet_emergency_c`**. |
| **network_nic** | **Warning**: `link_down`, `link_flapping`, **`crc_errors`**; **Critical**: **`fec_uncorrected`** incremental | Linux **rtnetlink**; **`cat /proc/net/dev`**; **`ethtool -S <iface>`** | `sudo ethtool -S eth0` | Counters **`rx_crc_errors`**, **`fec_uncorrectable_blocks`**, etc. (vendor-dependent names normalized in code). |
| **storage_controller** | **Critical**: lockup/disappear/unhealthy (**`controller_lockup`**, kmsg helpers) | **Kmsg**; **`ssacli ctrl all show detail`** (HPE); **`perccli64 /call show`** variants (Dell) | Vendor-specific — e.g. HPE **`ssacli`**, Dell **`perccli64`** — see **`storage_controller.rs`** | Parses slot/status/disk attachment lists; disappearance vs last poll ⇒ lockup semantics. |
| **storage_io** | **Critical**: missing disk / osd down / I/O errors (paths combined) | **`/proc/partitions`**, **udev**, **systemd D-Bus** unit state, correlated **kmsg** | *(no single universal CLI)* — e.g. **`systemctl status ceph-osd@12`** on host during incident | Alerts reference unit names + correlation window **`correlate_window_s`**. |
| **mempressure** | **Warning**: PSI sustained above **`psi_some_avg10_warn`**; **Critical**: OOM victim lines, **`oom_storm`** | **`cat /proc/pressure/memory`**; **kmsg** OOM pattern | Sample: `grep . /proc/pressure/memory` | Typical line: **`some avg10=...`** used with **`sustain_s`**. |
| **oshang** | **Warning** / **Critical** depending on pattern (hung task vs lockup severity) | **Kmsg** | — | Regex on **`hung_task`**, **`soft lockup`**, **`hard lockup`**, **`BUG:`** patterns. |
| **gpu_nvidia** | **Critical**: `ecc_uncorrected`, **`temperature`** (≥ coded temp); **Warning**: **`throttling`**; **Info**: **`power_at_limit`** | **`nvidia-smi --query-gpu=... --format=csv,noheader,nounits`** | `nvidia-smi --query-gpu=index,name,temperature.gpu,ecc.errors.uncorrected.volatile.total --format=csv,noheader,nounits` | One CSV row per GPU; thresholds in **`gpu_nvidia.rs`** (temp **≥ 85°C** → critical). |
| **gpu_amd** | **Critical**/ **Warning** for bad thermal / ECC-style conditions per module logic | **`rocm-smi`**; sysfs **`/sys/class/drm/card*/device/hwmon*/temp1_input`** | `rocm-smi --showtemp` *(ROCm-dependent)* | Fallback **hwmon** temps when **`rocm-smi`** unavailable. Full **NVIDIA** query fields match **`agent/src/modules/gpu_nvidia.rs`** (`nvidia-smi --query-gpu=...` CSV). |
| **bmc_redfish** | Mapped from Redfish **`Health`/state** (**`Critical`**/**`Warning`**) + fan stalled heuristic | **`reqwest`** HTTPS to **`redfish_url`** (not a shell CLI) | Diagnostic (operator): **`curl -k -u user https://bmc/redfish/v1/Chassis`** | Responses JSON — fans, PSU, temps; agent normalizes OEM quirks. |
| **bmc_eventlog** | **`Info`**/`Warning`/`Critical` per OEM severity strings when parsing SEL / event log rows | **`ipmitool`** (local SEL fragments) plus Redfish where implemented | Example: **`sudo ipmitool sel list`** during triage | Table-style SEL text ingested periodically; dedup deltas. |
| **syslog_rules** | Exactly what you put in **`severity`** column per rule (**`modules.syslog_rules`**) | **Journal** tail | — | Applies regex to **`line.text`**; optional NIC ignore for **`category = "network_nic"`** aligns with **`ignore_regex`**. |
| **boot** | **Warning** typical for unclean reboot / taint heuristic | **`/proc/sys/kernel/random/boot_id`**, **`/proc/sys/kernel/tainted`**; persisted state file under **`/var/lib/monitor-agent`** | `cat /proc/sys/kernel/random/boot_id` | Compare to previous boot to infer unclean transitions. |
| **lifecycle** | **Warning** (planned reboot/shutdown) **fixed** | **DBus** `PrepareForShutdown` + inhibitor; fallback **`/run/systemd/shutdown/scheduled`** | `cat /run/systemd/shutdown/scheduled` *(when shutdown scheduled)* | File contains **`MODE=reboot`** / **`MODE=poweroff`** style hints. |

**Agent housekeeping (SIGTERM handler in `main.rs`):**

- Sends **`severity: info`** for **`metric: agent_stopping`** once per graceful exit sequence.

---

## 5. Maintenance mode and metric validation

### 5.1 Suppress alerts during maintenance

Outbound **`Alert`** envelopes are **not** sent to the collector when any of these is true:

- **`[agent] maintenance = true`** in `config.toml`, or  
- **`log_level = "maintenance"`** or **`log_level = "maintainance"`** (accepted typo) — tracing still runs at **`info`**.

**Heartbeats** and **Hello** continue. The Hello includes **`maintenance: true`** so the GUI can show state, and the collector **drops** alerts for hosts known to be in maintenance (defense in depth).

### 5.2 Validate thresholds and probe output (JSON)

- **One-shot CLI** (prints JSON and exits):  
  **`monitor-agent --debug-snapshot`** (uses **`UMA_CONFIG`** / **`--config`**).  
- **HTTP** (when enabled): **`[agent] debug_listen = "127.0.0.1:19100"`** then  
  **`curl -s http://127.0.0.1:19100/v1/debug/snapshot`**

The JSON includes **`modules_effective`** (passwords redacted), **`syslog_rules`**, **`network_nic_ignore_regex`**, and **`probes`** — each probe has **`what`** (human summary), **`argv`**, **`exit_code`**, and truncated **`stdout`** / **`stderr`**.  
The agent **refuses** `debug_listen` on non-loopback addresses.

---

## 6. Quick customization checklist

1. **`/etc/monitor-agent/config.toml`**: Tune **`[modules.*]`** thresholds and **`[[modules.syslog_rules.rules]]`** / **`severity`**.  
2. **Fleet-wide deploy**: regenerate config via **`deploy.py`** or Ansible; optional **`agents[].config_append`** for rare per-host deltas. **`deploy-hosts.yaml`** also supports top-level **`maintenance`** and **`agent_debug_listen`** (see example file).  
3. **Outbound chat noise**: **`collector.toml`** webhook **`severity_min`** + **`states`** (+ **`notify_on_resolved`**), not agent severity rewriting.  
4. **Tier policy change** where Rust hard-codes **`Severity::*`**: patch module + **`cargo build --release -p uma-agent`**.

---

## 7. Related files

| File | Role |
|------|------|
| `shared/src/lib.rs` | **`Severity`** enum ordering, **`Alert`**, **`cat::*`**. |
| `agent/src/config.rs` | All **`default_*`** struct defaults (mirrored in **`agent/config.example.toml`**). |
| `collector/src/webhooks.rs` | **`severity_min`**, **`states`**, **`notify_on_resolved`**. |

For **GUI** grouping and history, **`category`** + **`severity`** are both indexed; filters are entirely client/UI policy.

