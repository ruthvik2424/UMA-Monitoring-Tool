#!/usr/bin/env python3
"""
Ferrous deploy — single-file Python alternative to the Ansible playbooks.

Usage:
    ./deploy.py status                      Show service status on every host.
    ./deploy.py pki                         Generate CA + all certs (local only).
    ./deploy.py build [--version VERSION]   Build binaries on the builder host,
                                             cache locally in artifacts/VERSION/.
    ./deploy.py collector [--version VERSION] Install monitor-collector.
    ./deploy.py agent [HOST ...] [--version VERSION] Install monitor-agent on every host
                                             (or just the named ones).
    ./deploy.py all [--version VERSION]     pki + build + collector + agent.
    ./deploy.py restart [HOST ...]          systemctl restart on every agent.
    ./deploy.py uninstall HOST [HOST...]    Stop + remove agent from those hosts.
    ./deploy.py logs HOST                   Tail journalctl -u monitor-agent.
    ./deploy.py versions                    List available versions in artifacts/.

Version options:
    --version VERSION                       Deploy specific version (default: v3.0.0)
    
Available versions: v1.0.0, v2.0.0, v3.0.0 (current), v4.0.0+ (future)

Config: deploy-hosts.yaml in this directory (see deploy-hosts.example.yaml).
"""

from __future__ import annotations

import argparse
import concurrent.futures as cf
import getpass
import os
import re
import shlex
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

try:
    import yaml
except ImportError:
    print("ERROR: pyyaml is required. Install with: pip3 install pyyaml", file=sys.stderr)
    sys.exit(1)

# ============================================================================
# Constants & paths
# ============================================================================

FERROUS_VERSION = "v3.0.0"  # Current version

ROOT       = Path(__file__).resolve().parent
CERTS_DIR  = ROOT / "certs" / "out"
CERTS_GEN  = ROOT / "certs" / "gen-certs.sh"
ARTIFACTS  = ROOT / "artifacts"
SYSTEMD    = ROOT / "systemd"
HOSTS_YAML = ROOT / "deploy-hosts.yaml"

def get_artifacts_dir(version: str = FERROUS_VERSION) -> Path:
    """Get version-specific artifacts directory."""
    return ARTIFACTS / version

C_RESET = "\033[0m"
C_DIM   = "\033[2m"
C_RED   = "\033[31m"
C_GRN   = "\033[32m"
C_YLW   = "\033[33m"
C_CYAN  = "\033[36m"
C_BOLD  = "\033[1m"

def color(s: str, c: str) -> str:
    return f"{c}{s}{C_RESET}" if sys.stdout.isatty() else s

def info(msg: str) -> None:
    print(color(f"[*] {msg}", C_CYAN))

def ok(msg: str) -> None:
    print(color(f"[ok] {msg}", C_GRN))

def warn(msg: str) -> None:
    print(color(f"[!!] {msg}", C_YLW))

def err(msg: str) -> None:
    print(color(f"[ERR] {msg}", C_RED), file=sys.stderr)

# ============================================================================
# Config schema
# ============================================================================

@dataclass
class HostCfg:
    name: str           # cert CN / display name
    host: str           # SSH-reachable IP or DNS
    user: str = "ubuntu"
    bmc: dict | None = None        # {url, username, password_env, insecure}
    gpu_nvidia: bool = False
    gpu_amd: bool = False
    tags: dict[str, str] = field(default_factory=dict)
    extra_modules: dict[str, dict[str, Any]] = field(default_factory=dict)
    # Raw TOML appended after the rendered template (per-host knobs without editing deploy.py).
    config_append: str = ""

@dataclass
class Cfg:
    collector: HostCfg
    builder: HostCfg
    agents: list[HostCfg]
    collector_url: str
    collector_san_extras: list[str] = field(default_factory=list)
    parallel: int = 10
    log_level: str = "info"
    # When true, adds maintenance = true to rendered agent config (suppresses outbound alerts).
    maintenance: bool = False
    # When non-empty, sets agent.debug_listen (e.g. '127.0.0.1:19100' for GET /v1/debug/snapshot).
    agent_debug_listen: str = ""
    # Outbound webhooks for Slack / Google Chat / generic JSON. Each item
    # is a dict with keys: name, kind, url, severity_min, states, ...
    # See deploy-hosts.example.yaml.
    webhooks: list[dict] = field(default_factory=list)

def load_cfg() -> Cfg:
    if not HOSTS_YAML.exists():
        err(f"missing config: {HOSTS_YAML}")
        err("Copy deploy-hosts.example.yaml to deploy-hosts.yaml and edit it.")
        sys.exit(1)
    raw = yaml.safe_load(HOSTS_YAML.read_text())

    def host(d: dict) -> HostCfg:
        return HostCfg(
            name=d["name"],
            host=d["host"],
            user=d.get("user", "ubuntu"),
            bmc=d.get("bmc"),
            gpu_nvidia=d.get("gpu_nvidia", False),
            gpu_amd=d.get("gpu_amd", False),
            tags=d.get("tags", {}),
            extra_modules=d.get("extra_modules", {}),
            config_append=str(d.get("config_append") or ""),
        )

    cfg = Cfg(
        collector=host(raw["collector"]),
        builder=host(raw.get("builder") or raw["collector"]),
        agents=[host(a) for a in raw["agents"]],
        collector_url=raw.get("collector_url", "") or "",
        collector_san_extras=raw.get("collector_san_extras", []),
        parallel=raw.get("parallel", 10),
        log_level=raw.get("log_level", "info"),
        maintenance=bool(raw.get("maintenance", False)),
        agent_debug_listen=str(raw.get("agent_debug_listen") or ""),
        webhooks=raw.get("webhooks", []) or [],
    )
    # ---- Validation — fail fast on bad collector_url ----
    if not cfg.collector_url:
        err(f"collector_url is missing/empty in {HOSTS_YAML}")
        err('  example:  collector_url: "wss://10.240.19.245:9443/v1/ingest"')
        sys.exit(2)
    if "collector.local" in cfg.collector_url:
        err(f"collector_url is set to the agent\u2019s placeholder default 'collector.local' in {HOSTS_YAML}")
        err("  Replace it with the collector's actual IP or DNS name, e.g.")
        err('    collector_url: "wss://10.240.19.245:9443/v1/ingest"')
        sys.exit(2)
    if not cfg.collector_url.startswith("wss://"):
        warn(f"collector_url '{cfg.collector_url}' does not start with wss:// \u2014 the agent only speaks WebSocket-over-TLS")
    # The host part of collector_url must appear in collector_san_extras OR
    # match the collector.name (which is automatically in the cert). Otherwise
    # rustls will reject the server cert.
    import re as _re
    m = _re.match(r"wss?://([^:/]+)", cfg.collector_url)
    if m:
        url_host = m.group(1)
        san_set = set(cfg.collector_san_extras) | {cfg.collector.name, cfg.collector.host}
        if url_host not in san_set:
            warn(f"collector_url host '{url_host}' is not in collector_san_extras/collector.name/.host")
            warn(f"  TLS will fail. Add it: collector_san_extras: [{url_host!r}, ...]")
    return cfg

# ============================================================================
# Local + remote shell
# ============================================================================

# Cached sudo password — prompted once.
_sudo_pass: str | None = None
def sudo_password() -> str:
    global _sudo_pass
    if _sudo_pass is None:
        _sudo_pass = getpass.getpass(prompt="sudo password (same on every host): ")
    return _sudo_pass

def run_local(cmd: list[str] | str, check: bool = True, capture: bool = False, cwd: Path | None = None) -> subprocess.CompletedProcess:
    if isinstance(cmd, list):
        printable = " ".join(shlex.quote(c) for c in cmd)
    else:
        printable = cmd
    info(f"local: {printable}")
    return subprocess.run(
        cmd, check=check, shell=isinstance(cmd, str),
        text=True, capture_output=capture, cwd=str(cwd) if cwd else None,
    )

SSH_OPTS = [
    "-o", "ConnectTimeout=10",
    "-o", "StrictHostKeyChecking=accept-new",
    "-o", "ServerAliveInterval=15",
    "-o", "BatchMode=yes",
]

def ssh(h: HostCfg, cmd: str, sudo: bool = False, capture: bool = True, check: bool = True) -> subprocess.CompletedProcess:
    """Run a shell command on remote host. If sudo=True, uses 'sudo -S' and feeds the password via stdin."""
    target = f"{h.user}@{h.host}"
    if sudo:
        full = f"sudo -S -p '' -- bash -c {shlex.quote(cmd)}"
        proc = subprocess.run(
            ["ssh", *SSH_OPTS, target, full],
            input=sudo_password() + "\n",
            text=True, capture_output=capture, check=False,
        )
    else:
        proc = subprocess.run(
            ["ssh", *SSH_OPTS, target, "bash", "-c", shlex.quote(cmd)],
            text=True, capture_output=capture, check=False,
        )
    if check and proc.returncode != 0:
        raise RuntimeError(
            f"ssh {target} failed (rc={proc.returncode})\n"
            f"  cmd:    {cmd}\n"
            f"  stdout: {(proc.stdout or '').strip()}\n"
            f"  stderr: {(proc.stderr or '').strip()}"
        )
    return proc

def scp_to(h: HostCfg, local: Path, remote: str) -> None:
    """Copy local file → remote (as the SSH user, NOT root)."""
    target = f"{h.user}@{h.host}:{remote}"
    info(f"scp {h.name}: {local.name} → {remote}")
    proc = subprocess.run(["scp", *SSH_OPTS, str(local), target], capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(f"scp {target} failed: {proc.stderr.strip()}")

def scp_from(h: HostCfg, remote: str, local: Path) -> None:
    """Copy remote file → local."""
    target = f"{h.user}@{h.host}:{remote}"
    info(f"scp {h.name}: {remote} → {local}")
    proc = subprocess.run(["scp", *SSH_OPTS, target, str(local)], capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(f"scp {target} failed: {proc.stderr.strip()}")

def install_remote(h: HostCfg, local: Path, remote_path: str, mode: str, owner: str = "root", group: str = "root") -> None:
    """Push a local file and `install` it into place at `remote_path` with given ownership/perms."""
    tmp = f"/tmp/uma-{int(time.time()*1000)}-{local.name}"
    scp_to(h, local, tmp)
    cmd = (
        f"install -o {owner} -g {group} -m {mode} {shlex.quote(tmp)} {shlex.quote(remote_path)} "
        f"&& rm -f {shlex.quote(tmp)}"
    )
    ssh(h, cmd, sudo=True, capture=True, check=True)

def parallel(items: list, fn, max_workers: int = 10) -> list:
    """Run fn(item) for each item in parallel; return list of (item, result_or_exc)."""
    out: list[tuple[Any, Any]] = []
    with cf.ThreadPoolExecutor(max_workers=max_workers) as ex:
        futs = {ex.submit(fn, it): it for it in items}
        for fut in cf.as_completed(futs):
            it = futs[fut]
            try:
                out.append((it, fut.result()))
            except Exception as e:
                out.append((it, e))
    return out

# ============================================================================
# PKI — call gen-certs.sh locally
# ============================================================================

def cmd_pki(cfg: Cfg, args) -> None:
    info("Bootstrapping PKI (idempotent)")
    CERTS_DIR.mkdir(parents=True, exist_ok=True)

    if not (CERTS_DIR / "ca.crt").exists():
        run_local([str(CERTS_GEN), "ca"], cwd=CERTS_GEN.parent)
    else:
        ok("CA already exists — skipping")

    server_crt = CERTS_DIR / f"{cfg.collector.name}.server.crt"
    if not server_crt.exists():
        san = list(cfg.collector_san_extras) + [cfg.collector.host]
        run_local([str(CERTS_GEN), "server", cfg.collector.name, *san], cwd=CERTS_GEN.parent)
    else:
        ok(f"server cert {server_crt.name} exists — skipping")

    for a in cfg.agents:
        crt = CERTS_DIR / f"{a.name}.client.crt"
        if not crt.exists():
            run_local([str(CERTS_GEN), "client", a.name], cwd=CERTS_GEN.parent)
        else:
            ok(f"client cert {crt.name} exists — skipping")

    listing = "\n  ".join(sorted(p.name for p in CERTS_DIR.iterdir()))
    print(f"\nCerts in {CERTS_DIR}:\n  {listing}")

# ============================================================================
# Build — on the builder host (Linux), pull artifacts back
# ============================================================================

def cmd_versions(cfg: Cfg, args) -> None:
    """List available versions in artifacts directory."""
    if not ARTIFACTS.exists():
        warn("No artifacts directory found")
        return
    
    versions = []
    for item in ARTIFACTS.iterdir():
        if item.is_dir() and item.name.startswith('v'):
            versions.append(item.name)
    
    if not versions:
        warn("No versions found in artifacts/")
        return
    
    versions.sort(key=lambda x: [int(n) for n in x[1:].split('.')])  # Sort by version number
    
    info("Available versions:")
    for v in versions:
        version_dir = ARTIFACTS / v
        binaries = []
        if (version_dir / "monitor-agent").exists():
            binaries.append("agent")
        if (version_dir / "monitor-collector").exists():
            binaries.append("collector")
        
        status = " (current)" if v == FERROUS_VERSION else ""
        info(f"  {v}: {', '.join(binaries) if binaries else 'no binaries'}{status}")

def cmd_build(cfg: Cfg, args) -> None:
    version = getattr(args, 'version', FERROUS_VERSION)
    musl    = getattr(args, 'musl', False)
    b = cfg.builder
    build_type = "musl (static)" if musl else "glibc (dynamic)"
    info(f"Building binaries {version} [{build_type}] on {b.name} ({b.host})")

    # 1. Install build deps + Rust on builder if missing.
    info("Ensuring build deps + Rust toolchain on builder...")
    base_pkgs = (
        "pkg-config build-essential libssl-dev libudev-dev "
        "libdbus-1-dev libsqlite3-dev curl rsync"
    )
    musl_pkgs = "musl-tools musl-dev"
    pkgs = f"{base_pkgs} {musl_pkgs}" if musl else base_pkgs
    ssh(b, f"apt-get update -qq && apt-get install -y {pkgs} >/dev/null", sudo=True)

    has_cargo = ssh(b, "test -x $HOME/.cargo/bin/cargo && echo yes || echo no", capture=True)
    if "yes" not in has_cargo.stdout:
        info("Installing rustup on builder (one-time, ~2 min)...")
        ssh(b, (
            "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | "
            "sh -s -- -y --default-toolchain stable --profile minimal"
        ))

    if musl:
        ssh(b, "source $HOME/.cargo/env && rustup target add x86_64-unknown-linux-musl")

    # 2. Sync source to builder via rsync.
    info(f"Rsyncing source → {b.user}@{b.host}:~/ultimate-monitoring-agent/")
    subprocess.run([
        "rsync", "-az", "--delete",
        "--exclude=target/", "--exclude=.git/", "--exclude=artifacts/",
        "--exclude=.DS_Store", "--exclude=certs/out/", "--exclude=*.db",
        "--exclude=*.db-*", "--exclude=*.log", "--exclude=__pycache__/",
        "--exclude=deploy-hosts.yaml", "--exclude=deploy-hosts_*.yaml",
        "--exclude=hosts.csv", "--exclude=backup/", "--exclude=copy_app/",
        str(ROOT) + "/",
        f"{b.user}@{b.host}:~/ultimate-monitoring-agent/",
    ], check=True)

    # 3. cargo build — glibc (default, fast) or musl (static, portable).
    #
    #    musl cross-compilation requires PKG_CONFIG_ALLOW_CROSS because
    #    libudev-sys uses pkg-config to locate udev headers.  On a standard
    #    Ubuntu builder this is sufficient; the resulting binary is statically
    #    linked against musl libc and has no runtime glibc dependency.
    #
    #    If you hit "libudev-sys: pkg-config sysroot" errors with --musl, run:
    #      sudo apt-get install -y musl-tools musl-dev libsystemd-dev libudev-dev
    #    and re-run with --musl.

    if musl:
        build_cmd = (
            "cd ~/ultimate-monitoring-agent && "
            "source $HOME/.cargo/env && "
            "PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_SYSROOT_DIR=/ "
            "cargo build --release --target x86_64-unknown-linux-musl --workspace 2>&1"
        )
        bin_path = "~/ultimate-monitoring-agent/target/x86_64-unknown-linux-musl/release"
    else:
        build_cmd = (
            "cd ~/ultimate-monitoring-agent && "
            "source $HOME/.cargo/env && "
            "cargo build --release --workspace 2>&1"
        )
        bin_path = "~/ultimate-monitoring-agent/target/release"

    info(f"Running cargo build (this takes ~3-8 min on first run)...")
    ssh(b, build_cmd, capture=False, check=True)

    # 4. Pull artifacts back into artifacts/VERSION/.
    artifacts_dir = get_artifacts_dir(version)
    artifacts_dir.mkdir(parents=True, exist_ok=True)
    for bn in ("monitor-agent", "monitor-collector"):
        scp_from(b, f"{bin_path}/{bn}", artifacts_dir / bn)

    sizes = {
        p.name: f"{p.stat().st_size / (1024*1024):.1f}MB"
        for p in artifacts_dir.iterdir()
        if p.name.startswith("monitor-")
    }
    ok(f"Built {version} [{build_type}] → artifacts/{version}/  sizes={sizes}")

# ============================================================================
# Config rendering — pure Python f-strings, no Jinja
# ============================================================================

def _bool(b: bool) -> str:
    return "true" if b else "false"

def _toml_escape(s: str) -> str:
    return s.replace("\\", "\\\\").replace('"', '\\"')

def _bool_kv_line(key: str, b: bool) -> str:
    """Emit TOML `key = true` only when b is true (omit when false — default in agent)."""
    return f"{key} = {_bool(b)}\n" if b else ""

def _debug_listen_line(addr: str) -> str:
    addr = (addr or "").strip()
    if not addr:
        return ""
    return f'debug_listen = "{_toml_escape(addr)}"\n'

def render_agent_config(cfg: Cfg, h: HostCfg) -> str:
    bmc = h.bmc or {}
    bmc_url = bmc.get("url", "")
    bmc_user = bmc.get("username", "")
    pass_env = bmc.get("password_env", "")
    bmc_pass = os.environ.get(pass_env, "") if pass_env else bmc.get("password", "")
    bmc_insecure = bmc.get("insecure", False)
    bmc_enabled = bool(bmc_url and bmc_user)

    extra_tag_lines = "\n".join(
        f'{k} = "{_toml_escape(v)}"' for k, v in (h.tags or {}).items()
    )

    main = rf"""
# Managed by deploy.py — do not edit by hand.
# Host:      {h.name} ({h.host})
# Generated: {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}

[agent]
log_level = "{cfg.log_level}"
{_bool_kv_line("maintenance", cfg.maintenance)}{_debug_listen_line(cfg.agent_debug_listen)}
[collector]
url              = "{cfg.collector_url}"
keepalive_s      = 5
reconnect_min_ms = 250
reconnect_max_ms = 30000

[tls]
ca_cert     = "/etc/monitor-agent/ca.crt"
client_cert = "/etc/monitor-agent/client.crt"
client_key  = "/etc/monitor-agent/client.key"

[heartbeat]
interval_s = 5

[dedup]
re_notify_after_s = 1800
recovery_window_s = 60

[tags]
host = "{h.name}"
{extra_tag_lines}

# ---- Hardware modules ----

[modules.nvme]
enabled           = true
poll_s            = 60
wear_warn_pct     = 80
wear_critical_pct = 90
temp_warn_c       = 70
temp_critical_c   = 80

[modules.disk_smart]
enabled = true
poll_s  = 300

[modules.memory_ecc]
enabled          = true
poll_s           = 5
ce_per_hour_warn = 50

[modules.cpu_mce]
enabled = true

[modules.pcie_aer]
enabled                = true
corrected_per_min_warn = 100

[modules.thermal]
enabled           = true
poll_s            = 5
inlet_warn_c      = 32.0
inlet_critical_c  = 36.0
inlet_emergency_c = 38.0

[modules.network_nic]
enabled        = true
flap_window_s  = 10
flap_threshold = 2
counter_poll_s = 5
ignore_regex   = ["^lo$", "^docker.*", "^veth.*", "^br-.*", "^cali.*", "^cni.*", "^tun.*", "^tap.*", "^virbr.*"]

[modules.storage_controller]
enabled = true
poll_s  = 15

[modules.storage_io]
enabled            = true
watch_ceph_osd     = true
correlate_window_s = 30

[modules.mempressure]
enabled              = true
poll_s               = 2
psi_some_avg10_warn  = 50.0
sustain_s            = 30
oom_storm_count      = 3
oom_storm_window_s   = 300

[modules.oshang]
enabled = true

[modules.gpu_nvidia]
enabled = {_bool(h.gpu_nvidia)}

[modules.gpu_amd]
enabled = {_bool(h.gpu_amd)}

[modules.bmc_redfish]
enabled          = {_bool(bmc_enabled)}
poll_s           = 10
ipmi_device      = "/dev/ipmi0"
redfish_url      = "{_toml_escape(bmc_url)}"
redfish_username = "{_toml_escape(bmc_user)}"
redfish_password = "{_toml_escape(bmc_pass)}"
redfish_insecure = {_bool(bmc_insecure)}

[modules.bmc_eventlog]
enabled          = {_bool(bmc_enabled)}
poll_s           = 10
redfish_url      = "{_toml_escape(bmc_url)}"
redfish_username = "{_toml_escape(bmc_user)}"
redfish_password = "{_toml_escape(bmc_pass)}"
redfish_insecure = {_bool(bmc_insecure)}

[modules.syslog_rules]
enabled = true

# ---- Built-in default rules (rendered automatically by deploy.py) ----

# NIC link-down/up from kernel log (backup vs rtnetlink). For netdev lines like
# `eth0: Link DOWN`, both journal + netlink may see the event — dedupe in the GUI / tracker.
# For network_nic rules, iface names matching [modules.network_nic] ignore_regex are skipped by the agent.
[[modules.syslog_rules.rules]]
name     = "nic_link_down_kernel"
regex    = '(?i)([^\s:]+:\s+Link\s+DOWN|NIC Link is Down|Link Down|link is down|operstate.*down|carrier lost on)'
severity = "warning"
category = "network_nic"
title    = "NIC link went down (kernel log)"
message  = "Kernel reported a NIC link-down event. Check cable, SFP, switch port. Trace: {{match}}"

[[modules.syslog_rules.rules]]
name     = "nic_link_up_kernel"
regex    = '(?i)NIC Link is Up|carrier restored|link up at'
severity = "info"
category = "network_nic"
title    = "NIC link came up"
message  = "Kernel reported a NIC link-up event. Trace: {{match}}"

# OOM kills — backup catcher (mempressure module is the primary path).
[[modules.syslog_rules.rules]]
name     = "oom_kill_kernel"
regex    = '(?i)Out of memory: Killed process|invoked oom-killer'
severity = "critical"
category = "memory_pressure"
title    = "OOM killer fired (kernel log)"
message  = "Linux OOM killer terminated a process due to memory exhaustion. Investigate the workload that grew unbounded. Trace: {{match}}"

# HPE/Dell SmartArray + LSI MegaRAID controller lockup — backup catcher.
[[modules.syslog_rules.rules]]
name     = "controller_lockup_kernel"
regex    = '(?i)(hpsa|smartpqi|megaraid_sas).*(controller lockup detected|controller is offline|hard reset|fatal error)'
severity = "critical"
category = "storage_controller"
title    = "Storage controller lockup (kernel log)"
message  = "Kernel logged a SmartArray/MegaRAID controller lockup. Disks behind this controller will be unreachable; OSDs/filesystems will go down. Schedule a power cycle. Trace: {{match}}"

# libceph kernel client lost contact with an OSD (kernel RBD/CephFS only —
# userspace OSD daemons are watched via D-Bus by the storage_io module).
[[modules.syslog_rules.rules]]
name     = "libceph_osd_down"
regex    = '(?i)libceph: osd\d+ \S+ down|libceph: wrong peer|libceph: socket closed'
severity = "warning"
category = "storage_io"
title    = "libceph: OSD reported down"
message  = "Kernel libceph client lost connection to a Ceph OSD or peer. If many OSDs go down at once, look for a network or upstream failure. Trace: {{match}}"

# systemd-managed service stopped sending sd_notify WATCHDOG=1 — closest
# local approximation of "no heartbeat" (true peer-loss is a collector concern).
[[modules.syslog_rules.rules]]
name     = "systemd_watchdog_timeout"
regex    = '(?i)Watchdog timeout for [\w@\-\.:]+\.service|Watchdog (timed out|has expired)|Killing process .* due to watchdog timeout'
severity = "critical"
category = "syslog"
title    = "systemd service watchdog timeout"
message  = "A systemd-managed service stopped sending watchdog pings — systemd is killing it. Common causes: deadlock, blocked on I/O. Trace: {{match}}"

# TCP keepalive expired — remote peer unresponsive.
[[modules.syslog_rules.rules]]
name     = "tcp_keepalive_timeout"
regex    = '(?i)TCP.*keepalive timer expired|connection timed out, killing'
severity = "warning"
category = "network_nic"
title    = "TCP keepalive timeout"
message  = "Kernel reported a TCP keepalive timeout — peer is unresponsive. Trace: {{match}}"

[modules.boot]
enabled = true

[modules.lifecycle]
enabled = true
"""
    if (h.config_append or "").strip():
        main += (
            "\n# ----------------------------------------------------------------------------\n"
            "# Per-host overrides from deploy-hosts.yaml (`config_append`)\n"
            "# ----------------------------------------------------------------------------\n"
            f"{h.config_append.strip()}\n"
        )
    return main

def render_collector_config(cfg: Cfg) -> str:
    # Render webhook blocks (if any). Each entry in cfg.webhooks is a dict
    # already in the right shape — we just emit TOML.
    webhook_blocks = []
    for w in cfg.webhooks:
        states_list = list(w.get("states", ["firing"]))
        states_toml = "[" + ", ".join(f'"{s}"' for s in states_list) + "]"
        webhook_blocks.append(f"""
[[webhooks]]
name                 = "{_toml_escape(w['name'])}"
kind                 = "{_toml_escape(w['kind'])}"
url                  = "{_toml_escape(w['url'])}"
severity_min         = "{_toml_escape(w.get('severity_min', 'warning'))}"
states               = {states_toml}
notify_on_resolved   = {_bool(w.get('notify_on_resolved', False))}
host_filter          = "{_toml_escape(w.get('host_filter', '.*'))}"
category_filter      = "{_toml_escape(w.get('category_filter', '.*'))}"
rate_limit_per_min   = {int(w.get('rate_limit_per_min', 60))}
gui_url              = "{_toml_escape(w.get('gui_url', ''))}"
insecure_tls         = {_bool(w.get('insecure_tls', False))}
""")
    webhook_section = "".join(webhook_blocks)

    return f"""\
# Managed by deploy.py — do not edit by hand.
bind_ingest = "0.0.0.0:9443"
bind_gui    = "0.0.0.0:8443"
db_path         = "/var/lib/monitor-collector/collector.sqlite"
retention_days  = 30
offline_after_s = 10
log_level       = "{cfg.log_level}"

[tls]
ca_cert     = "/etc/monitor-collector/ca.crt"
server_cert = "/etc/monitor-collector/server.crt"
server_key  = "/etc/monitor-collector/server.key"
require_client_cert = true
{webhook_section}"""

# ============================================================================
# Install: collector
# ============================================================================

def cmd_collector(cfg: Cfg, args) -> None:
    version = getattr(args, 'version', FERROUS_VERSION)
    h = cfg.collector
    info(f"Installing monitor-collector {version} on {h.name} ({h.host})")

    # Pre-flight: artifacts + certs present
    artifacts_dir = get_artifacts_dir(version)
    coll_bin   = artifacts_dir / "monitor-collector"
    server_crt = CERTS_DIR / f"{h.name}.server.crt"
    server_key = CERTS_DIR / f"{h.name}.server.key"
    ca_crt     = CERTS_DIR / "ca.crt"
    for p in (coll_bin, server_crt, server_key, ca_crt):
        if not p.exists():
            err(f"missing prerequisite: {p}")
            err("Run: ./deploy.py build && ./deploy.py pki")
            sys.exit(2)

    # Apt prereqs.
    ssh(h, "apt-get update >/dev/null && apt-get install -y libsqlite3-0 ca-certificates >/dev/null", sudo=True)

    # Create system user + dirs.
    ssh(h, (
        "id -u monitor-collector >/dev/null 2>&1 || "
        "useradd --system --shell /usr/sbin/nologin --home /var/lib/monitor-collector --create-home monitor-collector; "
        "install -d -o monitor-collector -g monitor-collector -m 0750 /var/lib/monitor-collector; "
        "install -d -o monitor-collector -g monitor-collector -m 0755 /etc/monitor-collector; "
        "chown -R monitor-collector:monitor-collector /var/lib/monitor-collector"
    ), sudo=True)

    # Push binary, certs, config, unit.
    install_remote(h, coll_bin,                             "/usr/local/bin/monitor-collector", "0755", "root", "root")
    install_remote(h, ca_crt,                               "/etc/monitor-collector/ca.crt",     "0644", "root", "monitor-collector")
    install_remote(h, server_crt,                           "/etc/monitor-collector/server.crt", "0640", "monitor-collector", "monitor-collector")
    install_remote(h, server_key,                           "/etc/monitor-collector/server.key", "0600", "monitor-collector", "monitor-collector")
    install_remote(h, SYSTEMD / "monitor-collector.service","/etc/systemd/system/monitor-collector.service", "0644", "root", "root")

    # Render + push config.
    cfg_text = render_collector_config(cfg)
    tmp = ARTIFACTS / f"_collector-{h.name}.toml"
    tmp.write_text(cfg_text)
    install_remote(h, tmp, "/etc/monitor-collector/config.toml", "0640", "monitor-collector", "monitor-collector")
    tmp.unlink()

    # Reload + restart + verify.
    ssh(h, "systemctl daemon-reload && systemctl enable monitor-collector >/dev/null && systemctl restart monitor-collector", sudo=True)
    time.sleep(2)
    st = ssh(h, "systemctl is-active monitor-collector", check=False).stdout.strip()
    if st == "active":
        ok(f"monitor-collector active on {h.name}")
    else:
        err(f"monitor-collector NOT active (state={st}). Showing recent journal:")
        log = ssh(h, "journalctl -u monitor-collector -n 20 --no-pager", sudo=True, check=False).stdout
        print(log)

    # Port check.
    ports = ssh(h, "ss -tlnp 2>/dev/null | grep -E ':9443|:8443' || true", sudo=True, check=False).stdout
    if "9443" in ports and "8443" in ports:
        ok("collector listening on both 9443 (agents) and 8443 (GUI)")
    else:
        warn(f"collector ports look wrong:\n{ports}")

# ============================================================================
# Install: agent (parallel across many hosts)
# ============================================================================

def cmd_agent(cfg: Cfg, args) -> None:
    version = getattr(args, 'version', FERROUS_VERSION)
    targets = cfg.agents
    if args.hosts:
        wanted = set(args.hosts)
        targets = [a for a in cfg.agents if a.name in wanted or a.host in wanted]
        missing = wanted - {a.name for a in targets} - {a.host for a in targets}
        if missing:
            err(f"hosts not found in deploy-hosts.yaml: {missing}")
            sys.exit(2)

    if not targets:
        warn("no targets selected.")
        return

    artifacts_dir = get_artifacts_dir(version)
    agent_bin = artifacts_dir / "monitor-agent"
    ca_crt    = CERTS_DIR / "ca.crt"
    if not agent_bin.exists() or not ca_crt.exists():
        err(f"missing {agent_bin} or certs/out/ca.crt — run ./deploy.py build --version {version} && ./deploy.py pki")
        sys.exit(2)

    info(f"Installing agent {version} on {len(targets)} host(s) in parallel (max {cfg.parallel}).")
    sudo_password()  # prompt once before fanning out

    results = parallel(targets, lambda h: install_one_agent(cfg, h, version), max_workers=cfg.parallel)
    print()
    fail = 0
    for h, r in sorted(results, key=lambda x: x[0].name):
        if isinstance(r, Exception):
            err(f"  {h.name:<30} FAILED  {r}")
            fail += 1
        else:
            ok(f"  {h.name:<30} {r}")
    if fail:
        sys.exit(1)

def install_one_agent(cfg: Cfg, h: HostCfg, version: str = FERROUS_VERSION) -> str:
    artifacts_dir = get_artifacts_dir(version)
    agent_bin   = artifacts_dir / "monitor-agent"
    ca_crt      = CERTS_DIR / "ca.crt"
    client_crt  = CERTS_DIR / f"{h.name}.client.crt"
    client_key  = CERTS_DIR / f"{h.name}.client.key"
    for p in (client_crt, client_key):
        if not p.exists():
            raise RuntimeError(f"missing client cert {p.name} — run ./deploy.py pki first")

    # Apt prereqs (idempotent). edac-utils intentionally excluded — loading
    # the EDAC kernel module conflicts with HPE iLO/Dell iDRAC/BMC firmware.
    ssh(h, (
        "apt-get update >/dev/null 2>&1; "
        "apt-get install -y "
        "smartmontools nvme-cli ethtool ipmitool lm-sensors rasdaemon "
        "pciutils dmidecode libudev1 libdbus-1-3 >/dev/null"
    ), sudo=True, capture=True)

    # Dirs.
    ssh(h, (
        "install -d -m 0755 /etc/monitor-agent; "
        "install -d -m 0755 /var/lib/monitor-agent; "
        "install -d -m 0755 /var/log/monitor-agent"
    ), sudo=True)

    # Logrotate config (7-day error log, 3-day execution log).
    logrotate_src = ROOT / "ansible" / "roles" / "monitor-agent" / "files" / "logrotate-monitor-agent"
    if logrotate_src.exists():
        install_remote(h, logrotate_src, "/etc/logrotate.d/monitor-agent", "0644", "root", "root")

    # Hosts file pin (so collector hostname always resolves).
    coll_host = cfg.collector.host
    coll_name = cfg.collector.name
    ssh(h, (
        f"grep -qE '\\s{re.escape(coll_name)}(\\s|$)' /etc/hosts "
        f"&& sed -i -E 's|^.*\\s{re.escape(coll_name)}(\\s|$).*|{coll_host}\t{coll_name}|' /etc/hosts "
        f"|| echo '{coll_host}\t{coll_name}' >> /etc/hosts"
    ), sudo=True)

    # Push binary + certs.
    install_remote(h, agent_bin,  "/usr/local/bin/monitor-agent", "0755", "root", "root")
    install_remote(h, ca_crt,     "/etc/monitor-agent/ca.crt",     "0644", "root", "root")
    install_remote(h, client_crt, "/etc/monitor-agent/client.crt", "0644", "root", "root")
    install_remote(h, client_key, "/etc/monitor-agent/client.key", "0600", "root", "root")

    # Render + push config.
    cfg_text = render_agent_config(cfg, h)
    tmp = ARTIFACTS / f"_agent-{h.name}.toml"
    tmp.write_text(cfg_text)
    install_remote(h, tmp, "/etc/monitor-agent/config.toml", "0640", "root", "root")
    tmp.unlink()

    # Push systemd unit.
    install_remote(h, SYSTEMD / "monitor-agent.service",
                   "/etc/systemd/system/monitor-agent.service", "0644", "root", "root")

    # Sudoers rule — allows monitor-agent user to run smartctl/nvme without
    # a password. Needed because NoNewPrivileges=true in the systemd unit
    # blocks ambient capability (CAP_SYS_RAWIO) inheritance to child processes.
    sudoers_src = ROOT / "ansible" / "roles" / "monitor-agent" / "files" / "sudoers-monitor-agent"
    if sudoers_src.exists():
        install_remote(h, sudoers_src, "/etc/sudoers.d/monitor-agent", "0440", "root", "root")
        info(f"  {h.name}: sudoers rule installed")
    else:
        warn(f"  {h.name}: sudoers file not found at {sudoers_src} — smartctl/nvme will run without sudo")

    # Reload + start.
    ssh(h, "systemctl daemon-reload && systemctl enable monitor-agent >/dev/null && systemctl restart monitor-agent", sudo=True)
    time.sleep(2)

    # Verify it actually connected — IMPORTANT: filter logs to ONLY the
    # current agent process, otherwise stale errors from a previous run
    # masquerade as current failures.
    state = ssh(h, "systemctl is-active monitor-agent", check=False).stdout.strip()
    if state != "active":
        log = ssh(h, "journalctl -u monitor-agent -n 15 --no-pager", sudo=True, check=False).stdout
        raise RuntimeError(f"agent not active (state={state}). Tail:\n{log}")

    pid = ssh(h, "systemctl show monitor-agent --property=MainPID --value", check=False).stdout.strip()
    # Wait briefly for the new agent to attempt its first WSS connect.
    time.sleep(3)
    log = ssh(
        h,
        f"journalctl _PID={shlex.quote(pid)} --no-pager 2>/dev/null | tail -50",
        sudo=True, check=False,
    ).stdout
    if "connected to collector" in log:
        return "running, connected to collector"
    if "tls handshake" in log or "tcp connect" in log:
        return f"running but failing to connect (current process; see logs):\n{log[-400:]}"
    return f"running (state={state}; first connect not yet logged)"

# ============================================================================
# Status / restart / logs / uninstall
# ============================================================================

def cmd_status(cfg: Cfg, args) -> None:
    sudo_password()
    targets = [cfg.collector] + cfg.agents
    seen = set()
    uniq = []
    for h in targets:
        if h.host not in seen:
            uniq.append(h); seen.add(h.host)

    def check(h: HostCfg) -> str:
        out = []
        for unit in ("monitor-collector", "monitor-agent"):
            r = ssh(h, f"systemctl is-active {unit} 2>/dev/null || echo missing", check=False)
            out.append(f"{unit}={r.stdout.strip()}")
        return " · ".join(out)

    results = parallel(uniq, check, max_workers=cfg.parallel)
    print()
    for h, r in sorted(results, key=lambda x: x[0].name):
        if isinstance(r, Exception):
            print(f"  {color(h.name, C_RED):<30} {r}")
        else:
            print(f"  {h.name:<30} {r}")

def cmd_restart(cfg: Cfg, args) -> None:
    sudo_password()
    targets = cfg.agents
    if args.hosts:
        wanted = set(args.hosts)
        targets = [a for a in cfg.agents if a.name in wanted or a.host in wanted]
    parallel(targets, lambda h: ssh(h, "systemctl restart monitor-agent", sudo=True), max_workers=cfg.parallel)
    ok(f"restarted on {len(targets)} hosts")

def cmd_logs(cfg: Cfg, args) -> None:
    target = next((a for a in cfg.agents if a.name == args.host or a.host == args.host), None)
    if not target:
        err(f"host '{args.host}' not in deploy-hosts.yaml"); sys.exit(2)
    sudo_password()
    print(f"Tailing journalctl -u monitor-agent on {target.name} (Ctrl-C to exit)...")
    subprocess.run([
        "ssh", *SSH_OPTS, f"{target.user}@{target.host}",
        "sudo", "-S", "-p", "", "--", "journalctl", "-u", "monitor-agent", "-f", "-n", "60",
    ], input=sudo_password() + "\n", text=True)

def cmd_uninstall(cfg: Cfg, args) -> None:
    sudo_password()
    targets = [a for a in cfg.agents if a.name in args.hosts or a.host in args.hosts]
    if not targets:
        err("no matching hosts"); sys.exit(2)
    def remove(h: HostCfg) -> str:
        ssh(h, (
            "systemctl stop monitor-agent 2>/dev/null; "
            "systemctl disable monitor-agent 2>/dev/null; "
            "rm -f /usr/local/bin/monitor-agent /etc/systemd/system/monitor-agent.service; "
            "rm -rf /etc/monitor-agent; "
            "systemctl daemon-reload"
        ), sudo=True, check=False)
        return "uninstalled"
    for h, r in parallel(targets, remove, max_workers=cfg.parallel):
        ok(f"  {h.name:<30} {r}")

# ============================================================================
# all = pki + build + collector + agents
# ============================================================================

def cmd_all(cfg: Cfg, args) -> None:
    cmd_pki(cfg, args)
    cmd_build(cfg, args)
    cmd_collector(cfg, args)
    # Pass version through; hosts=[] means all agents
    cmd_agent(cfg, argparse.Namespace(hosts=[], version=getattr(args, 'version', FERROUS_VERSION)))

# ============================================================================
# CLI
# ============================================================================

def main() -> None:
    p = argparse.ArgumentParser(description="Ferrous deploy", formatter_class=argparse.RawDescriptionHelpFormatter,
                                 epilog=__doc__)
    sp = p.add_subparsers(dest="cmd", required=True)

    # Global version argument for deployment commands
    def add_version_arg(parser):
        parser.add_argument("--version", default="v3.0.0", 
                           help="version to deploy (default: v3.0.0)")

    sp.add_parser("status",   help="show systemd state on every host")
    sp.add_parser("pki",      help="generate CA + per-host certs locally")
    
    build = sp.add_parser("build",    help="build binaries on the builder host")
    add_version_arg(build)
    build.add_argument("--musl", action="store_true",
                       help="build static musl binary (default: glibc dynamic, faster & less error-prone)")
    
    collector = sp.add_parser("collector", help="install monitor-collector")
    add_version_arg(collector)

    rdr = sp.add_parser("render", help="print the rendered config.toml for one host (debugging)")
    rdr.add_argument("host", help="host name (must match an agent's `name` in deploy-hosts.yaml)")

    a = sp.add_parser("agent", help="install agent on all (or named) hosts")
    a.add_argument("hosts", nargs="*", help="optional list of host names; default = all")
    add_version_arg(a)

    all_cmd = sp.add_parser("all",      help="pki + build + collector + agents (everything)")
    add_version_arg(all_cmd)

    r = sp.add_parser("restart", help="restart monitor-agent on all (or named) hosts")
    r.add_argument("hosts", nargs="*")

    l = sp.add_parser("logs", help="tail journalctl -u monitor-agent on one host")
    l.add_argument("host")

    u = sp.add_parser("uninstall", help="remove monitor-agent from named hosts")
    u.add_argument("hosts", nargs="+")
    
    sp.add_parser("versions", help="list available versions in artifacts/")

    args = p.parse_args()
    cfg = load_cfg()

    {
        "status":    cmd_status,
        "pki":       cmd_pki,
        "build":     cmd_build,
        "collector": cmd_collector,
        "agent":     cmd_agent,
        "all":       cmd_all,
        "restart":   cmd_restart,
        "logs":      cmd_logs,
        "uninstall": cmd_uninstall,
        "render":    cmd_render,
        "versions":  cmd_versions,
    }[args.cmd](cfg, args)


def cmd_render(cfg: Cfg, args) -> None:
    target = next((a for a in cfg.agents if a.name == args.host or a.host == args.host), None)
    if not target:
        err(f"host '{args.host}' not found in deploy-hosts.yaml")
        sys.exit(2)
    print(render_agent_config(cfg, target))

if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\ninterrupted")
        sys.exit(130)
    except RuntimeError as e:
        err(str(e))
        sys.exit(1)
