# Pre-shutdown / pre-reboot detection

## Mechanism

`modules/lifecycle.rs` connects to the system D-Bus and:

1. **Takes an inhibitor lock** on `org.freedesktop.login1.Manager.Inhibit("shutdown", "monitor-agent", "...", "delay")`. logind will pause shutdown for up to `InhibitDelayMaxSec` (default ~5 s) once we hold this lock.
2. **Subscribes to the `PrepareForShutdown(bool start)` D-Bus signal**.
3. When the signal fires with `start=true`, builds a `host_rebooting` (or `host_shutting_down`) alert and sends it on the open WSS — through the *critical-bypass* mpsc lane.
4. Releases the inhibitor (by dropping the OwnedFd), allowing logind to proceed.

```
User runs `reboot`
  ↓ (microseconds)
systemd-logind processes the request
  ↓ logind sees our delay-inhibitor; holds shutdown
  ↓ logind emits PrepareForShutdown(true)
  ↓ (~50 µs D-Bus delivery)
agent lifecycle task receives signal
  ↓ (~30 µs build alert)
agent transport task pops critical lane
  ↓ (~20 µs serialize + write + flush)
TCP segment leaves NIC
  ↓ (< 1 ms LAN)
Collector receives, applies, broadcasts
  ↓ (~5 µs broadcast)
GUI WebSocket onmessage
  ↓ (~1 ms render)

  Total: ~1–3 ms from `reboot` keystroke to alert in the GUI.
```

The user sees the alert **before** their own SSH session disconnects.

## Coverage matrix

| User action                          | Pre-warning?     | Detected via                                          |
| ------------------------------------ | ---------------- | ----------------------------------------------------- |
| `reboot`                             | YES (~3 ms)      | logind `PrepareForShutdown`                           |
| `shutdown -h now` / `poweroff`       | YES (~3 ms)      | logind `PrepareForShutdown`                           |
| `shutdown -r +5`                     | YES (5 min ahead)| `/run/systemd/shutdown/scheduled` + logind            |
| `systemctl reboot`                   | YES (~3 ms)      | logind `PrepareForShutdown`                           |
| `systemctl reboot --force`           | YES (~3 ms)      | logind `PrepareForShutdown` (still goes through)      |
| `kexec -e`                           | NO               | Bypasses logind. Detected post-fact: heatmap red ≤10 s|
| `halt -f`                            | NO               | Bypasses logind. Detected post-fact                   |
| `echo b > /proc/sysrq-trigger`       | NO               | Kernel halts immediately. Detected post-fact          |
| Kernel panic                         | NO (post-fact)   | Heatmap red ≤10 s + `host_unclean_reboot` next start  |
| Power loss                           | NO (post-fact)   | Heatmap red ≤10 s + `host_unclean_reboot` next start  |
| Hardware-forced shutdown (BMC over-temp) | YES (sometimes) | `inlet_temp_emergency` alert fires ~minutes prior     |

## On agent restart

`modules/boot.rs` reads `/proc/sys/kernel/random/boot_id`, compares with the value previously persisted at `/var/lib/monitor-agent/last-boot`. If they differ:

- If `/proc/sys/kernel/tainted` has bit 0x80 (machine check), 0x4000 (lockup) or any other diagnostic bit set → emits `host_unclean_reboot` warning with the most likely cause named.
- Otherwise → emits the same alert with "no taint bits — possibly clean reboot we missed, or a hard power event" so the operator at least knows the host went down.

## Caveats

- The inhibitor mechanism requires `systemd-logind` to be running and reachable on the system D-Bus, which is the default on every modern Ubuntu. Containerized "agent inside a container" is **not** a supported deployment.
- `InhibitDelayMaxSec` can be lowered by the system admin via `/etc/systemd/logind.conf`. If it's set to 0 the agent still gets the signal but has no guaranteed window — the alert may or may not make it out depending on scheduler luck. We recommend leaving it at the default (5 s).
- Multiple inhibitors of the same name from different processes do not collide; logind aggregates them.
