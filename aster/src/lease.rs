//! Single-owner fenced leases — locks that stay correct on a P2P mesh.
//!
//! Design: `docs/_internal/aster-leases.md`. The short version:
//! the *grant* is cheap and imperfect (a CAS at the resource's own
//! serialization point, a TTL for liveness), and correctness comes from the
//! *fence* — every protected action carries `(resource, epoch)` over an
//! authenticated stream, and the resource itself rejects anything that is
//! not the current epoch from the current holder. A paused or partitioned
//! ex-holder that wakes and acts late is rejected by the thing it writes to.
//!
//! Two halves, both required:
//!
//! - **Granting** — [`LeaseHandle::acquire`] against a [`LeaseSerializer`]
//!   backend. The handle renews in the background and stops handing out its
//!   fence at the self-limit ([`LeaseHandle::fence`] returns `None`).
//! - **Enforcing** — the resource implements [`FencedResource`] (or the
//!   equivalent transaction hook), evaluating the fence rule *inside* its
//!   own serialization point. The library grants; only resources enforce —
//!   a `LeaseHandle` over an unfenced write API provides no safety.
//!
//! ```no_run
//! use std::sync::Arc;
//! use aster::lease::{AcquireOutcome, LeaseHandle, LeaseOptions, MemorySerializer};
//!
//! # async fn run(node_id: String) -> anyhow::Result<()> {
//! let serializer = Arc::new(MemorySerializer::new());
//! match LeaseHandle::acquire(serializer, "tree/42", &node_id, LeaseOptions::default()).await? {
//!     AcquireOutcome::Granted(lease) => {
//!         while let Some(fence) = lease.fence() {
//!             // stamp `fence` on the protected action, at send time
//!             # let _ = fence; break;
//!         }
//!         lease.release().await?; // graceful handoff: advances the fence
//!     }
//!     AcquireOutcome::Denied(reason, snapshot) => {
//!         // back off (jittered exponential); snapshot.remaining is a hint
//!         # let _ = (reason, snapshot);
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Backends in this crate today: [`MemorySerializer`] (reference, and the
//! store a designated grantor serves). Portal-store (SQLite row) and the S3
//! catalog object implement [`LeaseSerializer`]/[`FencedResource`] in their
//! own crates against the same [`transition`] state machine.

pub use aster_transport_core::lease::{
    check_fence, revoke, transition, AcquireOutcome, DenyReason, Fence, FenceRejection,
    FencedResource, LeaseAuthorizer, LeaseHandle, LeaseOp, LeaseOptions, LeaseSerializer,
    LeaseSnapshot, LeaseStatus, MemorySerializer, RowState, Transition, TransitionOutcome,
};
