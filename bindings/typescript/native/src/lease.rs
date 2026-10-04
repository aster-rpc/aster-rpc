//! Single-owner fenced leases — wraps [`aster::lease`] (plan T7).
//!
//! Granting: [`LeaseHandle`] acquired against the in-memory reference
//! serializer ([`MemorySerializer`]) or a remote designated grantor
//! ([`GrantorSerializer`], `aster.lease.Grantor` over RPC). The handle renews
//! in Rust and stops handing out its fence at the self-limit.
//!
//! Enforcing: `MemorySerializer.mutateFenced` runs a synchronous JavaScript
//! function under the serializer's lock after the fence check; the pure
//! [`lease_check_fence`] / [`lease_transition`] let a JavaScript-owned
//! resource apply the same rules inside its own serialization point.
//!
//! Deferred (needs the R6 upcall mechanism): a JavaScript `LeaseSerializer`
//! backend, a JavaScript `FencedResource`, and a lease authorizer callback.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi_derive::napi;
use tokio::sync::watch;

use aster::lease as l;
use aster::lease::LeaseSerializer as _;

use crate::error::to_napi_err;
use crate::keys::invalid;

// ── Value types ──────────────────────────────────────────────────────────────

/// The fencing token stamped on every protected action. `holder` is the hex
/// node id the epoch was issued to — never a bearer token on its own.
#[napi(object)]
#[derive(Clone)]
pub struct Fence {
    pub resource: String,
    pub epoch: i64,
    pub holder: String,
}

impl From<l::Fence> for Fence {
    fn from(f: l::Fence) -> Self {
        Self {
            resource: f.resource,
            epoch: f.epoch as i64,
            holder: f.holder,
        }
    }
}

impl Fence {
    fn to_rust(&self) -> Result<l::Fence> {
        Ok(l::Fence {
            resource: self.resource.clone(),
            epoch: epoch_in(self.epoch)?,
            holder: self.holder.clone(),
        })
    }
}

/// Advisory view of a serializer row (may be stale the moment it returns).
#[napi(object)]
pub struct LeaseSnapshot {
    pub resource: String,
    pub epoch: i64,
    pub holder: Option<String>,
    pub held: bool,
    /// Serializer-local hint of ms until expiry; absent when free.
    pub remaining_ms: Option<f64>,
}

impl From<l::LeaseSnapshot> for LeaseSnapshot {
    fn from(s: l::LeaseSnapshot) -> Self {
        Self {
            resource: s.resource,
            epoch: s.epoch as i64,
            holder: s.holder,
            held: s.held,
            remaining_ms: s.remaining.map(|d| d.as_secs_f64() * 1000.0),
        }
    }
}

/// Holder-side timing (defaults: ttl 30 000, renew 10 000, margin 1 000 ms).
#[napi(object)]
pub struct LeaseOptions {
    /// Serializer-side expiry interval, ms.
    pub ttl_ms: Option<f64>,
    /// Renew cadence, ms (must fit inside `ttlMs - holderMarginMs`).
    pub renew_interval_ms: Option<f64>,
    /// Subtracted from `ttlMs` for the holder's self-limit, ms.
    pub holder_margin_ms: Option<f64>,
}

fn ms_in(field: &str, ms: f64) -> Result<Duration> {
    if !ms.is_finite() || ms < 0.0 {
        return Err(invalid(format!(
            "{field} must be a non-negative number of ms"
        )));
    }
    Ok(Duration::from_secs_f64(ms / 1000.0))
}

fn epoch_in(epoch: i64) -> Result<u64> {
    u64::try_from(epoch).map_err(|_| invalid("epoch must be non-negative"))
}

fn options_in(o: Option<LeaseOptions>) -> Result<l::LeaseOptions> {
    let mut opts = l::LeaseOptions::default();
    if let Some(o) = o {
        if let Some(v) = o.ttl_ms {
            opts.ttl = ms_in("ttlMs", v)?;
        }
        if let Some(v) = o.renew_interval_ms {
            opts.renew_interval = ms_in("renewIntervalMs", v)?;
        }
        if let Some(v) = o.holder_margin_ms {
            opts.holder_margin = ms_in("holderMarginMs", v)?;
        }
    }
    Ok(opts)
}

fn deny_out(r: l::DenyReason) -> String {
    match r {
        l::DenyReason::HeldByOther => "held_by_other",
        l::DenyReason::EpochMismatch => "epoch_mismatch",
        l::DenyReason::NotHolder => "not_holder",
        l::DenyReason::Expired => "expired",
        l::DenyReason::NotHeld => "not_held",
        l::DenyReason::Unauthorized => "unauthorized",
    }
    .into()
}

fn status_out(s: l::LeaseStatus) -> String {
    match s {
        l::LeaseStatus::Held => "held",
        l::LeaseStatus::Released => "released",
        l::LeaseStatus::Fenced => "fenced",
        l::LeaseStatus::Lost => "lost",
    }
    .into()
}

/// Why a protected action was rejected at the resource. Never routine:
/// surface it loudly, never retry silently.
#[napi(object)]
pub struct FenceRejection {
    #[napi(ts_type = "'stale_epoch' | 'not_holder' | 'not_held' | 'future_epoch'")]
    pub kind: String,
    /// Carried epoch (`stale_epoch` / `future_epoch`).
    pub carried: Option<i64>,
    /// The row's current epoch (`stale_epoch` / `future_epoch`).
    pub current: Option<i64>,
    pub message: String,
}

impl From<l::FenceRejection> for FenceRejection {
    fn from(r: l::FenceRejection) -> Self {
        let message = r.to_string();
        let (kind, carried, current) = match r {
            l::FenceRejection::StaleEpoch { carried, current } => {
                ("stale_epoch", Some(carried as i64), Some(current as i64))
            }
            l::FenceRejection::NotHolder => ("not_holder", None, None),
            l::FenceRejection::NotHeld => ("not_held", None, None),
            l::FenceRejection::FutureEpoch { carried, current } => {
                ("future_epoch", Some(carried as i64), Some(current as i64))
            }
        };
        Self {
            kind: kind.into(),
            carried,
            current,
            message,
        }
    }
}

/// The durable fence fields of a serializer row.
#[napi(object)]
pub struct LeaseRowState {
    pub epoch: i64,
    pub holder: Option<String>,
    pub held: bool,
    pub ttl_ms: f64,
}

impl LeaseRowState {
    fn to_rust(&self) -> Result<l::RowState> {
        Ok(l::RowState {
            epoch: epoch_in(self.epoch)?,
            holder: self.holder.clone(),
            held: self.held,
            ttl: ms_in("ttlMs", self.ttl_ms)?,
        })
    }

    fn from_rust(r: l::RowState) -> Self {
        Self {
            epoch: r.epoch as i64,
            holder: r.holder,
            held: r.held,
            ttl_ms: r.ttl.as_secs_f64() * 1000.0,
        }
    }
}

/// A requested transition. `actor` is the candidate (`acquire`), holder
/// (`renew` / `release`) or authority (`revoke`).
#[napi(object)]
pub struct LeaseOp {
    #[napi(ts_type = "'acquire' | 'renew' | 'release' | 'revoke'")]
    pub kind: String,
    pub actor: String,
    /// `renew` / `release`.
    pub epoch: Option<i64>,
    /// `acquire`.
    pub ttl_ms: Option<f64>,
}

impl LeaseOp {
    fn to_rust(&self) -> Result<l::LeaseOp> {
        let epoch = || {
            self.epoch
                .ok_or_else(|| invalid(format!("{} needs an epoch", self.kind)))
                .and_then(epoch_in)
        };
        Ok(match self.kind.as_str() {
            "acquire" => l::LeaseOp::Acquire {
                candidate: self.actor.clone(),
                ttl: ms_in(
                    "ttlMs",
                    self.ttl_ms.ok_or_else(|| invalid("acquire needs ttlMs"))?,
                )?,
            },
            "renew" => l::LeaseOp::Renew {
                holder: self.actor.clone(),
                epoch: epoch()?,
            },
            "release" => l::LeaseOp::Release {
                holder: self.actor.clone(),
                epoch: epoch()?,
            },
            "revoke" => l::LeaseOp::Revoke {
                authority: self.actor.clone(),
            },
            other => return Err(invalid(format!("unknown lease op {other:?}"))),
        })
    }
}

/// The result of a transition at a serializer.
#[napi(object)]
pub struct TransitionOutcome {
    #[napi(ts_type = "'granted' | 'renewed' | 'released' | 'revoked' | 'denied'")]
    pub kind: String,
    /// `granted`: the issued fence.
    pub fence: Option<Fence>,
    /// `denied`: why.
    #[napi(
        ts_type = "'held_by_other' | 'epoch_mismatch' | 'not_holder' | 'expired' | 'not_held' | 'unauthorized'"
    )]
    pub deny_reason: Option<String>,
    /// `denied`: the row after the decision (a backoff hint).
    pub snapshot: Option<LeaseSnapshot>,
}

impl From<l::TransitionOutcome> for TransitionOutcome {
    fn from(o: l::TransitionOutcome) -> Self {
        let mut out = TransitionOutcome {
            kind: String::new(),
            fence: None,
            deny_reason: None,
            snapshot: None,
        };
        out.kind = match o {
            l::TransitionOutcome::Granted(f) => {
                out.fence = Some(f.into());
                "granted"
            }
            l::TransitionOutcome::Renewed => "renewed",
            l::TransitionOutcome::Released => "released",
            l::TransitionOutcome::Revoked => "revoked",
            l::TransitionOutcome::Denied(r, s) => {
                out.deny_reason = Some(deny_out(r));
                out.snapshot = Some(s.into());
                "denied"
            }
        }
        .into();
        out
    }
}

/// What the pure state machine decided: `apply` (store `next` in one CAS,
/// restart the expiry anchor when `resetGrant`) or `deny`.
#[napi(object)]
pub struct LeaseTransition {
    #[napi(ts_type = "'apply' | 'deny'")]
    pub kind: String,
    pub next: Option<LeaseRowState>,
    pub outcome: Option<TransitionOutcome>,
    pub reset_grant: Option<bool>,
    #[napi(
        ts_type = "'held_by_other' | 'epoch_mismatch' | 'not_holder' | 'expired' | 'not_held' | 'unauthorized'"
    )]
    pub deny_reason: Option<String>,
}

/// The normative lease state machine as a pure function: apply `op` to
/// `row` (`expired` judged on the backend's own clock).
#[napi]
pub fn lease_transition(
    row: LeaseRowState,
    expired: bool,
    op: LeaseOp,
    resource: String,
) -> Result<LeaseTransition> {
    Ok(
        match l::transition(&row.to_rust()?, expired, &op.to_rust()?, &resource) {
            l::Transition::Apply {
                next,
                outcome,
                reset_grant,
            } => LeaseTransition {
                kind: "apply".into(),
                next: Some(LeaseRowState::from_rust(next)),
                outcome: Some(outcome.into()),
                reset_grant: Some(reset_grant),
                deny_reason: None,
            },
            l::Transition::Deny(r) => LeaseTransition {
                kind: "deny".into(),
                next: None,
                outcome: None,
                reset_grant: None,
                deny_reason: Some(deny_out(r)),
            },
        },
    )
}

/// The fence rule against `row`: `null` when `writer` (the authenticated
/// peer) may act with `fence`, else the rejection.
#[napi]
pub fn lease_check_fence(
    row: LeaseRowState,
    fence: Fence,
    writer: String,
) -> Result<Option<FenceRejection>> {
    Ok(l::check_fence(&row.to_rust()?, &fence.to_rust()?, &writer)
        .err()
        .map(FenceRejection::from))
}

/// An acquire refused by the serializer (typically `held_by_other`). Back
/// off (jittered exponential; `snapshot.remainingMs` is a hint).
#[napi(object)]
pub struct LeaseDenied {
    #[napi(
        ts_type = "'held_by_other' | 'epoch_mismatch' | 'not_holder' | 'expired' | 'not_held' | 'unauthorized'"
    )]
    pub reason: String,
    pub snapshot: LeaseSnapshot,
}

async fn acquire(
    serializer: Arc<dyn l::LeaseSerializer>,
    resource: String,
    holder: String,
    opts: l::LeaseOptions,
) -> Result<Either<LeaseHandle, LeaseDenied>> {
    match l::LeaseHandle::acquire(serializer, &resource, &holder, opts)
        .await
        .map_err(to_napi_err)?
    {
        l::AcquireOutcome::Granted(handle) => Ok(Either::A(LeaseHandle::new(handle, holder))),
        l::AcquireOutcome::Denied(reason, snapshot) => Ok(Either::B(LeaseDenied {
            reason: deny_out(reason),
            snapshot: snapshot.into(),
        })),
    }
}

// ── Serializers ──────────────────────────────────────────────────────────────

/// The reference in-memory lease serializer (also what a designated grantor
/// serves). Expiry runs on this process's clock.
#[napi]
pub struct MemorySerializer {
    pub(crate) inner: l::MemorySerializer,
}

impl Default for MemorySerializer {
    fn default() -> Self {
        Self::new()
    }
}

#[napi]
impl MemorySerializer {
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            inner: l::MemorySerializer::new(),
        }
    }

    /// One ACQUIRE CAS for `holder` (any actor string — usually a node id).
    /// Resolves to a `LeaseHandle` on grant, else a `LeaseDenied`.
    #[napi]
    pub async fn acquire(
        &self,
        resource: String,
        holder: String,
        options: Option<LeaseOptions>,
    ) -> Result<Either<LeaseHandle, LeaseDenied>> {
        let opts = options_in(options)?;
        acquire(Arc::new(self.inner.clone()), resource, holder, opts).await
    }

    /// Advisory snapshot of `resource`.
    #[napi]
    pub async fn snapshot(&self, resource: String) -> Result<LeaseSnapshot> {
        Ok(self
            .inner
            .snapshot(&resource)
            .await
            .map_err(to_napi_err)?
            .into())
    }

    /// Apply one raw transition (the state machine under this serializer's
    /// lock). Prefer `acquire` + the handle; this is for tooling and tests.
    #[napi]
    pub async fn transition(&self, resource: String, op: LeaseOp) -> Result<TransitionOutcome> {
        let op = op.to_rust()?;
        Ok(self
            .inner
            .transition(&resource, op)
            .await
            .map_err(to_napi_err)?
            .into())
    }

    /// REVOKE `resource` as `authority`: advances the fence unconditionally.
    #[napi]
    pub async fn revoke(&self, resource: String, authority: String) -> Result<TransitionOutcome> {
        Ok(l::revoke(&self.inner, &resource, &authority)
            .await
            .map_err(to_napi_err)?
            .into())
    }

    /// Simulate a serializer restart: fence fields survive, held rows get a
    /// full fresh TTL, the fence never advances.
    #[napi]
    pub fn restart(&self) {
        self.inner.restart();
    }

    /// Run `fn` under the serializer's lock iff the fence rule passes for
    /// `writer` (the authenticated peer). Returns `null` after running it,
    /// or the rejection without running it. `fn` must be synchronous and
    /// must not call back into this serializer.
    #[napi]
    pub fn mutate_fenced(
        &self,
        fence: Fence,
        writer: String,
        #[napi(ts_arg_type = "() => unknown")] f: Function<(), Unknown>,
    ) -> Result<Option<FenceRejection>> {
        let fence = fence.to_rust()?;
        match self.inner.mutate_fenced(&fence, &writer, || f.call(())) {
            Ok(called) => called.map(|_| None),
            Err(rejection) => Ok(Some(rejection.into())),
        }
    }
}

/// A remote designated grantor (`aster.lease.Grantor`) as a lease backend.
/// Every op is bound to this node's authenticated identity.
#[napi]
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

#[napi]
impl GrantorSerializer {
    /// This node's id — the only holder the grantor binds our calls to.
    #[napi(getter)]
    pub fn self_id(&self) -> String {
        self.self_id.clone()
    }

    /// ACQUIRE `resource` at the grantor as this node.
    #[napi]
    pub async fn acquire(
        &self,
        resource: String,
        options: Option<LeaseOptions>,
    ) -> Result<Either<LeaseHandle, LeaseDenied>> {
        let opts = options_in(options)?;
        acquire(self.inner.clone(), resource, self.self_id.clone(), opts).await
    }

    /// Advisory snapshot (`inspect`).
    #[napi]
    pub async fn snapshot(&self, resource: String) -> Result<LeaseSnapshot> {
        Ok(self
            .inner
            .snapshot(&resource)
            .await
            .map_err(to_napi_err)?
            .into())
    }

    /// REVOKE `resource` as this node (the grantor gates it, by default on
    /// the `operator` role).
    #[napi]
    pub async fn revoke(&self, resource: String) -> Result<TransitionOutcome> {
        Ok(l::revoke(self.inner.as_ref(), &resource, &self.self_id)
            .await
            .map_err(to_napi_err)?
            .into())
    }
}

// ── The holder handle ────────────────────────────────────────────────────────

/// A held lease: background renewals in Rust, fence access gated by the
/// self-limit. Dropping it stops renewing without releasing (the lease then
/// lapses by TTL); `release()` hands off cleanly.
#[napi]
pub struct LeaseHandle {
    handle: Arc<Mutex<Option<l::LeaseHandle>>>,
    status: watch::Receiver<l::LeaseStatus>,
    resource: String,
    epoch: u64,
    holder: String,
}

impl LeaseHandle {
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
}

#[napi]
impl LeaseHandle {
    #[napi(getter)]
    pub fn resource(&self) -> String {
        self.resource.clone()
    }

    #[napi(getter)]
    pub fn epoch(&self) -> i64 {
        self.epoch as i64
    }

    #[napi(getter)]
    pub fn holder(&self) -> String {
        self.holder.clone()
    }

    #[napi(getter, ts_return_type = "'held' | 'released' | 'fenced' | 'lost'")]
    pub fn status(&self) -> String {
        status_out(*self.status.borrow())
    }

    /// The fence for a protected action, or `null` once the holder must stop
    /// (not held, or past the self-limit). Take it at send time; never cache.
    #[napi]
    pub fn fence(&self) -> Option<Fence> {
        self.handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|h| h.fence())
            .map(Fence::from)
    }

    /// Status changes from now on (`AsyncIterable`): yields each new status
    /// and ends after the first terminal one (`released` / `fenced` /
    /// `lost`). If the lease is already terminal, yields that and ends.
    #[napi]
    pub fn status_changes(&self) -> LeaseStatusStream {
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
    /// (advances the fence). Idempotent; later calls are no-ops.
    #[napi]
    pub async fn release(&self) -> Result<()> {
        match self.take() {
            Some(h) => h.release().await.map_err(to_napi_err),
            None => Ok(()),
        }
    }

    /// Stop renewing and hand back the fence for one final operation,
    /// **without** releasing at the serializer. `null` when the fence is no
    /// longer usable (or the handle was already released).
    #[napi]
    pub fn stop_renewing(&self) -> Option<Fence> {
        self.take().and_then(|h| h.stop_renewing()).map(Fence::from)
    }
}

/// `AsyncIterable` of lease status changes (see `statusChanges()`).
#[napi(async_iterator)]
pub struct LeaseStatusStream {
    rx: Arc<tokio::sync::Mutex<Option<watch::Receiver<l::LeaseStatus>>>>,
    /// A terminal status already observed at subscription time.
    pending: Arc<Mutex<Option<l::LeaseStatus>>>,
}

#[napi]
impl AsyncGenerator for LeaseStatusStream {
    type Yield = String;
    type Next = ();
    type Return = ();

    fn next(
        &mut self,
        _value: Option<Self::Next>,
    ) -> impl std::future::Future<Output = Result<Option<Self::Yield>>> + Send + 'static {
        let rx = self.rx.clone();
        let pending = self.pending.clone();
        async move {
            let mut guard = rx.lock().await;
            if let Some(s) = pending.lock().unwrap_or_else(|e| e.into_inner()).take() {
                *guard = None;
                return Ok(Some(status_out(s)));
            }
            let Some(rx) = guard.as_mut() else {
                return Ok(None);
            };
            if rx.changed().await.is_err() {
                *guard = None;
                return Ok(None);
            }
            let s = *rx.borrow_and_update();
            if s != l::LeaseStatus::Held {
                *guard = None;
            }
            Ok(Some(status_out(s)))
        }
    }
}
