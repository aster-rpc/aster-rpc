//! NAPI-RS bindings for Aster transport.
//!
//! Wraps `aster_transport_core` to expose Iroh P2P networking to
//! Node.js/Bun/Deno via NAPI. Mirrors the PyO3 bindings in
//! `bindings/python/rust/src/`.

use napi_derive::napi;

mod error;
mod node;
mod net;
mod blobs;
mod docs;
mod gossip;
mod hooks;
mod contract;

/// Module version (matches package.json).
#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
