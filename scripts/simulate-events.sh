#!/usr/bin/env bash
# UMA test harness — synthesizes representative hardware events on the
# host so you can verify each module end-to-end on a plain VM, before
# rolling the agent out to production.
#
# Mechanism: writes synthetic lines to /dev/kmsg. The agent reads kmsg
# regardless of source, so these look identical to real kernel events.
# Requires root (only root can write /dev/kmsg).
#
# Usage:
#   sudo ./simulate-events.sh all        # fire one example of every module
#   sudo ./simulate-events.sh nic_flap   # just the NIC-flap path
#   sudo ./simulate-events.sh list       # show available scenarios
#
# Open the GUI alongside (https://<vm>:8443) and watch each alert appear.

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Must run as root (writes /dev/kmsg)." >&2
  exit 1
fi

KMSG=/dev/kmsg

inject() { echo "<3>$1" > "$KMSG"; sleep 0.3; }   # <3> = LOG_ERR severity prefix

scenario_memory_ue() {
  echo "[mem-UE]   simulating uncorrectable ECC error..."
  inject "EDAC MC0: 1 UE memory read error on CPU0_DIMM_A1 (channel 0 slot 0 page 0xdeadbe offset 0x0 grain 32 syndrome 0x0)"
  inject "mce: [Hardware Error]: Machine check events logged"
}

scenario_memory_ce() {
  echo "[mem-CE]   simulating burst of correctable ECC..."
  for i in {1..6}; do
    inject "EDAC MC0: 1 CE memory read error on CPU0_DIMM_B2 (channel 0 slot 1 page 0x4242 offset 0x0)"
  done
}

scenario_cpu_mce() {
  echo "[cpu-MCE]  simulating CPU machine check..."
  inject "mce: [Hardware Error]: CPU 0: Machine Check: 0 Bank 4: 8800000040080a13"
  inject "mce: [Hardware Error]: STATUS 8800000040080a13 ADDR 0 MISC 0"
  inject "mce: [Hardware Error]: PROCESSOR 0:50654 TIME 1715600000 SOCKET 0 APIC 0 microcode 2000048"
}

scenario_pcie_aer_uncorr() {
  echo "[pcie-AER] simulating PCIe uncorrectable AER..."
  inject "pcieport 0000:00:01.0: AER: Uncorrected (Fatal) error received: 0000:01:00.0"
  inject "pcieport 0000:00:01.0: AER:   device [8086:1c10] error status/mask=00040000/00000000"
}

scenario_pcie_aer_corr() {
  echo "[pcie-AER] simulating sustained PCIe corrected errors..."
  for i in {1..120}; do
    inject "pcieport 0000:00:01.0: AER: Corrected error received: 0000:01:00.0"
  done
}

scenario_controller_lockup() {
  echo "[storage]  simulating HPE SmartArray controller lockup..."
  inject "hpsa 0000:03:00.0: controller lockup detected: 0xffff0000"
  inject "hpsa 0000:03:00.0: cmd_alloc returned NULL!"
  inject "scsi 1:0:0:0: rejecting I/O to offline device"
  inject "Buffer I/O error on dev sdb, logical block 0, async page read"
}

scenario_smartpqi_lockup() {
  echo "[storage]  simulating Gen10+ smartpqi lockup..."
  inject "smartpqi 0000:06:00.0: controller is offline: status code 0x6100c"
  inject "smartpqi 0000:06:00.0: hard reset performed"
}

scenario_hung_task() {
  echo "[oshang]   simulating hung task..."
  inject "INFO: task kworker/0:1:42 blocked for more than 122 seconds."
  inject "      Not tainted 5.15.0-100-generic #104-Ubuntu"
  inject 'Call Trace:'
}

scenario_softlockup() {
  echo "[oshang]   simulating soft lockup..."
  inject "watchdog: BUG: soft lockup - CPU#3 stuck for 23s! [postgres:12345]"
  inject "Modules linked in: nf_tables ip_set"
}

scenario_oom() {
  echo "[oom]      simulating OOM kill..."
  inject "anon_test invoked oom-killer: gfp_mask=0xcc0(GFP_KERNEL), order=0, oom_score_adj=0"
  inject "Out of memory: Killed process 24531 (anon_test) total-vm:8392704kB, anon-rss:8192340kB, file-rss:0kB, shmem-rss:0kB, UID:1000 pgtables:16448kB oom_score_adj:0"
}

scenario_oom_storm() {
  echo "[oom]      simulating OOM storm (4 kills in 10s)..."
  for pid in 24531 24598 24612 24640; do
    inject "Out of memory: Killed process $pid (anon_test_$pid) total-vm:8000000kB, anon-rss:7900000kB, file-rss:0kB, shmem-rss:0kB"
    sleep 1
  done
}

scenario_fs_error() {
  echo "[storage]  simulating EXT4 filesystem error..."
  inject "EXT4-fs error (device sdb1): ext4_lookup:1714: inode #2: comm ls: deleted inode referenced: 17"
}

scenario_nic_flap() {
  echo "[nic-flap] simulating NIC flap on a SAFE virtual interface..."
  IFNAME="$(ip -o link show type dummy 2>/dev/null | awk -F': ' 'NR==1{print $2}')"
  if [[ -z "$IFNAME" ]]; then
    echo "  no dummy interface present — creating uma-test0..."
    ip link add uma-test0 type dummy
    ip link set uma-test0 up
    IFNAME=uma-test0
  fi
  echo "  flapping $IFNAME..."
  for i in 1 2 3; do
    ip link set "$IFNAME" down; sleep 1.2
    ip link set "$IFNAME" up;   sleep 1.2
  done
  echo "  (interface $IFNAME left up)"
}

scenario_psi_pressure() {
  echo "[mempress] driving real memory pressure for 60s — needs stress-ng..."
  if ! command -v stress-ng >/dev/null; then
    echo "  install with: sudo apt-get install -y stress-ng"
    return
  fi
  # 70% of total RAM, sustained 60s. Should trip PSI threshold easily.
  TOTAL_KB=$(awk '/MemTotal/{print $2}' /proc/meminfo)
  USE_BYTES=$(( TOTAL_KB * 1024 * 7 / 10 ))
  stress-ng --vm 2 --vm-bytes "$USE_BYTES" --vm-method all --timeout 60s &
  echo "  PID $! — wait ~30s, watch for memory_pressure_high warning"
}

scenario_lifecycle() {
  echo "[lifecycle] WILL REBOOT THE HOST in 5s — Ctrl-C to abort"
  for i in 5 4 3 2 1; do echo "  $i..."; sleep 1; done
  systemctl reboot --check-inhibitors=no
}

scenario_host_offline() {
  echo "[heatmap]  stopping the agent for 15s — heatmap should turn red, then host_offline alert"
  systemctl stop monitor-agent
  for i in 15 10 5 0; do
    echo "  agent down for ${i}s..."
    sleep 5
  done
  echo "  restarting agent..."
  systemctl start monitor-agent
}

scenario_custom_syslog_rule() {
  echo "[syslog]   firing a string that the default syslog_rules ignores."
  echo "          add to config.toml under [modules.syslog_rules]:"
  cat <<'EOF'

  [[modules.syslog_rules.rules]]
  name = "uma_test"
  regex = "UMA_TEST_INJECTION"
  severity = "warning"
  category = "syslog"
  title = "UMA test rule fired"
  message = "Synthetic test event — your syslog_rules pipeline works. Match: {match}"

EOF
  inject "UMA_TEST_INJECTION timestamp=$(date +%s) — if you see this in the GUI, the rules engine is alive."
}

# ---------- Dispatcher ----------
SCENARIOS=(
  memory_ue memory_ce cpu_mce
  pcie_aer_uncorr pcie_aer_corr
  controller_lockup smartpqi_lockup
  hung_task softlockup
  oom oom_storm fs_error
  nic_flap psi_pressure
  custom_syslog_rule
  host_offline
  lifecycle
)

run_one() {
  local n="$1"
  local fn="scenario_$n"
  if declare -F "$fn" >/dev/null; then
    "$fn"
  else
    echo "unknown scenario: $n" >&2
    exit 1
  fi
}

case "${1:-}" in
  all)
    # Skip the destructive ones from "all".
    for s in memory_ue memory_ce cpu_mce pcie_aer_uncorr pcie_aer_corr \
             controller_lockup smartpqi_lockup hung_task softlockup \
             oom oom_storm fs_error nic_flap custom_syslog_rule; do
      run_one "$s"
      sleep 0.5
    done
    echo
    echo "Done. Open the GUI to verify each alert appeared."
    echo "Skipped destructive: psi_pressure, host_offline, lifecycle (run individually)."
    ;;
  list)
    printf '  %s\n' "${SCENARIOS[@]}"
    ;;
  "")
    cat <<EOF
Usage:
  sudo $0 all        — run every non-destructive scenario in sequence
  sudo $0 list       — list available scenarios
  sudo $0 <name>     — run one scenario

Destructive scenarios (run individually only):
  psi_pressure  — drives 70% RAM allocation for 60s
  host_offline  — stops monitor-agent for 15s
  lifecycle     — REBOOTS the host (the pre-shutdown alert demo)
EOF
    ;;
  *)
    run_one "$1"
    ;;
esac
