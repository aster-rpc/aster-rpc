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
mod keys;
mod lease;
mod net;
mod node;
mod rpc;
mod rpc_client;
mod ticket;
mod topology;
mod upcalls;

/// Exact version embedded by the shared Rust core build.
#[napi]
pub fn version() -> String {
    aster_transport_core::VERSION.to_string()
}
