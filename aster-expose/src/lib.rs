//! `aster-expose` — publish a local HTTP service over Aster, and the
//! ngrok-shaped inverse (a public edge that reverse-proxies browser traffic
//! through Aster to a backend).
//!
//! Design: `docs/_internal/working_ideas/aster-expose-http.md`.
//!
//! This crate is the **L7 terminate-and-relay** companion to the hyper-free
//! transport in `aster_transport_core`. Core provides the
//! [`AsterStreamIo`](aster_transport_core::stream_io::AsterStreamIo) duplex
//! adapter and the tunnel/session primitives; this crate owns the hyper/tower
//! wiring that core deliberately does not depend on.
//!
//! Build order (see the design doc §7): the `AsterStreamIo` → hyper round-trip
//! is validated first (`tests/hyper_spike.rs`), then the thin H3-object codec
//! ([`codec`]), then the service-keyed acceptor, `expose_local_http`,
//! `dial_http`, and `HttpEdge`.

pub mod body;
pub mod codec;
pub mod control;
#[cfg(feature = "edge")]
pub mod edge;
pub mod node;
pub mod relay;

/// The exact Salvo crate used by the public edge, for composing Aster routes
/// into a larger Salvo application without resolving a second Salvo source.
#[cfg(feature = "edge")]
pub use salvo;
