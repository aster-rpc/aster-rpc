//! Topology — wraps [`aster::topology`] (plan T7).
//!
//! v1: the local per-peer proximity view (`peers()` / `peer(id)`), populated
//! when the node runs with `enableMonitoring`. v2: the shared swarm view
//! (`enableShared(secret, options)`, `clusters()`, `myCluster()`,
//! `separated(a, b)`, `snapshot()`).
//!
//! The read-time `admitted` filter of `enableShared` can be JavaScript:
//! [`TopologyAdmittedFilter`] answers the synchronous Rust `AdmittedFn` from
//! a cache filled by batched upcalls (kind 16) — see its docs for the
//! sync / cache semantics.

use std::sync::Arc;
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster::topology::{
    ClusterView as RsClusterView, LadderLevel, PeerView as RsPeerView, SeparationVerdict,
    TopoConfig, Topology as RsTopology, TopologySnapshot as RsTopologySnapshot,
    TopologySwarmOptions as RsSwarmOptions,
};
use aster::{NamespaceSecret, NodeId};

use aster::binding_support::upcall::topology::{AdmittedCacheOptions, ForeignAdmittedFn};

use crate::config::aster_err;
use crate::keys::{invalid, key32};
use crate::upcalls::Upcalls;

fn level_out(l: LadderLevel) -> String {
    match l {
        LadderLevel::SameHost => "same_host",
        LadderLevel::SameLan => "same_lan",
        LadderLevel::SameSite => "same_site",
        LadderLevel::SameRegion => "same_region",
        LadderLevel::Far => "far",
    }
    .into()
}

fn verdict_out(v: SeparationVerdict) -> String {
    match v {
        SeparationVerdict::Unknown => "unknown",
        SeparationVerdict::Connected => "connected",
        SeparationVerdict::Separated => "separated",
    }
    .into()
}

/// One peer's proximity view (topology v1).
#[napi(object)]
pub struct PeerView {
    pub node_id: String,
    /// Locality ladder level; lower is closer.
    #[napi(ts_type = "'same_host' | 'same_lan' | 'same_site' | 'same_region' | 'far'")]
    pub level: String,
    /// The ladder rank, 0 (`same_host`) … 4 (`far`).
    pub level_rank: u32,
    /// The signal that justified the level.
    pub level_reason: String,
    /// Smoothed RTT, µs; absent until measured.
    pub rtt_us: Option<f64>,
    /// EWMA of |ΔRTT|, µs; absent until two samples exist.
    pub jitter_us: Option<f64>,
    /// Smoothed loss, parts per million of sent datagrams.
    pub loss_ppm: u32,
    /// Observed goodput (both directions), bits/s.
    pub throughput_bps: f64,
    /// cwnd/RTT ceiling estimate, bits/s.
    pub cwnd_ceiling_bps: f64,
    /// The selected path is direct (not relayed).
    pub direct: bool,
    pub relay_url: Option<String>,
    /// The selected path's remote `ip:port` when direct.
    pub remote_addr: Option<String>,
    pub congestion_events: f64,
    pub samples: f64,
    /// Confidence 0 … 0.99 (grows with samples, decays with staleness).
    pub confidence: f64,
    pub last_measured_unix_ms: f64,
    /// Since when RTT has held under the cluster threshold; 0 = not held.
    pub cluster_held_since_unix_ms: f64,
    pub is_connected: bool,
}

impl From<RsPeerView> for PeerView {
    fn from(p: RsPeerView) -> Self {
        Self {
            node_id: p.node_id,
            level: level_out(p.level),
            level_rank: p.level.as_i32() as u32,
            level_reason: p.level_reason,
            rtt_us: p.rtt_us.map(|v| v as f64),
            jitter_us: p.jitter_us.map(|v| v as f64),
            loss_ppm: p.loss_ppm,
            throughput_bps: p.throughput_bps as f64,
            cwnd_ceiling_bps: p.cwnd_ceiling_bps as f64,
            direct: p.direct,
            relay_url: p.relay_url,
            remote_addr: p.remote_addr.map(|a| a.to_string()),
            congestion_events: p.congestion_events as f64,
            samples: p.samples as f64,
            confidence: f64::from(p.confidence_ppm) / 1_000_000.0,
            last_measured_unix_ms: p.last_measured_unix_ms as f64,
            cluster_held_since_unix_ms: p.cluster_held_since_unix_ms as f64,
            is_connected: p.is_connected,
        }
    }
}

/// One derived cluster of the shared swarm view (topology v2).
#[napi(object)]
pub struct ClusterView {
    /// Sorted node ids (admitted and live members only).
    pub members: Vec<String>,
    pub bridge: String,
    /// Top members by bridge score (includes the bridge).
    pub witnesses: Vec<String>,
}

impl From<RsClusterView> for ClusterView {
    fn from(c: RsClusterView) -> Self {
        Self {
            members: c.members,
            bridge: c.bridge,
            witnesses: c.witnesses,
        }
    }
}

/// Reader timing / coverage rules (defaults are the design doc's).
#[napi(object)]
pub struct TopologyTiming {
    pub min_hold_ms: Option<f64>,
    pub liveness_ttl_ms: Option<f64>,
    pub edge_ttl_ms: Option<f64>,
    pub max_skew_ms: Option<f64>,
    pub separation_coverage: Option<u32>,
    pub witness_r: Option<u32>,
}

/// Options for `Topology.enableShared`.
#[napi(object)]
pub struct TopologySwarmOptions {
    /// Peers (node ids) to start doc sync with, beyond connected peers.
    pub bootstrap: Option<Vec<String>>,
    /// Heartbeat cadence for this node's records, ms (default 60 000).
    pub position_refresh_ms: Option<f64>,
    pub timing: Option<TopologyTiming>,
}

fn ms_in(field: &str, ms: f64) -> Result<Duration> {
    if !ms.is_finite() || ms < 0.0 {
        return Err(invalid(format!(
            "{field} must be a non-negative number of ms"
        )));
    }
    Ok(Duration::from_secs_f64(ms / 1000.0))
}

fn swarm_options_in(o: Option<TopologySwarmOptions>) -> Result<RsSwarmOptions> {
    let mut out = RsSwarmOptions::default();
    let Some(o) = o else { return Ok(out) };
    out.bootstrap = o
        .bootstrap
        .unwrap_or_default()
        .into_iter()
        .map(NodeId::from_hex)
        .collect();
    if let Some(ms) = o.position_refresh_ms {
        out.position_refresh = ms_in("positionRefreshMs", ms)?;
    }
    if let Some(t) = o.timing {
        let mut timing = TopoConfig::default();
        if let Some(v) = t.min_hold_ms {
            timing.min_hold = ms_in("minHoldMs", v)?;
        }
        if let Some(v) = t.liveness_ttl_ms {
            timing.liveness_ttl = ms_in("livenessTtlMs", v)?;
        }
        if let Some(v) = t.edge_ttl_ms {
            timing.edge_ttl = ms_in("edgeTtlMs", v)?;
        }
        if let Some(v) = t.max_skew_ms {
            timing.max_skew = ms_in("maxSkewMs", v)?;
        }
        if let Some(v) = t.separation_coverage {
            timing.separation_coverage = v as usize;
        }
        if let Some(v) = t.witness_r {
            timing.witness_r = v as usize;
        }
        out.timing = timing;
    }
    Ok(out)
}

/// This node's topology view. Obtain with `node.topology()`.
#[napi]
pub struct Topology {
    pub(crate) inner: RsTopology,
}

impl From<RsTopology> for Topology {
    fn from(inner: RsTopology) -> Self {
        Self { inner }
    }
}

#[napi]
impl Topology {
    /// Whether the node runs with monitoring (the view's prerequisite).
    #[napi(getter)]
    pub fn has_monitoring(&self) -> bool {
        self.inner.has_monitoring()
    }

    /// Every known peer's view, including recently disconnected ones.
    #[napi]
    pub fn peers(&self) -> Vec<PeerView> {
        self.inner.peers().into_iter().map(PeerView::from).collect()
    }

    /// One peer's view, or `null` (unknown, never sampled, or no monitoring).
    #[napi]
    pub fn peer(&self, node_id: String) -> Option<PeerView> {
        self.inner
            .peer(&NodeId::from_hex(node_id))
            .map(PeerView::from)
    }

    /// Join the topology swarm keyed by the shared namespace `secret` (hex or
    /// 32 bytes, normally delivered in a sealed grant). `admitted`, when
    /// given, filters record authors and vertices at read time.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn enable_shared<'env>(
        &self,
        env: &'env Env,
        secret: Either<String, Uint8Array>,
        options: Option<TopologySwarmOptions>,
        admitted: Option<&TopologyAdmittedFilter>,
    ) -> Result<PromiseRaw<'env, ()>> {
        let secret = NamespaceSecret::from_bytes(key32("namespace secret", &secret)?);
        let mut opts = swarm_options_in(options)?;
        opts.admitted = admitted.map(|f| f.inner.admitted_fn());
        let inner = self.inner.clone();
        env.spawn_future(async move { inner.enable_shared(&secret, opts).await.map_err(aster_err) })
    }

    /// The derived clusters of the joined swarm.
    #[napi]
    pub async fn clusters(&self) -> Result<Vec<ClusterView>> {
        Ok(self
            .inner
            .clusters()
            .await
            .map_err(aster_err)?
            .into_iter()
            .map(ClusterView::from)
            .collect())
    }

    /// The cluster containing this node, or `null` while it is not a vertex.
    #[napi]
    pub async fn my_cluster(&self) -> Result<Option<ClusterView>> {
        Ok(self
            .inner
            .my_cluster()
            .await
            .map_err(aster_err)?
            .map(ClusterView::from))
    }

    /// Separation verdict for two nodes.
    #[napi(ts_return_type = "Promise<'unknown' | 'connected' | 'separated'>")]
    pub async fn separated(&self, a: String, b: String) -> Result<String> {
        Ok(verdict_out(
            self.inner
                .separated(&NodeId::from_hex(a), &NodeId::from_hex(b))
                .await
                .map_err(aster_err)?,
        ))
    }

    /// One immutable observation: every answer comes from the same view.
    #[napi]
    pub async fn snapshot(&self) -> Result<TopologySnapshot> {
        Ok(TopologySnapshot {
            inner: Arc::new(self.inner.snapshot().await.map_err(aster_err)?),
        })
    }
}

/// One immutable derived view (see `Topology.snapshot`).
#[napi]
pub struct TopologySnapshot {
    pub(crate) inner: Arc<RsTopologySnapshot>,
}

#[napi]
impl TopologySnapshot {
    /// The clusters of this view.
    #[napi]
    pub fn clusters(&self) -> Vec<ClusterView> {
        self.inner
            .clusters()
            .iter()
            .cloned()
            .map(ClusterView::from)
            .collect()
    }

    /// Separation verdict between two nodes, from this view.
    #[napi(ts_return_type = "'unknown' | 'connected' | 'separated'")]
    pub fn separated(&self, a: String, b: String) -> String {
        verdict_out(self.inner.separated_hex(&a, &b))
    }
}

/// Cache tuning for [`TopologyAdmittedFilter`] (defaults: ttl 30 000 ms,
/// failure retry 1 000 ms, 256 ids per upcall).
#[napi(object)]
pub struct AdmittedCacheOptionsJs {
    pub ttl_ms: Option<f64>,
    pub failure_retry_ms: Option<f64>,
    pub max_batch: Option<u32>,
}

/// Lookup counters of a [`TopologyAdmittedFilter`].
#[napi(object)]
pub struct AdmittedCacheStatsJs {
    pub hits: f64,
    pub stale: f64,
    pub misses: f64,
    pub upcalls: f64,
}

/// A JavaScript topology `admitted` filter (upcall kind 16). The Rust
/// filter is synchronous and runs inside every shared-view derivation, so it
/// never waits for JavaScript: it answers from a cache — a fresh hit
/// directly, a stale entry (older than `ttlMs`) with the cached answer while
/// a refresh is queued, a miss with the kind's failure-mode answer
/// (fail-closed: filtered) while the id is queued. Queued ids are resolved
/// in batched upcalls. A failed batch caches the fallback for
/// `failureRetryMs`. Push revocations with `set` / `invalidate` to apply
/// them at the next view; warm the cache with `prefetch` before
/// `enableShared`.
#[napi]
pub struct TopologyAdmittedFilter {
    inner: ForeignAdmittedFn,
}

#[napi]
impl TopologyAdmittedFilter {
    #[napi(constructor)]
    pub fn new(upcalls: &Upcalls, options: Option<AdmittedCacheOptionsJs>) -> Result<Self> {
        let mut opts = AdmittedCacheOptions::default();
        if let Some(o) = options {
            if let Some(v) = o.ttl_ms {
                opts.ttl = ms_in("ttlMs", v)?;
            }
            if let Some(v) = o.failure_retry_ms {
                opts.failure_retry = ms_in("failureRetryMs", v)?;
            }
            if let Some(v) = o.max_batch {
                opts.max_batch = v.max(1) as usize;
            }
        }
        let u = upcalls.table();
        // `ForeignAdmittedFn::new` spawns its refresher task.
        let inner = within_runtime_if_available(move || ForeignAdmittedFn::new(u, opts));
        Ok(Self { inner })
    }

    /// The answer the filter gives `nodeId` right now (counts as a lookup;
    /// a miss or stale entry queues a refresh).
    #[napi]
    pub fn is_admitted(&self, node_id: String) -> bool {
        self.inner.is_admitted(&node_id)
    }

    /// Resolve `nodeIds` now with one upcall and cache the answers.
    #[napi]
    pub async fn prefetch(&self, node_ids: Vec<String>) -> Result<()> {
        self.inner
            .prefetch(&node_ids)
            .await
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Push an answer (e.g. on revocation); served for `ttlMs`.
    #[napi]
    pub fn set(&self, node_id: String, admitted: bool) {
        self.inner.set(&node_id, admitted);
    }

    /// Forget one id; the next lookup misses and re-resolves.
    #[napi]
    pub fn invalidate(&self, node_id: String) {
        self.inner.invalidate(&node_id);
    }

    /// Forget every id.
    #[napi]
    pub fn clear(&self) {
        self.inner.clear();
    }

    #[napi]
    pub fn stats(&self) -> AdmittedCacheStatsJs {
        let s = self.inner.stats();
        AdmittedCacheStatsJs {
            hits: s.hits as f64,
            stale: s.stale as f64,
            misses: s.misses as f64,
            upcalls: s.upcalls as f64,
        }
    }
}
