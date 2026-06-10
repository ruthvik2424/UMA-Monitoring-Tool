//! Per-category monitoring modules. Each module is an async task spawned by
//! `main.rs`. They're loosely coupled — they share only the `AlertSink`
//! and the `KmsgLine` broadcast channel. New modules should follow this
//! contract:
//!
//!   pub fn spawn(deps: Deps) -> JoinHandle<()>;
//!
//! and never panic — log + degrade-to-disabled if a probe fails at startup.

pub mod nvme;
pub mod lifecycle;
pub mod network_nic;
pub mod storage_controller;
pub mod storage_io;
pub mod mempressure;
pub mod oshang;
pub mod thermal;
pub mod disk_smart;
pub mod memory_ecc;
pub mod cpu_mce;
pub mod pcie_aer;
pub mod gpu_nvidia;
pub mod gpu_amd;
pub mod bmc_redfish;
pub mod bmc_eventlog;
pub mod syslog_rules;
pub mod boot;
