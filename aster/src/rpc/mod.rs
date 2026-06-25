//! Aster RPC — Rust-native services over the `aster_transport_core` reactor.
//!
//! Behind the `rpc` Cargo feature. This is the Rust *language layer* of the
//! Aster RPC framework (the peer of `bindings/python/aster`, etc.): it owns the
//! envelope + payload codec, server dispatch, the client stub, and (in later
//! steps) manifest publication and the call-time auth gate. Transport (framing,
//! reactor, stream pool) stays in `core`.
//!
//! Scope today is **Rust↔Rust** with all four call patterns. Cross-binding RPC
//! *payloads* are deferred on the Fory version gap; manifests / contract-ids are
//! cross-binding from the start (computed by core's Fory-independent canonical
//! encoder). See `docs/_internal/aster-rust.md` Step 2.
//!
//! ## Shape
//!
//! - [`Server`] owns the reactor accept+dispatch loop. Register one or more
//!   [`ServiceDispatch`] implementations, then [`Server::serve`].
//! - [`RpcConnection`] (via [`Node::rpc_connect`](crate::Node::rpc_connect)) is
//!   the client; its per-pattern helpers operate on raw payload bytes, so a
//!   generated stub (or hand-written caller) supplies encode/decode.
//! - [`Call`] is the per-call server context: pull requests, emit responses,
//!   finish with an [`RpcStatus`].

pub mod auth;
pub mod client;
pub mod codec;
pub mod contract;
pub mod empty;
pub mod interceptor;
pub mod projection;
pub mod runtime;
pub mod server;
pub mod status;
pub mod streaming;

// Manifest publication needs docs + blobs as well as rpc.
#[cfg(all(feature = "docs", feature = "blobs"))]
pub mod publish;

pub use auth::{
    require_all_of, require_any_of, require_role, AttributeStore, AuthContext, AuthOutcome,
    Authenticator, CapabilityKind, CapabilityRequirement, ROLE_ATTRIBUTE,
};
pub use client::{BidiSink, ResponseStream, RpcConnection};
pub use codec::{
    new_payload_fory, CallHeader, CodecError, RpcStatus, SerializationMode, StreamHeader,
};
pub use contract::{
    contract_id, encode_scalar_default, make_field, message_type_def, AsterType, ContractManifest,
    Leaf, ManifestMethod, MethodDef, MethodPattern, ScalarDefault, ScopeKind, ServiceContract,
    TypeDef, WireField,
};
pub use empty::Empty;
pub use interceptor::{CallContext, CircuitBreaker, Interceptor, Pipeline, RetryPolicy};
pub use projection::{Projection, ProjectionRegistry};
#[cfg(all(feature = "docs", feature = "blobs"))]
pub use publish::{
    fetch_and_verify_contract, publish_contract, FetchedContract, PublishedContract,
};
pub use runtime::{AsterServer, AsterServerBuilder};
pub use server::{
    Call, CallParts, Dispatcher, HttpTransport, Server, ServerHandle, ServiceDispatch, RPC_ALPN,
};

/// Reactor frame types, re-exported so a non-Iroh transport can build
/// [`CallParts`] (wire request frames into `request_receiver`, drain
/// `OutgoingFrame`s from `response_sender`).
pub use aster_transport_core::reactor::{OutgoingFrame, RequestFrame};
pub use status::{status_from_error, StatusCode};
pub use streaming::{BidiCall, MessageStream, RequestStream, ResponseSink};

/// The Apache Fory runtime, re-exported for `#[aster::service]`-generated code
/// (payload ser/de). Build one with [`new_payload_fory`].
pub use fory_core::Fory;

/// `#[async_trait]`, re-exported so service traits + impls don't need a direct
/// `async-trait` dependency. Annotate your service `impl` with
/// `#[aster::rpc::async_trait]`.
pub use async_trait::async_trait;

/// `#[aster::service(name = "...", version = N)]` — turn a trait of unary
/// `async fn(&self, Req) -> Result<Resp>` methods into a `…Server<T>`
/// dispatcher, a `…Client` stub, and a cross-binding `ServiceContract`.
pub use aster_macros::service;

/// `#[derive(AsterType)]` — generates the contract `TypeDef` for an RPC payload
/// struct (macro namespace; the trait of the same name is re-exported above).
pub use aster_macros::AsterType;
