//! Local topology view — "where are my peers?" (topology v1).
//!
//! A per-peer proximity view computed passively from connections the node
//! already makes: a locality-ladder level (L0 same-host … L4 far), path
//! quality (smoothed RTT, jitter, loss, throughput), and a confidence score
//! saying how much measurement backs the entry. See
//! `docs/aster-topology-getstarted.md`.
//!
//! Requires the node to be started with monitoring enabled
//! ([`AsterConfigBuilder::monitoring(true)`](crate::AsterConfigBuilder::monitoring));
//! otherwise the view is empty and [`Topology::has_monitoring`] returns false.

use crate::id::NodeId;
use aster_transport_core::CoreNode;

pub use aster_transport_core::{CoreLadderLevel as LadderLevel, CorePeerView as PeerView};

/// Clone-cheap handle to this node's local topology view. Obtain via
/// [`Node::topology`](crate::Node::topology).
#[derive(Clone)]
pub struct Topology {
    core: CoreNode,
}

impl Topology {
    pub(crate) fn new(core: CoreNode) -> Self {
        Self { core }
    }

    /// Whether the node was started with monitoring enabled — the
    /// prerequisite for this view to populate.
    pub fn has_monitoring(&self) -> bool {
        self.core.has_monitoring()
    }

    /// Snapshot of every known peer's [`PeerView`], including recently
    /// disconnected peers (their `is_connected` is false and confidence
    /// decays). Empty when monitoring is disabled or nothing has been
    /// sampled yet (the sampler ticks once per second).
    pub fn peers(&self) -> Vec<PeerView> {
        self.core.peer_views()
    }

    /// Snapshot for one peer. `None` when monitoring is disabled, the peer
    /// is unknown, or it has never been sampled.
    pub fn peer(&self, id: &NodeId) -> Option<PeerView> {
        self.core.peer_view(id.as_str())
    }
}
