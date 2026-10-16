//! Topology -- wraps [`aster::topology`] (plan P7).
//!
//! v1: the local per-peer proximity view (`peers()` / `peer(id)`), populated
//! when the node runs with `enable_monitoring=True`. v2: the shared swarm
//! view (`enable_shared(secret, ...)`, `clusters()`, `my_cluster()`,
//! `separated(a, b)`, `snapshot()`).
//!
//! `PeerView` / `ClusterView` / `TopologyTiming` are the frozen dataclasses
//! of `aster.topology`; durations are seconds.
//!
//! The read-time `admitted` filter of `enable_shared` may be Python (plan
//! R6): [`AdmittedFilter`] wraps `ForeignAdmittedFn`, a cache answered by
//! batched upcalls, so the Rust read path never waits on Python.

use std::sync::Arc;

use pyo3::prelude::*;
use pyo3::types::PyTuple;
use pyo3_async_runtimes::tokio::future_into_py;

use aster::topology::{
    ClusterView as RsClusterView, LadderLevel, PeerBlobStats, PeerView as RsPeerView, QuicBytes,
    QuicTraffic, SeparationVerdict, TopoConfig, Topology as RsTopology,
    TopologySnapshot as RsTopologySnapshot, TopologySwarmOptions,
};
use aster::{NamespaceSecret, NodeId};

use aster::binding_support::upcall::topology::{AdmittedCacheOptions, ForeignAdmittedFn};

use crate::error::aster_err_to_py;
use crate::keys::{key32, node_hex, node_id_out, py_value, secs_in};

fn level_out(l: LadderLevel) -> &'static str {
    match l {
        LadderLevel::SameHost => "same_host",
        LadderLevel::SameLan => "same_lan",
        LadderLevel::SameSite => "same_site",
        LadderLevel::SameRegion => "same_region",
        LadderLevel::Far => "far",
    }
}

fn verdict_out(v: SeparationVerdict) -> &'static str {
    match v {
        SeparationVerdict::Unknown => "unknown",
        SeparationVerdict::Connected => "connected",
        SeparationVerdict::Separated => "separated",
    }
}

pub(crate) fn ids_out<'py>(py: Python<'py>, ids: &[String]) -> PyResult<Bound<'py, PyTuple>> {
    PyTuple::new(py, ids.iter().map(|h| node_id_out(h.clone())))
}

fn peer_out<'py>(py: Python<'py>, p: RsPeerView) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.topology", "_peer_view" {
        node_id: node_id_out(p.node_id),
        level: level_out(p.level),
        level_rank: p.level.as_i32(),
        level_reason: p.level_reason,
        rtt_us: p.rtt_us,
        jitter_us: p.jitter_us,
        loss_ppm: p.loss_ppm,
        throughput_bps: p.throughput_bps,
        cwnd_ceiling_bps: p.cwnd_ceiling_bps,
        direct: p.direct,
        relay_url: p.relay_url,
        remote_addr: p.remote_addr.map(|a| a.to_string()),
        congestion_events: p.congestion_events,
        samples: p.samples,
        confidence: f64::from(p.confidence_ppm) / 1_000_000.0,
        last_measured_unix_ms: p.last_measured_unix_ms,
        cluster_held_since_unix_ms: p.cluster_held_since_unix_ms,
        is_connected: p.is_connected,
        quic_traffic: quic_traffic_out(py, &p.quic_traffic)?,
        blob_transfer: peer_blob_stats_out(py, &p.blob_transfer)?,
    })
}

fn quic_bytes_out(py: Python<'_>, b: QuicBytes) -> PyResult<Bound<'_, PyAny>> {
    py_value!(py, "aster.topology", "QuicBytes" {
        quic_bytes_sent: b.quic_bytes_sent,
        quic_bytes_received: b.quic_bytes_received,
    })
}

fn quic_traffic_out<'py>(py: Python<'py>, t: &QuicTraffic) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.topology", "QuicTraffic" {
        blobs: quic_bytes_out(py, t.blobs)?,
        blobs_zstd: quic_bytes_out(py, t.blobs_zstd)?,
        docs: quic_bytes_out(py, t.docs)?,
        gossip: quic_bytes_out(py, t.gossip)?,
        rpc: quic_bytes_out(py, t.rpc)?,
        other: quic_bytes_out(py, t.other)?,
    })
}

fn peer_blob_stats_out<'py>(py: Python<'py>, b: &PeerBlobStats) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.topology", "PeerBlobStats" {
        fetched_payload_bytes: b.fetched_payload_bytes,
        fetched_zstd_payload_bytes: b.fetched_zstd_payload_bytes,
        fetched_wire_bytes: b.fetched_wire_bytes,
        fetched_plain_bytes: b.fetched_plain_bytes,
        served_payload_bytes: b.served_payload_bytes,
        served_zstd_payload_bytes: b.served_zstd_payload_bytes,
        served_wire_bytes: b.served_wire_bytes,
        served_plain_bytes: b.served_plain_bytes,
        plain_fallbacks: b.plain_fallbacks,
    })
}

pub(crate) fn cluster_out<'py>(py: Python<'py>, c: &RsClusterView) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.topology", "ClusterView" {
        members: ids_out(py, &c.members)?,
        bridge: node_id_out(c.bridge.clone()),
        witnesses: ids_out(py, &c.witnesses)?,
    })
}

pub(crate) fn clusters_out<'py>(
    py: Python<'py>,
    cs: &[RsClusterView],
) -> PyResult<Bound<'py, PyTuple>> {
    let items = cs
        .iter()
        .map(|c| cluster_out(py, c))
        .collect::<PyResult<Vec<_>>>()?;
    PyTuple::new(py, items)
}

fn opt_secs(obj: &Bound<'_, PyAny>, field: &str) -> PyResult<Option<std::time::Duration>> {
    let v = obj.getattr(field)?;
    if v.is_none() {
        return Ok(None);
    }
    secs_in(field, v.extract()?).map(Some)
}

fn timing_in(obj: &Bound<'_, PyAny>) -> PyResult<TopoConfig> {
    let mut t = TopoConfig::default();
    if let Some(v) = opt_secs(obj, "min_hold")? {
        t.min_hold = v;
    }
    if let Some(v) = opt_secs(obj, "liveness_ttl")? {
        t.liveness_ttl = v;
    }
    if let Some(v) = opt_secs(obj, "edge_ttl")? {
        t.edge_ttl = v;
    }
    if let Some(v) = opt_secs(obj, "max_skew")? {
        t.max_skew = v;
    }
    if let Some(v) = obj
        .getattr("separation_coverage")?
        .extract::<Option<usize>>()?
    {
        t.separation_coverage = v;
    }
    if let Some(v) = obj.getattr("witness_r")?.extract::<Option<usize>>()? {
        t.witness_r = v;
    }
    Ok(t)
}

/// This node's topology view. Obtain with `node.topology()`.
#[pyclass(frozen, module = "aster._aster")]
pub struct Topology {
    inner: RsTopology,
}

impl Topology {
    pub(crate) fn inner(&self) -> &RsTopology {
        &self.inner
    }
}

impl From<RsTopology> for Topology {
    fn from(inner: RsTopology) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl Topology {
    /// Whether the node runs with monitoring (the view's prerequisite).
    #[getter]
    fn has_monitoring(&self) -> bool {
        self.inner.has_monitoring()
    }

    /// Every known peer's `PeerView` (recently disconnected ones too,
    /// decaying). Empty without monitoring.
    fn peers<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyAny>>> {
        self.inner
            .peers()
            .into_iter()
            .map(|p| peer_out(py, p))
            .collect()
    }

    /// One peer's `PeerView`, or `None` (unknown, never sampled, or no
    /// monitoring).
    fn peer<'py>(
        &self,
        py: Python<'py>,
        node_id: &Bound<'py, PyAny>,
    ) -> PyResult<Option<Bound<'py, PyAny>>> {
        self.inner
            .peer(&NodeId::from_hex(node_hex(node_id)?))
            .map(|p| peer_out(py, p))
            .transpose()
    }

    /// Join the topology swarm keyed by the shared namespace `secret` (32
    /// bytes or 64 hex, normally delivered in a sealed grant) and start
    /// publishing this node's measurements. `bootstrap`: peers to sync with
    /// beyond connected ones; `position_refresh`: heartbeat seconds (default
    /// 60); `timing`: a `TopologyTiming`; `admitted`: the read-time
    /// admission filter -- an `aster.topology.AdmittedFilter`, or a
    /// predicate `(NodeId) -> bool | Awaitable[bool]` wrapped in one with
    /// default cache options (records whose author it rejects, and vertices
    /// it rejects, are ignored). Awaitable.
    #[pyo3(signature = (secret, *, bootstrap=None, position_refresh=None, timing=None, admitted=None))]
    fn enable_shared<'py>(
        &self,
        py: Python<'py>,
        secret: &Bound<'py, PyAny>,
        bootstrap: Option<Vec<Bound<'py, PyAny>>>,
        position_refresh: Option<f64>,
        timing: Option<&Bound<'py, PyAny>>,
        admitted: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let secret = NamespaceSecret::from_bytes(key32("namespace secret", secret)?);
        let mut opts = TopologySwarmOptions::default();
        for b in bootstrap.unwrap_or_default() {
            opts.bootstrap.push(NodeId::from_hex(node_hex(&b)?));
        }
        if let Some(s) = position_refresh {
            let duration = secs_in("position_refresh", s)?;
            if std::time::Instant::now().checked_add(duration).is_none() {
                return Err(pyo3::exceptions::PyValueError::new_err(
                    "position_refresh is outside the supported timer range",
                ));
            }
            opts.position_refresh = duration;
        }
        if let Some(t) = timing.filter(|t| !t.is_none()) {
            opts.timing = timing_in(t)?;
        }
        if let Some(a) = admitted.filter(|a| !a.is_none()) {
            opts.admitted = Some(admitted_in(a)?.admitted_fn());
        }
        let inner = self.inner.clone();
        future_into_py(py, async move {
            inner
                .enable_shared(&secret, opts)
                .await
                .map_err(aster_err_to_py)
        })
    }

    /// The derived clusters of the joined swarm (awaitable tuple of
    /// `ClusterView`).
    fn clusters<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let cs = inner.clusters().await.map_err(aster_err_to_py)?;
            Python::attach(|py| clusters_out(py, &cs).map(|t| t.into_any().unbind()))
        })
    }

    /// The cluster containing this node, or `None` while it is not (yet) a
    /// vertex. Awaitable.
    fn my_cluster<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let c = inner.my_cluster().await.map_err(aster_err_to_py)?;
            Python::attach(|py| {
                c.map(|c| cluster_out(py, &c).map(Bound::unbind))
                    .transpose()
            })
        })
    }

    /// Separation verdict for two nodes: `"unknown"`, `"connected"` or
    /// `"separated"`. Each call may see a different view; use `snapshot()`
    /// for a consistent one. Awaitable.
    fn separated<'py>(
        &self,
        py: Python<'py>,
        a: &Bound<'py, PyAny>,
        b: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let a = NodeId::from_hex(node_hex(a)?);
        let b = NodeId::from_hex(node_hex(b)?);
        let inner = self.inner.clone();
        future_into_py(py, async move {
            Ok(verdict_out(
                inner.separated(&a, &b).await.map_err(aster_err_to_py)?,
            ))
        })
    }

    /// One immutable observation: every answer comes from the same derived
    /// view. Awaitable `TopologySnapshot`.
    fn snapshot<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let snap = Arc::new(inner.snapshot().await.map_err(aster_err_to_py)?);
            Python::attach(|py| {
                let clusters = clusters_out(py, snap.clusters())?.unbind();
                Ok(TopologySnapshot {
                    inner: snap,
                    clusters,
                })
            })
        })
    }

    fn __repr__(&self) -> String {
        format!("Topology(has_monitoring={})", self.inner.has_monitoring())
    }
}

/// One immutable derived topology view (see `Topology.snapshot()`).
#[pyclass(frozen, module = "aster._aster")]
pub struct TopologySnapshot {
    pub(crate) inner: Arc<RsTopologySnapshot>,
    clusters: Py<PyTuple>,
}

#[pymethods]
impl TopologySnapshot {
    /// A snapshot built from a serialized view (`aster.topology.TopologyView`:
    /// clusters, corroborated far pairs, separation coverage), answering
    /// `cluster_of` / `separated` by the same rules as a live view -- what a
    /// Python placement `TopologySource` returns.
    #[staticmethod]
    fn from_view(py: Python<'_>, view: &Bound<'_, PyAny>) -> PyResult<Self> {
        let snap = Arc::new(crate::upcall::view_in(view)?.into_snapshot());
        let clusters = clusters_out(py, snap.clusters())?.unbind();
        Ok(TopologySnapshot {
            inner: snap,
            clusters,
        })
    }

    /// The clusters of this view (tuple of `ClusterView`).
    #[getter]
    fn clusters(&self, py: Python<'_>) -> Py<PyTuple> {
        self.clusters.clone_ref(py)
    }

    /// The `ClusterView` containing `node` in this view, or `None`.
    fn cluster_of<'py>(
        &self,
        py: Python<'py>,
        node: &Bound<'py, PyAny>,
    ) -> PyResult<Option<Bound<'py, PyAny>>> {
        match self.inner.cluster_index_of_hex(&node_hex(node)?) {
            Some(i) => Ok(Some(self.clusters.bind(py).get_item(i)?)),
            None => Ok(None),
        }
    }

    /// Separation verdict between two nodes, from this view.
    fn separated(&self, a: &Bound<'_, PyAny>, b: &Bound<'_, PyAny>) -> PyResult<&'static str> {
        Ok(verdict_out(
            self.inner.separated_hex(&node_hex(a)?, &node_hex(b)?),
        ))
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        format!(
            "TopologySnapshot(clusters={})",
            self.clusters.bind(py).len()
        )
    }
}

/// `obj` as the native filter: an `_AdmittedFilter`, or anything
/// `aster.topology._as_admitted_filter` turns into one.
fn admitted_in(obj: &Bound<'_, PyAny>) -> PyResult<ForeignAdmittedFn> {
    if let Ok(f) = obj.cast::<AdmittedFilter>() {
        return Ok(f.get().inner.clone());
    }
    let native = obj
        .py()
        .import("aster.topology")?
        .getattr("_as_admitted_filter")?
        .call1((obj,))?;
    Ok(native.cast::<AdmittedFilter>()?.get().inner.clone())
}

/// The native half of `aster.topology.AdmittedFilter`: a cached, batched
/// read-time admission filter answered by Python (`ForeignAdmittedFn`).
#[pyclass(frozen, module = "aster._aster", name = "_AdmittedFilter")]
pub struct AdmittedFilter {
    inner: ForeignAdmittedFn,
}

#[pymethods]
impl AdmittedFilter {
    /// Bind `handler` (an `aster.topology.AdmittedFilter`) as the
    /// `topology_admitted` upcall handler. `ttl` / `failure_retry` are
    /// seconds.
    #[new]
    #[pyo3(signature = (handler, *, ttl=30.0, failure_retry=1.0, max_batch=256))]
    fn new(
        handler: &Bound<'_, PyAny>,
        ttl: f64,
        failure_retry: f64,
        max_batch: usize,
    ) -> PyResult<Self> {
        let opts = AdmittedCacheOptions {
            ttl: secs_in("ttl", ttl)?,
            failure_retry: secs_in("failure_retry", failure_retry)?,
            max_batch,
        };
        let upcalls = crate::upcall::bind(handler, "topology_admitted")?;
        // The refresher task spawns on construction.
        let _guard = crate::upcall::tokio_guard();
        Ok(Self {
            inner: ForeignAdmittedFn::new(upcalls, opts),
        })
    }

    /// Look one id up exactly as the read path does (never waits).
    fn is_admitted(&self, node_id: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(self.inner.is_admitted(&node_hex(node_id)?))
    }

    /// Resolve `node_ids` now with one upcall and cache the answers.
    /// Awaitable; raises `aster.upcalls.UpcallError` when the batch failed
    /// (the failure-mode answer is then cached for `failure_retry`).
    fn prefetch<'py>(
        &self,
        py: Python<'py>,
        node_ids: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ids = node_ids
            .iter()
            .map(node_hex)
            .collect::<PyResult<Vec<_>>>()?;
        let inner = self.inner.clone();
        future_into_py(py, async move {
            inner
                .prefetch(&ids)
                .await
                .map_err(|e| crate::upcall::upcall_err(&e))
        })
    }

    /// Push an answer (e.g. on revocation); served for `ttl`.
    fn set(&self, node_id: &Bound<'_, PyAny>, admitted: bool) -> PyResult<()> {
        self.inner.set(&node_hex(node_id)?, admitted);
        Ok(())
    }

    /// Forget one id; the next lookup misses and re-resolves.
    fn invalidate(&self, node_id: &Bound<'_, PyAny>) -> PyResult<()> {
        self.inner.invalidate(&node_hex(node_id)?);
        Ok(())
    }

    /// Forget every id.
    fn clear(&self) {
        self.inner.clear();
    }

    /// `(hits, stale, misses, upcalls)`.
    fn stats(&self) -> (u64, u64, u64, u64) {
        let s = self.inner.stats();
        (s.hits, s.stale, s.misses, s.upcalls)
    }
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Topology>()?;
    m.add_class::<TopologySnapshot>()?;
    m.add_class::<AdmittedFilter>()?;
    Ok(())
}
