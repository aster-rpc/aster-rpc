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
//! | [`DISPATCH`] | debug | one `rpc` span per call, native or foreign (service, method, call id, peer), and an `rpc completed` event with the final status and duration for every call; `rpc refused` for calls stopped before a handler; deadline expiry |
//! | [`DISPATCH`] | info | with the `otel` feature and an OTel tracer provider, the `rpc` span is created at info instead (with `otel.kind = server` and the caller's trace context as parent), so the OTel span filter can export it without debug logs |
//! | [`CLIENT`] | info | with the `otel` feature and an OTel tracer provider: one `rpc.client` span per outgoing call, whose trace context goes out in the call metadata |
//! | [`DISPATCH`] | warn | a corrupt request frame -- one that does not inflate, or a client-stream / bidi request that does not decode (the call fails with `INVALID_ARGUMENT`) |
//! | [`IDENTITY`] | debug | gateway identity (`aster-on-behalf-of`): `on-behalf-of accepted` (fields `principal`, `actor`, `iss`, `sub`, `jti`) and `on-behalf-of refused` (`actor`, `service`, `method`, `reason`); the call's `rpc` span also records `principal`, `actor`, `obo_iss`, `obo_jti` |
//! | [`SESSION`] | debug | session opened, sessions reaped with their connection, HTTP session ended |
//! | [`FOREIGN`] | debug / trace | foreign (binding) dispatch: call delivered (trace), cancelled / expired / abandoned (debug) |
//! | [`ADMISSION`] | debug / info | Gate 0: inbound peer accepted (debug) or rejected (info); outbound dial allowed / blocked (debug). With Enrollment, one event per connection decision (fields `peer`, `alpn`, `decision`, `reason`, `source` or `detail`): allows at debug, denials at info, at most one info line per denied peer per 10 s and 32 a second overall (the rest at debug, counted in `suppressed`); an outbound dial blocked because on-demand enrollment failed (debug) |
//! | [`UPCALL`] | trace / debug / warn | binding upcalls: issued / answered (trace), cancelled or late answers (debug), a decision upcall that fell back to its failure mode (warn) |
//! | [`OPS`] | info / debug | ops (health/metrics) listener bound and shut down (info); per-connection serve errors (debug) |
//!
//! The lower layers log under their own crate names: `aster_transport_core`
//! (reactor, framing) and `iroh` / `iroh_*` (QUIC, relay, discovery).

/// RPC serve loop lifecycle and connections.
pub const SERVER: &str = "aster::rpc::server";
/// Per-call dispatch: spans, completions, refusals, deadlines, corrupt frames.
pub const DISPATCH: &str = "aster::rpc::dispatch";
/// Outgoing calls (`RpcConnection`): the client span, with the `otel` feature.
pub const CLIENT: &str = "aster::rpc::client";
/// Gateway identity: on-behalf-of calls accepted or refused.
pub const IDENTITY: &str = "aster::rpc::identity";
/// Session-scoped service instances.
pub const SESSION: &str = "aster::rpc::session";
/// Foreign (language-binding) dispatch.
pub const FOREIGN: &str = "aster::rpc::foreign";
/// Gate 0 peer admission decisions.
pub const ADMISSION: &str = "aster::admission";
/// The `/healthz`, `/readyz`, `/metrics` HTTP listener (`aster::ops`).
pub const OPS: &str = "aster::ops";
/// Upcalls into binding code (`binding_support::upcall`).
pub const UPCALL: &str = "aster::upcall";
