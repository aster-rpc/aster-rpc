//! Binding support — **not a stable API**.
//!
//! Enabled by the `binding-support` feature. The language bindings (PyO3
//! `bindings/python/rust`, NAPI `bindings/typescript/native`, the C ABI `ffi/`)
//! wrap the `aster` crate (plan D1), but still drive a few transport-core
//! pieces directly: the reactor ring, the client-call pool, tunnels and the raw
//! admission hook channels. This module is the one sanctioned way to reach them
//! from `aster` handles, so a binding never has to construct core objects in
//! parallel to the facade.
//!
//! | From | Accessor | Gives |
//! |---|---|---|
//! | [`AsterConfig`](crate::AsterConfig) | `core_endpoint_config()` | lowered [`CoreEndpointConfig`] |
//! | [`Node`](crate::Node) | `core_node()` / `from_core_node(..)` | [`CoreNode`] (reactor, net/blobs/docs/gossip clients, `take_hook_receiver`) |
//! | [`Connection`](crate::Connection) | `core_connection()` / `from_core_connection(..)` | [`CoreConnection`] (pool keys, tunnels, framing) |
//! | [`Admission`](crate::Admission) | `from_core_receiver(..)` / `into_core_receiver()` | [`CoreHookReceiver`] |
//! | `rpc::RpcConnection` (`rpc`) | `core_connection()` / `from_core_connection(..)` | [`CoreConnection`] |
//!
//! The transport core is re-exported as [`core`] so a binding links exactly
//! the version `aster` was built against; [`reactor`], [`pool`], [`tunnel`]
//! and [`ring`] are re-exported for the hot paths.
//!
//! Binding profile (see `aster/Cargo.toml`): `default` + `rpc` +
//! `binding-support`, plus `metrics`, and optionally `coordination-client` /
//! `expose` as the binding grows those surfaces.

/// The transport core this `aster` build links.
pub use aster_transport_core as core;
pub use aster_transport_core::{pool, reactor, ring, tunnel};
pub use aster_transport_core::{
    CoreConnection, CoreEndpointConfig, CoreHookReceiver, CoreNode, CoreRecvStream, CoreSendStream,
};

use crate::Node;

/// Start the transport-core reactor (the SPSC ring the Python and C ABI
/// bindings poll) for `node` on `runtime`. Equivalent to
/// [`reactor::start_reactor_on`] with the node's core handle.
pub fn start_reactor_on(
    runtime: &tokio::runtime::Handle,
    node: &Node,
    channel_capacity: usize,
) -> reactor::ReactorHandle {
    reactor::start_reactor_on(runtime, node.core_node().clone(), channel_capacity)
}

/// Foreign (binding-implemented) services served by `aster::rpc::Server`:
/// call events, the shared call table, and the ring sink.
#[cfg(feature = "rpc")]
pub mod dispatch;
