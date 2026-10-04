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
//! Python-implemented extension points (plan R6, `aster.upcalls`): a Python
//! `LeaseSerializer` backend ([`ForeignLeaseSerializer`]), a Python
//! `FencedResource` ([`ForeignFencedResource`]) and a Python lease
//! authorizer (`MemorySerializer.set_authorizer`) are served by upcall: Rust
//! emits a typed event, the `aster.upcalls` pump runs the Python coroutine
//! and answers by id.

use std::sync::{Arc, Mutex};

use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;
use tokio::sync::watch;

use aster::lease as l;

use aster::binding_support::upcall::lease::{
    AuthorizedSerializer, ForeignFencedResource as RsForeignFencedResource, ForeignLeaseAuthorizer,
    ForeignLeaseSerializer as RsForeignLeaseSerializer,
};

use crate::error::err_to_py;
use crate::keys::{actor_in as node_hex, invalid, py_value, secs_in};
use crate::upcall::{anyhow_to_py, upcall_err_class};

// ============================================================================
// Value conversion
// ============================================================================

pub(crate) fn fence_out<'py>(py: Python<'py>, f: l::Fence) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.lease", "Fence" {
        resource: f.resource,
        epoch: f.epoch,
        holder: f.holder,
    })
}

pub(crate) fn fence_in(obj: &Bound<'_, PyAny>) -> PyResult<l::Fence> {
    Ok(l::Fence {
        resource: obj.getattr("resource")?.extract()?,
        epoch: obj
            .getattr("epoch")?
            .extract()
            .map_err(|_| invalid("Fence.epoch must be a non-negative int"))?,
        holder: node_hex(&obj.getattr("holder")?)?,
    })
}

pub(crate) fn snapshot_out<'py>(
    py: Python<'py>,
    s: l::LeaseSnapshot,
) -> PyResult<Bound<'py, PyAny>> {
    py_value!(py, "aster.lease", "LeaseSnapshot" {
        resource: s.resource,
        epoch: s.epoch,
        holder: s.holder,
        held: s.held,
        remaining: s.remaining.map(|d| d.as_secs_f64()),
    })
}

pub(crate) fn deny_out(r: l::DenyReason) -> &'static str {
    match r {
        l::DenyReason::HeldByOther => "held_by_other",
        l::DenyReason::EpochMismatch => "epoch_mismatch",
        l::DenyReason::NotHolder => "not_holder",
        l::DenyReason::Expired => "expired",
        l::DenyReason::NotHeld => "not_held",
        l::DenyReason::Unauthorized => "unauthorized",
    }
}

pub(crate) fn status_out(s: l::LeaseStatus) -> &'static str {
    match s {
        l::LeaseStatus::Held => "held",
        l::LeaseStatus::Released => "released",
        l::LeaseStatus::Fenced => "fenced",
        l::LeaseStatus::Lost => "lost",
    }
}

pub(crate) fn outcome_out<'py>(
    py: Python<'py>,
    o: l::TransitionOutcome,
) -> PyResult<Bound<'py, PyAny>> {
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

pub(crate) fn rejection_out<'py>(
    py: Python<'py>,
    r: l::FenceRejection,
) -> PyResult<Bound<'py, PyAny>> {
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

pub(crate) fn op_in(obj: &Bound<'_, PyAny>) -> PyResult<l::LeaseOp> {
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

pub(crate) fn op_out<'py>(py: Python<'py>, op: &l::LeaseOp) -> PyResult<Bound<'py, PyAny>> {
    match op {
        l::LeaseOp::Acquire { candidate, ttl } => py_value!(py, "aster.lease", "LeaseOp" {
            kind: "acquire",
            actor: candidate.clone(),
            ttl: ttl.as_secs_f64(),
        }),
        l::LeaseOp::Renew { holder, epoch } => py_value!(py, "aster.lease", "LeaseOp" {
            kind: "renew",
            actor: holder.clone(),
            epoch: *epoch,
        }),
        l::LeaseOp::Release { holder, epoch } => py_value!(py, "aster.lease", "LeaseOp" {
            kind: "release",
            actor: holder.clone(),
            epoch: *epoch,
        }),
        l::LeaseOp::Revoke { authority } => py_value!(py, "aster.lease", "LeaseOp" {
            kind: "revoke",
            actor: authority.clone(),
        }),
    }
}

pub(crate) fn deny_in(s: &str) -> PyResult<l::DenyReason> {
    Ok(match s {
        "held_by_other" => l::DenyReason::HeldByOther,
        "epoch_mismatch" => l::DenyReason::EpochMismatch,
        "not_holder" => l::DenyReason::NotHolder,
        "expired" => l::DenyReason::Expired,
        "not_held" => l::DenyReason::NotHeld,
        "unauthorized" => l::DenyReason::Unauthorized,
        other => return Err(invalid(format!("unknown deny reason {other:?}"))),
    })
}

/// A `LeaseSnapshot` (`remaining` in seconds or `None`).
pub(crate) fn snapshot_in(obj: &Bound<'_, PyAny>) -> PyResult<l::LeaseSnapshot> {
    let holder = obj.getattr("holder")?;
    let remaining = obj.getattr("remaining")?;
    Ok(l::LeaseSnapshot {
        resource: obj
            .getattr("resource")?
            .extract()
            .map_err(|_| invalid("LeaseSnapshot.resource must be a str"))?,
        epoch: obj
            .getattr("epoch")?
            .extract()
            .map_err(|_| invalid("LeaseSnapshot.epoch must be a non-negative int"))?,
        holder: if holder.is_none() {
            None
        } else {
            Some(node_hex(&holder)?)
        },
        held: obj.getattr("held")?.is_truthy()?,
        remaining: if remaining.is_none() {
            None
        } else {
            Some(secs_in("LeaseSnapshot.remaining", remaining.extract()?)?)
        },
    })
}

/// A `TransitionOutcome` dataclass.
pub(crate) fn outcome_in(obj: &Bound<'_, PyAny>) -> PyResult<l::TransitionOutcome> {
    let kind: String = obj
        .getattr("kind")?
        .extract()
        .map_err(|_| invalid("TransitionOutcome.kind must be a str"))?;
    let need = |attr: &str| -> PyResult<Bound<'_, PyAny>> {
        let v = obj.getattr(attr)?;
        if v.is_none() {
            return Err(invalid(format!("a {kind} TransitionOutcome needs {attr}")));
        }
        Ok(v)
    };
    Ok(match kind.as_str() {
        "granted" => l::TransitionOutcome::Granted(fence_in(&need("fence")?)?),
        "renewed" => l::TransitionOutcome::Renewed,
        "released" => l::TransitionOutcome::Released,
        "revoked" => l::TransitionOutcome::Revoked,
        "denied" => l::TransitionOutcome::Denied(
            deny_in(&need("deny_reason")?.extract::<String>()?)?,
            snapshot_in(&need("snapshot")?)?,
        ),
        other => return Err(invalid(format!("unknown TransitionOutcome kind {other:?}"))),
    })
}

/// A `FenceRejectedError` (its `kind`, `carried`, `current`).
pub(crate) fn rejection_in(obj: &Bound<'_, PyAny>) -> PyResult<l::FenceRejection> {
    let kind: String = obj.getattr("kind")?.extract()?;
    let epochs = || -> PyResult<(u64, u64)> {
        let carried = obj.getattr("carried")?;
        let current = obj.getattr("current")?;
        if carried.is_none() || current.is_none() {
            return Err(invalid(format!(
                "a {kind} FenceRejectedError needs carried and current"
            )));
        }
        Ok((carried.extract()?, current.extract()?))
    };
    Ok(match kind.as_str() {
        "stale_epoch" => {
            let (carried, current) = epochs()?;
            l::FenceRejection::StaleEpoch { carried, current }
        }
        "future_epoch" => {
            let (carried, current) = epochs()?;
            l::FenceRejection::FutureEpoch { carried, current }
        }
        "not_holder" => l::FenceRejection::NotHolder,
        "not_held" => l::FenceRejection::NotHeld,
        other => return Err(invalid(format!("unknown fence rejection {other:?}"))),
    })
}

/// `LeaseOptions` (or `None` for the defaults), validated as the Rust
/// holder engine would.
pub(crate) fn options_in(obj: Option<&Bound<'_, PyAny>>) -> PyResult<l::LeaseOptions> {
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

/// A serializer error: an upcall failure raises `aster.upcalls.UpcallError`,
/// anything else `IrohError`.
fn lease_err(e: anyhow::Error) -> PyErr {
    anyhow_to_py(e, err_to_py)
}

async fn acquire(
    serializer: Arc<dyn l::LeaseSerializer>,
    resource: String,
    holder: String,
    opts: l::LeaseOptions,
) -> PyResult<Py<PyAny>> {
    let outcome = l::LeaseHandle::acquire(serializer, &resource, &holder, opts)
        .await
        .map_err(lease_err)?;
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
    let s = serializer.snapshot(resource).await.map_err(lease_err)?;
    Python::attach(|py| snapshot_out(py, s).map(Bound::unbind))
}

async fn revoke_at(
    serializer: &dyn l::LeaseSerializer,
    resource: &str,
    authority: &str,
) -> PyResult<Py<PyAny>> {
    let o = l::revoke(serializer, resource, authority)
        .await
        .map_err(lease_err)?;
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
    /// The Python authorizer, asked (by upcall, asynchronously) before every
    /// transition started through this object.
    authorizer: Mutex<Option<ForeignLeaseAuthorizer>>,
}

impl MemorySerializer {
    /// The serializer every entry point uses: the store itself, or the
    /// store behind the Python authorizer.
    pub(crate) fn serializer(&self) -> Arc<dyn l::LeaseSerializer> {
        let auth = self
            .authorizer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match auth {
            Some(a) => Arc::new(AuthorizedSerializer::new(self.inner.clone(), a)),
            None => Arc::new(self.inner.clone()),
        }
    }
}

#[pymethods]
impl MemorySerializer {
    #[new]
    fn new() -> Self {
        Self {
            inner: l::MemorySerializer::new(),
            authorizer: Mutex::new(None),
        }
    }

    /// Install a Python lease authorizer (an `aster.lease.LeaseAuthorizer`:
    /// `async def authorize(op) -> bool`, or any callable returning a bool
    /// or an awaitable of one), or remove it with `None`. Refused ops are
    /// `denied` with `unauthorized`; no answer applies the
    /// `lease_authorize` upcall policy (fail-closed by default). Applies to
    /// operations started after the call (a held lease keeps the
    /// serializer it was acquired through, renewals included).
    fn set_authorizer(&self, authorizer: Option<&Bound<'_, PyAny>>) -> PyResult<()> {
        let auth = match authorizer.filter(|a| !a.is_none()) {
            Some(a) => Some(ForeignLeaseAuthorizer::new(crate::upcall::bind(
                a,
                "lease_authorizer",
            )?)),
            None => None,
        };
        *self.authorizer.lock().unwrap_or_else(|e| e.into_inner()) = auth;
        Ok(())
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
        future_into_py(py, acquire(self.serializer(), resource, holder, opts))
    }

    /// Advisory snapshot of `resource` (awaitable `LeaseSnapshot`).
    fn snapshot<'py>(&self, py: Python<'py>, resource: String) -> PyResult<Bound<'py, PyAny>> {
        let ser = self.inner.clone();
        future_into_py(py, async move { snapshot_of(&ser, &resource).await })
    }

    /// Apply one raw transition (the state machine under this serializer's
    /// lock, after the authorizer). Prefer `acquire` + the lease; this is
    /// for tooling and tests.
    fn transition<'py>(
        &self,
        py: Python<'py>,
        resource: String,
        op: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let op = op_in(op)?;
        let ser = self.serializer();
        future_into_py(py, async move {
            let o = ser.transition(&resource, op).await.map_err(lease_err)?;
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
        let ser = self.serializer();
        future_into_py(py, async move {
            revoke_at(ser.as_ref(), &resource, &authority).await
        })
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
                h.release().await.map_err(lease_err)?;
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

// ============================================================================
// Python-implemented serializers and fenced resources (upcalls)
// ============================================================================

/// `obj` (a Python `LeaseSerializer`) as a Rust lease backend.
pub(crate) fn foreign_serializer(obj: &Bound<'_, PyAny>) -> PyResult<Arc<dyn l::LeaseSerializer>> {
    if let Ok(m) = obj.cast::<MemorySerializer>() {
        return Ok(m.get().serializer());
    }
    if let Ok(g) = obj.cast::<GrantorSerializer>() {
        return Ok(g.get().inner.clone());
    }
    Ok(Arc::new(RsForeignLeaseSerializer::new(
        crate::upcall::bind(obj, "lease_serializer")?,
    )))
}

/// ACQUIRE through a Python serializer (backs `LeaseSerializer.acquire`):
/// the `LeaseHandle` and its renewals run in Rust, every transition is an
/// upcall into `obj`.
#[pyfunction]
#[pyo3(name = "_foreign_lease_acquire", signature = (serializer, resource, holder, options=None))]
fn foreign_lease_acquire<'py>(
    py: Python<'py>,
    serializer: &Bound<'py, PyAny>,
    resource: String,
    holder: &Bound<'py, PyAny>,
    options: Option<&Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    let opts = options_in(options)?;
    let holder = node_hex(holder)?;
    let ser = foreign_serializer(serializer)?;
    future_into_py(py, acquire(ser, resource, holder, opts))
}

/// REVOKE through a Python serializer.
#[pyfunction]
#[pyo3(name = "_foreign_lease_revoke")]
fn foreign_lease_revoke<'py>(
    py: Python<'py>,
    serializer: &Bound<'py, PyAny>,
    resource: String,
    authority: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let authority = node_hex(authority)?;
    let ser = foreign_serializer(serializer)?;
    future_into_py(py, async move {
        revoke_at(ser.as_ref(), &resource, &authority).await
    })
}

/// One transition through Rust's `LeaseSerializer` view of `serializer`
/// (round trip: Rust encodes, the Python method answers, Rust decodes).
#[pyfunction]
#[pyo3(name = "_foreign_lease_transition")]
fn foreign_lease_transition<'py>(
    py: Python<'py>,
    serializer: &Bound<'py, PyAny>,
    resource: String,
    op: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let op = op_in(op)?;
    let ser = foreign_serializer(serializer)?;
    future_into_py(py, async move {
        let o = ser.transition(&resource, op).await.map_err(lease_err)?;
        Python::attach(|py| outcome_out(py, o).map(Bound::unbind))
    })
}

/// One snapshot through Rust's view of `serializer`.
#[pyfunction]
#[pyo3(name = "_foreign_lease_snapshot")]
fn foreign_lease_snapshot<'py>(
    py: Python<'py>,
    serializer: &Bound<'py, PyAny>,
    resource: String,
) -> PyResult<Bound<'py, PyAny>> {
    let ser = foreign_serializer(serializer)?;
    future_into_py(
        py,
        async move { snapshot_of(ser.as_ref(), &resource).await },
    )
}

/// `FencedResource::mutate` on a Python resource, through Rust
/// (`ForeignFencedResource`): returns the output bytes, raises
/// `FenceRejectedError` when the resource refused the fence, or
/// `aster.upcalls.FenceOutcomeUnknownError` when there was no answer (the
/// write may or may not have happened).
#[pyfunction]
#[pyo3(name = "_foreign_fenced_mutate")]
fn foreign_fenced_mutate<'py>(
    py: Python<'py>,
    resource: &Bound<'py, PyAny>,
    fence: &Bound<'py, PyAny>,
    writer: &Bound<'py, PyAny>,
    op: Vec<u8>,
) -> PyResult<Bound<'py, PyAny>> {
    use l::FencedResource as _;
    let fence = fence_in(fence)?;
    let writer = node_hex(writer)?;
    let res = RsForeignFencedResource::new(crate::upcall::bind(resource, "fenced_resource")?);
    future_into_py(py, async move {
        match res.mutate(&fence, &writer, op).await {
            Ok(Ok(out)) => Ok(crate::PyBytesResult(out)),
            Ok(Err(e)) => Err(upcall_err_class(&e, "FenceOutcomeUnknownError")),
            Err(rejection) => Err(Python::attach(|py| {
                rejection_out(py, rejection).map(PyErr::from_value)
            })?),
        }
    })
}

pub(crate) fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<MemorySerializer>()?;
    m.add_class::<GrantorSerializer>()?;
    m.add_class::<Lease>()?;
    m.add_class::<LeaseStatusStream>()?;
    m.add_class::<LeaseGrantorServer>()?;
    m.add_function(wrap_pyfunction!(lease_transition, m)?)?;
    m.add_function(wrap_pyfunction!(check_fence, m)?)?;
    m.add_function(wrap_pyfunction!(foreign_lease_acquire, m)?)?;
    m.add_function(wrap_pyfunction!(foreign_lease_revoke, m)?)?;
    m.add_function(wrap_pyfunction!(foreign_lease_transition, m)?)?;
    m.add_function(wrap_pyfunction!(foreign_lease_snapshot, m)?)?;
    m.add_function(wrap_pyfunction!(foreign_fenced_mutate, m)?)?;
    Ok(())
}
