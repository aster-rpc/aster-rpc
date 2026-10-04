//! NAPI-RS bindings for Aster transport.
//!
//! Wraps the `aster` crate (`Node`, `AsterConfig`, identity files — plan D1)
//! and, through `aster::binding_support`, the transport-core pieces the
//! binding still drives directly (reactor, call pool, tunnels, hooks).

#![allow(dead_code)] // NAPI exports are called from JavaScript, not Rust tests.

use napi_derive::napi;

mod admission;
mod attestation;
mod blobs;
mod call;
mod config;
mod contract;
mod coordination;
mod crypto;
mod docs;
mod envelope;
mod error;
mod gossip;
mod grants;
mod keys;
mod lease;
mod net;
mod node;
mod reactor;
mod rpc;
mod ticket;
mod topology;

/// Exact version embedded by the shared Rust core build.
#[napi]
pub fn version() -> String {
    aster_transport_core::VERSION.to_string()
}
