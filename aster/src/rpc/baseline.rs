//! Baseline (`aster.*`) services and payload types shipped with the framework.
//!
//! The `aster.*` service namespace and the `aster/…` wire-type package are
//! reserved for these (enforced at compile time by the `#[aster::service]` /
//! `#[derive(AsterType)]` macros outside this crate). Baseline services give
//! every Aster node a uniform, discoverable ops surface — the same RPCs, on
//! every node, regardless of which product runs there.
//!
//! Shipped today:
//!
//! - [`Value`] / [`Attr`] — a typed name/value primitive shared by baseline
//!   payloads (wire `aster/Value`, `aster/Attr`).
//! - `aster.ops.NodeInfo` ([`NodeInfo`]) — "describe yourself": returns the
//!   node's [`NodeIdentity`]. Registered **by default** by
//!   [`AsterServerBuilder`](super::runtime::AsterServerBuilder); disable with
//!   [`builtin_node_info(false)`](super::runtime::AsterServerBuilder::builtin_node_info)
//!   or gate it with
//!   [`node_info_requires`](super::runtime::AsterServerBuilder::node_info_requires).
//!
//! ## Why `Value`'s variants are one-field messages
//!
//! ContractIdentity §11.3.3 v1 restricts union variants to **message** types —
//! `Int(i64)`-style primitive variants are outside the cross-binding contract
//! scope (a `UnionVariantDef` carries only a message `type_ref`). Revisiting
//! that is a spec revision that must land in every binding's canonical encoder
//! simultaneously; until then the payload is one wrapper message per primitive.
//! Construction stays ergonomic via `From`: `Value::from(42i64)`,
//! `Value::from("text")`.
//!
//! Nullability is *positional*, per §11.3.3 rule 2: an absent value is
//! `Option<Value>` on the field (see [`Attr::value`]), not a `Null` variant.

use fory_derive::{ForyStruct, ForyUnion};

use super::auth::CapabilityRequirement;
use super::server::{Call, ServiceDispatch};

// ── Value: typed primitive union ─────────────────────────────────────────────

/// Wrapper message for an `i64` union payload (wire `aster/IntValue`).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/IntValue")]
pub struct IntValue {
    pub value: i64,
}

/// Wrapper message for an `f64` union payload (wire `aster/FloatValue`).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/FloatValue")]
pub struct FloatValue {
    pub value: f64,
}

/// Wrapper message for a `bool` union payload (wire `aster/BoolValue`).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/BoolValue")]
pub struct BoolValue {
    pub value: bool,
}

/// Wrapper message for a `String` union payload (wire `aster/TextValue`).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/TextValue")]
pub struct TextValue {
    pub value: String,
}

/// A typed primitive value (wire `aster/Value`): `Int | Float | Bool | Text`.
///
/// Construct via `From` (`Value::from(42i64)`, `Value::from(true)`,
/// `Value::from("s")`) and read via the `as_*` accessors. Absence is
/// `Option<Value>` at the field using it — not a variant (§11.3.3 rule 2).
#[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "aster/Value")]
pub enum Value {
    Int(IntValue),
    Float(FloatValue),
    Bool(BoolValue),
    #[fory(default)]
    Text(TextValue),
    /// ForyUnion's runtime forward-compat carrier — not part of the contract.
    #[fory(unknown)]
    Unknown(fory_core::UnknownCase),
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(v.value),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(v) => Some(v.value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(v) => Some(v.value),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(v) => Some(&v.value),
            _ => None,
        }
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Value::Int(IntValue { value })
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Value::Float(FloatValue { value })
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(BoolValue { value })
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::Text(TextValue { value })
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::Text(TextValue {
            value: value.to_string(),
        })
    }
}

// ── Attr + NodeIdentity ───────────────────────────────────────────────────────

/// A typed name/value pair (wire `aster/Attr`). `value: None` = declared null.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/Attr")]
pub struct Attr {
    pub name: String,
    pub value: Option<Value>,
}

impl Attr {
    /// An attribute with a value: `Attr::new("region", "eu-west")`.
    pub fn new(name: impl Into<String>, value: impl Into<Value>) -> Self {
        Attr {
            name: name.into(),
            value: Some(value.into()),
        }
    }

    /// An attribute declared with no value.
    pub fn null(name: impl Into<String>) -> Self {
        Attr {
            name: name.into(),
            value: None,
        }
    }
}

/// A node's self-description (wire `aster/NodeIdentity`) — the response of
/// `aster.ops.NodeInfo.describe`. Generic identity only: which node this is
/// and how it labels itself. Domain policy (admission state, expiry, …)
/// belongs to the application's own services, which may embed this record.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/NodeIdentity")]
pub struct NodeIdentity {
    /// Hex public key (`NodeId`) — filled by the server at start; any value
    /// set by the application is overwritten.
    pub node_id: String,
    /// Human-readable name, application-assigned.
    pub node_name: String,
    /// Free-form labels.
    pub tags: Vec<String>,
    /// Roles this node claims for itself. Distinct from `attributes` because
    /// roles are Gate-3-load-bearing on the peer side.
    pub roles: Vec<String>,
    /// Everything else, as typed name/value pairs.
    pub attributes: Vec<Attr>,
}

// ── aster.ops.NodeInfo ────────────────────────────────────────────────────────

/// Baseline identity service: `aster.ops.NodeInfo.describe` returns the node's
/// [`NodeIdentity`]. Hosted by default on every
/// [`AsterServer`](super::runtime::AsterServer).
#[aster_macros::service(name = "aster.ops.NodeInfo", version = 1)]
pub trait NodeInfo {
    /// The node's self-description.
    #[rpc(idempotent)]
    async fn describe(&self) -> aster::Result<NodeIdentity>;
}

/// A clone-shared, live handle to the identity served by the baseline
/// `aster.ops.NodeInfo` service. Keep a clone (from
/// [`AsterServer::node_identity`](super::runtime::AsterServer::node_identity))
/// and update the record while the server runs — e.g. flip a `draining`
/// attribute during a rolling deploy. Cheap to clone (`Arc` bump), same
/// pattern as [`AttributeStore`](super::auth::AttributeStore).
///
/// `node_id` is pinned: it is stamped from the node's key at server start,
/// and [`set`](Self::set) / [`update`](Self::update) preserve it regardless
/// of what the caller writes.
#[derive(Clone, Default)]
pub struct NodeIdentityHandle {
    inner: std::sync::Arc<std::sync::RwLock<NodeIdentity>>,
}

impl NodeIdentityHandle {
    pub(crate) fn new(identity: NodeIdentity) -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::RwLock::new(identity)),
        }
    }

    /// A snapshot of the identity currently being served.
    pub fn get(&self) -> NodeIdentity {
        self.inner.read().unwrap().clone()
    }

    /// Replace the served identity (name, tags, roles, attributes). The
    /// pinned `node_id` is preserved — any value in `identity.node_id` is
    /// ignored.
    pub fn set(&self, mut identity: NodeIdentity) {
        let mut guard = self.inner.write().unwrap();
        identity.node_id = guard.node_id.clone();
        *guard = identity;
    }

    /// Mutate the served identity in place. The pinned `node_id` is restored
    /// after the closure runs, so it cannot be changed from here either.
    pub fn update(&self, f: impl FnOnce(&mut NodeIdentity)) {
        let mut guard = self.inner.write().unwrap();
        let node_id = guard.node_id.clone();
        f(&mut guard);
        guard.node_id = node_id;
    }
}

/// The stock implementation served by `AsterServerBuilder`: serves the live
/// [`NodeIdentityHandle`] state (node_id stamped at server start).
pub(crate) struct LiveNodeInfo {
    pub(crate) handle: NodeIdentityHandle,
}

#[async_trait::async_trait]
impl NodeInfo for LiveNodeInfo {
    async fn describe(&self) -> aster::Result<NodeIdentity> {
        Ok(self.handle.get())
    }
}

// ── aster.net.Topology ────────────────────────────────────────────────────────

/// One peer in the node's local proximity view (wire `aster/PeerView`) — the
/// RPC projection of [`topology::PeerView`](crate::topology::PeerView).
/// Deliberately address-free: the in-process view carries the peer's socket
/// address, the wire view does not leak network layout beyond node ids.
///
/// All-integer fields per the cross-binding convention (no floats):
/// rates/confidence are parts-per-million, RTT/jitter microseconds.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/PeerView")]
pub struct PeerView {
    /// Hex public key of the peer.
    pub node_id: String,
    /// Locality ladder level: 0 same-host, 1 same-lan, 2 same-site,
    /// 3 same-region, 4 far.
    pub level: i32,
    /// The signal that justified the level (e.g. "verified private path").
    pub level_reason: String,
    /// Smoothed RTT, microseconds. 0 = not yet measured.
    pub rtt_us: i64,
    /// EWMA of |ΔRTT|, microseconds. 0 = not yet derivable.
    pub jitter_us: i64,
    /// Smoothed loss rate, parts per million of sent datagrams.
    pub loss_ppm: i32,
    /// Passively observed goodput (both directions), bits/sec.
    pub throughput_bps: i64,
    /// cwnd/RTT throughput ceiling estimate, bits/sec.
    pub cwnd_ceiling_bps: i64,
    /// Selected path is direct (vs relayed).
    pub direct: bool,
    /// Relay URL when the selected path is relayed; "" when direct.
    pub relay_url: String,
    /// Congestion events observed on sampled paths.
    pub congestion_events: i64,
    /// Number of sampler ticks folded into this entry.
    pub samples: i64,
    /// Confidence 0..990_000 ppm; grows with samples, decays with staleness.
    pub confidence_ppm: i32,
    pub last_measured_unix_ms: i64,
    pub is_connected: bool,
}

impl From<&aster_transport_core::CorePeerView> for PeerView {
    fn from(v: &aster_transport_core::CorePeerView) -> Self {
        PeerView {
            node_id: v.node_id.clone(),
            level: v.level.as_i32(),
            level_reason: v.level_reason.clone(),
            rtt_us: v.rtt_us.unwrap_or(0) as i64,
            jitter_us: v.jitter_us.unwrap_or(0) as i64,
            loss_ppm: v.loss_ppm as i32,
            throughput_bps: v.throughput_bps as i64,
            cwnd_ceiling_bps: v.cwnd_ceiling_bps as i64,
            direct: v.direct,
            relay_url: v.relay_url.clone().unwrap_or_default(),
            congestion_events: v.congestion_events as i64,
            samples: v.samples as i64,
            confidence_ppm: v.confidence_ppm as i32,
            last_measured_unix_ms: v.last_measured_unix_ms as i64,
            is_connected: v.is_connected,
        }
    }
}

/// Request for `aster.net.Topology.peer` (wire `aster/PeerQuery`).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/PeerQuery")]
pub struct PeerQuery {
    /// Hex public key of the peer to look up.
    pub node_id: String,
}

/// Response of the `aster.net.Topology` methods (wire `aster/PeerList`).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/PeerList")]
pub struct PeerList {
    pub peers: Vec<PeerView>,
}

/// One derived cluster (wire `aster/ClusterView`): mutually-close admitted
/// live producers, with the deterministically elected bridge and witness
/// set. Node ids are hex strings, consistent with [`PeerView::node_id`].
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/ClusterView")]
pub struct ClusterView {
    /// Sorted hex node ids (admitted ∧ live members only).
    pub members: Vec<String>,
    pub bridge: String,
    /// Top-ranked members by bridge score; includes the bridge.
    pub witnesses: Vec<String>,
}

impl From<&aster_transport_core::CoreClusterView> for ClusterView {
    fn from(c: &aster_transport_core::CoreClusterView) -> Self {
        ClusterView {
            members: c.members.clone(),
            bridge: c.bridge.clone(),
            witnesses: c.witnesses.clone(),
        }
    }
}

/// Response of `aster.net.Topology.clusters` (wire `aster/ClusterList`).
/// Empty when the node has not joined a topology swarm.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/ClusterList")]
pub struct ClusterList {
    pub clusters: Vec<ClusterView>,
}

/// Request for `aster.net.Topology.separated` (wire `aster/SeparatedQuery`).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/SeparatedQuery")]
pub struct SeparatedQuery {
    pub node_a: String,
    pub node_b: String,
}

/// `separated` verdict codes for [`SeparatedVerdict::verdict`].
pub const SEPARATED_UNKNOWN: i32 = 0;
pub const SEPARATED_CONNECTED: i32 = 1;
pub const SEPARATED_SEPARATED: i32 = 2;

/// Response of `aster.net.Topology.separated` (wire `aster/SeparatedVerdict`).
/// `Unknown` (0) is a real answer: absence of measurement is never treated
/// as evidence of distance.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster/SeparatedVerdict")]
pub struct SeparatedVerdict {
    /// 0 unknown, 1 connected, 2 separated.
    pub verdict: i32,
}

/// Baseline topology service: query the node's local proximity view
/// (topology v1 — ladder levels + path quality; clusters arrive with v2).
///
/// **Opt-in**, unlike `aster.ops.NodeInfo`: the view discloses inferred
/// network layout and topology is a producer-node feature, so operators
/// enable it deliberately via
/// [`builtin_topology(true)`](super::runtime::AsterServerBuilder::builtin_topology)
/// (and usually gate it with
/// [`topology_requires`](super::runtime::AsterServerBuilder::topology_requires)).
/// Requires the node to be started with monitoring enabled.
#[aster_macros::service(name = "aster.net.Topology", version = 1)]
pub trait Topology {
    /// Every known peer's view entry.
    #[rpc(idempotent)]
    async fn peers(&self) -> aster::Result<PeerList>;

    /// One peer's view entry (0-or-1 element list).
    #[rpc(idempotent)]
    async fn peer(&self, query: PeerQuery) -> aster::Result<PeerList>;

    /// The derived clusters of the joined topology swarm (v2). Empty when
    /// this node has not joined a swarm.
    #[rpc(idempotent)]
    async fn clusters(&self) -> aster::Result<ClusterList>;

    /// Separation verdict for two nodes per the coverage rule (v2).
    /// `Unknown` when no swarm is joined or evidence is insufficient.
    #[rpc(idempotent)]
    async fn separated(&self, query: SeparatedQuery) -> aster::Result<SeparatedVerdict>;
}

/// The stock implementation served by `AsterServerBuilder`: reads the core
/// node's live topology view.
pub(crate) struct LiveTopology {
    pub(crate) core: aster_transport_core::CoreNode,
}

#[async_trait::async_trait]
impl Topology for LiveTopology {
    async fn peers(&self) -> aster::Result<PeerList> {
        Ok(PeerList {
            peers: self.core.peer_views().iter().map(PeerView::from).collect(),
        })
    }

    async fn peer(&self, query: PeerQuery) -> aster::Result<PeerList> {
        Ok(PeerList {
            peers: self
                .core
                .peer_view(&query.node_id)
                .iter()
                .map(PeerView::from)
                .collect(),
        })
    }

    async fn clusters(&self) -> aster::Result<ClusterList> {
        let Some(swarm) = self.core.topology_swarm() else {
            return Ok(ClusterList::default());
        };
        let view = swarm.view().await.map_err(crate::Error::from)?;
        Ok(ClusterList {
            clusters: view.clusters.iter().map(ClusterView::from).collect(),
        })
    }

    async fn separated(&self, query: SeparatedQuery) -> aster::Result<SeparatedVerdict> {
        use aster_transport_core::CoreSeparationVerdict as V;
        let Some(swarm) = self.core.topology_swarm() else {
            return Ok(SeparatedVerdict {
                verdict: SEPARATED_UNKNOWN,
            });
        };
        let view = swarm.view().await.map_err(crate::Error::from)?;
        let verdict = match view.separated(&query.node_a, &query.node_b) {
            V::Unknown => SEPARATED_UNKNOWN,
            V::Connected => SEPARATED_CONNECTED,
            V::Separated => SEPARATED_SEPARATED,
        };
        Ok(SeparatedVerdict { verdict })
    }
}

// ── Service-wide requirement override ─────────────────────────────────────────

/// Wraps a [`ServiceDispatch`] and imposes a service-wide capability
/// requirement (Gate 3) on top of whatever the inner dispatch declares. Used
/// by the builder's `node_info_requires`.
pub(crate) struct RequireGuard<D> {
    pub(crate) inner: D,
    pub(crate) requires: Option<CapabilityRequirement>,
}

#[async_trait::async_trait]
impl<D: ServiceDispatch> ServiceDispatch for RequireGuard<D> {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn version(&self) -> i32 {
        self.inner.version()
    }
    fn methods(&self) -> &[&str] {
        self.inner.methods()
    }
    fn service_requires(&self) -> Option<CapabilityRequirement> {
        self.requires
            .clone()
            .or_else(|| self.inner.service_requires())
    }
    fn method_requires(&self, method: &str) -> Option<CapabilityRequirement> {
        self.inner.method_requires(method)
    }
    async fn dispatch(&self, method: &str, call: Call) {
        self.inner.dispatch(method, call).await
    }
}
