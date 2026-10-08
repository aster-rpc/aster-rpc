//! NAPI-RS bindings for Aster transport.
//!
//! Wraps the `aster` crate (`Node`, `AsterConfig`, identity files — plan D1).
//! RPC goes through `aster::rpc` (`rpc.rs`: `DispatchServer` on
//! `aster::rpc::Server`; `rpc_client.rs`: `RpcConnection`); the transport-core
//! pieces the binding still drives directly (tunnels, hooks) come through
//! `aster::binding_support`.

#![allow(dead_code)] // NAPI exports are called from JavaScript, not Rust tests.

use napi_derive::napi;

mod admission;
mod attestation;
mod audit_outbox;
mod blobs;
mod config;
mod contract;
mod coordination;
mod crypto;
mod docs;
mod envelope;
mod error;
mod expose;
mod gossip;
mod grants;
mod health;
mod keys;
mod lease;
mod net;
mod node;
mod observability;
mod rpc;
mod rpc_client;
mod ticket;
mod topology;
mod upcalls;

/// Opt-in Rust log sink (`ASTER_LOG_STDERR=1`, filter `ASTER_LOG`), installed
/// when the addon loads.
#[napi_derive::module_init]
fn init_observability() {
    aster_otel::init_stderr_from_env();
}

/// Exact version embedded by the shared Rust core build.
#[napi]
pub fn version() -> String {
    aster_transport_core::VERSION.to_string()
}
