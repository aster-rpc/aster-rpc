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

use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::id::{NamespaceSecret, NodeId};
use aster_transport_core::{CoreNode, CoreTopoSwarm, CoreTopoSwarmConfig};

pub use aster_transport_core::{
    AdmittedFn, CoreClusterView as ClusterView, CoreLadderLevel as LadderLevel,
    CorePeerView as PeerView, CoreSeparationVerdict as SeparationVerdict, TopoConfig,
};

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

    // ── Topology v2: the shared swarm view ──────────────────────────────

    /// Join a topology swarm: import the shared namespace from its secret
    /// (normally delivered inside a sealed grant — see `aster::grants`),
    /// start publishing this node's measurements, and derive the
    /// swarm-wide cluster view. Requires the node to be started with
    /// [`AsterConfigBuilder::monitoring(true)`](crate::AsterConfigBuilder::monitoring).
    pub async fn enable_shared(
        &self,
        secret: &NamespaceSecret,
        opts: TopologySwarmOptions,
    ) -> Result<()> {
        let position_refresh = if opts.position_refresh.is_zero() {
            Duration::from_secs(60)
        } else {
            opts.position_refresh
        };
        let config = CoreTopoSwarmConfig {
            bootstrap: opts.bootstrap.iter().map(|n| n.to_string()).collect(),
            admitted: opts.admitted,
            position_refresh,
            topo: opts.timing,
        };
        self.core
            .topology_join_swarm(&secret.to_bytes(), config)
            .await?;
        Ok(())
    }

    fn swarm(&self) -> Result<Arc<CoreTopoSwarm>> {
        self.core.topology_swarm().ok_or_else(|| {
            Error::InvalidArgument(
                "topology swarm not joined — call Topology::enable_shared first".into(),
            )
        })
    }

    /// The derived clusters of the joined swarm. Every member derives the
    /// same partition from the same replicated records (eventual
    /// agreement; see the design doc's agreement boundary).
    pub async fn clusters(&self) -> Result<Vec<ClusterView>> {
        Ok(self.swarm()?.view().await?.clusters.clone())
    }

    /// The cluster containing this node, once its own records have
    /// replicated and qualified. `None` while this node is not (yet) a
    /// vertex of the derived graph.
    pub async fn my_cluster(&self) -> Result<Option<ClusterView>> {
        let swarm = self.swarm()?;
        let self_hex = self.core.node_id();
        Ok(swarm.view().await?.cluster_of(&self_hex).cloned())
    }

    /// Separation verdict for two nodes per the coverage rule. `Unknown`
    /// is a real answer — absence of measurement is never evidence of
    /// distance.
    pub async fn separated(&self, a: &NodeId, b: &NodeId) -> Result<SeparationVerdict> {
        Ok(self
            .swarm()?
            .view()
            .await?
            .separated(a.as_str(), b.as_str()))
    }
}

/// Options for [`Topology::enable_shared`].
#[derive(Clone, Default)]
pub struct TopologySwarmOptions {
    /// Peers to start doc sync with, in addition to every peer the node is
    /// already connected to.
    pub bootstrap: Vec<NodeId>,
    /// Read-time admission filter (hex node id → admitted?). `None` =
    /// every holder of the namespace secret counts; the secret itself is
    /// admission-gated by how it was distributed.
    pub admitted: Option<AdmittedFn>,
    /// Heartbeat cadence for position/edge records. `Duration::ZERO` (the
    /// `Default`) means the design-doc default of 60s.
    pub position_refresh: Duration,
    /// Reader timing/coverage rules. `TopologySwarmOptions::default()`
    /// fills the design-doc defaults.
    pub timing: TopoConfig,
}
