//! # Aster
//!
//! Native Rust API for **Aster** — peer-to-peer transport built on
//! [iroh](https://github.com/n0-computer/iroh): content-addressed blob storage,
//! CRDT documents, and gossip pub-sub over QUIC.
//!
//! This crate is an idiomatic facade over the `aster_transport_core` backend;
//! it is the supported entry point for Rust consumers (the language bindings —
//! Python, TypeScript, Java, Kotlin — wrap the same core).
//!
//! ## Quick start
//!
//! ```no_run
//! use aster::{AsterConfig, Node, RelayMode};
//!
//! # async fn run() -> aster::Result<()> {
//! let node = Node::start(
//!     AsterConfig::builder()
//!         .persistent("/var/lib/my-app")
//!         .relay(RelayMode::Default)
//!         .build(),
//! )
//! .await?;
//!
//! # #[cfg(feature = "blobs")]
//! # {
//! let hash = node.blobs().add_bytes(b"hello".to_vec()).await?;
//! let bytes = node.blobs().read_to_bytes(&hash).await?;
//! assert_eq!(bytes, b"hello");
//! # }
//!
//! node.shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! ## Cargo features
//!
//! `blobs`, `docs`, `gossip` are enabled by default and gate the corresponding
//! [`Node`] accessors. `discovery`, `hooks`, `metrics` are opt-in. Admission
//! ([`Node::take_admission`]) additionally requires the node to be started with
//! [`AsterConfigBuilder::hooks(true)`].
//!
//! `binding-support` is for the language bindings (PyO3, NAPI, C ABI) only: it
//! adds `#[doc(hidden)]` accessors to the transport-core handles they drive
//! directly (reactor, client-call pool, tunnels, raw hook receiver). It is not
//! a stable API. See the "binding profile" note in `Cargo.toml`.
//!
//! ## Native interop
//!
//! The facade is the stable entry point. Advanced Rust consumers can use
//! [`native`] for the exact Iroh, Fory and (when enabled) Salvo crates carried
//! by this release, plus clone-cheap handles to the running node.

// Lets `#[derive(aster::AsterType)]` (which emits `::aster::…` paths) be used on
// types defined inside this crate itself, e.g. `rpc::Empty`.
extern crate self as aster;

/// Exact Aster build version (`major.minor.commit-offset`) embedded at compile time.
pub const VERSION: &str = aster_transport_core::VERSION;

mod admission;
#[cfg(feature = "binding-support")]
#[doc(hidden)]
pub mod binding_support;
mod config;
pub mod crypto;
mod error;
pub mod grants;
mod id;
pub mod lease;
pub mod log_targets;
pub mod native;
mod net;
mod node;
mod ticket;
pub mod topology;

#[cfg(feature = "attestation")]
pub mod attestation;
#[cfg(feature = "blobs")]
mod blobs;
#[cfg(feature = "docs")]
mod docs;
#[cfg(feature = "expose")]
pub mod expose;
#[cfg(feature = "gossip")]
mod gossip;
/// Ops endpoints: `/healthz`, `/readyz`, `/metrics` (feature `health`).
#[cfg(feature = "health")]
pub mod ops;
#[cfg(feature = "rpc")]
pub mod rpc;

/// Coordination: the replicated register/lease group (§5 of the coordination
/// plan). `coordination-client` compiles the wire contract and client stub;
/// `coordination` adds the voter.
#[cfg(feature = "coordination-client")]
pub mod coordination;

#[cfg(feature = "coordination")]
mod durable_storage;

pub use admission::{alpns, Admission, ConnectRequest, Gate0, HandshakeRequest};
pub use aster_transport_core::hpke_envelope::{
    hpke_generate_keypair, hpke_open, hpke_public_key_from_private, hpke_seal, HpkeEnvelope,
    HpkeKeyPair, HPKE_ENVELOPE_ALG,
};
pub use aster_transport_core::namespace::DecryptedCapabilityPayload;
pub use aster_transport_core::trust::{
    AdmissionSource, GateDecision, GatePolicy, HookFailureMode, PeerAdmission, PeerAdmissionStore,
    TrustMode,
};
pub use config::{
    AsterConfig, AsterConfigBuilder, AsterIdentityPeer, LoadedIdentity, PublishedServices,
    RelayMode,
};

/// The Aster RPC ALPN (`aster/1`). Also [`alpns::RPC`] and, with the `rpc`
/// feature, `rpc::RPC_ALPN`.
pub const RPC_ALPN: &[u8] = aster_transport_core::reactor::RPC_ALPN;
pub use crypto::{
    hpke_x25519_public_from_identity, hpke_x25519_public_from_node_id,
    hpke_x25519_secret_from_identity,
};
pub use error::{Error, Result};
pub use id::{
    AuthorId, Hash, NamespaceCapability, NamespaceId, NamespaceSecret, NodeAddr, NodeId, PublicKey,
    SecretKey,
};
pub use net::{Connection, PathInfo, PathRemote, RecvStream, SendStream};
pub use node::Node;
pub use ticket::{Credential, Ticket};

#[cfg(feature = "blobs")]
pub use blobs::{BlobFormat, BlobStatus, Blobs, FetchReport, FetchStrategy, ProviderOutcome};
#[cfg(feature = "docs")]
pub use docs::{
    default_author_id, AuthorityFingerprint, AuthorityReport, Doc, DocEntry, DocEvent, DocEvents,
    Docs, DownloadPolicy, LocalAccess, RetireOutcome, ShareMode,
};
#[cfg(feature = "expose")]
pub use expose::Expose;
#[cfg(feature = "gossip")]
pub use gossip::{Gossip, GossipEvent, GossipTopic};

// Convenience: `#[aster::service]` and `#[derive(aster::AsterType)]` at the crate
// root (both also available under `aster::rpc`).
#[cfg(feature = "rpc")]
pub use rpc::{service, AsterServer, AsterType};
