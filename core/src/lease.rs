//! Single-owner fenced leases — Tier 1 of
//! `docs/_internal/aster-leases.md`.
//!
//! The grant and the enforcement are different problems and must not share a
//! mechanism: the *grant* is cheap and imperfect (a CAS at the resource's own
//! serialization point plus a TTL for liveness), and correctness comes from
//! the *fence* — every protected action carries `(resource, epoch)` over an
//! authenticated stream, and the resource accepts it only when
//! `epoch == current ∧ writer == holder ∧ state == Held`.
//!
//! Layout of this module, mirroring the design doc's surface sketch:
//!
//! - [`transition`] — the normative serializer state machine as a pure
//!   function (ACQUIRE / RENEW / RELEASE / REVOKE), so every backend applies
//!   identical predicates under its own lock;
//! - [`LeaseSerializer`] — the backend trait (one CAS per transition);
//!   [`MemorySerializer`] is the reference co-located implementation and the
//!   store a designated grantor exposes over RPC;
//! - [`check_fence`] / [`FencedResource`] — the resource-side half. The
//!   library grants; **only resources enforce** — a consumer holding a
//!   [`LeaseHandle`] but writing through an unfenced API has no safety;
//! - [`LeaseHandle`] — the holder engine: renew loop over the serializer and
//!   the Chubby-style self-limit (counted from request *send*), past which
//!   [`LeaseHandle::fence`] stops handing out the fence.
//!
//! All timing uses `tokio::time`, so tests drive expiry deterministically
//! with a paused clock. Serializer expiry is evaluated on the serializer's
//! own clock; on restart a serializer re-arms a full fresh TTL and never
//! advances the fence (see [`MemorySerializer::restart`]).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use tokio::sync::watch;
use tokio::time::Instant;

/// The fencing token a holder stamps on every protected action. `epoch` is
/// monotonic per resource and never reused, so it doubles as the incarnation
/// id; `holder` is bound to the authenticated writer at enforcement time —
/// the epoch alone is visible to every directory reader and must never act
/// as a bearer token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fence {
    pub resource: String,
    pub epoch: u64,
    /// Hex node id of the holder the epoch was issued to.
    pub holder: String,
}

/// Observer view of a serializer row. Advisory outside the serializer's own
/// lock — a snapshot can be stale the moment it is returned; nothing may
/// gate a write on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseSnapshot {
    pub resource: String,
    pub epoch: u64,
    pub holder: Option<String>,
    pub held: bool,
    /// Serializer-local hint of time until expiry. `None` when free.
    pub remaining: Option<Duration>,
}

/// A requested transition — the four verbs of the state machine. Takeover
/// after expiry is `Acquire` under the expired predicate; there is no
/// separate steal verb.
#[derive(Clone, Debug)]
pub enum LeaseOp {
    Acquire { candidate: String, ttl: Duration },
    Renew { holder: String, epoch: u64 },
    Release { holder: String, epoch: u64 },
    Revoke { authority: String },
}

impl LeaseOp {
    /// The node performing the transition (for authorization hooks).
    pub fn actor(&self) -> &str {
        match self {
            LeaseOp::Acquire { candidate, .. } => candidate,
            LeaseOp::Renew { holder, .. } | LeaseOp::Release { holder, .. } => holder,
            LeaseOp::Revoke { authority } => authority,
        }
    }
}

/// Why a transition was refused. Every reason maps to a row of the design
/// doc's predicates; none of them is an error in the backend sense.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DenyReason {
    /// ACQUIRE against a live (unexpired) holder — no live-steal.
    HeldByOther,
    /// Caller's epoch is no longer the row's epoch.
    EpochMismatch,
    /// Caller is not the recorded holder for this epoch.
    NotHolder,
    /// RENEW after the serializer's clock expired the lease.
    Expired,
    /// RENEW/RELEASE on a row that is not held.
    NotHeld,
    /// Refused by the serializer's authorization hook.
    Unauthorized,
}

/// Result of a transition at the serializer.
#[derive(Clone, Debug)]
pub enum TransitionOutcome {
    /// ACQUIRE succeeded: the issued fence.
    Granted(Fence),
    Renewed,
    Released,
    Revoked,
    /// Predicate refused; the snapshot (post-decision) lets a candidate back
    /// off intelligently.
    Denied(DenyReason, LeaseSnapshot),
}

/// Durable fence fields of a serializer row. `granted_at` is deliberately
/// *not* here: it is volatile timing state — monotonic instants must never
/// be persisted or compared across a boot (restart rule).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowState {
    pub epoch: u64,
    pub holder: Option<String>,
    pub held: bool,
    pub ttl: Duration,
}

impl RowState {
    /// The state of a resource nothing has ever acquired: epoch 0, free.
    pub fn initial() -> Self {
        Self {
            epoch: 0,
            holder: None,
            held: false,
            ttl: Duration::ZERO,
        }
    }
}

/// Effect of applying an op to a row, as decided by [`transition`].
#[derive(Clone, Debug)]
pub enum Transition {
    /// Store `next` (one CAS) and report `outcome`. `reset_grant` says the
    /// expiry anchor restarts (`granted_at = now`) — true for every grant
    /// and renewal, false for release/revoke (nothing is held afterwards).
    Apply {
        next: RowState,
        outcome: TransitionOutcome,
        reset_grant: bool,
    },
    Deny(DenyReason),
}

/// The normative state machine, as a pure function so every backend applies
/// identical predicates: the backend supplies the row it read, whether that
/// row is expired *by the backend's own clock*, and the op; it must apply
/// the returned row under the same lock/CAS that read the input row.
///
/// Authorization is deliberately not here — it is the serializer's admission
/// decision (who may call at all), applied before the state machine.
pub fn transition(row: &RowState, expired: bool, op: &LeaseOp, resource: &str) -> Transition {
    match op {
        LeaseOp::Acquire { candidate, ttl } => {
            if row.held && !expired {
                return Transition::Deny(DenyReason::HeldByOther);
            }
            let next = RowState {
                epoch: row.epoch + 1,
                holder: Some(candidate.clone()),
                held: true,
                ttl: *ttl,
            };
            let fence = Fence {
                resource: resource.to_string(),
                epoch: next.epoch,
                holder: candidate.clone(),
            };
            Transition::Apply {
                next,
                outcome: TransitionOutcome::Granted(fence),
                reset_grant: true,
            }
        }
        LeaseOp::Renew { holder, epoch } => {
            if !row.held {
                return Transition::Deny(DenyReason::NotHeld);
            }
            if *epoch != row.epoch {
                return Transition::Deny(DenyReason::EpochMismatch);
            }
            if row.holder.as_deref() != Some(holder.as_str()) {
                return Transition::Deny(DenyReason::NotHolder);
            }
            if expired {
                return Transition::Deny(DenyReason::Expired);
            }
            Transition::Apply {
                next: row.clone(),
                outcome: TransitionOutcome::Renewed,
                reset_grant: true,
            }
        }
        // RELEASE has no expiry predicate: as long as no one has taken over
        // (epoch still matches), the holder may release — the epoch bump is
        // the point, so its own in-flight writes are rejected from here on.
        LeaseOp::Release { holder, epoch } => {
            if *epoch != row.epoch {
                return Transition::Deny(DenyReason::EpochMismatch);
            }
            if row.holder.as_deref() != Some(holder.as_str()) {
                return Transition::Deny(DenyReason::NotHolder);
            }
            Transition::Apply {
                next: RowState {
                    epoch: row.epoch + 1,
                    holder: None,
                    held: false,
                    ttl: row.ttl,
                },
                outcome: TransitionOutcome::Released,
                reset_grant: false,
            }
        }
        // REVOKE advances the fence unconditionally (authorization gates who
        // may call it). The directory tombstone is the separate, second step
        // — a tombstone alone revokes nothing.
        LeaseOp::Revoke { .. } => Transition::Apply {
            next: RowState {
                epoch: row.epoch + 1,
                holder: None,
                held: false,
                ttl: row.ttl,
            },
            outcome: TransitionOutcome::Revoked,
            reset_grant: false,
        },
    }
}

// ── Fence enforcement — the resource-side half ─────────────────────────────

/// Why a protected action was rejected at the resource. A rejection is
/// never routine: surface it loudly (metric + audit), never retry silently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FenceRejection {
    /// Carried epoch is older than the current grant — the classic fence.
    StaleEpoch { carried: u64, current: u64 },
    /// Writer is not the holder the current epoch was issued to (replay /
    /// impersonation — the fence is not a bearer token).
    NotHolder,
    /// Nothing is held at this epoch (e.g. equal-epoch write after the row
    /// was released — release advances the epoch, so this presents as
    /// `StaleEpoch`; `NotHeld` covers a row that was never re-acquired).
    NotHeld,
    /// Carried epoch is *ahead* of the serializer — impossible under a
    /// single serialization point. Serializer-integrity alarm; page-worthy.
    FutureEpoch { carried: u64, current: u64 },
}

impl fmt::Display for FenceRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FenceRejection::StaleEpoch { carried, current } => {
                write!(f, "fenced: stale epoch {carried} (current {current})")
            }
            FenceRejection::NotHolder => write!(f, "fenced: writer is not the holder"),
            FenceRejection::NotHeld => write!(f, "fenced: lease not held"),
            FenceRejection::FutureEpoch { carried, current } => write!(
                f,
                "fence integrity alarm: epoch {carried} ahead of serializer ({current})"
            ),
        }
    }
}

impl std::error::Error for FenceRejection {}

/// The fence rule: accept iff `epoch == row.epoch ∧ writer == row.holder ∧
/// row.held`. `writer` is the *authenticated* peer identity (the QUIC
/// handshake proves it on Aster streams) — never a field of the request.
///
/// Must be evaluated against the row inside the resource's own serialization
/// point, atomically with the protected write ([`MemorySerializer::
/// mutate_fenced`] is the reference shape). Checking a snapshot outside the
/// lock reintroduces the race this design exists to close.
pub fn check_fence(row: &RowState, fence: &Fence, writer: &str) -> Result<(), FenceRejection> {
    if fence.epoch < row.epoch {
        return Err(FenceRejection::StaleEpoch {
            carried: fence.epoch,
            current: row.epoch,
        });
    }
    if fence.epoch > row.epoch {
        return Err(FenceRejection::FutureEpoch {
            carried: fence.epoch,
            current: row.epoch,
        });
    }
    if !row.held {
        return Err(FenceRejection::NotHeld);
    }
    if row.holder.as_deref() != Some(writer) {
        return Err(FenceRejection::NotHolder);
    }
    Ok(())
}

/// The resource-side contract — **mandatory for Tier 1**. A resource
/// qualifies only when its mutation API accepts a [`Fence`] plus the
/// authenticated writer and evaluates [`check_fence`] inside its own
/// serialization point. A grant API without this half is Tier 0 with extra
/// steps.
#[async_trait::async_trait]
pub trait FencedResource: Send + Sync {
    type Op: Send;
    type Output: Send;

    /// Perform `op` iff the fence rule passes; the check and the mutation
    /// must be atomic (same transaction / same lock / same conditional PUT).
    async fn mutate(
        &self,
        fence: &Fence,
        writer: &str,
        op: Self::Op,
    ) -> Result<Self::Output, FenceRejection>;
}

// ── The serializer trait and the reference backend ─────────────────────────

/// A lease serialization point: applies [`transition`] under its own
/// lock/CAS, evaluating expiry on its own clock. Implementations: the
/// portal-store SQLite row, the S3 catalog object, a designated grantor
/// exposing [`MemorySerializer`] over RPC.
#[async_trait::async_trait]
pub trait LeaseSerializer: Send + Sync {
    /// Apply one transition (one CAS). Backend errors are `Err`; predicate
    /// refusals are `Ok(Denied(..))` — a refusal is information, not a
    /// fault.
    async fn transition(&self, resource: &str, op: LeaseOp) -> Result<TransitionOutcome>;

    /// Advisory snapshot (may be stale immediately; hints only).
    async fn snapshot(&self, resource: &str) -> Result<LeaseSnapshot>;
}

/// Authorization hook: may `op` be attempted at all? Returning false yields
/// `Denied(Unauthorized)`. In a real deployment this checks a
/// trust-directory lease role; the default admits everyone (the serializer
/// endpoint itself is then the boundary).
pub type LeaseAuthorizer = Arc<dyn Fn(&LeaseOp) -> bool + Send + Sync>;

struct MemRow {
    state: RowState,
    /// Volatile expiry anchor — never persisted; see `restart`.
    granted_at: Option<Instant>,
}

/// Reference co-located serializer: rows guarded by one lock, expiry on this
/// process's clock, fenced mutation under the same lock as the fence check.
/// This is also the store a designated grantor serves over RPC, and the
/// in-memory model every durable backend (portal-store row, S3 catalog
/// object) must match — the state machine itself lives in [`transition`].
#[derive(Clone)]
pub struct MemorySerializer {
    inner: Arc<MemoryInner>,
}

struct MemoryInner {
    rows: Mutex<HashMap<String, MemRow>>,
    authorizer: Mutex<Option<LeaseAuthorizer>>,
}

impl Default for MemorySerializer {
    fn default() -> Self {
        Self::new()
    }
}

impl MemorySerializer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MemoryInner {
                rows: Mutex::new(HashMap::new()),
                authorizer: Mutex::new(None),
            }),
        }
    }

    /// Install the authorization hook (ACQUIRE/RENEW need the lease role;
    /// REVOKE the granting authority).
    pub fn set_authorizer(&self, authorizer: LeaseAuthorizer) {
        *self.inner.authorizer.lock().unwrap() = Some(authorizer);
    }

    /// Simulate a serializer restart per the pinned restart rule: durable
    /// fence fields (`epoch`, `holder`, `held`, `ttl`) survive; the volatile
    /// expiry anchor is re-armed to a **full fresh TTL** (conservative
    /// toward the holder — a live holder just renews; a dead one's takeover
    /// is delayed at most one extra TTL). The fence is never advanced here.
    pub fn restart(&self) {
        let now = Instant::now();
        let mut rows = self.inner.rows.lock().unwrap();
        for row in rows.values_mut() {
            row.granted_at = row.state.held.then_some(now);
        }
    }

    /// Reference fenced mutation: evaluate [`check_fence`] and run `f`
    /// under the same lock that guards the row — the co-location the design
    /// doc requires. `writer` is the authenticated peer identity.
    pub fn mutate_fenced<R>(
        &self,
        fence: &Fence,
        writer: &str,
        f: impl FnOnce() -> R,
    ) -> Result<R, FenceRejection> {
        let rows = self.inner.rows.lock().unwrap();
        let state = rows
            .get(&fence.resource)
            .map(|r| r.state.clone())
            .unwrap_or_else(RowState::initial);
        check_fence(&state, fence, writer)?;
        Ok(f())
    }

    fn snapshot_locked(resource: &str, row: Option<&MemRow>, now: Instant) -> LeaseSnapshot {
        let (state, granted_at) = match row {
            Some(r) => (r.state.clone(), r.granted_at),
            None => (RowState::initial(), None),
        };
        let remaining = (state.held)
            .then(|| granted_at.map(|at| state.ttl.saturating_sub(now - at)))
            .flatten();
        LeaseSnapshot {
            resource: resource.to_string(),
            epoch: state.epoch,
            holder: state.holder,
            held: state.held,
            remaining,
        }
    }
}

#[async_trait::async_trait]
impl LeaseSerializer for MemorySerializer {
    async fn transition(&self, resource: &str, op: LeaseOp) -> Result<TransitionOutcome> {
        let authorized = match self.inner.authorizer.lock().unwrap().as_ref() {
            Some(auth) => auth(&op),
            None => true,
        };
        let now = Instant::now();
        let mut rows = self.inner.rows.lock().unwrap();
        if !authorized {
            let snap = Self::snapshot_locked(resource, rows.get(resource), now);
            return Ok(TransitionOutcome::Denied(DenyReason::Unauthorized, snap));
        }
        let row = rows.get(resource);
        let state = row
            .map(|r| r.state.clone())
            .unwrap_or_else(RowState::initial);
        let expired = state.held
            && row
                .and_then(|r| r.granted_at)
                .is_none_or(|at| now - at >= state.ttl);
        match transition(&state, expired, &op, resource) {
            Transition::Apply {
                next,
                outcome,
                reset_grant,
            } => {
                rows.insert(
                    resource.to_string(),
                    MemRow {
                        state: next,
                        granted_at: reset_grant.then_some(now),
                    },
                );
                Ok(outcome)
            }
            Transition::Deny(reason) => {
                let snap = Self::snapshot_locked(resource, rows.get(resource), now);
                Ok(TransitionOutcome::Denied(reason, snap))
            }
        }
    }

    async fn snapshot(&self, resource: &str) -> Result<LeaseSnapshot> {
        let now = Instant::now();
        let rows = self.inner.rows.lock().unwrap();
        Ok(Self::snapshot_locked(resource, rows.get(resource), now))
    }
}

// ── The holder engine ───────────────────────────────────────────────────────

/// Holder-side timing knobs (design-doc defaults; provisional).
#[derive(Clone, Copy, Debug)]
pub struct LeaseOptions {
    /// Serializer-side expiry interval.
    pub ttl: Duration,
    /// Renew cadence — ttl/3 by default so two missed renewals survive.
    pub renew_interval: Duration,
    /// Subtracted from `ttl` for the holder's self-limit (absorbs RTT plus
    /// processing allowance; RTT is a margin input, not a skew bound).
    pub holder_margin: Duration,
}

impl Default for LeaseOptions {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(30),
            renew_interval: Duration::from_secs(10),
            holder_margin: Duration::from_secs(1),
        }
    }
}

impl LeaseOptions {
    fn validate(&self) -> Result<()> {
        if self.ttl.is_zero() {
            bail!("lease ttl must be non-zero");
        }
        if self.holder_margin >= self.ttl {
            bail!("holder_margin must be smaller than ttl");
        }
        if self.renew_interval >= self.ttl - self.holder_margin {
            bail!("renew_interval must fit inside ttl - holder_margin");
        }
        Ok(())
    }
}

/// Holder-side lease status, published on a watch channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseStatus {
    Held,
    /// Released gracefully by this holder.
    Released,
    /// A renewal was refused — someone else holds a newer epoch (or the
    /// lease was revoked). Stop everything; re-acquire to continue.
    Fenced,
    /// The self-limit passed without a successful renewal (serializer
    /// unreachable). The lease may still be ours serializer-side, but the
    /// holder must stop acting; re-acquire to continue.
    Lost,
}

/// Outcome of an acquire attempt.
pub enum AcquireOutcome {
    Granted(LeaseHandle),
    /// Predicate refused (typically `HeldByOther`). The snapshot's
    /// `remaining` hint plus jittered exponential backoff is the intended
    /// retry policy — attempts against a live holder are policy violations
    /// even though they lose the CAS anyway.
    Denied(DenyReason, LeaseSnapshot),
}

/// A held lease: fence access gated by the self-limit, background renewals,
/// and a watch channel for fenced/released/lost. Dropping the handle stops
/// renewing without releasing — the lease then lapses at the serializer via
/// TTL; call [`LeaseHandle::release`] for a clean handoff.
pub struct LeaseHandle {
    fence: Fence,
    serializer: Arc<dyn LeaseSerializer>,
    self_limit: Arc<Mutex<Instant>>,
    status_rx: watch::Receiver<LeaseStatus>,
    status_tx: Arc<watch::Sender<LeaseStatus>>,
    renew_task: tokio::task::JoinHandle<()>,
}

impl LeaseHandle {
    /// One ACQUIRE CAS at the serializer (legal only on free-or-expired).
    /// On grant, starts the renew loop and arms the self-limit from the
    /// request *send* instant (conservative regardless of one-way delay).
    pub async fn acquire(
        serializer: Arc<dyn LeaseSerializer>,
        resource: &str,
        holder: &str,
        opts: LeaseOptions,
    ) -> Result<AcquireOutcome> {
        opts.validate()?;
        let send = Instant::now();
        let outcome = serializer
            .transition(
                resource,
                LeaseOp::Acquire {
                    candidate: holder.to_string(),
                    ttl: opts.ttl,
                },
            )
            .await?;
        let fence = match outcome {
            TransitionOutcome::Granted(fence) => fence,
            TransitionOutcome::Denied(reason, snap) => {
                return Ok(AcquireOutcome::Denied(reason, snap))
            }
            other => bail!("serializer returned {other:?} for an acquire"),
        };
        let self_limit = Arc::new(Mutex::new(send + opts.ttl - opts.holder_margin));
        let (status_tx, status_rx) = watch::channel(LeaseStatus::Held);
        let status_tx = Arc::new(status_tx);
        let renew_task = tokio::spawn(renew_loop(
            serializer.clone(),
            fence.clone(),
            opts,
            self_limit.clone(),
            status_tx.clone(),
        ));
        Ok(AcquireOutcome::Granted(LeaseHandle {
            fence,
            serializer,
            self_limit,
            status_rx,
            status_tx,
            renew_task,
        }))
    }

    /// The fence for a protected action — `None` once the holder must stop
    /// acting (status not `Held`, or self-limit passed). Stamp it on the
    /// action *at send time*; do not cache across awaits.
    pub fn fence(&self) -> Option<Fence> {
        if *self.status_rx.borrow() != LeaseStatus::Held {
            return None;
        }
        if Instant::now() >= *self.self_limit.lock().unwrap() {
            return None;
        }
        Some(self.fence.clone())
    }

    pub fn epoch(&self) -> u64 {
        self.fence.epoch
    }

    pub fn resource(&self) -> &str {
        &self.fence.resource
    }

    pub fn status(&self) -> LeaseStatus {
        *self.status_rx.borrow()
    }

    /// Watch for status changes (fenced / released / lost).
    pub fn watch(&self) -> watch::Receiver<LeaseStatus> {
        self.status_rx.clone()
    }

    /// Graceful release: stop renewing, then RELEASE at the serializer —
    /// which advances the fence, so this holder's own in-flight writes are
    /// rejected from here on. The next candidate may acquire immediately.
    pub async fn release(self) -> Result<()> {
        self.renew_task.abort();
        self.status_tx.send_replace(LeaseStatus::Released);
        self.serializer
            .transition(
                &self.fence.resource,
                LeaseOp::Release {
                    holder: self.fence.holder.clone(),
                    epoch: self.fence.epoch,
                },
            )
            .await?;
        Ok(())
    }
}

impl Drop for LeaseHandle {
    fn drop(&mut self) {
        self.renew_task.abort();
    }
}

async fn renew_loop(
    serializer: Arc<dyn LeaseSerializer>,
    fence: Fence,
    opts: LeaseOptions,
    self_limit: Arc<Mutex<Instant>>,
    status: Arc<watch::Sender<LeaseStatus>>,
) {
    loop {
        tokio::time::sleep(opts.renew_interval).await;
        let send = Instant::now();
        let result = serializer
            .transition(
                &fence.resource,
                LeaseOp::Renew {
                    holder: fence.holder.clone(),
                    epoch: fence.epoch,
                },
            )
            .await;
        match result {
            Ok(TransitionOutcome::Renewed) => {
                *self_limit.lock().unwrap() = send + opts.ttl - opts.holder_margin;
            }
            Ok(TransitionOutcome::Denied(..)) => {
                status.send_replace(LeaseStatus::Fenced);
                return;
            }
            Ok(other) => {
                tracing::error!(resource = %fence.resource, ?other, "unexpected renew outcome");
                status.send_replace(LeaseStatus::Fenced);
                return;
            }
            // Backend error: keep retrying on cadence, but never past the
            // self-limit — expiry there costs availability, never integrity.
            Err(err) => {
                tracing::warn!(resource = %fence.resource, %err, "lease renewal failed");
            }
        }
        if Instant::now() >= *self_limit.lock().unwrap() {
            status.send_replace(LeaseStatus::Lost);
            return;
        }
    }
}

/// REVOKE at the serializer (the fence advance — the actual safety). The
/// directory tombstone that withdraws *authority* is the caller's separate,
/// second step; a tombstone alone revokes nothing.
pub async fn revoke(
    serializer: &dyn LeaseSerializer,
    resource: &str,
    authority: &str,
) -> Result<TransitionOutcome> {
    serializer
        .transition(
            resource,
            LeaseOp::Revoke {
                authority: authority.to_string(),
            },
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(30);

    fn acquire_op(who: &str) -> LeaseOp {
        LeaseOp::Acquire {
            candidate: who.into(),
            ttl: TTL,
        }
    }

    #[test]
    fn acquire_free_grants_epoch_1() {
        let row = RowState::initial();
        match transition(&row, false, &acquire_op("a"), "r") {
            Transition::Apply { next, .. } => {
                assert_eq!(next.epoch, 1);
                assert_eq!(next.holder.as_deref(), Some("a"));
                assert!(next.held);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn acquire_live_holder_denied_takeover_after_expiry_allowed() {
        let held = RowState {
            epoch: 3,
            holder: Some("a".into()),
            held: true,
            ttl: TTL,
        };
        assert!(matches!(
            transition(&held, false, &acquire_op("b"), "r"),
            Transition::Deny(DenyReason::HeldByOther)
        ));
        match transition(&held, true, &acquire_op("b"), "r") {
            Transition::Apply { next, .. } => {
                assert_eq!(next.epoch, 4);
                assert_eq!(next.holder.as_deref(), Some("b"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn renew_predicates() {
        let held = RowState {
            epoch: 2,
            holder: Some("a".into()),
            held: true,
            ttl: TTL,
        };
        let renew = |holder: &str, epoch| LeaseOp::Renew {
            holder: holder.into(),
            epoch,
        };
        assert!(matches!(
            transition(&held, false, &renew("a", 2), "r"),
            Transition::Apply {
                outcome: TransitionOutcome::Renewed,
                reset_grant: true,
                ..
            }
        ));
        assert!(matches!(
            transition(&held, false, &renew("a", 1), "r"),
            Transition::Deny(DenyReason::EpochMismatch)
        ));
        assert!(matches!(
            transition(&held, false, &renew("b", 2), "r"),
            Transition::Deny(DenyReason::NotHolder)
        ));
        assert!(matches!(
            transition(&held, true, &renew("a", 2), "r"),
            Transition::Deny(DenyReason::Expired)
        ));
        assert!(matches!(
            transition(&RowState::initial(), false, &renew("a", 0), "r"),
            Transition::Deny(DenyReason::NotHeld)
        ));
    }

    #[test]
    fn release_bumps_epoch_even_when_expired() {
        let held = RowState {
            epoch: 5,
            holder: Some("a".into()),
            held: true,
            ttl: TTL,
        };
        let release = LeaseOp::Release {
            holder: "a".into(),
            epoch: 5,
        };
        for expired in [false, true] {
            match transition(&held, expired, &release, "r") {
                Transition::Apply {
                    next, reset_grant, ..
                } => {
                    assert_eq!(next.epoch, 6);
                    assert!(!next.held);
                    assert!(next.holder.is_none());
                    assert!(!reset_grant);
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn fence_rule_rejections() {
        let row = RowState {
            epoch: 4,
            holder: Some("a".into()),
            held: true,
            ttl: TTL,
        };
        let fence = |epoch| Fence {
            resource: "r".into(),
            epoch,
            holder: "a".into(),
        };
        assert!(check_fence(&row, &fence(4), "a").is_ok());
        assert_eq!(
            check_fence(&row, &fence(3), "a"),
            Err(FenceRejection::StaleEpoch {
                carried: 3,
                current: 4
            })
        );
        assert_eq!(
            check_fence(&row, &fence(5), "a"),
            Err(FenceRejection::FutureEpoch {
                carried: 5,
                current: 4
            })
        );
        assert_eq!(
            check_fence(&row, &fence(4), "b"),
            Err(FenceRejection::NotHolder)
        );
        let free = RowState {
            epoch: 4,
            holder: None,
            held: false,
            ttl: TTL,
        };
        assert_eq!(
            check_fence(&free, &fence(4), "a"),
            Err(FenceRejection::NotHeld)
        );
    }
}
