//! Single-owner fenced leases -- wraps [`aster::lease`] (plan P7).
//!
//! Granting: a [`Lease`] acquired against the in-memory reference serializer
//! ([`MemorySerializer`]) or a remote designated grantor
//! ([`GrantorSerializer`], `aster.lease.Grantor` over RPC). The handle renews
//! in Rust and stops handing out its fence at the self-limit.
//!
//! Enforcing: `MemorySerializer.mutate_fenced` runs a synchronous Python
//! callable under the serializer's lock after the fence check; the pure
//! `lease_transition` / `check_fence` let a Python-owned resource apply the
//! same rules inside its own serialization point.
//!
//! Value types (`Fence`, `LeaseSnapshot`, `TransitionOutcome`, ...) are the
//! frozen dataclasses of `aster.lease`, built here by keyword; durations are
//! seconds (`float`).
//!
//! Deferred (needs the R6 upcall mechanism): a Python `LeaseSerializer`
//! backend, a Python `FencedResource`, and a lease authorizer callback.

use std::sync::{Arc, Mutex};

use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;
use tokio::sync::watch;

use aster::lease as l;
use aster::lease::LeaseSerializer as _;

use crate::error::err_to_py;
use crate::keys::{actor_in as node_hex, invalid, py_value, secs_in};

// ============================================================================
// Value conversion
// ============================================================================

fn fence_out<'py>(py: Python<'py>, f: l::Fence) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.lease", "Fence" {
        resource: f.resource,
        epoch: f.epoch,
        holder: f.holder,
    })
}

fn fence_in(obj: &Bound<'_, PyAny>) -> PyResult<l::Fence> {
    Ok(l::Fence {
        resource: obj.getattr("resource")?.extract()?,
        epoch: obj
            .getattr("epoch")?
            .extract()
            .map_err(|_| invalid("Fence.epoch must be a non-negative int"))?,
        holder: node_hex(&obj.getattr("holder")?)?,
    })
}

fn snapshot_out<'py>(py: Python<'py>, s: l::LeaseSnapshot) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.lease", "LeaseSnapshot" {
        resource: s.resource,
        epoch: s.epoch,
        holder: s.holder,
        held: s.held,
        remaining: s.remaining.map(|d| d.as_secs_f64()),
    })
}

fn deny_out(r: l::DenyReason) -> &'static str {
    match r {
        l::DenyReason::HeldByOther => "held_by_other",
        l::DenyReason::EpochMismatch => "epoch_mismatch",
        l::DenyReason::NotHolder => "not_holder",
        l::DenyReason::Expired => "expired",
        l::DenyReason::NotHeld => "not_held",
        l::DenyReason::Unauthorized => "unauthorized",
    }
}

fn status_out(s: l::LeaseStatus) -> &'static str {
    match s {
        l::LeaseStatus::Held => "held",
        l::LeaseStatus::Released => "released",
        l::LeaseStatus::Fenced => "fenced",
        l::LeaseStatus::Lost => "lost",
    }
}

fn outcome_out<'py>(py: Python<'py>, o: l::TransitionOutcome) -> PyResult<Bound<'py, PyAny>> {
    let (kind, fence, deny, snap) = match o {
        l::TransitionOutcome::Granted(f) => ("granted", Some(fence_out(py, f)?), None, None),
        l::TransitionOutcome::Renewed => ("renewed", None, None, None),
        l::TransitionOutcome::Released => ("released", None, None, None),
        l::TransitionOutcome::Revoked => ("revoked", None, None, None),
        l::TransitionOutcome::Denied(r, s) => (
            "denied",
            None,
            Some(deny_out(r)),
            Some(snapshot_out(py, s)?),
        ),
    };
    py_value!(py, "aster.lease", "TransitionOutcome" {
        kind: kind,
        fence: fence,
        deny_reason: deny,
        snapshot: snap,
    })
}

fn rejection_out<'py>(py: Python<'py>, r: l::FenceRejection) -> PyResult<Bound<'py, PyAny>> {
    let message = r.to_string();
    let (kind, carried, current) = match r {
        l::FenceRejection::StaleEpoch { carried, current } => {
            ("stale_epoch", Some(carried), Some(current))
        }
        l::FenceRejection::NotHolder => ("not_holder", None, None),
        l::FenceRejection::NotHeld => ("not_held", None, None),
        l::FenceRejection::FutureEpoch { carried, current } => {
            ("future_epoch", Some(carried), Some(current))
        }
    };
    py_value!(py, "aster.lease", "FenceRejectedError" {
        kind: kind,
        message: message,
        carried: carried,
        current: current,
    })
}

fn row_in(obj: &Bound<'_, PyAny>) -> PyResult<l::RowState> {
    let holder = obj.getattr("holder")?;
    Ok(l::RowState {
        epoch: obj
            .getattr("epoch")?
            .extract()
            .map_err(|_| invalid("LeaseRowState.epoch must be a non-negative int"))?,
        holder: if holder.is_none() {
            None
        } else {
            Some(node_hex(&holder)?)
        },
        held: obj.getattr("held")?.extract()?,
        ttl: secs_in("LeaseRowState.ttl", obj.getattr("ttl")?.extract()?)?,
    })
}

fn row_out<'py>(py: Python<'py>, r: l::RowState) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.lease", "LeaseRowState" {
        epoch: r.epoch,
        holder: r.holder,
        held: r.held,
        ttl: r.ttl.as_secs_f64(),
    })
}

fn op_in(obj: &Bound<'_, PyAny>) -> PyResult<l::LeaseOp> {
    let kind: String = obj.getattr("kind")?.extract()?;
    let actor = node_hex(&obj.getattr("actor")?)?;
    let epoch = || -> PyResult<u64> {
        let e = obj.getattr("epoch")?;
        if e.is_none() {
            return Err(invalid(format!("{kind} needs an epoch")));
        }
        e.extract()
            .map_err(|_| invalid("LeaseOp.epoch must be a non-negative int"))
    };
    Ok(match kind.as_str() {
        "acquire" => {
            let ttl = obj.getattr("ttl")?;
            if ttl.is_none() {
                return Err(invalid("acquire needs a ttl"));
            }
            l::LeaseOp::Acquire {
                candidate: actor,
                ttl: secs_in("LeaseOp.ttl", ttl.extract()?)?,
            }
        }
        "renew" => l::LeaseOp::Renew {
            holder: actor,
            epoch: epoch()?,
        },
        "release" => l::LeaseOp::Release {
            holder: actor,
            epoch: epoch()?,
        },
        "revoke" => l::LeaseOp::Revoke { authority: actor },
        other => return Err(invalid(format!("unknown lease op {other:?}"))),
    })
}

/// `LeaseOptions` (or `None` for the defaults), validated as the Rust
/// holder engine would.
fn options_in(obj: Option<&Bound<'_, PyAny>>) -> PyResult<l::LeaseOptions> {
    let mut opts = l::LeaseOptions::default();
    if let Some(o) = obj.filter(|o| !o.is_none()) {
        opts.ttl = secs_in("ttl", o.getattr("ttl")?.extract()?)?;
        opts.renew_interval = secs_in("renew_interval", o.getattr("renew_interval")?.extract()?)?;
        opts.holder_margin = secs_in("holder_margin", o.getattr("holder_margin")?.extract()?)?;
    }
    if opts.ttl.is_zero() {
        return Err(invalid("lease ttl must be non-zero"));
    }
    if opts.holder_margin >= opts.ttl {
        return Err(invalid("holder_margin must be smaller than ttl"));
    }
    if opts.renew_interval >= opts.ttl - opts.holder_margin {
        return Err(invalid(
            "renew_interval must fit inside ttl - holder_margin",
        ));
    }
    Ok(opts)
}

// ============================================================================
// Pure rules
// ============================================================================

/// The normative lease state machine as a pure function (backs
/// `aster.lease.lease_transition`).
#[pyfunction]
#[pyo3(name = "_lease_transition")]
fn lease_transition<'py>(
    py: Python<'py>,
    row: &Bound<'py, PyAny>,
    expired: bool,
    op: &Bound<'py, PyAny>,
    resource: &str,
) -> PyResult<Bound<'py, PyAny>> {
    match l::transition(&row_in(row)?, expired, &op_in(op)?, resource) {
        l::Transition::Apply {
            next,
            outcome,
            reset_grant,
        } => py_value!(py, "aster.lease", "LeaseTransition" {
            kind: "apply",
            next: row_out(py, next)?,
            outcome: outcome_out(py, outcome)?,
            reset_grant: reset_grant,
        }),
        l::Transition::Deny(r) => py_value!(py, "aster.lease", "LeaseTransition" {
            kind: "deny",
            deny_reason: deny_out(r),
        }),
    }
}

/// The fence rule against `row`: `None` when `writer` may act with `fence`,
/// else a `FenceRejectedError` (returned, not raised).
#[pyfunction]
#[pyo3(name = "_check_fence")]
fn check_fence<'py>(
    py: Python<'py>,
    row: &Bound<'py, PyAny>,
    fence: &Bound<'py, PyAny>,
    writer: &Bound<'py, PyAny>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    match l::check_fence(&row_in(row)?, &fence_in(fence)?, &node_hex(writer)?) {
        Ok(()) => Ok(None),
        Err(r) => rejection_out(py, r).map(Some),
    }
}

// ============================================================================
// Acquire
// ============================================================================

async fn acquire(
    serializer: Arc<dyn l::LeaseSerializer>,
    resource: String,
    holder: String,
    opts: l::LeaseOptions,
) -> PyResult<Py<PyAny>> {
    let outcome = l::LeaseHandle::acquire(serializer, &resource, &holder, opts)
        .await
        .map_err(err_to_py)?;
    Python::attach(|py| {
        let v = match outcome {
            l::AcquireOutcome::Granted(handle) => {
                let lease = Py::new(py, Lease::new(handle, holder))?;
                py_value!(py, "aster.lease", "AcquireOutcome" {
                    granted: true,
                    lease: lease,
                })?
            }
            l::AcquireOutcome::Denied(reason, snapshot) => {
                py_value!(py, "aster.lease", "AcquireOutcome" {
                    granted: false,
                    reason: deny_out(reason),
                    snapshot: snapshot_out(py, snapshot)?,
                })?
            }
        };
        Ok(v.unbind())
    })
}

async fn snapshot_of(serializer: &dyn l::LeaseSerializer, resource: &str) -> PyResult<Py<PyAny>> {
    let s = serializer.snapshot(resource).await.map_err(err_to_py)?;
    Python::attach(|py| snapshot_out(py, s).map(Bound::unbind))
}

async fn revoke_at(
    serializer: &dyn l::LeaseSerializer,
    resource: &str,
    authority: &str,
) -> PyResult<Py<PyAny>> {
    let o = l::revoke(serializer, resource, authority)
        .await
        .map_err(err_to_py)?;
    Python::attach(|py| outcome_out(py, o).map(Bound::unbind))
}

// ============================================================================
// Serializers
// ============================================================================

/// The reference in-memory lease serializer (also what a designated grantor
/// serves with `node.serve_lease_grantor`). Expiry runs on this process's
/// clock.
#[pyclass(frozen, module = "aster._aster")]
pub struct MemorySerializer {
    pub(crate) inner: l::MemorySerializer,
}

#[pymethods]
impl MemorySerializer {
    #[new]
    fn new() -> Self {
        Self {
            inner: l::MemorySerializer::new(),
        }
    }

    /// One ACQUIRE CAS for `holder` (any actor string -- usually a node id).
    /// Awaitable -> `AcquireOutcome` (`.lease` on grant; `.reason` and
    /// `.snapshot` when denied).
    #[pyo3(signature = (resource, holder, options=None))]
    fn acquire<'py>(
        &self,
        py: Python<'py>,
        resource: String,
        holder: &Bound<'py, PyAny>,
        options: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let opts = options_in(options)?;
        let holder = node_hex(holder)?;
        let ser: Arc<dyn l::LeaseSerializer> = Arc::new(self.inner.clone());
        future_into_py(py, acquire(ser, resource, holder, opts))
    }

    /// Advisory snapshot of `resource` (awaitable `LeaseSnapshot`).
    fn snapshot<'py>(&self, py: Python<'py>, resource: String) -> PyResult<Bound<'py, PyAny>> {
        let ser = self.inner.clone();
        future_into_py(py, async move { snapshot_of(&ser, &resource).await })
    }

    /// Apply one raw transition (the state machine under this serializer's
    /// lock). Prefer `acquire` + the lease; this is for tooling and tests.
    fn transition<'py>(
        &self,
        py: Python<'py>,
        resource: String,
        op: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let op = op_in(op)?;
        let ser = self.inner.clone();
        future_into_py(py, async move {
            let o = ser.transition(&resource, op).await.map_err(err_to_py)?;
            Python::attach(|py| outcome_out(py, o).map(Bound::unbind))
        })
    }

    /// REVOKE `resource` as `authority`: advances the fence unconditionally.
    fn revoke<'py>(
        &self,
        py: Python<'py>,
        resource: String,
        authority: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let authority = node_hex(authority)?;
        let ser = self.inner.clone();
        future_into_py(
            py,
            async move { revoke_at(&ser, &resource, &authority).await },
        )
    }

    /// Simulate a serializer restart: fence fields survive, held rows get a
    /// full fresh TTL, the fence never advances.
    fn restart(&self) {
        self.inner.restart();
    }

    /// Run `func()` under the serializer's lock iff the fence rule passes for
    /// `writer` (the authenticated peer id -- never a request field) and
    /// return its result; raise `FenceRejectedError` without running it
    /// otherwise. `func` must be synchronous and must not call back into this
    /// serializer.
    #[pyo3(name = "mutate_fenced")]
    fn mutate_fenced<'py>(
        &self,
        py: Python<'py>,
        fence: &Bound<'py, PyAny>,
        writer: &Bound<'py, PyAny>,
        func: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let fence = fence_in(fence)?;
        let writer = node_hex(writer)?;
        match self.inner.mutate_fenced(&fence, &writer, || func.call0()) {
            Ok(called) => called,
            Err(rejection) => Err(PyErr::from_value(rejection_out(py, rejection)?)),
        }
    }

    fn __repr__(&self) -> String {
        "MemorySerializer()".into()
    }
}

/// A remote designated grantor (`aster.lease.Grantor`) as a lease backend --
/// from `await node.lease_grantor(grantor_id)`. Every op is bound to this
/// node's authenticated identity.
#[pyclass(frozen, module = "aster._aster")]
pub struct GrantorSerializer {
    inner: Arc<aster::rpc::lease::GrantorSerializer>,
    self_id: String,
}

impl GrantorSerializer {
    pub(crate) fn new(inner: aster::rpc::lease::GrantorSerializer, self_id: String) -> Self {
        Self {
            inner: Arc::new(inner),
            self_id,
        }
    }
}

#[pymethods]
impl GrantorSerializer {
    /// This node's id -- the holder every grant is issued to.
    #[getter]
    fn self_id(&self) -> crate::ids::NodeId {
        crate::keys::node_id_out(self.self_id.clone())
    }

    /// ACQUIRE `resource` at the grantor as this node (awaitable
    /// `AcquireOutcome`).
    #[pyo3(signature = (resource, options=None))]
    fn acquire<'py>(
        &self,
        py: Python<'py>,
        resource: String,
        options: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let opts = options_in(options)?;
        let ser: Arc<dyn l::LeaseSerializer> = self.inner.clone();
        future_into_py(py, acquire(ser, resource, self.self_id.clone(), opts))
    }

    /// Advisory snapshot (`inspect` at the grantor).
    fn snapshot<'py>(&self, py: Python<'py>, resource: String) -> PyResult<Bound<'py, PyAny>> {
        let ser = self.inner.clone();
        future_into_py(
            py,
            async move { snapshot_of(ser.as_ref(), &resource).await },
        )
    }

    /// REVOKE `resource` as this node (the grantor gates it; by default only
    /// the `operator` role may).
    fn revoke<'py>(&self, py: Python<'py>, resource: String) -> PyResult<Bound<'py, PyAny>> {
        let ser = self.inner.clone();
        let me = self.self_id.clone();
        future_into_py(
            py,
            async move { revoke_at(ser.as_ref(), &resource, &me).await },
        )
    }

    fn __repr__(&self) -> String {
        format!("GrantorSerializer(self_id='{}')", self.self_id)
    }
}

// ============================================================================
// The holder handle
// ============================================================================

/// A held lease: renewed in the background (in Rust), fence access gated by
/// the self-limit. `async with lease:` (or `await lease.release()`) hands
/// off cleanly; dropping it without releasing lets the lease lapse by TTL.
#[pyclass(frozen, module = "aster._aster")]
pub struct Lease {
    handle: Arc<Mutex<Option<l::LeaseHandle>>>,
    status: watch::Receiver<l::LeaseStatus>,
    resource: String,
    epoch: u64,
    holder: String,
}

impl Lease {
    fn new(handle: l::LeaseHandle, holder: String) -> Self {
        Self {
            status: handle.watch(),
            resource: handle.resource().to_string(),
            epoch: handle.epoch(),
            holder,
            handle: Arc::new(Mutex::new(Some(handle))),
        }
    }

    fn take(&self) -> Option<l::LeaseHandle> {
        self.handle.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn release_fut<'py>(&self, py: Python<'py>, ret: bool) -> PyResult<Bound<'py, PyAny>> {
        let h = self.take();
        future_into_py(py, async move {
            if let Some(h) = h {
                h.release().await.map_err(err_to_py)?;
            }
            Ok(if ret { Some(false) } else { None })
        })
    }
}

#[pymethods]
impl Lease {
    #[getter]
    fn resource(&self) -> String {
        self.resource.clone()
    }

    /// The epoch this lease was granted at.
    #[getter]
    fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The holder the epoch was issued to.
    #[getter]
    fn holder(&self) -> String {
        self.holder.clone()
    }

    /// `"held"`, `"released"`, `"fenced"` or `"lost"`.
    #[getter]
    fn status(&self) -> &'static str {
        status_out(*self.status.borrow())
    }

    /// The `Fence` for a protected action, or `None` once the holder must
    /// stop (not held, or past the self-limit). Take it at send time; never
    /// cache it across awaits.
    fn fence<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
        let fence = self
            .handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|h| h.fence());
        fence.map(|f| fence_out(py, f)).transpose()
    }

    /// Status changes from now on, as an async iterator: yields each new
    /// status and ends after the first terminal one (`released` / `fenced` /
    /// `lost`). If the lease is already terminal, yields that and ends.
    fn status_changes(&self) -> LeaseStatusStream {
        let mut rx = self.status.clone();
        let current = *rx.borrow_and_update();
        LeaseStatusStream {
            rx: Arc::new(tokio::sync::Mutex::new(Some(rx))),
            pending: Arc::new(Mutex::new(
                (current != l::LeaseStatus::Held).then_some(current),
            )),
        }
    }

    /// Graceful release: stop renewing, then RELEASE at the serializer
    /// (which advances the fence, so this holder's in-flight writes are
    /// rejected from here on). Awaitable; idempotent.
    fn release<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.release_fut(py, false)
    }

    /// Stop renewing and return the fence for exactly one final operation,
    /// **without** releasing at the serializer; `None` when the fence is no
    /// longer usable (or the lease was already released).
    fn stop_renewing<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
        self.take()
            .and_then(|h| h.stop_renewing())
            .map(|f| fence_out(py, f))
            .transpose()
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        let obj: Py<PyAny> = slf.into_any().unbind();
        future_into_py(py, async move { Ok(obj) })
    }

    #[pyo3(signature = (_exc_type=None, _exc=None, _tb=None))]
    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Option<Bound<'py, PyAny>>,
        _exc: Option<Bound<'py, PyAny>>,
        _tb: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.release_fut(py, true)
    }

    fn __repr__(&self) -> String {
        format!(
            "Lease(resource={:?}, epoch={}, holder={:?}, status={:?})",
            self.resource,
            self.epoch,
            self.holder,
            status_out(*self.status.borrow())
        )
    }
}

/// Async iterator of lease status changes (see `Lease.status_changes()`).
#[pyclass(frozen, module = "aster._aster")]
pub struct LeaseStatusStream {
    rx: Arc<tokio::sync::Mutex<Option<watch::Receiver<l::LeaseStatus>>>>,
    /// A terminal status already observed at subscription time.
    pending: Arc<Mutex<Option<l::LeaseStatus>>>,
}

#[pymethods]
impl LeaseStatusStream {
    fn __aiter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.rx.clone();
        let pending = self.pending.clone();
        future_into_py(py, async move {
            let end = || Err(pyo3::exceptions::PyStopAsyncIteration::new_err(()));
            let mut guard = rx.lock().await;
            if let Some(s) = pending.lock().unwrap_or_else(|e| e.into_inner()).take() {
                *guard = None;
                return Ok(status_out(s));
            }
            let Some(rx) = guard.as_mut() else {
                return end();
            };
            if rx.changed().await.is_err() {
                *guard = None;
                return end();
            }
            let s = *rx.borrow_and_update();
            if s != l::LeaseStatus::Held {
                *guard = None;
            }
            Ok(status_out(s))
        })
    }
}

/// A running lease grantor (from `node.serve_lease_grantor`); `close()`
/// stops serving. Also a context manager.
#[pyclass(frozen, module = "aster._aster")]
pub struct LeaseGrantorServer {
    pub(crate) handle: Mutex<Option<aster::rpc::ServerHandle>>,
}

#[pymethods]
impl LeaseGrantorServer {
    /// Stop serving (idempotent).
    fn close(&self) {
        if let Some(h) = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            h.abort();
        }
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (_exc_type=None, _exc=None, _tb=None))]
    fn __exit__(
        &self,
        _exc_type: Option<Bound<'_, PyAny>>,
        _exc: Option<Bound<'_, PyAny>>,
        _tb: Option<Bound<'_, PyAny>>,
    ) -> bool {
        self.close();
        false
    }
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<MemorySerializer>()?;
    m.add_class::<GrantorSerializer>()?;
    m.add_class::<Lease>()?;
    m.add_class::<LeaseStatusStream>()?;
    m.add_class::<LeaseGrantorServer>()?;
    m.add_function(wrap_pyfunction!(lease_transition, m)?)?;
    m.add_function(wrap_pyfunction!(check_fence, m)?)?;
    Ok(())
}
