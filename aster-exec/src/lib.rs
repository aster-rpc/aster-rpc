//! Aster Exec v1 job contract and local supervisor primitives.
//!
//! This crate does not expose a network service until a qualified lifetime
//! owner, verified Exec grants, and durable audit admission are installed.

#[cfg(feature = "policy")]
pub mod authorization;
pub mod contract;
pub mod ledger;
#[cfg(feature = "policy")]
pub mod policy;
#[cfg(feature = "service")]
pub mod service;
pub mod spool;
pub mod state;

#[cfg(target_os = "linux")]
pub mod linux_cgroup;
#[cfg(target_os = "linux")]
pub mod linux_supervisor;
#[cfg(target_os = "linux")]
pub mod local_control;
#[cfg(target_os = "linux")]
pub mod systemd_unit;

#[cfg(target_os = "linux")]
pub mod linux_helper;

#[cfg(all(target_os = "linux", feature = "service"))]
pub mod linux_adapter;
