//! NAPI-RS bindings for Aster transport.
//!
//! Wraps `aster_transport_core` to expose Iroh P2P networking to
//! Node.js/Bun/Deno via NAPI. Mirrors the PyO3 bindings in
//! `bindings/python/rust/src/`.

use napi_derive::napi;

mod blobs;
mod call;
mod contract;
mod docs;
mod error;
mod gossip;
mod hooks;
mod net;
mod node;
mod reactor;
mod ticket;

/// Module version (matches package.json).
#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
