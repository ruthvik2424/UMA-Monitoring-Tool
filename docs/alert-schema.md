# Alert JSON schema

Every WebSocket frame is a JSON object with a `type` discriminator. There are five envelope types.

## `alert`

The canonical event record. `state=firing` for new/re-notified, `state=resolved` when the underlying condition has cleared for `recovery_window_s`.

```json
{
  "type": "alert",
  "id": "01HXM5T6...",                    // ULID, generated on the agent
  "ts": "2026-05-13T04:26:00.123456Z",    // RFC3339, microsecond precision
  "host": "node-42.dc1.internal",
  "host_id": "uuid-from-/etc/machine-id",
  "category": "nvme",                     // see shared/src/lib.rs::cat::*
  "subsystem": "smart",                   // optional sub-bucket
  "severity": "critical",                 // info | warning | critical
  "state": "firing",                      // firing | resolved
  "device": "/dev/nvme0n1",               // optional
  "device_model": "Samsung MZ1L21T9HCLS", // optional
  "metric": "critical_warning",
  "value": "0x04",
  "threshold": "0x00",
  "title": "NVMe drive reporting reliability degradation",
  "message": "NVMe drive /dev/nvme0n1 raised critical_warning 0x04 (reliability-degraded). Wear 78%. 12 media errors. Replace within next maintenance window.",
  "fingerprint": "host_id|nvme|/dev/nvme0n1|critical_warning",
  "first_seen": "2026-05-13T04:26:00.123456Z",
  "last_seen":  "2026-05-13T04:26:00.123456Z",
  "occurrences": 1,
  "raw": { "critical_warning": 4, "percentage_used": 78 },
  "correlated": []
}
```

### Severity policy

Canonical description: **[`docs/severity-categorization-and-commands.md`](./severity-categorization-and-commands.md)** (tier meanings, customization, and per-module CLI examples).

Summary:

- **info** — informational state changes (agent stopping, host_started). Hidden by default.
- **warning** — degraded but functioning, or pre-event warnings (PSI memory pressure, NIC flap, inlet 32 °C).
- **critical** — fault that affects service or precedes failure (controller lockup, ECC UE, MCE, OOM kill, soft/hard lockup, inlet 36 °C+).

### Fingerprint policy

`host_id | category | device_or_- | metric`. The same logical fault ALWAYS produces the same fingerprint, even across agent restarts and across differing alert IDs/timestamps.

## `heartbeat`

Drives the connectivity heatmap.

```json
{
  "type": "heartbeat",
  "host": "node-42.dc1.internal",
  "host_id": "...",
  "ts": "...",
  "uptime_s": 1234567,
  "firing_count": 3,
  "agent_version": "0.1.0"
}
```

## `hello`

Sent once per connection right after the WSS handshake. Carries vendor + tags so the GUI can group hosts.

```json
{
  "type": "hello",
  "host": "node-42.dc1.internal",
  "host_id": "...",
  "agent_version": "0.1.0",
  "started_at": "...",
  "vendor": {
    "system_manufacturer": "HPE",
    "system_product_name": "ProLiant DL380 Gen10",
    "bios_version": "U30 v2.86 (10/05/2024)",
    "bmc_vendor": "hpe",
    "gpu_vendors": ["nvidia"],
    "kernel": "5.15.0-100-generic",
    "os_release": "Ubuntu 22.04.4 LTS"
  },
  "tags": { "dc": "dc1", "rack": "R12", "role": "ceph-osd" }
}
```

## `snapshot`

Sent by the collector to a freshly-connected GUI client.

```json
{
  "type": "snapshot",
  "generated_at": "...",
  "alerts": [ /* every currently-firing alert */ ],
  "hosts":  [ /* every known host with online/last_seen/firing_count */ ]
}
```

## `host_status`

Sent when the collector decides a host crossed the online ↔ offline boundary.

```json
{
  "type": "host_status",
  "host": "node-42.dc1.internal",
  "host_id": "...",
  "online": false,
  "last_seen": "...",
  "firing_count": 3
}
```
