//! Escape hatch to the exact native crates carried by this Aster release.
//!
//! The rest of [`aster`](crate) is the compatibility-oriented facade. This
//! module is for Rust applications that need an upstream API which the facade
//! does not wrap yet. Re-exporting the crates here guarantees that values from
//! [`Node::native_endpoint`](crate::Node::native_endpoint) and the protocol
//! handles are the same concrete types as the dependency graph selected by
//! Aster; consumers must not guess a second Iroh or Salvo revision.
//!
//! These APIs follow their upstream crates' compatibility policy. Aster may
//! change the pinned native stack in an Aster minor release even when its own
//! facade remains source compatible.

pub use iroh;
pub use iroh_docs;
pub use iroh_tickets;

#[cfg(any(feature = "blobs", feature = "docs"))]
pub use iroh_blobs;
#[cfg(feature = "gossip")]
pub use iroh_gossip;

// Fory's derive macros currently emit absolute `::fory_core` paths. They are
// re-exported for discovery and runtime use, but a crate deriving Fory payload
// types must still list fory-core and fory-derive as direct dependencies.
#[cfg(feature = "rpc")]
pub use fory_core;
#[cfg(feature = "rpc")]
pub use fory_derive;

#[cfg(feature = "expose-edge")]
pub use aster_expose::salvo;
