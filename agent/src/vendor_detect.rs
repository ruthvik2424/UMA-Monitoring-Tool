//! Vendor detection. Runs once at startup; the result is cached and passed
//! to every module so we never poke vendor-specific tools that aren't
//! installed (or, worse, hang waiting for a missing BMC).

use std::path::Path;
use std::process::Command;
use uma_shared::VendorInfo;

#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // tool-presence flags consumed by future modules + future config branches
pub struct VendorProfile {
    pub info: VendorInfo,
    pub has_smartctl: bool,
    pub has_nvme_cli: bool,
    pub has_ipmitool: bool,
    pub has_ssacli: bool,
    pub has_perccli: bool,
    pub has_storcli: bool,
    pub has_megacli: bool,
    pub has_nvml: bool,
    pub has_rocm_smi: bool,
    pub has_ethtool: bool,
}

impl VendorProfile {
    pub fn detect() -> Self {
        let info = VendorInfo {
            system_manufacturer: read_dmi("sys_vendor"),
            system_product_name: read_dmi("product_name"),
            bios_version: read_dmi("bios_version"),
            bmc_vendor: detect_bmc_vendor(),
            gpu_vendors: detect_gpu_vendors(),
            kernel: read_kernel(),
            os_release: read_os_release(),
        };
        Self {
            info,
            has_smartctl: which("smartctl"),
            has_nvme_cli: which("nvme"),
            has_ipmitool: which("ipmitool"),
            has_ssacli: which("ssacli"),
            has_perccli: which("perccli64") || which("perccli"),
            has_storcli: which("storcli64") || which("storcli"),
            has_megacli: which("megacli") || which("MegaCli64"),
            has_nvml: Path::new("/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1").exists()
                || Path::new("/usr/lib64/libnvidia-ml.so.1").exists()
                || Path::new("/lib/x86_64-linux-gnu/libnvidia-ml.so.1").exists(),
            has_rocm_smi: which("rocm-smi"),
            has_ethtool: which("ethtool"),
        }
    }
}

fn read_dmi(field: &str) -> String {
    std::fs::read_to_string(format!("/sys/class/dmi/id/{field}"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn read_kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn read_os_release() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_string())
        })
        .unwrap_or_default()
}

fn detect_bmc_vendor() -> String {
    // 1. DMI hint.
    let mfr = read_dmi("sys_vendor").to_lowercase();
    if mfr.contains("hpe") || mfr.contains("hewlett") { return "hpe".into(); }
    if mfr.contains("dell") { return "dell".into(); }
    if mfr.contains("supermicro") { return "supermicro".into(); }
    if mfr.contains("lenovo") { return "lenovo".into(); }

    // 2. ipmitool fallback.
    if let Ok(out) = Command::new("ipmitool").args(["mc", "info"]).output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).to_lowercase();
            if s.contains("hewlett") || s.contains("hpe") { return "hpe".into(); }
            if s.contains("dell") { return "dell".into(); }
            if s.contains("supermicro") { return "supermicro".into(); }
            if s.contains("lenovo") { return "lenovo".into(); }
        }
    }
    "unknown".into()
}

fn detect_gpu_vendors() -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(out) = Command::new("lspci").arg("-nn").output() {
        let s = String::from_utf8_lossy(&out.stdout).to_lowercase();
        if s.contains("nvidia") { v.push("nvidia".into()); }
        if s.contains("amd/ati") || s.contains("advanced micro devices") && s.contains("vga") {
            v.push("amd".into());
        }
        if s.contains("intel") && (s.contains("vga") || s.contains("display")) {
            v.push("intel".into());
        }
    }
    v
}

fn which(bin: &str) -> bool {
    if let Ok(path) = std::env::var("PATH") {
        for p in path.split(':') {
            if Path::new(p).join(bin).is_file() {
                return true;
            }
        }
    }
    false
}
