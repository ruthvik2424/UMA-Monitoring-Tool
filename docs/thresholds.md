# Default thresholds and the rationale behind each

Every threshold lives in `agent/config.example.toml`. These defaults are tuned for "alert with time to act, don't cry wolf".

## NVMe (`modules.nvme`)

| Setting              | Default | Why                                                                    |
| -------------------- | ------- | ---------------------------------------------------------------------- |
| `wear_warn_pct`      | 80      | Vendors typically rate endurance for 100% wear; 80% gives months notice |
| `wear_critical_pct`  | 90      | Plan a swap window now                                                 |
| `temp_warn_c`        | 70      | NVMe drives throttle around 75–80 °C                                    |
| `temp_critical_c`    | 80      | Imminent throttling and reduced lifetime                                |

Plus: any `critical_warning` bit set is critical regardless of value; any non-zero `media_errors` is a warning; any new `num_err_log_entries` since last poll is a warning.

## Disk SMART (`modules.disk_smart`)

Polled every 5 min (smartctl is heavy). Critical on:
- `smart_status.passed = false`
- ATA attribute `when_failed != "-"` (drive's own self-assessment)

Warning on first non-zero observation of:
- `5`  reallocated_sector_ct
- `197` current_pending_sector
- `198` offline_uncorrectable
- `187` reported_uncorrect

## Memory ECC (`modules.memory_ecc`)

| Setting              | Default | Why                                                                    |
| -------------------- | ------- | ---------------------------------------------------------------------- |
| `ce_per_hour_warn`   | 50      | Sustained correctable rate is the leading indicator of UE              |

Any UE → critical, immediately, both via EDAC sysfs delta and kmsg.

## Inlet temperature (`modules.thermal`)

| Setting              | Default | Why                                                                    |
| -------------------- | ------- | ---------------------------------------------------------------------- |
| `inlet_warn_c`       | 32.0    | Target operating range upper edge for most modern DCs                  |
| `inlet_critical_c`   | 36.0    | Vendor warning ranges typically begin here                              |
| `inlet_emergency_c`  | 38.0    | Below the ~42 °C HPE/Dell forced-shutdown — gives operator headroom    |

Plus rate-of-rise calculation; if `(42 - current) / rate_C_per_min` < 5 min, the alert message says so explicitly.

## NIC (`modules.network_nic`)

| Setting              | Default | Why                                                                    |
| -------------------- | ------- | ---------------------------------------------------------------------- |
| `flap_window_s`      | 10      | Cover the user's "up/down within 2-3 s" pattern with margin            |
| `flap_threshold`     | 2       | Two transitions inside 10 s = flapping; one is just a maintenance event |
| `counter_poll_s`     | 5       | Cheap; via /proc/net/dev + ethtool                                     |

Any new CRC error → warning. Any new uncorrectable FEC error → critical.

## Memory pressure & OOM (`modules.mempressure`)

| Setting                  | Default | Why                                                                    |
| ------------------------ | ------- | ---------------------------------------------------------------------- |
| `psi_some_avg10_warn`    | 50.0    | At 50% `some`, half of the time is spent stalled on memory             |
| `sustain_s`              | 30      | Avoids alerting on transient bursts                                    |
| `oom_storm_count`        | 3       | One OOM is bad; three in 5 min means thrashing                         |
| `oom_storm_window_s`     | 300     | Five minutes is a useful operator-action window                        |

## PCIe AER (`modules.pcie_aer`)

| Setting                  | Default | Why                                                                    |
| ------------------------ | ------- | ---------------------------------------------------------------------- |
| `corrected_per_min_warn` | 100     | Sustained corrected errors precede uncorrected by hours-to-days         |

Any uncorrectable AER → critical immediately.

## Storage controller (`modules.storage_controller`)

Poll every 15 s. Any disappearance of a previously-enumerated controller, or a status flip away from "OK", → critical with the slot number and the list of disks that were behind it.

## Lifecycle (`modules.lifecycle`)

Always emits when systemd-logind fires `PrepareForShutdown(true)`. Severity is `warning` (a planned reboot isn't fatal, but you should know about it).
