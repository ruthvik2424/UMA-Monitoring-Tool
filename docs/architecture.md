# UMA architecture

## Goals

1. **Zero perceptible latency** between a hardware event and the operator seeing it. On a < 1 ms LAN, target end-to-end is **5 ms p99**.
2. **No fluff in the GUI** — alerts only, plus a connectivity heatmap. No graphs, no inventory, no dashboards, no asset DB.
3. **Survive the failures we report** — the agent must outlive memory pressure, OOM kills, and one CPU's worth of soft lockup long enough to actually emit the alert about them.
4. **Scale to 500+ hosts** without becoming a heavyweight pipeline. One static binary per host, one static binary in the middle, one HTML page on the side.

## Process model

Each agent is a single multithreaded Tokio runtime with these tasks:

- One **`/dev/kmsg` reader** that broadcasts parsed records to anyone subscribed.
- One **transport** task that owns the WSS connection and drains two mpsc channels (normal + critical) into it. The critical channel is drained with biased select-priority so a flooded normal queue cannot block a fatal-event alert.
- N **module tasks**, each a self-contained loop (poller or kmsg-tailer) that builds `AlertBuilder`s, passes them through a per-module dedup tracker, and pushes accepted alerts onto the bus.
- One **heartbeat** task (every 5 s by default).
- One **D-Bus lifecycle** task holding a logind delay-inhibitor and listening for `PrepareForShutdown`.
- One **signal handler** task: on SIGTERM/SIGINT, push a final `agent_stopping` alert, sleep ~250 ms for drain, exit.

Every channel is bounded; on overflow we drop the oldest stale entry rather than the newest signal. The transport's send buffer is pre-allocated so the critical hot path never allocates.

## Wire protocol

Single envelope, JSON. See [`alert-schema.md`](alert-schema.md). Every WSS frame is one of `alert`, `heartbeat`, `hello`, `snapshot`, `host_status`. The same struct is shared between agent, collector, and GUI via the `uma-shared` crate, so backward-compatibility is enforced by the type system.

## Latency budget

Target: kernel-event to GUI-render ≤ 5 ms p99 on the LAN.

| Step                               | Cost          | Notes                                          |
| ---------------------------------- | ------------- | ---------------------------------------------- |
| `/dev/kmsg` read                   | ~10 µs        | nonblocking, AsyncFd                           |
| regex match + `AlertBuilder`       | ~30 µs        | regex pre-compiled                             |
| dedup tracker insert               | ~5 µs         | parking_lot mutex + HashMap                    |
| serde_json::to_writer              | ~30 µs        | tiny payload                                   |
| WSS frame write + flush            | ~20 µs        | TCP_NODELAY, no Nagle                          |
| LAN one-way                        | < 1 ms        | per the user's spec                            |
| Collector deserialize + apply      | ~50 µs        | RwLock writes only on state change             |
| `broadcast::send` to GUI clients   | ~5 µs/client  | O(1) per subscriber                            |
| Browser WebSocket → onmessage      | ~1 ms         | depends on browser                             |
| `JSON.parse` + DOM `appendChild`   | ~1 ms         | virtual list keeps DOM bounded                 |
| **Total**                          | **~3–5 ms**   | dominated by LAN + browser                     |

## Why Rust (not C)

For 500+ hosts and twelve different vendor surfaces (smartctl JSON, nvme-cli JSON, ipmitool, sysfs, /dev/kmsg, rtnetlink, udev, NVML, ROCm, ssacli, perccli, Redfish), a memory-safe statically-typed language with first-class async I/O is a hard requirement to keep the agent reliable. Rust matches C's runtime cost, links to a single static musl binary (~5 MB stripped), has no GC pause to threaten the latency budget, and fails-fast on parser surprises rather than corrupting state.

If a *single* module crashes its task, the runtime supervisor logs and restarts it without taking the agent down — the supervisor lives outside the module's scope.

## Deduplication

Two layers:

1. **Per-module on the agent**: `AlertTracker` (parking_lot mutex over `HashMap<fingerprint, Entry>`). On a repeat detection, we increment a counter. We re-emit only after `re_notify_after_s` (default 30 min). On `recovery_window_s` of OK readings we emit a `resolved` event with the same fingerprint.
2. **On the collector**: same fingerprint→Alert map. If a freshly-restarted agent re-fires an existing alert, the collector recognizes the fingerprint and only broadcasts a state change. This is the safety net.

## Survivability

- `OOMScoreAdjust=-1000` (systemd unit + agent writes /proc/self/oom_score_adj at startup as belt-and-braces).
- `MemoryMin=64M` (cgroup guarantee).
- `Restart=always`, `RestartSec=1`, `WatchdogSec=10`.
- Pre-allocated 64 KB send buffer.
- Critical-bypass mpsc lane.
- SIGTERM-handled best-effort flush.

Combined, this means even on a host that is being OOM-killed the agent stays alive long enough to send the `oom_kill` and (if it escalates) `oom_storm` alerts.
