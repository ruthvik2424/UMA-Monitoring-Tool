# UMA — Ultimate Monitoring Agent

Lightweight Linux baremetal monitoring built for **zero-latency hardware-fault alerting** on a 500+ host fleet (HPE / Dell / Supermicro, NVIDIA / AMD / Intel GPUs). Two pieces:

- **`monitor-agent`** (Rust, ~5 MB static musl binary, ~10–20 MB RAM): one per host. Watches SMART/NVMe, ECC, MCE, PCIe AER, NIC flaps, SmartArray controllers, Ceph OSD state, OOM killer, soft/hard lockups, inlet temperature, BMC SEL/IML, and more — and pushes plain-English JSON alerts on a persistent mTLS WebSocket.
- **`monitor-collector`** (Rust): central aggregator + tiny SPA. Accepts agent connections, fans out to GUI subscribers in O(1) per client, persists to a SQLite ring buffer, and serves a vanilla-JS web UI that shows **only alerts** plus a connectivity heatmap.

End-to-end target on a < 1 ms LAN: **5 ms p99** from kernel event to DOM render. Pre-shutdown alerts (`reboot`, `shutdown -r now`) arrive **before the user's SSH session closes**.

---

## Architecture

```
[Agent on each host]                           [Central Collector]                [GUI]
- Rust binary (~5 MB)                          - axum + tokio + rustls            - vanilla SPA
- ~12 modules, each a tokio task               - mTLS WSS ingest :9443            - WebSocket subscribe
- Persistent WSS to collector                  - broadcast::Sender fanout         - Alerts view
- Critical-bypass mpsc lane                    - SQLite ring buffer (30 d)        - Connectivity heatmap
- TCP_NODELAY + flush                          - Optional BMC syslog ingest       - filters / silence / details
- OOMScoreAdjust=-1000                         :6514 tcp+tls / :514 udp
```

## What it catches (real failure modes from the field)

| Symptom                                                  | Module                                        | How / latency                                                                       |
| -------------------------------------------------------- | --------------------------------------------- | ----------------------------------------------------------------------------------- |
| HPE SmartArray controller lockup in slot 3 / 6           | `storage_controller` + `storage_io`           | kmsg `hpsa`/`smartpqi` patterns + `ssacli` enumeration diff + udev block-removal    |
| Disks vanish → Ceph OSDs go down                         | `storage_io`                                  | systemd D-Bus watch on `ceph-osd@*`, correlated to recent `controller_lockup`       |
| NIC flaps every 2–3 s                                    | `network_nic`                                 | rtnetlink (instant), 10 s sliding window, threshold 2 → alert with full timeline    |
| Uncorrectable ECC / MCE / PCIe AER → server reboots      | `memory_ecc`, `cpu_mce`, `pcie_aer`           | kmsg push in same async tick as kernel logs the event — alert is on the wire in µs  |
| OOM killer firing                                        | `mempressure`                                 | PSI pre-warn + per-victim alerts + `oom_storm` if ≥3 in 5 min                       |
| OS hangs (hung tasks, soft / hard lockups)               | `oshang`                                      | kmsg patterns; in-tick push beats the kernel panic                                  |
| Inlet temp 38 → 42 °C → vendor auto-shutdown             | `thermal`                                     | tiered 32 / 36 / 38 °C thresholds **below** vendor critical, with rate-of-rise ETA  |
| Server reboot via `reboot` / `shutdown` / `systemctl`    | `lifecycle`                                   | systemd-logind D-Bus `PrepareForShutdown` + delay inhibitor — fires in 1–3 ms       |
| Hard crash / power loss                                  | collector + `boot`                            | heatmap goes red in ≤ 10 s, `host_unclean_reboot` on next agent start               |
| BMC events (PSU fault, pre-boot memory error, intrusion) | `bmc_eventlog` + collector syslog ingest      | Redfish poll + iLO/iDRAC native Remote Syslog forwarded to collector                |

## Repo layout

```
ultimate-monitoring-agent/
├── Cargo.toml             workspace
├── shared/                Alert schema (used by agent + collector + GUI)
├── agent/                 monitor-agent
│   └── src/modules/       one file per detection category
├── collector/             monitor-collector
├── gui/                   index.html + app.js + style.css (no build step)
├── ansible/               deploy.yml + roles/{monitor-agent,monitor-collector}
│   └── configure-bmc.yml  one-time iLO/iDRAC remote-syslog setup
├── systemd/               hardened unit files
├── certs/                 gen-certs.sh — tiny mTLS PKI
├── docs/                  architecture, alert schema, thresholds, lifecycle, severity+categorization-and-commands.md
```

## Quickstart (single host smoke test)

```bash
# 1. Install Rust + the musl target.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add x86_64-unknown-linux-musl

# 2. Build both binaries.
cargo build --release --target x86_64-unknown-linux-musl

# 3. Generate a tiny CA + collector cert + per-host client cert.
./certs/gen-certs.sh ca
./certs/gen-certs.sh server localhost 127.0.0.1
./certs/gen-certs.sh client $(hostname)

# 4. Run the collector.
sudo install -d /etc/monitor-collector /var/lib/monitor-collector
sudo cp certs/out/ca.crt                   /etc/monitor-collector/ca.crt
sudo cp certs/out/localhost.server.crt     /etc/monitor-collector/server.crt
sudo cp certs/out/localhost.server.key     /etc/monitor-collector/server.key
sudo cp collector/config.example.toml      /etc/monitor-collector/config.toml
sudo target/x86_64-unknown-linux-musl/release/monitor-collector

# 5. In another shell, run the agent.
sudo install -d /etc/monitor-agent /var/lib/monitor-agent
sudo cp certs/out/ca.crt                   /etc/monitor-agent/ca.crt
sudo cp certs/out/$(hostname).client.crt   /etc/monitor-agent/client.crt
sudo cp certs/out/$(hostname).client.key   /etc/monitor-agent/client.key
sudo cp agent/config.example.toml          /etc/monitor-agent/config.toml
sudo sed -i 's|wss://collector.dc1.internal:9443|wss://localhost:9443|' /etc/monitor-agent/config.toml
sudo target/x86_64-unknown-linux-musl/release/monitor-agent

# 6. Open the GUI.
xdg-open https://localhost:8443    # accept the self-signed warning

# 7. Trigger the pre-shutdown alert path: in a third shell, run
#    `sudo systemctl reboot --check-inhibitors=no` and watch the alert
#    appear in the GUI before the SSH session disconnects.
```

## Module dependencies (apt packages)

```bash
sudo apt-get install -y \
  smartmontools nvme-cli ethtool ipmitool lm-sensors \
  rasdaemon pciutils dmidecode jq stress-ng
```
> **Do not install `edac-utils` on servers with HPE iLO, Dell iDRAC, or Supermicro BMC.**
> The `edac_core` kernel module it loads fights with the BMC firmware over memory controller
> registers, causing missed or duplicated ECC events in iLO IML / iDRAC SEL.
> UMA's `memory_ecc` module now reads ECC data via IPMI SDR (BMC side-channel) by default,
> and `cpu_mce` covers MCE/hardware-error events via `/dev/kmsg` — no EDAC driver needed.
> Only add `edac-utils` on hosts **without** a management controller (bare VMs, non-managed servers)
> and set `edac_sysfs_enable = true` in `config.toml`.

Vendor storage controller tools (HPE/Dell/LSI) are NOT in Ubuntu repos —
install from the vendor's downloads or your private artifact mirror:

| Tool        | Vendor         | Used by                |
| ----------- | -------------- | ---------------------- |
| `ssacli`    | HPE            | `storage_controller`   |
| `perccli64` | Dell           | `storage_controller`   |
| `storcli64` | Broadcom/LSI   | `storage_controller`   |

If a tool is missing the corresponding module logs `WARN: <tool> not present — disabling`
and the rest of the agent runs fine.

## Verifying every module on a test VM (`scripts/simulate-events.sh`)

The agent treats `/dev/kmsg` as truth — root can write to it, so we synthesize
representative hardware events on a plain VM and watch the GUI react. This is
how you build confidence before fleet rollout.

```bash
sudo scripts/simulate-events.sh list      # list scenarios
sudo scripts/simulate-events.sh all       # fire every non-destructive scenario
sudo scripts/simulate-events.sh nic_flap  # one scenario at a time

# Destructive scenarios (run individually only):
sudo scripts/simulate-events.sh psi_pressure   # 70% RAM for 60s
sudo scripts/simulate-events.sh host_offline   # stops agent for 15s
sudo scripts/simulate-events.sh lifecycle      # REBOOTS the host
```

Open the GUI alongside (`https://<vm>:8443`) and tick each alert off as it appears.

## Documentation

| Doc | Contents |
|-----|----------|
| [`docs/severity-categorization-and-commands.md`](docs/severity-categorization-and-commands.md) | Info / warning / critical policy, customization (`config.toml`, `syslog_rules`, webhooks), and per-module commands / example data |
| [`docs/alert-schema.md`](docs/alert-schema.md) | JSON envelope + `alert` field reference |
| [`docs/thresholds.md`](docs/thresholds.md) | Default numeric thresholds and rationale |
| [`docs/architecture.md`](docs/architecture.md) | System design |
| [`docs/lifecycle-detection.md`](docs/lifecycle-detection.md) | Pre-shutdown / reboot alerting |

## Production deploy

```bash
cargo build --release --target x86_64-unknown-linux-musl
./certs/gen-certs.sh ca
./certs/gen-certs.sh server collector.dc1.internal collector
for h in $(awk '/^uma_agents/{f=1;next}/^\[/{f=0}f' inventory.ini); do
  ./certs/gen-certs.sh client "$h"
done
ansible-playbook -i inventory.ini ansible/deploy.yml
ansible-playbook -i inventory.ini ansible/configure-bmc.yml --ask-vault-pass
```

## Repository

[https://github.com/ruthvik2424/UMA-Monitoring-Tool](https://github.com/ruthvik2424/UMA-Monitoring-Tool)

## License

Apache-2.0.
