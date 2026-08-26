//! The §9.1 resource transitions, as **one** decision function per command,
//! shared by preflight and apply.
//!
//! ## Decision versus projection
//!
//! §8.2.1 requires preflight and apply to agree about what a request means.
//! §10 requires them to *answer* differently: an admission reason is denied
//! before proposal, every other refusal is admitted precisely so it reaches the
//! log and saves a result. Those are two separate things, and the spike broke
//! the second while fixing the first -- it mapped every refusal to a
//! pre-proposal denial, including `6 AclRevisionMismatch`, which wedges a client
//! that may not change the request at a sequence it has already sent.
//!
//! So [`decide`] returns what the transition *is*. Projection lives in
//! `admission`, keyed on the reason.
//!
//! ## Three shapes of refusal
//!
//! * [`Decision::Refuse`] carries a §5.2 reason. It is a coordination outcome.
//! * [`Decision::RefuseShapeless`] carries none, because §5.2 has none for a
//!   request-shape bound -- an oversized value, an over-long namespace. §10
//!   answers these with an `RpcStatus` at stage 4. They still reach `apply` in
//!   principle (a faulty leader can propose one), and apply may not skip a
//!   committed entry, so it commits `Rejected` with reason `0 None`: rejected,
//!   and there genuinely is no coordination reason. See the note on
//!   [`Decision::RefuseShapeless`].
//! * An authorization failure is [`Reason::PermissionChanged`], not shapeless.
//!   At preflight §10 answers it with a status; at apply it is a saved rejection
//!   and the name is exactly right, because a command that was unauthorized at
//!   proposal should never have been proposed.
//!
//! Apply performs no clock, random, network, filesystem, policy-service or
//! address-book read. Everything below is a function of the replicated row set
//! and the compiled constants.

use super::limits::{
    self, MAX_COUNTER, MAX_RESOURCES_PER_GROUP, RIGHT_ACQUIRE, RIGHT_MUTATE, RIGHT_RELEASE,
    RIGHT_RENEW, RIGHT_REVOKE,
};
use super::shape::{self, ShapeLimit};
use super::state::{key_parts, NodeId, ResourceKey, ResourceRow, State};
use crate::coordination::reason::{Outcome, RefusalReason};

/// A resource identity **exactly as it arrived**, before validation.
///
/// Kept separate from the durable [`ResourceKey`] so that an unvalidated
/// identity has nowhere to be stored. A raw key reaches [`decide`], which either
/// validates it into a `ResourceKey` or refuses the request; nothing downstream
/// ever sees this type.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RawResourceKey {
    pub namespace: String,
    pub key: Vec<u8>,
}

impl RawResourceKey {
    pub fn new(namespace: impl Into<String>, key: impl Into<Vec<u8>>) -> Self {
        Self {
            namespace: namespace.into(),
            key: key.into(),
        }
    }

    /// Validate into the durable key type, or name the limit it violates.
    pub fn validate(&self) -> Result<ResourceKey, ShapeLimit> {
        shape::validate_resource_key(&self.namespace, &self.key)
    }
}

impl From<&ResourceKey> for RawResourceKey {
    fn from(k: &ResourceKey) -> Self {
        RawResourceKey::new(k.namespace(), k.key())
    }
}

/// A lease fence: the group that minted it, the resource, the epoch it was
/// granted at, and the holder.
///
/// The group id is load-bearing, not descriptive. §4.1's recovery invariant:
/// "all fences include the group id so a fence from a disaster-recovery
/// replacement group is invalid even if its numeric epoch is the same." A
/// fence type without the group made that invariant unenforceable — the
/// outer request's group says where the request is *addressed*, never where
/// the fence was *minted*.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fence {
    pub group: [u8; 32],
    pub resource: RawResourceKey,
    pub epoch: u64,
    pub holder: NodeId,
}

/// The resource-affecting client actions of §9.1.
///
/// `Expire` is deliberately absent: it is a **system** command with its own
/// reply space (`SystemReplyV1`), no client session, and no authorization
/// check. Modelling it as a client action would give it a sequence it does not
/// have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceAction {
    Acquire {
        resource: RawResourceKey,
        ttl_ms: u64,
    },
    Renew {
        fence: Fence,
    },
    CompareAndSet {
        fence: Fence,
        expected_value_revision: u64,
        new_value: Vec<u8>,
    },
    Release {
        fence: Fence,
    },
    ReleaseWithValue {
        fence: Fence,
        expected_value_revision: u64,
        final_value: Vec<u8>,
    },
    Revoke {
        resource: RawResourceKey,
        expected_epoch: Option<u64>,
    },
}

impl ResourceAction {
    /// The fence this action presents, if it is a fenced operation.
    pub fn fence(&self) -> Option<&Fence> {
        match self {
            ResourceAction::Renew { fence }
            | ResourceAction::CompareAndSet { fence, .. }
            | ResourceAction::Release { fence }
            | ResourceAction::ReleaseWithValue { fence, .. } => Some(fence),
            ResourceAction::Acquire { .. } | ResourceAction::Revoke { .. } => None,
        }
    }

    /// The raw, still-unvalidated resource this action names.
    pub fn raw_resource(&self) -> &RawResourceKey {
        match self {
            ResourceAction::Acquire { resource, .. } | ResourceAction::Revoke { resource, .. } => {
                resource
            }
            ResourceAction::Renew { fence }
            | ResourceAction::CompareAndSet { fence, .. }
            | ResourceAction::Release { fence }
            | ResourceAction::ReleaseWithValue { fence, .. } => &fence.resource,
        }
    }

    /// Is this one of §12.1's **safety-reducing** commands, which are never
    /// refused for want of growth budget?
    ///
    /// `release_with_value` qualifies only when the final value is not larger
    /// than the current one, so it is decided against the row rather than by
    /// the variant alone.
    fn is_safety_reducing(&self, current_value_len: usize) -> bool {
        match self {
            ResourceAction::Release { .. } | ResourceAction::Revoke { .. } => true,
            ResourceAction::ReleaseWithValue { final_value, .. } => {
                final_value.len() <= current_value_len
            }
            ResourceAction::Acquire { .. }
            | ResourceAction::Renew { .. }
            | ResourceAction::CompareAndSet { .. } => false,
        }
    }
}

/// What a successful transition writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceEffect {
    /// The validated identity this transition writes to. Produced here so apply
    /// cannot re-derive it, and so an unvalidated key has no path into state.
    pub key: ResourceKey,
    /// The row after the transition. Apply writes this verbatim rather than
    /// recomputing it, so the two cannot drift.
    pub row: ResourceRow,
    pub outcome: Outcome,
    /// Whether this transition creates a resource row that did not exist. Only
    /// `acquire` can, among the actions here (§9.1 also lists `set_grant` and
    /// administrator initialization, which live in their own module).
    pub creates_resource: bool,
}

/// The outcome of deciding one resource action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Apply(Box<ResourceEffect>),
    /// Refused with a §5.2 reason.
    Refuse(RefusalReason),
    /// Refused by a request-shape bound, which §5.2 gives no reason code.
    ///
    /// §10 answers this with an `RpcStatus` at stage 4, so a correct leader
    /// never proposes one. Apply still has to be total -- a faulty leader can
    /// propose anything, and skipping a committed entry diverges the voters --
    /// so at apply it commits `Rejected` with reason `0 None`. That is the
    /// honest encoding rather than an invented reason code: the request is
    /// shapeless to the protocol, so there is no coordination reason to report.
    /// Being `Consumed` it is terminal, which is also correct: the client cannot
    /// fix the bound without changing a request it may no longer change.
    RefuseShapeless(ShapeLimit),
}

impl Decision {
    fn apply(
        key: ResourceKey,
        row: ResourceRow,
        outcome: Outcome,
        creates_resource: bool,
    ) -> Decision {
        Decision::Apply(Box::new(ResourceEffect {
            key,
            row,
            outcome,
            creates_resource,
        }))
    }
}

/// The rights an action requires (§9.1).
fn required_rights(action: &ResourceAction) -> u64 {
    match action {
        ResourceAction::Acquire { .. } => RIGHT_ACQUIRE,
        ResourceAction::Renew { .. } => RIGHT_RENEW,
        ResourceAction::CompareAndSet { .. } => RIGHT_MUTATE,
        ResourceAction::Release { .. } => RIGHT_RELEASE,
        // The one action needing two rights.
        ResourceAction::ReleaseWithValue { .. } => RIGHT_RELEASE | RIGHT_MUTATE,
        ResourceAction::Revoke { .. } => RIGHT_REVOKE,
    }
}

/// The rights `actor` holds on `resource`, or 0.
///
/// Public because value confidentiality depends on it: a reply may only carry a
/// register value to an actor holding `READ`, and that check belongs at the
/// projection boundary as much as at the transition.
pub fn granted_rights(state: &State, resource: &ResourceKey, actor: &NodeId) -> u64 {
    state
        .acl
        .get(&(resource.clone(), *actor))
        .copied()
        .unwrap_or(0)
}

/// Check every counter a transition would advance, **together**, before any of
/// them moves.
///
/// A fence that advanced the epoch but not the lease revision is not a legal
/// state, so a per-counter check that stopped halfway would leave one. Returns
/// `false` if any named counter is already at the §4 ceiling.
fn counters_have_room(counters: &[u64]) -> bool {
    counters.iter().all(|c| *c < MAX_COUNTER)
}

/// Would this transition's growth charge fit?
///
/// Only the resource row's own charge changes here; the delta is the value
/// length, since `256 + KEY` is unchanged for an existing row and is the whole
/// charge for a new one.
fn growth_fits(state: &State, key: &ResourceKey, old_len: Option<usize>, new_len: usize) -> bool {
    let (ns, k) = key_parts(key);
    let old = old_len.map_or(0, |len| limits::charge_resource(ns, k, len));
    let new = limits::charge_resource(ns, k, new_len);
    if new <= old {
        return true;
    }
    state.growth_bytes.saturating_add(new - old) <= limits::GROWTH_BUDGET
}

/// Decide one resource action against committed state.
///
/// The check order is normative where it changes the answer, and each ordering
/// decision is commented at its site.
pub fn decide(state: &State, actor: &NodeId, action: &ResourceAction) -> Decision {
    // Request-shape bounds first, identity included: a malformed request is
    // shapeless to the protocol, and evaluating it against replicated state
    // would be answering a question that was never well posed. Validating here
    // also means every path below holds a `ResourceKey` that state, saved
    // replies and audit rows can accept -- an unvalidated identity has nowhere
    // to go.
    let key = match action.raw_resource().validate() {
        Ok(key) => key,
        Err(limit) => return Decision::RefuseShapeless(limit),
    };
    if let Some(limit) = check_shape(action) {
        return Decision::RefuseShapeless(limit);
    }

    let existing = state.resources.get(&key);

    // Authorization. At apply this is `12 PermissionChanged` and the name is
    // exact: a command unauthorized at proposal time should never have been
    // proposed, so reaching apply unauthorized means the ACL moved underneath
    // it.
    let needed = required_rights(action);
    if granted_rights(state, &key, actor) & needed != needed {
        return Decision::Refuse(RefusalReason::PermissionChanged);
    }

    // §4.1's recovery invariant, enforced where every voter can see it: a
    // fence names the group that minted it, and a fence minted by another
    // group -- a disaster-recovery replacement's, for instance -- is not this
    // group's lease at any epoch, even when resource, holder and epoch all
    // coincide. `EpochMismatch` is exact: no epoch of this group ever
    // validates that fence, and the recovery -- re-read, then re-acquire --
    // is the stale-fence recovery.
    if let Some(fence) = action.fence() {
        if fence.group != state.authority.group_id {
            return Decision::Refuse(RefusalReason::EpochMismatch);
        }
    }

    match action {
        ResourceAction::Acquire { ttl_ms, .. } => {
            decide_acquire(state, actor, key, existing, *ttl_ms)
        }
        ResourceAction::Renew { fence } => decide_renew(actor, key, existing, fence),
        ResourceAction::CompareAndSet {
            fence,
            expected_value_revision,
            new_value,
        } => decide_compare_and_set(
            state,
            actor,
            key,
            existing,
            fence,
            *expected_value_revision,
            new_value,
        ),
        ResourceAction::Release { fence } => decide_release(actor, key, existing, fence),
        ResourceAction::ReleaseWithValue {
            fence,
            expected_value_revision,
            final_value,
        } => decide_release_with_value(
            state,
            actor,
            key,
            existing,
            fence,
            *expected_value_revision,
            final_value,
            action,
        ),
        ResourceAction::Revoke { expected_epoch, .. } => {
            decide_revoke(key, existing, *expected_epoch)
        }
    }
}

/// The §12.1 request-shape bounds beyond the resource identity, which
/// [`decide`] has already validated.
///
/// The order is fixed so that a request violating two limits reports the same
/// one on every voter: apply must be deterministic, and "whichever check ran
/// first" is not determinism unless the order is part of the code.
fn check_shape(action: &ResourceAction) -> Option<ShapeLimit> {
    let value = match action {
        ResourceAction::CompareAndSet { new_value, .. } => Some(new_value),
        ResourceAction::ReleaseWithValue { final_value, .. } => Some(final_value),
        ResourceAction::Acquire { .. }
        | ResourceAction::Renew { .. }
        | ResourceAction::Release { .. }
        | ResourceAction::Revoke { .. } => None,
    };
    if let Some(limit) = value.and_then(|v| shape::check_value(v)) {
        return Some(limit);
    }
    if let ResourceAction::Acquire { ttl_ms, .. } = action {
        if let Some(limit) = shape::check_ttl(*ttl_ms) {
            return Some(limit);
        }
    }
    None
}

/// Does `fence` name this row's current lease, held by `actor`?
///
/// Returns the reason the fence does not match, most specific first: an unheld
/// row is `NotHeld` before it is anything else, and a fence presented by the
/// wrong node is `NotHolder` before its epoch is compared -- reporting
/// `EpochMismatch` to a node that was never the holder would send it to refresh
/// a fence it can never hold.
fn check_fence(actor: &NodeId, row: &ResourceRow, fence: &Fence) -> Option<RefusalReason> {
    if !row.held {
        return Some(RefusalReason::NotHeld);
    }
    if fence.holder != *actor || row.holder != Some(*actor) {
        return Some(RefusalReason::NotHolder);
    }
    if row.epoch != fence.epoch {
        return Some(RefusalReason::EpochMismatch);
    }
    None
}

fn decide_acquire(
    state: &State,
    actor: &NodeId,
    key: ResourceKey,
    existing: Option<&ResourceRow>,
    ttl_ms: u64,
) -> Decision {
    let row = existing.cloned().unwrap_or_default();

    // "Acquire never takes a held row, even if a leader timer believes it is
    // old." A leader must commit the exact `Expire` first; log order then
    // decides an expiry-versus-renew race.
    //
    // **For `acquire`, `1 HeldByOther` means "already held, including by this
    // caller."** The name reads as though it excluded the current holder; it
    // does not. A holder re-acquiring is refused rather than silently treated
    // as a renew, because the two have different effects on the epoch and
    // substituting one for the other would move a fence the client still
    // believes in. The refusal changes nothing at all.
    if row.held {
        return Decision::Refuse(RefusalReason::HeldByOther);
    }

    if existing.is_none() {
        // Creating a row: the slot cap applies, and it is terminal, so it is
        // checked before the retryable budget.
        if state.resources.len() as u64 >= MAX_RESOURCES_PER_GROUP {
            return Decision::Refuse(RefusalReason::ResourceSlotsExhausted);
        }
        if !growth_fits(state, &key, None, 0) {
            return Decision::Refuse(RefusalReason::StateBudgetExhausted);
        }
    }

    if !counters_have_room(&[row.epoch, row.lease_revision]) {
        return Decision::Refuse(RefusalReason::CounterExhausted);
    }

    let mut next = row;
    next.epoch += 1;
    next.lease_revision += 1;
    next.held = true;
    next.holder = Some(*actor);
    next.ttl_ms = ttl_ms;
    Decision::apply(key, next, Outcome::Granted, existing.is_none())
}

fn decide_renew(
    actor: &NodeId,
    key: ResourceKey,
    existing: Option<&ResourceRow>,
    fence: &Fence,
) -> Decision {
    let Some(row) = existing else {
        return Decision::Refuse(RefusalReason::NotHeld);
    };
    if let Some(reason) = check_fence(actor, row, fence) {
        return Decision::Refuse(reason);
    }
    if !counters_have_room(&[row.lease_revision]) {
        return Decision::Refuse(RefusalReason::CounterExhausted);
    }

    // Lease revision only: the epoch and the value are unchanged, so a renew
    // never invalidates a fence the holder is still using.
    let mut next = row.clone();
    next.lease_revision += 1;
    Decision::apply(key, next, Outcome::Renewed, false)
}

#[allow(clippy::too_many_arguments)]
fn decide_compare_and_set(
    state: &State,
    actor: &NodeId,
    key: ResourceKey,
    existing: Option<&ResourceRow>,
    fence: &Fence,
    expected_value_revision: u64,
    new_value: &[u8],
) -> Decision {
    let Some(row) = existing else {
        return Decision::Refuse(RefusalReason::NotHeld);
    };
    if let Some(reason) = check_fence(actor, row, fence) {
        return Decision::Refuse(reason);
    }
    if row.value_revision != expected_value_revision {
        return Decision::Refuse(RefusalReason::ValueRevisionMismatch);
    }
    if !growth_fits(state, &key, Some(row.value.len()), new_value.len()) {
        return Decision::Refuse(RefusalReason::StateBudgetExhausted);
    }
    if !counters_have_room(&[row.value_revision]) {
        return Decision::Refuse(RefusalReason::CounterExhausted);
    }

    // "compare_and_set does not renew": the lease revision is untouched, so a
    // holder cannot keep a lease alive by writing to it.
    let mut next = row.clone();
    next.value = new_value.to_vec();
    next.value_revision += 1;
    Decision::apply(key, next, Outcome::Mutated, false)
}

fn decide_release(
    actor: &NodeId,
    key: ResourceKey,
    existing: Option<&ResourceRow>,
    fence: &Fence,
) -> Decision {
    let Some(row) = existing else {
        return Decision::Refuse(RefusalReason::NotHeld);
    };
    if let Some(reason) = check_fence(actor, row, fence) {
        return Decision::Refuse(reason);
    }
    if !counters_have_room(&[row.epoch, row.lease_revision]) {
        return Decision::Refuse(RefusalReason::CounterExhausted);
    }

    // The value is **preserved**. A plain release is safety-reducing but it is
    // not a way out of budget exhaustion, because the row's charge is unchanged.
    let mut next = row.clone();
    next.held = false;
    next.holder = None;
    next.epoch += 1;
    next.lease_revision += 1;
    Decision::apply(key, next, Outcome::Released, false)
}

#[allow(clippy::too_many_arguments)]
fn decide_release_with_value(
    state: &State,
    actor: &NodeId,
    key: ResourceKey,
    existing: Option<&ResourceRow>,
    fence: &Fence,
    expected_value_revision: u64,
    final_value: &[u8],
    action: &ResourceAction,
) -> Decision {
    let Some(row) = existing else {
        return Decision::Refuse(RefusalReason::NotHeld);
    };
    if let Some(reason) = check_fence(actor, row, fence) {
        return Decision::Refuse(reason);
    }
    if row.value_revision != expected_value_revision {
        return Decision::Refuse(RefusalReason::ValueRevisionMismatch);
    }
    // Safety-reducing only when the value does not grow, which is why this is
    // decided against the row rather than by the variant.
    if !action.is_safety_reducing(row.value.len())
        && !growth_fits(state, &key, Some(row.value.len()), final_value.len())
    {
        return Decision::Refuse(RefusalReason::StateBudgetExhausted);
    }
    if !counters_have_room(&[row.epoch, row.lease_revision, row.value_revision]) {
        return Decision::Refuse(RefusalReason::CounterExhausted);
    }

    // All five effects or none: "a failed release_with_value leaves both the
    // value and lease unchanged."
    let mut next = row.clone();
    next.value = final_value.to_vec();
    next.value_revision += 1;
    next.held = false;
    next.holder = None;
    next.epoch += 1;
    next.lease_revision += 1;
    Decision::apply(key, next, Outcome::Released, false)
}

fn decide_revoke(
    key: ResourceKey,
    existing: Option<&ResourceRow>,
    expected_epoch: Option<u64>,
) -> Decision {
    // Revoke is not one of §9.1's lazily-creating commands, so there is nothing
    // to fence on a resource that does not exist.
    let Some(row) = existing else {
        return Decision::Refuse(RefusalReason::NotHeld);
    };
    if let Some(expected) = expected_epoch {
        if row.epoch != expected {
            return Decision::Refuse(RefusalReason::EpochMismatch);
        }
    }
    if !counters_have_room(&[row.epoch, row.lease_revision]) {
        return Decision::Refuse(RefusalReason::CounterExhausted);
    }

    // No `held` precondition (§9.1): revoke is an **unconditional** fencing
    // operation, so it advances the epoch and lease revision whether or not
    // anyone holds the lease.
    //
    // The transition itself is *not* idempotent -- two separate revoke commands
    // advance the counters twice. Only a replay of the same sequenced command
    // is idempotent, and that is the session table's doing, not this
    // transition's. The value is preserved, as with a plain release.
    let mut next = row.clone();
    next.held = false;
    next.holder = None;
    next.epoch += 1;
    next.lease_revision += 1;
    Decision::apply(key, next, Outcome::Revoked, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::model::limits::{ALL_RIGHTS, GROWTH_BUDGET, MAX_TTL_MS, MIN_TTL_MS};
    use crate::coordination::reason::Reason;

    const HOLDER: NodeId = [1u8; 32];
    const OTHER: NodeId = [2u8; 32];

    fn raw() -> RawResourceKey {
        RawResourceKey::new("ns", b"k".to_vec())
    }

    fn key() -> ResourceKey {
        raw().validate().expect("a valid fixture identity")
    }

    /// A state where `actor` holds every right on `key()`.
    fn state_with_rights(actor: NodeId, rights: u64) -> State {
        let mut s = State::default();
        s.acl.insert((key(), actor), rights);
        s.growth_bytes = s.recompute_growth();
        s
    }

    fn with_row(mut s: State, row: ResourceRow) -> State {
        s.resources.insert(key(), row);
        s.growth_bytes = s.recompute_growth();
        s
    }

    fn held_row(holder: NodeId, epoch: u64) -> ResourceRow {
        ResourceRow {
            epoch,
            held: true,
            holder: Some(holder),
            ttl_ms: 30_000,
            lease_revision: 1,
            value_revision: 0,
            value: Vec::new(),
            acl_revision: 1,
        }
    }

    fn fence(epoch: u64, holder: NodeId) -> Fence {
        Fence {
            // The test states are `State::default()`-rooted, so their
            // authority group is the zero id; a fence naming it is this
            // group's own.
            group: [0u8; 32],
            resource: raw(),
            epoch,
            holder,
        }
    }

    /// **§4.1's recovery invariant, at the replicated decision** — the frozen
    /// sentence: "all fences include the group id so a fence from a
    /// disaster-recovery replacement group is invalid even if its numeric
    /// epoch is the same."
    ///
    /// Two groups, coincident rows: the same resource, held by the same
    /// actor, at the same epoch — everything the pre-group fence check ever
    /// compared. A fence minted by group A must be refused by group B on
    /// **every** fenced operation, while the identical fence naming B's own
    /// group applies. `EpochMismatch` is the reported reason: no epoch of B
    /// ever validates A's fence, and the recovery — re-read, re-acquire — is
    /// the stale-fence recovery.
    #[test]
    fn a_fence_minted_by_another_group_is_refused_on_every_fenced_operation() {
        const GROUP_A: [u8; 32] = [0xA1; 32];
        const GROUP_B: [u8; 32] = [0xB2; 32];

        let mut group_b = with_row(state_with_rights(HOLDER, ALL_RIGHTS), held_row(HOLDER, 7));
        group_b.authority.group_id = GROUP_B;

        let minted_by = |group: [u8; 32]| Fence {
            group,
            resource: raw(),
            epoch: 7,
            holder: HOLDER,
        };

        type Build = Box<dyn Fn(Fence) -> ResourceAction>;
        let operations: Vec<(&str, Build)> = vec![
            ("renew", Box::new(|fence| ResourceAction::Renew { fence })),
            (
                "compare_and_set",
                Box::new(|fence| ResourceAction::CompareAndSet {
                    fence,
                    expected_value_revision: 0,
                    new_value: b"v".to_vec(),
                }),
            ),
            (
                "release",
                Box::new(|fence| ResourceAction::Release { fence }),
            ),
            (
                "release_with_value",
                Box::new(|fence| ResourceAction::ReleaseWithValue {
                    fence,
                    expected_value_revision: 0,
                    final_value: Vec::new(),
                }),
            ),
        ];

        for (name, build) in &operations {
            // The foreign fence: resource, holder and epoch all coincide with
            // B's row — only the minting group differs.
            let decision = decide(&group_b, &HOLDER, &build(minted_by(GROUP_A)));
            assert_eq!(
                decision,
                Decision::Refuse(RefusalReason::EpochMismatch),
                "{name}: a fence minted by another group must be refused"
            );

            // And the same fence naming B's own group applies, so the refusal
            // above is the group's doing and nothing else's.
            let own = decide(&group_b, &HOLDER, &build(minted_by(GROUP_B)));
            assert!(
                matches!(own, Decision::Apply(_)),
                "{name}: the same fence minted by this group applies: {own:?}"
            );
        }
    }

    fn effect(d: Decision) -> ResourceEffect {
        match d {
            Decision::Apply(e) => *e,
            other => panic!("expected an applied transition, got {other:?}"),
        }
    }

    fn reason(d: Decision) -> Reason {
        match d {
            Decision::Refuse(r) => r.get(),
            other => panic!("expected a reason-coded refusal, got {other:?}"),
        }
    }

    // ── Acquire ─────────────────────────────────────────────────────────────

    /// §9.1's initial state, reached by acquiring a resource that does not
    /// exist. Every field is asserted, because "lazily created" is the only
    /// place these values are ever established.
    #[test]
    fn acquiring_an_absent_resource_creates_it_from_the_declared_initial_state() {
        let s = state_with_rights(HOLDER, ALL_RIGHTS);
        let e = effect(decide(
            &s,
            &HOLDER,
            &ResourceAction::Acquire {
                resource: raw(),
                ttl_ms: 30_000,
            },
        ));

        assert!(e.creates_resource);
        assert_eq!(e.outcome, Outcome::Granted);
        // From epoch 0 / lease_revision 0, each advanced once.
        assert_eq!(e.row.epoch, 1);
        assert_eq!(e.row.lease_revision, 1);
        assert!(e.row.held);
        assert_eq!(e.row.holder, Some(HOLDER));
        assert_eq!(e.row.ttl_ms, 30_000);
        // Untouched by an acquire.
        assert_eq!(e.row.value_revision, 0);
        assert!(e.row.value.is_empty());
        assert_eq!(e.row.acl_revision, 0);
    }

    /// "Acquire never takes a held row, even if a leader timer believes it is
    /// old." For `acquire`, `1 HeldByOther` means **already held, including by
    /// this caller** -- the name reads as though it excluded the current
    /// holder, and it does not.
    #[test]
    fn acquire_refuses_a_held_row_including_the_callers_own_and_changes_nothing() {
        let before = held_row(HOLDER, 5);
        let acquire = ResourceAction::Acquire {
            resource: raw(),
            ttl_ms: 60_000,
        };

        // The current holder re-acquiring: refused, not silently renewed.
        let mine = with_row(state_with_rights(HOLDER, ALL_RIGHTS), before.clone());
        assert_eq!(
            reason(decide(&mine, &HOLDER, &acquire)),
            Reason::HeldByOther
        );

        // Another node: the same reason, which is what the name suggests.
        let theirs = with_row(state_with_rights(OTHER, ALL_RIGHTS), before.clone());
        assert_eq!(
            reason(decide(&theirs, &OTHER, &acquire)),
            Reason::HeldByOther
        );

        // And the refusal changes nothing -- in particular it does not extend
        // the lease, move the TTL, or advance any counter.
        assert_eq!(mine.resources.get(&key()), Some(&before));
        assert_eq!(theirs.resources.get(&key()), Some(&before));
    }

    #[test]
    fn a_ttl_outside_the_v1_bounds_is_a_shapeless_refusal() {
        let s = state_with_rights(HOLDER, ALL_RIGHTS);
        for ttl in [0, MIN_TTL_MS - 1, MAX_TTL_MS + 1, u64::MAX] {
            let d = decide(
                &s,
                &HOLDER,
                &ResourceAction::Acquire {
                    resource: raw(),
                    ttl_ms: ttl,
                },
            );
            assert!(
                matches!(d, Decision::RefuseShapeless(_)),
                "ttl {ttl} must be shapeless, got {d:?}"
            );
        }
        // And the boundaries themselves are legal.
        for ttl in [MIN_TTL_MS, MAX_TTL_MS] {
            let d = decide(
                &s,
                &HOLDER,
                &ResourceAction::Acquire {
                    resource: raw(),
                    ttl_ms: ttl,
                },
            );
            assert!(matches!(d, Decision::Apply(_)), "ttl {ttl} is in bounds");
        }
    }

    /// A shape violation is decided before authorization, and before any
    /// replicated state is consulted: an ill-shaped request was never a
    /// well-posed question.
    #[test]
    fn a_shape_violation_precedes_authorization() {
        let s = State::default(); // no grants at all
        let d = decide(
            &s,
            &HOLDER,
            &ResourceAction::Acquire {
                resource: RawResourceKey::new("n".repeat(limits::MAX_NAMESPACE + 1), b"k".to_vec()),
                ttl_ms: 30_000,
            },
        );
        assert!(matches!(d, Decision::RefuseShapeless(_)));
    }

    // ── Authorization ───────────────────────────────────────────────────────

    /// Each action needs its own right, and only that right.
    #[test]
    fn every_action_requires_its_declared_right() {
        let cases: Vec<(ResourceAction, u64)> = vec![
            (
                ResourceAction::Acquire {
                    resource: raw(),
                    ttl_ms: 30_000,
                },
                RIGHT_ACQUIRE,
            ),
            (
                ResourceAction::Renew {
                    fence: fence(5, HOLDER),
                },
                RIGHT_RENEW,
            ),
            (
                ResourceAction::CompareAndSet {
                    fence: fence(5, HOLDER),
                    expected_value_revision: 0,
                    new_value: b"v".to_vec(),
                },
                RIGHT_MUTATE,
            ),
            (
                ResourceAction::Release {
                    fence: fence(5, HOLDER),
                },
                RIGHT_RELEASE,
            ),
            (
                ResourceAction::ReleaseWithValue {
                    fence: fence(5, HOLDER),
                    expected_value_revision: 0,
                    final_value: b"v".to_vec(),
                },
                RIGHT_RELEASE | RIGHT_MUTATE,
            ),
            (
                ResourceAction::Revoke {
                    resource: raw(),
                    expected_epoch: None,
                },
                RIGHT_REVOKE,
            ),
        ];

        for (action, needed) in cases {
            // With everything except the needed bits: refused.
            let without = with_row(
                state_with_rights(HOLDER, ALL_RIGHTS & !needed),
                held_row(HOLDER, 5),
            );
            assert_eq!(
                reason(decide(&without, &HOLDER, &action)),
                Reason::PermissionChanged,
                "{action:?} must require its declared right"
            );

            // With exactly the needed bits: not an authorization failure.
            let with = with_row(state_with_rights(HOLDER, needed), held_row(HOLDER, 5));
            let d = decide(&with, &HOLDER, &action);
            assert!(
                !matches!(d, Decision::Refuse(RefusalReason::PermissionChanged)),
                "{action:?} must be satisfied by exactly its declared right"
            );
        }
    }

    /// `release_with_value` is the one action needing two rights; either alone
    /// is insufficient.
    #[test]
    fn release_with_value_needs_both_release_and_mutate() {
        let action = ResourceAction::ReleaseWithValue {
            fence: fence(5, HOLDER),
            expected_value_revision: 0,
            final_value: b"v".to_vec(),
        };
        for rights in [RIGHT_RELEASE, RIGHT_MUTATE] {
            let s = with_row(state_with_rights(HOLDER, rights), held_row(HOLDER, 5));
            assert_eq!(
                reason(decide(&s, &HOLDER, &action)),
                Reason::PermissionChanged
            );
        }
        let s = with_row(
            state_with_rights(HOLDER, RIGHT_RELEASE | RIGHT_MUTATE),
            held_row(HOLDER, 5),
        );
        assert_eq!(
            effect(decide(&s, &HOLDER, &action)).outcome,
            Outcome::Released
        );
    }

    // ── Fence checking ──────────────────────────────────────────────────────

    /// The fence reasons are ordered most-specific-first. Reporting
    /// `EpochMismatch` to a node that was never the holder would send it to
    /// refresh a fence it can never hold.
    #[test]
    fn fence_failures_report_the_most_specific_reason() {
        let rights = state_with_rights(HOLDER, ALL_RIGHTS);

        // Unheld row: NotHeld, whatever the fence claims.
        let free = with_row(rights.clone(), ResourceRow::default());
        assert_eq!(
            reason(decide(
                &free,
                &HOLDER,
                &ResourceAction::Renew {
                    fence: fence(99, HOLDER)
                }
            )),
            Reason::NotHeld
        );

        // Held by someone else: NotHolder, not EpochMismatch, even though the
        // epoch is also wrong.
        let theirs = with_row(rights.clone(), held_row(OTHER, 5));
        assert_eq!(
            reason(decide(
                &theirs,
                &HOLDER,
                &ResourceAction::Renew {
                    fence: fence(99, HOLDER)
                }
            )),
            Reason::NotHolder
        );

        // Right holder, stale epoch: EpochMismatch.
        let mine = with_row(rights, held_row(HOLDER, 5));
        assert_eq!(
            reason(decide(
                &mine,
                &HOLDER,
                &ResourceAction::Renew {
                    fence: fence(4, HOLDER)
                }
            )),
            Reason::EpochMismatch
        );
    }

    /// A client cannot present another node's fence, even a currently valid
    /// one.
    #[test]
    fn a_fence_naming_another_holder_is_refused() {
        let s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), held_row(OTHER, 5));
        // The fence is accurate -- OTHER really does hold epoch 5 -- but HOLDER
        // is the one presenting it.
        assert_eq!(
            reason(decide(
                &s,
                &HOLDER,
                &ResourceAction::Renew {
                    fence: fence(5, OTHER)
                }
            )),
            Reason::NotHolder
        );
    }

    /// A resource that does not exist has no lease to operate on, and none of
    /// these actions lazily creates one (§9.1 lists only acquire, set_grant and
    /// administrator initialization).
    #[test]
    fn fence_operations_on_an_absent_resource_are_not_held() {
        let s = state_with_rights(HOLDER, ALL_RIGHTS);
        for action in [
            ResourceAction::Renew {
                fence: fence(1, HOLDER),
            },
            ResourceAction::Release {
                fence: fence(1, HOLDER),
            },
            ResourceAction::CompareAndSet {
                fence: fence(1, HOLDER),
                expected_value_revision: 0,
                new_value: b"v".to_vec(),
            },
            ResourceAction::Revoke {
                resource: raw(),
                expected_epoch: None,
            },
        ] {
            assert_eq!(
                reason(decide(&s, &HOLDER, &action)),
                Reason::NotHeld,
                "{action:?} on an absent resource"
            );
        }
    }

    // ── Transition effects ──────────────────────────────────────────────────

    /// Renew advances the lease revision and **nothing else**: a renew that
    /// moved the epoch would invalidate the fence the holder is still using.
    #[test]
    fn renew_advances_only_the_lease_revision() {
        let before = held_row(HOLDER, 5);
        let s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), before.clone());
        let e = effect(decide(
            &s,
            &HOLDER,
            &ResourceAction::Renew {
                fence: fence(5, HOLDER),
            },
        ));
        assert_eq!(e.outcome, Outcome::Renewed);
        assert_eq!(e.row.lease_revision, before.lease_revision + 1);
        assert_eq!(e.row.epoch, before.epoch);
        assert_eq!(e.row.value_revision, before.value_revision);
        assert_eq!(e.row.holder, before.holder);
        assert!(e.row.held);
    }

    /// "compare_and_set does not renew" -- so a holder cannot keep a lease alive
    /// by writing to it.
    #[test]
    fn compare_and_set_does_not_renew_the_lease() {
        let before = held_row(HOLDER, 5);
        let s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), before.clone());
        let e = effect(decide(
            &s,
            &HOLDER,
            &ResourceAction::CompareAndSet {
                fence: fence(5, HOLDER),
                expected_value_revision: 0,
                new_value: b"hello".to_vec(),
            },
        ));
        assert_eq!(e.outcome, Outcome::Mutated);
        assert_eq!(e.row.value, b"hello");
        assert_eq!(e.row.value_revision, before.value_revision + 1);
        assert_eq!(
            e.row.lease_revision, before.lease_revision,
            "a mutation must not extend the lease"
        );
        assert_eq!(e.row.epoch, before.epoch);
        assert!(e.row.held);
    }

    #[test]
    fn compare_and_set_requires_the_exact_value_revision() {
        let mut before = held_row(HOLDER, 5);
        before.value_revision = 3;
        let s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), before);
        for expected in [0u64, 2, 4] {
            assert_eq!(
                reason(decide(
                    &s,
                    &HOLDER,
                    &ResourceAction::CompareAndSet {
                        fence: fence(5, HOLDER),
                        expected_value_revision: expected,
                        new_value: b"v".to_vec(),
                    }
                )),
                Reason::ValueRevisionMismatch
            );
        }
    }

    /// A plain release preserves the value, which is why it is safety-reducing
    /// without being a way out of budget exhaustion.
    #[test]
    fn release_clears_the_lease_but_preserves_the_value() {
        let mut before = held_row(HOLDER, 5);
        before.value = b"payload".to_vec();
        before.value_revision = 2;
        let s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), before.clone());
        let e = effect(decide(
            &s,
            &HOLDER,
            &ResourceAction::Release {
                fence: fence(5, HOLDER),
            },
        ));
        assert_eq!(e.outcome, Outcome::Released);
        assert!(!e.row.held);
        assert_eq!(e.row.holder, None);
        assert_eq!(e.row.epoch, before.epoch + 1);
        assert_eq!(e.row.lease_revision, before.lease_revision + 1);
        assert_eq!(e.row.value, b"payload", "the value survives a release");
        assert_eq!(e.row.value_revision, before.value_revision);
    }

    #[test]
    fn release_with_value_applies_all_five_effects() {
        let mut before = held_row(HOLDER, 5);
        before.value = b"old".to_vec();
        before.value_revision = 2;
        let s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), before.clone());
        let e = effect(decide(
            &s,
            &HOLDER,
            &ResourceAction::ReleaseWithValue {
                fence: fence(5, HOLDER),
                expected_value_revision: 2,
                final_value: b"final".to_vec(),
            },
        ));
        assert_eq!(e.row.value, b"final");
        assert_eq!(e.row.value_revision, 3);
        assert!(!e.row.held);
        assert_eq!(e.row.holder, None);
        assert_eq!(e.row.epoch, before.epoch + 1);
        assert_eq!(e.row.lease_revision, before.lease_revision + 1);
    }

    /// "A failed release_with_value leaves both the value and lease unchanged" --
    /// there is no half-applied form.
    #[test]
    fn a_failed_release_with_value_changes_nothing() {
        let mut before = held_row(HOLDER, 5);
        before.value = b"old".to_vec();
        before.value_revision = 2;
        let s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), before);
        let d = decide(
            &s,
            &HOLDER,
            &ResourceAction::ReleaseWithValue {
                fence: fence(5, HOLDER),
                expected_value_revision: 99,
                final_value: b"final".to_vec(),
            },
        );
        assert_eq!(reason(d), Reason::ValueRevisionMismatch);
    }

    /// Revoke has no `held` precondition (§9.1): it is an unconditional fencing
    /// operation, so it advances the epoch and lease revision whether or not
    /// anyone holds the lease.
    ///
    /// See [`repeated_revoke_commands_advance_the_counters_each_time`] for why
    /// this is not the same as calling the transition idempotent.
    #[test]
    fn revoke_fences_a_held_row_and_advances_a_free_one() {
        let held = held_row(OTHER, 5);
        let s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), held.clone());
        let e = effect(decide(
            &s,
            &HOLDER,
            &ResourceAction::Revoke {
                resource: raw(),
                expected_epoch: None,
            },
        ));
        assert_eq!(e.outcome, Outcome::Revoked);
        assert!(!e.row.held);
        assert_eq!(e.row.holder, None);
        assert_eq!(e.row.epoch, held.epoch + 1);

        // A free row: still advances, still clears.
        let free = ResourceRow {
            epoch: 9,
            lease_revision: 4,
            ..Default::default()
        };
        let s2 = with_row(state_with_rights(HOLDER, ALL_RIGHTS), free);
        let e2 = effect(decide(
            &s2,
            &HOLDER,
            &ResourceAction::Revoke {
                resource: raw(),
                expected_epoch: None,
            },
        ));
        assert_eq!(e2.row.epoch, 10);
        assert_eq!(e2.row.lease_revision, 5);
    }

    #[test]
    fn revoke_honours_an_expected_epoch_when_one_is_given() {
        let s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), held_row(OTHER, 5));
        assert_eq!(
            reason(decide(
                &s,
                &HOLDER,
                &ResourceAction::Revoke {
                    resource: raw(),
                    expected_epoch: Some(4),
                }
            )),
            Reason::EpochMismatch
        );
        assert!(matches!(
            decide(
                &s,
                &HOLDER,
                &ResourceAction::Revoke {
                    resource: raw(),
                    expected_epoch: Some(5),
                }
            ),
            Decision::Apply(_)
        ));
    }

    /// The revoke *transition* is not idempotent: two separate revoke commands
    /// advance the counters twice. Only a replay of the same sequenced command
    /// returns the first answer without re-applying, and that is the session
    /// table's doing rather than this transition's.
    #[test]
    fn repeated_revoke_commands_advance_the_counters_each_time() {
        let mut row = held_row(OTHER, 5);
        row.lease_revision = 2;
        let mut s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), row);

        let revoke = ResourceAction::Revoke {
            resource: raw(),
            expected_epoch: None,
        };

        let first = effect(decide(&s, &HOLDER, &revoke));
        assert_eq!((first.row.epoch, first.row.lease_revision), (6, 3));

        // Commit it, then revoke again: the counters move a second time.
        s.resources.insert(key(), first.row.clone());
        let second = effect(decide(&s, &HOLDER, &revoke));
        assert_eq!(
            (second.row.epoch, second.row.lease_revision),
            (7, 4),
            "a second revoke command is a second fencing event, not a no-op"
        );
    }

    // ── Counters ────────────────────────────────────────────────────────────

    /// All of a command's counters are checked together, before any of them
    /// moves. A fence that advanced the epoch but not the lease revision is not
    /// a legal state.
    #[test]
    fn a_counter_at_the_ceiling_refuses_the_whole_command() {
        // Release advances epoch and lease_revision; either at the ceiling
        // refuses, and the other must not have moved.
        for (epoch, lease) in [(MAX_COUNTER, 1u64), (5, MAX_COUNTER)] {
            let mut row = held_row(HOLDER, epoch);
            row.lease_revision = lease;
            let s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), row);
            assert_eq!(
                reason(decide(
                    &s,
                    &HOLDER,
                    &ResourceAction::Release {
                        fence: fence(epoch, HOLDER)
                    }
                )),
                Reason::CounterExhausted,
                "epoch {epoch}, lease {lease}"
            );
        }
    }

    /// One below the ceiling still applies, so the guard is not off by one.
    #[test]
    fn one_below_the_ceiling_still_applies() {
        let mut row = held_row(HOLDER, MAX_COUNTER - 1);
        row.lease_revision = MAX_COUNTER - 1;
        let s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), row);
        let e = effect(decide(
            &s,
            &HOLDER,
            &ResourceAction::Release {
                fence: fence(MAX_COUNTER - 1, HOLDER),
            },
        ));
        assert_eq!(e.row.epoch, MAX_COUNTER);
        assert_eq!(e.row.lease_revision, MAX_COUNTER);
    }

    // ── Budget and slots ────────────────────────────────────────────────────

    /// A value that grows past the budget is refused; the same command
    /// shrinking the value is not.
    #[test]
    fn growth_is_charged_on_the_value_delta_not_the_command() {
        let mut row = held_row(HOLDER, 5);
        row.value = vec![0u8; 1000];
        let mut s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), row);
        s.growth_bytes = GROWTH_BUDGET - 10;

        // Growing by more than the remaining headroom: refused.
        assert_eq!(
            reason(decide(
                &s,
                &HOLDER,
                &ResourceAction::CompareAndSet {
                    fence: fence(5, HOLDER),
                    expected_value_revision: 0,
                    new_value: vec![0u8; 2000],
                }
            )),
            Reason::StateBudgetExhausted
        );

        // Shrinking, at the same saturated budget: allowed.
        assert!(matches!(
            decide(
                &s,
                &HOLDER,
                &ResourceAction::CompareAndSet {
                    fence: fence(5, HOLDER),
                    expected_value_revision: 0,
                    new_value: vec![0u8; 100],
                }
            ),
            Decision::Apply(_)
        ));
    }

    /// `release_with_value` is safety-reducing only when the value does not
    /// grow -- decided against the row, not by the variant.
    #[test]
    fn release_with_value_bypasses_the_budget_only_when_it_does_not_grow() {
        let mut row = held_row(HOLDER, 5);
        row.value = vec![0u8; 1000];
        let mut s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), row);
        s.growth_bytes = GROWTH_BUDGET - 10;

        // Not larger: applies even at a saturated budget.
        assert!(matches!(
            decide(
                &s,
                &HOLDER,
                &ResourceAction::ReleaseWithValue {
                    fence: fence(5, HOLDER),
                    expected_value_revision: 0,
                    final_value: vec![0u8; 1000],
                }
            ),
            Decision::Apply(_)
        ));

        // Larger: it is not safety-reducing, so the budget binds.
        assert_eq!(
            reason(decide(
                &s,
                &HOLDER,
                &ResourceAction::ReleaseWithValue {
                    fence: fence(5, HOLDER),
                    expected_value_revision: 0,
                    final_value: vec![0u8; 5000],
                }
            )),
            Reason::StateBudgetExhausted
        );
    }

    /// A plain release and a revoke preserve the value, so they never free
    /// growth -- an earlier revision offered them as a way out of exhaustion.
    #[test]
    fn release_and_revoke_free_no_growth_bytes() {
        let mut row = held_row(HOLDER, 5);
        row.value = vec![0u8; 1000];
        let s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), row);

        for action in [
            ResourceAction::Release {
                fence: fence(5, HOLDER),
            },
            ResourceAction::Revoke {
                resource: raw(),
                expected_epoch: None,
            },
        ] {
            let e = effect(decide(&s, &HOLDER, &action));
            assert_eq!(
                e.row.value.len(),
                1000,
                "{action:?} must preserve the value"
            );
        }
    }

    /// Creating a resource at the slot cap is refused with the terminal `22`,
    /// checked before the retryable budget -- answering `19` would tell the
    /// client to free bytes, and every retry would hit the slot cap again.
    #[test]
    fn the_resource_slot_cap_is_checked_before_the_byte_budget() {
        let mut s = state_with_rights(HOLDER, ALL_RIGHTS);
        for i in 0..MAX_RESOURCES_PER_GROUP {
            s.resources.insert(
                ResourceKey::new("f", i.to_be_bytes().to_vec()).expect("valid"),
                ResourceRow::default(),
            );
        }
        // Both conditions hold at once.
        s.growth_bytes = GROWTH_BUDGET;

        assert_eq!(
            reason(decide(
                &s,
                &HOLDER,
                &ResourceAction::Acquire {
                    resource: raw(),
                    ttl_ms: 30_000,
                }
            )),
            Reason::ResourceSlotsExhausted,
            "the terminal reason must win over the retryable one"
        );
    }

    /// "Updating an existing row is never refused by a slot cap." Refusing it
    /// would freeze every lease in the group the moment the cap was reached.
    #[test]
    fn an_existing_resource_still_transitions_at_the_slot_cap() {
        let mut s = with_row(
            state_with_rights(HOLDER, ALL_RIGHTS),
            ResourceRow::default(),
        );
        for i in 0..MAX_RESOURCES_PER_GROUP {
            s.resources.insert(
                ResourceKey::new("f", i.to_be_bytes().to_vec()).expect("valid"),
                ResourceRow::default(),
            );
        }
        assert!(s.resources.len() as u64 > MAX_RESOURCES_PER_GROUP);

        assert!(
            matches!(
                decide(
                    &s,
                    &HOLDER,
                    &ResourceAction::Acquire {
                        resource: raw(),
                        ttl_ms: 30_000,
                    }
                ),
                Decision::Apply(_)
            ),
            "an existing row creates no slot, so the cap must not apply"
        );
    }

    // ── Cross-cutting ───────────────────────────────────────────────────────

    /// No decision can emit a reserved reason -- and this is now structural
    /// rather than checked: [`RefusalReason`] has no constant for a reserved
    /// code, so a refusal path cannot name one. The test asserts the property
    /// still holds end to end.
    #[test]
    fn no_transition_emits_a_reserved_reason() {
        let s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), held_row(OTHER, 5));
        let actions = [
            ResourceAction::Acquire {
                resource: raw(),
                ttl_ms: 30_000,
            },
            ResourceAction::Renew {
                fence: fence(5, HOLDER),
            },
            ResourceAction::Release {
                fence: fence(5, HOLDER),
            },
            ResourceAction::Revoke {
                resource: raw(),
                expected_epoch: Some(1),
            },
        ];
        for action in actions {
            if let Decision::Refuse(r) = decide(&s, &HOLDER, &action) {
                assert!(
                    !r.get().is_reserved(),
                    "{action:?} emitted the reserved {r:?}"
                );
                assert_ne!(r.get(), Reason::None, "a refusal must name a cause");
            }
        }
    }

    /// The decision is a pure function of `(state, actor, action)`: the same
    /// inputs give the same answer, which is what makes every voter agree.
    #[test]
    fn deciding_twice_gives_the_same_answer_and_mutates_nothing() {
        let s = with_row(state_with_rights(HOLDER, ALL_RIGHTS), held_row(HOLDER, 5));
        let action = ResourceAction::Renew {
            fence: fence(5, HOLDER),
        };
        let first = decide(&s, &HOLDER, &action);
        let second = decide(&s, &HOLDER, &action);
        assert_eq!(first, second);
        // The state is untouched: `decide` writes nothing.
        assert_eq!(s.resources.get(&key()).unwrap().lease_revision, 1);
    }
}
