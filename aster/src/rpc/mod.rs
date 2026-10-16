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

pub mod aster_client;
pub mod auth;
pub mod baseline;
pub mod client;
pub mod codec;
pub mod context;
pub mod contract;
mod deadline;
pub mod drain;
pub mod dynamic;
pub mod empty;
pub(crate) mod foreign;
pub mod identity;
pub mod interceptor;
pub mod lease;
pub mod node_config;
pub(crate) mod observe;
pub mod projection;
pub mod proxy;
pub mod runtime;
pub mod scalars;
pub mod server;
mod sessions;
pub mod status;
pub mod streaming;
pub mod surface;

// Manifest publication needs docs + blobs as well as rpc.
#[cfg(all(feature = "docs", feature = "blobs"))]
pub mod publish;

pub use aster_client::{AsterClient, AsterClientBuilder};
pub use auth::{
    require_all_of, require_any_of, require_role, AttributeSource, AttributeStore, AuthContext,
    AuthOutcome, Authenticator, CapabilityKind, CapabilityRequirement, RoleChangeError,
    SourcedAttributes, ROLE_ATTRIBUTE,
};
pub use client::{
    BidiSink, CallOptions, RequestSink, ResponseFuture, ResponseStream, RpcConnection, RpcSession,
};
pub use codec::{
    decode_rpc_status, encode_stream_header, new_payload_fory, CallHeader, CodecError, RpcStatus,
    SerializationMode, StreamHeader,
};
pub use context::CallContext;
pub use contract::{
    contract_id, encode_scalar_default, make_field, make_union_variant, message_shape,
    message_type_def, resolve_type_def, shape_field, shape_variant, union_shape, union_type_def,
    AsterType, BigInt, ContractArtifacts, ContractManifest, Date, Decimal, Leaf, ManifestMethod,
    MethodDef, MethodPattern, PayloadRegistry, ScalarDefault, ScopeKind, ServiceContract,
    ShapeField, ShapeFn, ShapeVariant, Timestamp, TypeDef, TypeDefSet, TypeShape, UnionVariantDef,
    WireField,
};
pub use drain::{draining_status, DrainReport, DRAINING_DETAIL_KEY, DRAIN_CLOSE_CODE};
pub use dynamic::{DynamicService, MethodDesc, MethodLimits, TypeRef, ValidationError};
pub use empty::Empty;
pub use identity::{ForwardedIdentity, IdentitySettings, GATEWAY_ROLE, ON_BEHALF_OF_METADATA};
pub use interceptor::{CircuitBreaker, ClientCallContext, Interceptor, Pipeline, RetryPolicy};
pub use node_config::{AttrValue, NodeProperties, CONFIG_ADMIN_ROLE};
#[cfg(feature = "docs")]
pub use node_config::{NodeConfigClient, NodeConfigHandle};
#[cfg(feature = "otel")]
pub use observe::otel::{current_trace_context, with_trace_context, ExtractContext};
pub use projection::{Projection, ProjectionRegistry};
pub use proxy::{DynContract, DynResponseStream, DynValue, DynamicClient};
#[cfg(all(feature = "docs", feature = "blobs"))]
pub use publish::{
    fetch_and_verify_contract, publish_contract, FetchedContract, PublishedContract,
};
pub use runtime::{AsterServer, AsterServerBuilder};
pub use scalars::{rescale_decimal, FixedDecimal, ScalarError, Ulid, Uuid, MAX_DECIMAL_PRECISION};
pub use server::{
    AlpnHandler, Call, CallParts, Dispatcher, HttpTransport, PeerProvenance, Route, Routed, Server,
    ServerDrainer, ServerHandle, ServiceDispatch, DEFAULT_HANDLER_TIMEOUT, RPC_ALPN,
};
pub use surface::{
    ManifestPublication, PublishedService, RpcSurface, RpcSurfaceBuilder, SurfaceActivation,
    SurfaceState,
};

/// Reactor frame types, re-exported so a non-Iroh transport can build
/// [`CallParts`] (wire request frames into `request_receiver`, drain
/// `OutgoingFrame`s from `response_sender`).
pub use aster_transport_core::reactor::{OutgoingFrame, RequestFrame};
pub use sessions::DEFAULT_MAX_SESSIONS_PER_CONNECTION;
pub use status::{status_from_error, StatusCode};
pub use streaming::{BidiCall, MessageStream, RequestStream, ResponseSink};

/// The Apache Fory runtime, re-exported for `#[aster::service]`-generated code
/// (payload ser/de). Build one with [`new_payload_fory`].
///
/// Note: `#[derive(ForyUnion)]` payload enums must declare their mandatory
/// forward-compat variant as `#[fory(unknown)] Unknown(fory_core::UnknownCase)`
/// — the ForyUnion derive only accepts that exact path spelling, so it cannot
/// be re-exported here. That variant is a runtime artifact; the `AsterType`
/// derive excludes it from the cross-binding contract.
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
