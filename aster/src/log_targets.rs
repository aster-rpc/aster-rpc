//! The [`tracing`] targets this crate logs under.
//!
//! `aster` emits events and spans through the `tracing` facade and installs no
//! subscriber: the application chooses one (e.g. `tracing-subscriber`) and
//! filters by these targets. Every target starts with `aster::`, so
//! `RUST_LOG=aster=debug` turns on all of them. With no subscriber installed
//! the instrumentation costs a level check per site and allocates nothing.
//!
//! | Target | Level | What |
//! |---|---|---|
//! | [`SERVER`] | info | `AsterServer` started / shutting down |
//! | [`SERVER`] | debug | RPC serve loop start/end; connection closed (with sessions reaped); a connection on an ALPN nobody handles |
//! | [`DISPATCH`] | debug | one `rpc` span per native call (service, method, call id, peer) and an `rpc completed` event with the final status and duration for every call; `rpc refused` for calls stopped before a handler; deadline expiry |
//! | [`DISPATCH`] | warn | a corrupt request frame -- one that does not inflate, or a client-stream / bidi request that does not decode (the call fails with `INVALID_ARGUMENT`) |
//! | [`SESSION`] | debug | session opened, sessions reaped with their connection, HTTP session ended |
//! | [`FOREIGN`] | debug / trace | foreign (binding) dispatch: call delivered (trace), cancelled / expired / abandoned (debug) |
//! | [`ADMISSION`] | debug / info | Gate 0: inbound peer accepted (debug) or rejected (info); outbound dial allowed / blocked (debug) |
//! | [`HEALTH`] | info / debug | health listener bound and shut down (info); per-connection serve errors (debug) |
//!
//! The lower layers log under their own crate names: `aster_transport_core`
//! (reactor, framing) and `iroh` / `iroh_*` (QUIC, relay, discovery).

/// RPC serve loop lifecycle and connections.
pub const SERVER: &str = "aster::rpc::server";
/// Per-call dispatch: spans, completions, refusals, deadlines, corrupt frames.
pub const DISPATCH: &str = "aster::rpc::dispatch";
/// Session-scoped service instances.
pub const SESSION: &str = "aster::rpc::session";
/// Foreign (language-binding) dispatch.
pub const FOREIGN: &str = "aster::rpc::foreign";
/// Gate 0 peer admission decisions.
pub const ADMISSION: &str = "aster::admission";
/// The `/healthz`, `/readyz`, `/metrics` HTTP listener.
pub const HEALTH: &str = "aster::health";
