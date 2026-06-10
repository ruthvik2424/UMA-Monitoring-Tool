# UMA Ansible — full deployment

## Variable structure (per-host module config)

Three layers, lowest to highest precedence:

| Layer                                         | What goes here                                                         |
| --------------------------------------------- | ---------------------------------------------------------------------- |
| `group_vars/uma_agents.yml`                   | Fleet-wide module defaults (every module + every threshold).           |
| `host_vars/<inventory_hostname>.yml`          | Per-host **overrides** — only the keys you want to differ. Recursive merge. |
| `-e foo=bar` / `--extra-vars`                 | One-off command-line overrides (highest precedence).                    |

The merging happens via `combine(recursive=True)` in `templates/agent-config.toml.j2`. Set ANY module's `enabled: true|false` and any threshold per host without rewriting the whole module block.

### Example: enable iLO + NVIDIA on one specific host

```yaml
# host_vars/baremetal-host-01.yml
uma_dc:   "dc1"
uma_role: "ai-training"

uma_modules_overrides:
  gpu_nvidia:
    enabled: true

  bmc_redfish:
    enabled: true
    redfish_url: "https://10.0.0.50"
    redfish_username: "monitor"
    redfish_password: "{{ vault_hpe_ilo_password }}"
    redfish_insecure: true

  bmc_eventlog:
    enabled: true
    redfish_url: "https://10.0.0.50"
    redfish_username: "monitor"
    redfish_password: "{{ vault_hpe_ilo_password }}"
    redfish_insecure: true

  thermal:
    inlet_warn_c:      28.0
    inlet_critical_c:  32.0
    inlet_emergency_c: 35.0
```

Two ready-made examples in `host_vars/`:
- `example-hpe-with-gpu-and-ilo.yml.example` — HPE iLO + 4× NVIDIA + tighter thermal
- `example-dell-with-idrac.yml.example` — Dell iDRAC + Ceph OSD tuning

Copy → rename → adjust per host.

### Storing BMC passwords with ansible-vault

```bash
cp group_vars/all/vault.yml.example group_vars/all/vault.yml
$EDITOR group_vars/all/vault.yml          # fill in passwords
ansible-vault encrypt group_vars/all/vault.yml

# Run any playbook with --ask-vault-pass:
ansible-playbook -i inventory.fleet.ini install-agent-fast.yml \
  --ask-become-pass --ask-vault-pass
```

Reference vaulted secrets from host_vars: `redfish_password: "{{ vault_hpe_ilo_password }}"`.

### Modules covered (ALL of them now render in config.toml)

`nvme · disk_smart · memory_ecc · cpu_mce · pcie_aer · thermal · network_nic · storage_controller · storage_io · mempressure · oshang · gpu_nvidia · gpu_amd · bmc_redfish · bmc_eventlog · syslog_rules · boot · lifecycle`

Every one has an `enabled: true|false` flag plus its own thresholds. Defaults sit in `group_vars/uma_agents.yml`; per-host changes go in `host_vars/`.

---



One command end-to-end:

```bash
cp inventory.example.ini inventory.ini
$EDITOR inventory.ini                          # add your hosts
ansible-playbook -i inventory.ini site.yml
```

`site.yml` chains three sub-playbooks:

| Playbook            | What it does                                                                                                            | Idempotent? |
| ------------------- | ----------------------------------------------------------------------------------------------------------------------- | ----------- |
| `00-build.yml`      | Runs `cargo build --release --workspace` on the controller. Skips compile if nothing source-changed.                    | yes         |
| `01-pki.yml`        | Bootstraps the CA (one-time), issues a server cert per `uma_collectors`, issues a client cert per `uma_agents`.         | yes         |
| `deploy.yml`        | Pushes binary + systemd unit + config + certs, fixes ownership, enables/starts services on every host.                  | yes         |

## Targeted runs

```bash
# Just (re-)build:
ansible-playbook -i inventory.ini site.yml --tags build

# Just (re-)issue certs (e.g. after adding new hosts to inventory):
ansible-playbook -i inventory.ini site.yml --tags pki

# Just push the artifacts (e.g. after a fresh build):
ansible-playbook -i inventory.ini site.yml --tags deploy

# Limit to a single host (useful when debugging one node):
ansible-playbook -i inventory.ini site.yml --limit baremetal-host-01

# Dry-run:
ansible-playbook -i inventory.ini site.yml --check
```

## Pre-requisites on the controller

```bash
# Rust toolchain (rust-toolchain.toml in the workspace pins to stable):
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# System libs needed at compile time:
sudo apt-get install -y \
  pkg-config build-essential libssl-dev \
  libudev-dev libdbus-1-dev libsqlite3-dev

# Ansible:
sudo apt-get install -y ansible
```

## What gets installed on each target

**Every agent host** (`uma_agents`):

| Path                                 | Source                                              | Mode      |
| ------------------------------------ | --------------------------------------------------- | --------- |
| `/usr/local/bin/monitor-agent`       | `target/release/monitor-agent`                      | 0755      |
| `/etc/systemd/system/monitor-agent.service` | `systemd/monitor-agent.service`              | 0644      |
| `/etc/monitor-agent/config.toml`     | rendered from `templates/config.toml.j2`            | 0640      |
| `/etc/monitor-agent/ca.crt`          | `certs/out/ca.crt`                                  | 0644      |
| `/etc/monitor-agent/client.crt`      | `certs/out/{{ inventory_hostname }}.client.crt`     | 0644      |
| `/etc/monitor-agent/client.key`      | `certs/out/{{ inventory_hostname }}.client.key`     | 0600      |
| `/var/lib/monitor-agent/`            | (state dir — `boot.rs` writes here)                  | 0755      |
| apt: `smartmontools nvme-cli ethtool ipmitool lm-sensors edac-utils rasdaemon pciutils dmidecode jq` | (optional, controlled by `uma_install_apt_tools`) | — |

Service is `enable --now`'d. Verify with:

```bash
ssh node-01 'sudo systemctl status monitor-agent --no-pager -l | head -20'
```

**The collector host** (`uma_collectors`):

| Path                                            | Source                                      | Owner               |
| ----------------------------------------------- | ------------------------------------------- | ------------------- |
| `/usr/local/bin/monitor-collector`              | `target/release/monitor-collector`          | root                |
| `/etc/systemd/system/monitor-collector.service` | `systemd/monitor-collector.service`         | root                |
| `/etc/monitor-collector/config.toml`            | rendered                                    | monitor-collector   |
| `/etc/monitor-collector/ca.crt`                 | `certs/out/ca.crt`                          | root:monitor-coll.. |
| `/etc/monitor-collector/server.crt`             | `certs/out/{{ uma_collector_fqdn }}.server.crt` | monitor-collector   |
| `/etc/monitor-collector/server.key`             | `certs/out/{{ uma_collector_fqdn }}.server.key` | monitor-collector  |
| `/var/lib/monitor-collector/`                   | (SQLite history dir)                        | monitor-collector   |

GUI then accessible at `https://<collector-fqdn>:8443`.

## Adding a new host

```bash
# 1. Add a line to inventory.ini under [uma_agents]:
echo 'node-99.dc1.internal ansible_user=ubuntu' >> inventory.ini

# 2. Re-run pki + deploy (build is skipped if nothing changed):
ansible-playbook -i inventory.ini site.yml --limit node-99.dc1.internal
```

The PKI step issues a fresh client cert for that host only; existing certs are untouched (idempotency via `creates:`).

## Rotating a cert

```bash
# To force re-issue of, say, node-99's client cert:
rm certs/out/node-99.dc1.internal.client.{crt,key}
ansible-playbook -i inventory.ini site.yml --tags pki,deploy --limit node-99.dc1.internal

# To force a CA roll (everyone affected — needs full redeploy):
rm -rf certs/out
ansible-playbook -i inventory.ini site.yml --tags pki,deploy
```

## Uninstall

```bash
# Stop services, remove binaries + configs + certs (KEEPS history db):
ansible-playbook -i inventory.ini uninstall.yml

# Wipe everything including SQLite history & monitor-collector user:
ansible-playbook -i inventory.ini uninstall.yml -e uma_purge_data=true
```

## Optional: BMC remote-syslog bootstrap

```bash
# One-time per BMC: enable iLO/iDRAC remote syslog forwarding to the collector.
# Requires uma_bmcs group + bmc_address/username/password per host (see inventory).
ansible-playbook -i inventory.ini configure-bmc.yml --ask-vault-pass
```
