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
//! ## Dependency note
//!
//! Aster pins iroh / noq to patched forks. An external Rust project depending on
//! this crate via git **must** copy the `[patch.crates-io]` block from the repo
//! root `Cargo.toml` into its own — see the crate README.

mod admission;
mod config;
mod error;
mod id;
mod net;
mod node;
mod ticket;

#[cfg(feature = "attestation")]
pub mod attestation;
#[cfg(feature = "blobs")]
mod blobs;
#[cfg(feature = "docs")]
mod docs;
#[cfg(feature = "gossip")]
mod gossip;
#[cfg(feature = "rpc")]
pub mod rpc;

pub use admission::{alpns, Admission, ConnectRequest, Gate0, HandshakeRequest};
pub use config::{AsterConfig, AsterConfigBuilder, RelayMode};
pub use error::{Error, Result};
pub use id::{
    AuthorId, Hash, NamespaceId, NamespaceSecret, NodeAddr, NodeId, PublicKey, SecretKey,
};
pub use net::{Connection, RecvStream, SendStream};
pub use node::Node;
pub use ticket::{Credential, Ticket};
pub use aster_transport_core::trust::{
    AdmissionSource, GateDecision, GatePolicy, HookFailureMode, PeerAdmission, PeerAdmissionStore,
    TrustMode,
};

#[cfg(feature = "blobs")]
pub use blobs::{BlobFormat, BlobStatus, Blobs};
#[cfg(feature = "docs")]
pub use docs::{
    default_author_id, Doc, DocEntry, DocEvent, DocEvents, Docs, DownloadPolicy, ShareMode,
};
#[cfg(feature = "gossip")]
pub use gossip::{Gossip, GossipEvent, GossipTopic};

// Convenience: `#[aster::service]` and `#[derive(aster::AsterType)]` at the crate
// root (both also available under `aster::rpc`).
#[cfg(feature = "rpc")]
pub use rpc::{service, AsterType};
