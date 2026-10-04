//! Upcalls -- Rust asking Python a question (plan R6 / D6; design
//! `docs/_internal/bindings/upcall-design.md`, PyO3 wiring §4).
//!
//! One [`UpcallRuntime`] per event loop owns any number of *channels*. A
//! channel is one [`Upcalls`] table plus a bounded [`UpcallQueue`], bound to
//! one Python handler object (a lease serializer, a topology filter, a
//! placement manager's providers, ...): ids are per table, so a channel is
//! what routes an answer back to the right adapter. Every channel's sink
//! also wakes the runtime, so the Python pump (`aster.upcalls`) awaits one
//! [`UpcallRuntime::ready`], [`drain`](UpcallRuntime::drain)s events across
//! channels -- **decoded here** into the `aster.*` dataclasses -- runs each
//! handler as an asyncio task and answers with the synchronous
//! [`respond`](UpcallRuntime::respond), which encodes the handler's value
//! back to UCv1 (no event-loop hop). `Cancel` events surface as
//! `("cancel", reason)` so the pump cancels the matching task.
//!
//! The UCv1 codec for every message type is also exposed as
//! `_upcall_decode(type, bytes)` / `_upcall_encode(type, value)` so the
//! conformance vectors (`conformance/vectors/upcalls.json`) run through the
//! same conversions the pump uses.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pyo3::exceptions::{PyBaseException, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyTuple};
use pyo3_async_runtimes::tokio::future_into_py;
use tokio::sync::Notify;

use aster::binding_support::upcall::codec::{self, HandshakeDecision, TopologyViewParts};
use aster::binding_support::upcall::expose::HttpBodies;
use aster::binding_support::upcall::{
    CancelReason, HookFailureMode, UpcallError, UpcallEvent, UpcallKind, UpcallPolicy, UpcallQueue,
    UpcallSink, UpcallStatus, Upcalls,
};
use aster::coordination::client::manager::ProvisionRequest;
use aster::topology::ClusterView;

use crate::coordination::{
    group_in, id32, log_in, nid, node_id_in, opt_log_out, policy_in, policy_out, seed_in, seed_out,
};
use crate::keys::{invalid, node_hex, node_id_out, py_value};
use crate::lease::{
    fence_in, fence_out, op_in, op_out, outcome_in, outcome_out, rejection_in, rejection_out,
    snapshot_in, snapshot_out,
};

/// Most events one `drain` returns.
const DRAIN_MAX: usize = 1024;

// ============================================================================
// Kinds, policies, errors
// ============================================================================

pub(crate) fn kind_in(obj: &Bound<'_, PyAny>) -> PyResult<UpcallKind> {
    if let Ok(code) = obj.extract::<u16>() {
        return UpcallKind::from_code(code)
            .ok_or_else(|| invalid(format!("unknown upcall kind code {code}")));
    }
    let name: String = obj
        .extract()
        .map_err(|_| invalid("upcall kind must be a kind name or code"))?;
    UpcallKind::ALL
        .iter()
        .copied()
        .find(|k| k.name() == name)
        .ok_or_else(|| invalid(format!("unknown upcall kind {name:?}")))
}

fn mode_in(s: &str) -> PyResult<HookFailureMode> {
    match s {
        "fail_open" => Ok(HookFailureMode::FailOpen),
        "fail_closed" => Ok(HookFailureMode::FailClosed),
        other => Err(invalid(format!(
            "failure_mode must be 'fail_open' or 'fail_closed', got {other:?}"
        ))),
    }
}

fn mode_out(m: HookFailureMode) -> &'static str {
    match m {
        HookFailureMode::FailOpen => "fail_open",
        _ => "fail_closed",
    }
}

fn policy_from(timeout: f64, failure_mode: &str) -> PyResult<UpcallPolicy> {
    if !timeout.is_finite() || timeout <= 0.0 {
        return Err(invalid(
            "upcall timeout must be a positive number of seconds",
        ));
    }
    Ok(UpcallPolicy::new(
        Duration::from_secs_f64(timeout),
        mode_in(failure_mode)?,
    ))
}

fn error_kind(e: &UpcallError) -> &'static str {
    match e {
        UpcallError::Timeout => "timeout",
        UpcallError::Overloaded => "overloaded",
        UpcallError::Closed => "closed",
        UpcallError::Foreign(_) => "foreign",
        UpcallError::Unimplemented(_) => "unimplemented",
        UpcallError::Decode(_) => "decode",
    }
}

/// `aster.upcalls.UpcallError` (or `class`, a subclass) for `e`.
pub(crate) fn upcall_err_class(e: &UpcallError, class: &str) -> PyErr {
    Python::attach(|py| {
        let build = || -> PyResult<PyErr> {
            let cls = py.import("aster.upcalls")?.getattr(class)?;
            let kw = PyDict::new(py);
            kw.set_item("kind", error_kind(e))?;
            Ok(PyErr::from_value(cls.call((e.to_string(),), Some(&kw))?))
        };
        build().unwrap_or_else(|err| err)
    })
}

pub(crate) fn upcall_err(e: &UpcallError) -> PyErr {
    upcall_err_class(e, "UpcallError")
}

/// The `UpcallError` somewhere in an error chain, if any.
pub(crate) fn find_upcall_error(e: &anyhow::Error) -> Option<&UpcallError> {
    e.chain().find_map(|c| c.downcast_ref::<UpcallError>())
}

/// Map an `anyhow` error from an upcall-backed path: an upcall failure
/// raises `aster.upcalls.UpcallError`, anything else `fallback`.
pub(crate) fn anyhow_to_py(
    e: anyhow::Error,
    fallback: impl FnOnce(anyhow::Error) -> PyErr,
) -> PyErr {
    match find_upcall_error(&e) {
        Some(u) => upcall_err(u),
        None => fallback(e),
    }
}

/// As [`anyhow_to_py`] for an `aster::Error`.
pub(crate) fn aster_to_py(e: aster::Error, fallback: impl FnOnce(aster::Error) -> PyErr) -> PyErr {
    if let aster::Error::Transport(inner) = &e {
        if let Some(u) = find_upcall_error(inner) {
            return upcall_err(u);
        }
    }
    fallback(e)
}

// ============================================================================
// The runtime
// ============================================================================

/// Delivers into one channel's queue and wakes the runtime's pump.
struct WakingSink {
    queue: Arc<UpcallQueue>,
    wake: Arc<Notify>,
}

impl UpcallSink for WakingSink {
    fn deliver(&self, event: UpcallEvent) -> bool {
        let ok = self.queue.deliver(event);
        if ok {
            self.wake.notify_one();
        }
        ok
    }
}

struct Channel {
    upcalls: Upcalls,
    queue: Arc<UpcallQueue>,
}

struct RtInner {
    channels: Mutex<BTreeMap<u64, Arc<Channel>>>,
    next_channel: AtomicU64,
    wake: Arc<Notify>,
    capacity: usize,
    policies: Mutex<HashMap<UpcallKind, UpcallPolicy>>,
    default_policy: Mutex<UpcallPolicy>,
    closed: AtomicBool,
    /// Round-robin start for `drain`, so one busy channel cannot starve
    /// the others.
    cursor: AtomicU64,
    /// The expose HTTP body-handle table (R9), one per runtime: every
    /// `expose_http` handler bound here streams bodies through it.
    bodies: HttpBodies,
    /// `max_chunk` of the `expose_http` handler bound on each channel
    /// (what `HttpBodyStream.write` splits at).
    chunk_limits: Mutex<HashMap<u64, usize>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Lock, ignoring poisoning.
pub(crate) fn lock_mutex<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock(m)
}

impl RtInner {
    fn channels(&self) -> Vec<(u64, Arc<Channel>)> {
        lock(&self.channels)
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }

    fn has_events(&self) -> bool {
        lock(&self.channels).values().any(|c| !c.queue.is_empty())
    }

    fn channel(&self, id: u64) -> Option<Arc<Channel>> {
        lock(&self.channels).get(&id).cloned()
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        for (_, ch) in self.channels() {
            // Cancel{shutdown} for every pending upcall reaches the pump
            // before the queue stops accepting.
            ch.upcalls.close();
            ch.queue.close();
        }
        self.wake.notify_one();
    }
}

/// The native half of `aster.upcalls.UpcallRuntime`. Not used directly.
#[pyclass(frozen, module = "aster._aster", name = "_UpcallRuntime")]
pub struct UpcallRuntime {
    inner: Arc<RtInner>,
}

impl UpcallRuntime {
    /// The `Upcalls` table of `channel`, for building an adapter on it.
    pub(crate) fn upcalls(&self, channel: u64) -> PyResult<Upcalls> {
        self.inner
            .channel(channel)
            .map(|c| c.upcalls.clone())
            .ok_or_else(|| invalid(format!("no upcall channel {channel}")))
    }

    /// Record `channel`'s `expose_http` chunk limit.
    pub(crate) fn set_max_chunk(&self, channel: u64, max_chunk: usize) {
        lock(&self.inner.chunk_limits).insert(channel, max_chunk);
    }

    /// The runtime's expose HTTP body-handle table.
    pub(crate) fn bodies(&self) -> HttpBodies {
        self.inner.bodies.clone()
    }
}

/// A decoded event for the Python pump:
/// `(channel, invocation_id, kind, deadline_unix_ms, args)`; for a `Cancel`
/// event `args` is the reason name.
type PyEvent = Py<PyTuple>;

fn cancel_name(ev: &UpcallEvent) -> &'static str {
    match ev.cancel_reason() {
        Some(CancelReason::Timeout) => "timeout",
        Some(CancelReason::Abandoned) => "abandoned",
        Some(CancelReason::Shutdown) => "shutdown",
        None => "unknown",
    }
}

#[pymethods]
impl UpcallRuntime {
    /// A runtime whose channels each queue at most `capacity` upcalls.
    #[new]
    #[pyo3(signature = (capacity=1024))]
    fn new(capacity: usize) -> Self {
        crate::ensure_tokio_runtime();
        Self {
            inner: Arc::new(RtInner {
                channels: Mutex::new(BTreeMap::new()),
                next_channel: AtomicU64::new(1),
                wake: Arc::new(Notify::new()),
                capacity: capacity.max(1),
                policies: Mutex::new(HashMap::new()),
                default_policy: Mutex::new(UpcallPolicy::default()),
                closed: AtomicBool::new(false),
                cursor: AtomicU64::new(0),
                bodies: HttpBodies::new(),
                chunk_limits: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Open a channel (one `Upcalls` table + queue) and return its id.
    fn open_channel(&self) -> PyResult<u64> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(upcall_err(&UpcallError::Closed));
        }
        let queue = UpcallQueue::new(self.inner.capacity);
        let upcalls = Upcalls::new(Arc::new(WakingSink {
            queue: queue.clone(),
            wake: self.inner.wake.clone(),
        }));
        upcalls.set_default_policy(*lock(&self.inner.default_policy));
        for (k, p) in lock(&self.inner.policies).iter() {
            upcalls.set_policy(*k, *p);
        }
        let id = self.inner.next_channel.fetch_add(1, Ordering::Relaxed);
        lock(&self.inner.channels).insert(id, Arc::new(Channel { upcalls, queue }));
        Ok(id)
    }

    /// Close one channel: its pending upcalls fail (`Closed`; decisions
    /// fall back to their failure mode) and later ones fail at once.
    fn close_channel(&self, channel: u64) {
        let ch = lock(&self.inner.channels).remove(&channel);
        if let Some(ch) = ch {
            ch.upcalls.close();
            ch.queue.close();
        }
    }

    /// Set `kind`'s timeout (seconds) and failure mode (`"fail_open"` /
    /// `"fail_closed"`, decision kinds only) on every channel, now and
    /// later.
    fn set_policy(
        &self,
        kind: &Bound<'_, PyAny>,
        timeout: f64,
        failure_mode: &str,
    ) -> PyResult<()> {
        let kind = kind_in(kind)?;
        let p = policy_from(timeout, failure_mode)?;
        lock(&self.inner.policies).insert(kind, p);
        for (_, ch) in self.inner.channels() {
            ch.upcalls.set_policy(kind, p);
        }
        Ok(())
    }

    /// The policy for kinds without their own.
    fn set_default_policy(&self, timeout: f64, failure_mode: &str) -> PyResult<()> {
        let p = policy_from(timeout, failure_mode)?;
        *lock(&self.inner.default_policy) = p;
        let own = lock(&self.inner.policies).clone();
        for (_, ch) in self.inner.channels() {
            ch.upcalls.set_default_policy(p);
            // `Upcalls` has no "clear"; re-apply explicit ones (unchanged).
            for (k, kp) in &own {
                ch.upcalls.set_policy(*k, *kp);
            }
        }
        Ok(())
    }

    /// `(timeout_seconds, failure_mode)` for `kind`.
    fn policy(&self, kind: &Bound<'_, PyAny>) -> PyResult<(f64, &'static str)> {
        let kind = kind_in(kind)?;
        let p = lock(&self.inner.policies)
            .get(&kind)
            .copied()
            .unwrap_or_else(|| *lock(&self.inner.default_policy));
        Ok((p.timeout.as_secs_f64(), mode_out(p.failure_mode)))
    }

    /// Upcalls waiting for an answer, across channels.
    fn pending(&self) -> usize {
        self.inner
            .channels()
            .iter()
            .map(|(_, c)| c.upcalls.pending_count())
            .sum()
    }

    /// Open channels.
    fn channel_count(&self) -> usize {
        lock(&self.inner.channels).len()
    }

    #[getter]
    fn closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Awaitable: `True` once events can be drained, `False` once the
    /// runtime is closed and drained.
    fn ready<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            loop {
                let notified = inner.wake.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if inner.has_events() {
                    return Ok(true);
                }
                if inner.closed.load(Ordering::Acquire) {
                    return Ok(false);
                }
                notified.await;
            }
        })
    }

    /// Take up to `max` events across channels, decoded:
    /// `(channel, invocation_id, kind, deadline_unix_ms, args)`. An
    /// undecodable request is answered `ERROR` here and not returned.
    #[pyo3(signature = (max=64))]
    fn drain(&self, py: Python<'_>, max: usize) -> PyResult<Vec<PyEvent>> {
        let max = max.clamp(1, DRAIN_MAX);
        let channels = self.inner.channels();
        let mut raw: Vec<(u64, Upcalls, UpcallEvent)> = Vec::new();
        if !channels.is_empty() {
            let start = self.inner.cursor.fetch_add(1, Ordering::Relaxed) as usize % channels.len();
            for i in 0..channels.len() {
                let (id, ch) = &channels[(start + i) % channels.len()];
                let room = max - raw.len();
                if room == 0 {
                    break;
                }
                ch.queue
                    .drain(room, |ev| raw.push((*id, ch.upcalls.clone(), ev)));
            }
        }
        let mut out = Vec::with_capacity(raw.len());
        for (channel, upcalls, ev) in raw {
            let args: Bound<'_, PyAny> = if ev.kind == UpcallKind::Cancel {
                cancel_name(&ev).into_pyobject(py)?.into_any()
            } else {
                let max_chunk = lock(&self.inner.chunk_limits)
                    .get(&channel)
                    .copied()
                    .unwrap_or(crate::expose::DEFAULT_MAX_CHUNK);
                match decode_request(py, ev.kind, &ev.payload, &self.inner.bodies, max_chunk) {
                    Ok(t) => t.into_any(),
                    Err(e) => {
                        let _ = upcalls.respond(
                            ev.invocation_id,
                            UpcallStatus::Error,
                            format!("undecodable {} request: {e}", ev.kind).into_bytes(),
                        );
                        continue;
                    }
                }
            };
            let t = PyTuple::new(
                py,
                [
                    channel.into_pyobject(py)?.into_any(),
                    ev.invocation_id.into_pyobject(py)?.into_any(),
                    ev.kind.name().into_pyobject(py)?.into_any(),
                    ev.deadline_unix_ms.into_pyobject(py)?.into_any(),
                    args,
                ],
            )?;
            out.push(t.unbind());
        }
        Ok(out)
    }

    /// Answer `invocation_id` on `channel` with the handler's `value`,
    /// encoded as `kind`'s UCv1 response. Returns whether a waiter got it
    /// (`False`: cancelled, timed out or already answered -- ignore).
    /// Raises `ValueError` when `value` does not encode; the caller then
    /// answers with `respond_error`.
    fn respond(
        &self,
        channel: u64,
        invocation_id: u64,
        kind: &Bound<'_, PyAny>,
        value: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let kind = kind_in(kind)?;
        let bytes = encode_response(kind, value)?;
        Ok(self.answer(channel, invocation_id, UpcallStatus::Ok, bytes))
    }

    /// Answer with `ERROR` and a message.
    fn respond_error(&self, channel: u64, invocation_id: u64, message: &str) -> bool {
        self.answer(
            channel,
            invocation_id,
            UpcallStatus::Error,
            message.as_bytes().to_vec(),
        )
    }

    /// Answer with `UNIMPLEMENTED` (no handler for the kind).
    fn respond_unimplemented(&self, channel: u64, invocation_id: u64) -> bool {
        self.answer(
            channel,
            invocation_id,
            UpcallStatus::Unimplemented,
            Vec::new(),
        )
    }

    /// Close every channel (pending upcalls fail; `Cancel{shutdown}` is
    /// queued for each) and stop accepting new ones. Idempotent.
    fn close(&self) {
        self.inner.close();
    }

    fn __repr__(&self) -> String {
        format!(
            "_UpcallRuntime(channels={}, pending={}, closed={})",
            self.channel_count(),
            self.pending(),
            self.closed()
        )
    }
}

impl UpcallRuntime {
    fn answer(&self, channel: u64, id: u64, status: UpcallStatus, payload: Vec<u8>) -> bool {
        match self.inner.channel(channel) {
            Some(ch) => ch.upcalls.respond(id, status, payload).is_ok(),
            None => false,
        }
    }
}

/// Bind the Python object `obj` as an upcall handler of `role` (see
/// `aster.upcalls._bind`: one channel per object and role on the current
/// loop's runtime, its pump started) and return the channel's table.
pub(crate) fn bind(obj: &Bound<'_, PyAny>, role: &str) -> PyResult<Upcalls> {
    let py = obj.py();
    let bound = py
        .import("aster.upcalls")?
        .getattr("_bind")?
        .call1((obj, role))?;
    let (rt, channel): (Bound<'_, UpcallRuntime>, u64) = bound.extract()?;
    rt.get().upcalls(channel)
}

/// As [`bind`], plus the runtime's HTTP body table (expose HTTP handlers).
pub(crate) fn bind_http(
    obj: &Bound<'_, PyAny>,
    role: &str,
    max_chunk: usize,
) -> PyResult<(Upcalls, HttpBodies)> {
    let py = obj.py();
    let bound = py
        .import("aster.upcalls")?
        .getattr("_bind")?
        .call1((obj, role))?;
    let (rt, channel): (Bound<'_, UpcallRuntime>, u64) = bound.extract()?;
    let rt = rt.get();
    rt.set_max_chunk(channel, max_chunk);
    Ok((rt.upcalls(channel)?, rt.bodies()))
}

/// Enter the shared tokio runtime (for adapters that spawn on
/// construction).
pub(crate) fn tokio_guard() -> tokio::runtime::EnterGuard<'static> {
    pyo3_async_runtimes::tokio::get_runtime().enter()
}

// ============================================================================
// UCv1 <-> Python values
// ============================================================================

fn cerr(e: anyhow::Error) -> PyErr {
    PyValueError::new_err(e.to_string())
}

fn tuple<'py>(py: Python<'py>, items: Vec<Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyTuple>> {
    PyTuple::new(py, items)
}

pub(crate) fn bytes_out<'py>(py: Python<'py>, b: &[u8]) -> Bound<'py, PyAny> {
    PyBytes::new(py, b).into_any()
}

/// Raw bytes from `bytes` / `bytearray` / `memoryview` (not `str`).
pub(crate) fn raw_bytes(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<Vec<u8>> {
    if let Ok(b) = obj.cast::<PyBytes>() {
        return Ok(b.as_bytes().to_vec());
    }
    if obj.extract::<String>().is_ok() {
        return Err(invalid(format!("{what} must be bytes, not str")));
    }
    let b = obj
        .py()
        .import("builtins")?
        .getattr("bytes")?
        .call1((obj,))
        .map_err(|_| invalid(format!("{what} must be bytes")))?;
    Ok(b.cast::<PyBytes>()?.as_bytes().to_vec())
}

fn strict_bool(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<bool> {
    obj.extract::<bool>()
        .map_err(|_| invalid(format!("{what} must be a bool")))
}

pub(crate) fn str_in(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<String> {
    obj.extract()
        .map_err(|_| invalid(format!("{what} must be a str")))
}

/// Exactly `n` items of a tuple-like request value.
pub(crate) fn items<'py>(
    obj: &Bound<'py, PyAny>,
    n: usize,
    what: &str,
) -> PyResult<Vec<Bound<'py, PyAny>>> {
    let v: Vec<Bound<'py, PyAny>> = obj
        .try_iter()
        .map_err(|_| invalid(format!("{what} must be a {n}-tuple")))?
        .collect::<PyResult<_>>()?;
    if v.len() != n {
        return Err(invalid(format!(
            "{what} must be a {n}-tuple, got {}",
            v.len()
        )));
    }
    Ok(v)
}

fn hexes_in(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<Vec<String>> {
    obj.try_iter()
        .map_err(|_| invalid(format!("{what} must be an iterable of node ids")))?
        .map(|i| node_hex(&i?))
        .collect()
}

fn ids_out<'py>(py: Python<'py>, ids: &[String]) -> PyResult<Bound<'py, PyAny>> {
    Ok(PyTuple::new(py, ids.iter().map(|h| node_id_out(h.clone())))?.into_any())
}

fn view_out<'py>(py: Python<'py>, v: &TopologyViewParts) -> PyResult<Bound<'py, PyAny>> {
    let clusters = crate::topology::clusters_out(py, &v.clusters)?;
    let pairs = v
        .far_pairs
        .iter()
        .map(|(a, b)| {
            PyTuple::new(py, [node_id_out(a.clone()), node_id_out(b.clone())]).map(Bound::into_any)
        })
        .collect::<PyResult<Vec<_>>>()?;
    py_value!(py, "aster.topology", "TopologyView" {
        clusters: clusters,
        far_pairs: PyTuple::new(py, pairs)?,
        separation_coverage: v.separation_coverage,
    })
}

pub(crate) fn view_in(obj: &Bound<'_, PyAny>) -> PyResult<TopologyViewParts> {
    let mut clusters = Vec::new();
    for c in obj
        .getattr("clusters")?
        .try_iter()
        .map_err(|_| invalid("TopologyView.clusters must be an iterable of ClusterView"))?
    {
        let c = c?;
        clusters.push(ClusterView {
            members: hexes_in(&c.getattr("members")?, "ClusterView.members")?,
            bridge: node_hex(&c.getattr("bridge")?)?,
            witnesses: hexes_in(&c.getattr("witnesses")?, "ClusterView.witnesses")?,
        });
    }
    let mut far_pairs = Vec::new();
    for p in obj
        .getattr("far_pairs")?
        .try_iter()
        .map_err(|_| invalid("TopologyView.far_pairs must be an iterable of pairs"))?
    {
        let p = items(&p?, 2, "TopologyView.far_pairs[]")?;
        far_pairs.push((node_hex(&p[0])?, node_hex(&p[1])?));
    }
    Ok(TopologyViewParts {
        clusters,
        far_pairs,
        separation_coverage: obj
            .getattr("separation_coverage")?
            .extract()
            .map_err(|_| invalid("TopologyView.separation_coverage must be a non-negative int"))?,
    })
}

fn provision_out<'py>(py: Python<'py>, r: &ProvisionRequest) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.coordination", "ProvisionRequest" {
        group_id: r.group_id.to_hex(),
        policy_digest: hex::encode(r.policy_digest),
        membership_log_id: opt_log_out(py, &r.membership_log_id)?,
        node_id: nid(r.node_id.as_bytes()),
    })
}

fn provision_in(obj: &Bound<'_, PyAny>) -> PyResult<ProvisionRequest> {
    let log = obj.getattr("membership_log_id")?;
    Ok(ProvisionRequest {
        group_id: group_in(&obj.getattr("group_id")?)?,
        policy_digest: id32(&obj.getattr("policy_digest")?, "policy_digest")?,
        membership_log_id: log_in(Some(&log))?,
        node_id: node_id_in(&obj.getattr("node_id")?, "node_id")?,
    })
}

fn handshake_out<'py>(py: Python<'py>, d: &HandshakeDecision) -> PyResult<Bound<'py, PyAny>> {
    match d {
        HandshakeDecision::Accept => Ok(true.into_pyobject(py)?.to_owned().into_any()),
        HandshakeDecision::Reject { code, reason } => Ok(tuple(
            py,
            vec![code.into_pyobject(py)?.into_any(), bytes_out(py, reason)],
        )?
        .into_any()),
    }
}

/// `True` accepts; `False` rejects with `403 "peer not admitted"` (the
/// gate's default); `(code, reason)` rejects with those.
fn handshake_in(obj: &Bound<'_, PyAny>) -> PyResult<HandshakeDecision> {
    if let Ok(b) = obj.extract::<bool>() {
        return Ok(if b {
            HandshakeDecision::Accept
        } else {
            HandshakeDecision::Reject {
                code: 403,
                reason: b"peer not admitted".to_vec(),
            }
        });
    }
    let v = items(obj, 2, "a handshake rejection (code, reason)")?;
    let code: u32 = v[0]
        .extract()
        .map_err(|_| invalid("handshake reject code must be a u32"))?;
    let reason = if let Ok(s) = v[1].extract::<String>() {
        s.into_bytes()
    } else {
        raw_bytes(&v[1], "handshake reject reason")?
    };
    Ok(HandshakeDecision::Reject { code, reason })
}

/// The UCv1 request type a kind sends, and the response type it expects.
fn types_of(kind: UpcallKind) -> Option<(&'static str, &'static str)> {
    Some(match kind {
        UpcallKind::LeaseTransition => ("LeaseTransitionRequest", "TransitionOutcome"),
        UpcallKind::LeaseSnapshot => ("LeaseSnapshotRequest", "LeaseSnapshot"),
        UpcallKind::LeaseAuthorize => ("LeaseAuthorizeRequest", "Decision"),
        UpcallKind::FencedMutate => ("FencedMutateRequest", "FencedMutateResponse"),
        UpcallKind::TopologyAdmitted => ("TopologyAdmittedRequest", "TopologyAdmittedResponse"),
        UpcallKind::TopologySnapshot => ("TopologySnapshotRequest", "TopologySnapshot"),
        UpcallKind::PlacementPolicy => ("PlacementPolicyRequest", "PlacementPolicy"),
        UpcallKind::PlacementProvision => ("PlacementProvisionRequest", "MemberSeed"),
        UpcallKind::ExposeAuthorize => ("ExposeAuthorizeRequest", "Decision"),
        UpcallKind::AdmissionConnect => ("AdmissionConnectRequest", "Decision"),
        UpcallKind::AdmissionHandshake => {
            ("AdmissionHandshakeRequest", "AdmissionHandshakeResponse")
        }
        UpcallKind::ExposeHttp => ("ExposeHttpRequest", "ExposeHttpResponse"),
        UpcallKind::ExposeRoutePolicy => ("ExposeRoutePolicyRequest", "ExposeRouteGrant"),
        _ => return None,
    })
}

fn decode_request<'py>(
    py: Python<'py>,
    kind: UpcallKind,
    b: &[u8],
    bodies: &HttpBodies,
    max_chunk: usize,
) -> PyResult<Bound<'py, PyAny>> {
    let (req, _) = types_of(kind).ok_or_else(|| invalid(format!("no codec for {kind}")))?;
    decode_type_with(py, req, b, Some((bodies, max_chunk)))
}

fn encode_response(kind: UpcallKind, value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    let (_, resp) = types_of(kind).ok_or_else(|| invalid(format!("no codec for {kind}")))?;
    encode_type(resp, value)
}

/// Decode UCv1 `b` as message type `ty` into its Python value (requests
/// are the handler's positional arguments as a tuple).
pub(crate) fn decode_type<'py>(py: Python<'py>, ty: &str, b: &[u8]) -> PyResult<Bound<'py, PyAny>> {
    decode_type_with(py, ty, b, None)
}

/// [`decode_type`]; an `ExposeHttpRequest` gets a body handle on `bodies`
/// (detached without one).
fn decode_type_with<'py>(
    py: Python<'py>,
    ty: &str,
    b: &[u8],
    bodies: Option<(&HttpBodies, usize)>,
) -> PyResult<Bound<'py, PyAny>> {
    use crate::expose as ex;
    let t = |v: Vec<Bound<'py, PyAny>>| tuple(py, v).map(Bound::into_any);
    match ty {
        "ExposeHttpRequest" => {
            let req = codec::decode_expose_http_request(b).map_err(cerr)?;
            t(vec![ex::http_request_out(
                py,
                req,
                bodies.map(|(b, m)| (b.clone(), m)),
            )?])
        }
        "ExposeHttpResponse" => {
            ex::http_response_out(py, codec::decode_expose_http_response(b).map_err(cerr)?)
        }
        "ExposeHttpTrailers" => {
            ex::headers_out(py, &codec::decode_expose_http_trailers(b).map_err(cerr)?)
        }
        "ExposeRoutePolicyRequest" => {
            let (peer, meta, specs) = codec::decode_expose_route_policy_request(b).map_err(cerr)?;
            t(vec![
                node_id_out(peer).into_pyobject(py)?.into_any(),
                bytes_out(py, &meta),
                ex::route_specs_out(py, &specs)?,
            ])
        }
        "ExposeRouteGrant" => {
            let v = codec::decode_expose_route_grant(b).map_err(cerr)?;
            Ok(PyTuple::new(py, v)?.into_any())
        }
        "LeaseTransitionRequest" => {
            let (resource, op) = codec::decode_lease_transition_request(b).map_err(cerr)?;
            t(vec![
                resource.into_pyobject(py)?.into_any(),
                op_out(py, &op)?,
            ])
        }
        "TransitionOutcome" => outcome_out(py, codec::decode_transition_outcome(b).map_err(cerr)?),
        "LeaseSnapshotRequest" => {
            let resource = codec::decode_lease_snapshot_request(b).map_err(cerr)?;
            t(vec![resource.into_pyobject(py)?.into_any()])
        }
        "LeaseSnapshot" => snapshot_out(py, codec::decode_lease_snapshot(b).map_err(cerr)?),
        "LeaseAuthorizeRequest" => {
            let op = codec::decode_lease_authorize_request(b).map_err(cerr)?;
            t(vec![op_out(py, &op)?])
        }
        "FencedMutateRequest" => {
            let (fence, writer, op) = codec::decode_fenced_mutate_request(b).map_err(cerr)?;
            t(vec![
                fence_out(py, fence)?,
                writer.into_pyobject(py)?.into_any(),
                bytes_out(py, &op),
            ])
        }
        "FencedMutateResponse" => match codec::decode_fenced_mutate_response(b).map_err(cerr)? {
            Ok(out) => Ok(bytes_out(py, &out)),
            Err(r) => rejection_out(py, r),
        },
        "Decision" => Ok(codec::decode_decision(b)
            .map_err(cerr)?
            .into_pyobject(py)?
            .to_owned()
            .into_any()),
        "TopologyAdmittedRequest" => {
            let ids = codec::decode_topology_admitted_request(b).map_err(cerr)?;
            t(vec![ids_out(py, &ids)?])
        }
        "TopologyAdmittedResponse" => {
            let v = codec::decode_topology_admitted_response(b).map_err(cerr)?;
            Ok(PyTuple::new(py, v)?.into_any())
        }
        "TopologySnapshotRequest" => {
            codec::decode_topology_snapshot_request(b).map_err(cerr)?;
            t(vec![])
        }
        "TopologySnapshot" => view_out(py, &codec::decode_topology_snapshot(b).map_err(cerr)?),
        "ExposeAuthorizeRequest" => {
            let (service, peer, meta) = codec::decode_expose_authorize_request(b).map_err(cerr)?;
            t(vec![
                service.into_pyobject(py)?.into_any(),
                node_id_out(peer).into_pyobject(py)?.into_any(),
                bytes_out(py, &meta),
            ])
        }
        "AdmissionConnectRequest" => {
            let (peer, alpn) = codec::decode_admission_connect_request(b).map_err(cerr)?;
            t(vec![
                node_id_out(peer).into_pyobject(py)?.into_any(),
                bytes_out(py, &alpn),
            ])
        }
        "AdmissionHandshakeRequest" => {
            let (peer, alpn, alive) = codec::decode_admission_handshake_request(b).map_err(cerr)?;
            t(vec![
                node_id_out(peer).into_pyobject(py)?.into_any(),
                bytes_out(py, &alpn),
                alive.into_pyobject(py)?.to_owned().into_any(),
            ])
        }
        "AdmissionHandshakeResponse" => handshake_out(
            py,
            &codec::decode_admission_handshake_response(b).map_err(cerr)?,
        ),
        "PlacementPolicyRequest" => {
            codec::decode_placement_policy_request(b).map_err(cerr)?;
            t(vec![])
        }
        "PlacementPolicy" => policy_out(py, &codec::decode_placement_policy(b).map_err(cerr)?),
        "PlacementProvisionRequest" => {
            let r = codec::decode_provision_request(b).map_err(cerr)?;
            t(vec![provision_out(py, &r)?])
        }
        "MemberSeed" => seed_out(py, &codec::decode_member_seed(b).map_err(cerr)?),
        other => Err(invalid(format!("unknown UCv1 message type {other:?}"))),
    }
}

/// Encode the Python value `v` as UCv1 message type `ty`.
pub(crate) fn encode_type(ty: &str, v: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    use crate::expose as ex;
    Ok(match ty {
        "ExposeHttpRequest" => {
            let a = items(v, 1, ty)?;
            codec::encode_expose_http_request(&ex::http_request_in(&a[0])?)
        }
        "ExposeHttpResponse" => codec::encode_expose_http_response(&ex::http_response_in(v)?),
        "ExposeHttpTrailers" => codec::encode_expose_http_trailers(&ex::headers_in(v, "trailers")?),
        "ExposeRoutePolicyRequest" => {
            let a = items(v, 3, ty)?;
            codec::encode_expose_route_policy_request(
                &node_hex(&a[0])?,
                &raw_bytes(&a[1], "metadata")?,
                &ex::route_specs_in(&a[2])?,
            )
        }
        "ExposeRouteGrant" => codec::encode_expose_route_grant(&ex::grant_in(v)?),
        "LeaseTransitionRequest" => {
            let a = items(v, 2, ty)?;
            codec::encode_lease_transition_request(&str_in(&a[0], "resource")?, &op_in(&a[1])?)
        }
        "TransitionOutcome" => codec::encode_transition_outcome(&outcome_in(v)?),
        "LeaseSnapshotRequest" => {
            let a = items(v, 1, ty)?;
            codec::encode_lease_snapshot_request(&str_in(&a[0], "resource")?)
        }
        "LeaseSnapshot" => codec::encode_lease_snapshot(&snapshot_in(v)?),
        "LeaseAuthorizeRequest" => {
            let a = items(v, 1, ty)?;
            codec::encode_lease_authorize_request(&op_in(&a[0])?)
        }
        "FencedMutateRequest" => {
            let a = items(v, 3, ty)?;
            codec::encode_fenced_mutate_request(
                &fence_in(&a[0])?,
                &crate::keys::actor_in(&a[1])?,
                &raw_bytes(&a[2], "op")?,
            )
        }
        "FencedMutateResponse" => {
            if v.is_instance_of::<PyBaseException>() {
                codec::encode_fenced_mutate_response(&Err(rejection_in(v)?))
            } else {
                codec::encode_fenced_mutate_response(&Ok(raw_bytes(v, "fenced mutation output")?))
            }
        }
        "Decision" => codec::encode_decision(strict_bool(v, "a decision")?),
        "TopologyAdmittedRequest" => {
            let a = items(v, 1, ty)?;
            codec::encode_topology_admitted_request(&hexes_in(&a[0], "node_ids")?)
        }
        "TopologyAdmittedResponse" => {
            let answers = v
                .try_iter()
                .map_err(|_| invalid("admitted answers must be an iterable of bool"))?
                .map(|i| strict_bool(&i?, "an admitted answer"))
                .collect::<PyResult<Vec<bool>>>()?;
            codec::encode_topology_admitted_response(&answers)
        }
        "TopologySnapshotRequest" => {
            items(v, 0, ty)?;
            codec::encode_topology_snapshot_request()
        }
        "TopologySnapshot" => codec::encode_topology_snapshot(&view_in(v)?),
        "ExposeAuthorizeRequest" => {
            let a = items(v, 3, ty)?;
            codec::encode_expose_authorize_request(
                &str_in(&a[0], "service_id")?,
                &node_hex(&a[1])?,
                &raw_bytes(&a[2], "metadata")?,
            )
        }
        "AdmissionConnectRequest" => {
            let a = items(v, 2, ty)?;
            codec::encode_admission_connect_request(&node_hex(&a[0])?, &raw_bytes(&a[1], "alpn")?)
        }
        "AdmissionHandshakeRequest" => {
            let a = items(v, 3, ty)?;
            codec::encode_admission_handshake_request(
                &node_hex(&a[0])?,
                &raw_bytes(&a[1], "alpn")?,
                strict_bool(&a[2], "is_alive")?,
            )
        }
        "AdmissionHandshakeResponse" => {
            codec::encode_admission_handshake_response(&handshake_in(v)?)
        }
        "PlacementPolicyRequest" => {
            items(v, 0, ty)?;
            codec::encode_placement_policy_request()
        }
        "PlacementPolicy" => codec::encode_placement_policy(&policy_in(v)?),
        "PlacementProvisionRequest" => {
            let a = items(v, 1, ty)?;
            codec::encode_provision_request(&provision_in(&a[0])?)
        }
        "MemberSeed" => codec::encode_member_seed(&seed_in(v)?),
        other => return Err(invalid(format!("unknown UCv1 message type {other:?}"))),
    })
}

/// Decode UCv1 bytes as `type` (conformance / debugging).
#[pyfunction]
#[pyo3(name = "_upcall_decode")]
fn upcall_decode<'py>(py: Python<'py>, ty: &str, data: &[u8]) -> PyResult<Bound<'py, PyAny>> {
    decode_type(py, ty, data)
}

/// Encode a Python value as UCv1 `type` (conformance / debugging).
#[pyfunction]
#[pyo3(name = "_upcall_encode")]
fn upcall_encode<'py>(
    py: Python<'py>,
    ty: &str,
    value: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyBytes>> {
    Ok(PyBytes::new(py, &encode_type(ty, value)?))
}

/// The kind table: `[(code, name, is_decision)]`.
#[pyfunction]
#[pyo3(name = "_upcall_kinds")]
fn upcall_kinds() -> Vec<(u16, &'static str, bool)> {
    UpcallKind::ALL
        .iter()
        .map(|k| (k.code(), k.name(), k.is_decision()))
        .collect()
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<UpcallRuntime>()?;
    m.add_function(wrap_pyfunction!(upcall_decode, m)?)?;
    m.add_function(wrap_pyfunction!(upcall_encode, m)?)?;
    m.add_function(wrap_pyfunction!(upcall_kinds, m)?)?;
    Ok(())
}
