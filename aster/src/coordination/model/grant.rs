//! `set_grant` (§8.1, §8.3, §12.1) -- one decision function, shared by
//! preflight and apply.
//!
//! This is the most-corrected command in the subsystem. The spike found four
//! separate defects in it across three review rounds, and every one of them is
//! a rule below with a test named for its consequence:
//!
//! * the §8.3 holder fence was simply absent, so a holder stripped of `RENEW`,
//!   `MUTATE` or `RELEASE` kept its lease -- the opposite of what revocation is
//!   for;
//! * a same-rights re-issue advanced `acl_revision`, invalidating every other
//!   client's `expected_acl_revision` for a command that changed nothing;
//! * revision arithmetic was unchecked, and a fence advances **three** counters,
//!   so a partial advance was reachable;
//! * the per-resource grant cap existed as a constant and was never enforced.
//!
//! ## Why it crosses two capped tables
//!
//! §9.1 creates a resource lazily on the first authorized `acquire`,
//! `set_grant`, or administrator initialization. So a `set_grant` naming a
//! resource that does not exist creates **two** rows, one in each capped table,
//! and the three rules of §12.1 apply: both rows or neither; resource-slot
//! exhaustion wins when a new resource is needed; and `rights = 0` on an absent
//! resource creates nothing at all.
//!
//! That last one is not tidiness. Grant removal is safety-reducing, so it can
//! never be refused for capacity -- and if removal lazily created a resource,
//! it would be an unbounded channel past the resource cap.

use super::limits::{
    self, LEASE_SUSTAINING_RIGHTS, MAX_ACL_ROWS_PER_GROUP, MAX_COUNTER, MAX_GRANTS_PER_RESOURCE,
    MAX_RESOURCES_PER_GROUP,
};
use super::resource::RawResourceKey;
use super::shape::{self, ShapeLimit};
use super::state::{key_parts, NodeId, ResourceKey, ResourceRow, State};
use crate::coordination::reason::{Outcome, RefusalReason};

/// A `set_grant` request (§5.2 method 7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantAction {
    pub resource: RawResourceKey,
    /// The node whose rights change. Never the caller: `target_node_id` names
    /// the object being changed, not the actor.
    pub subject: NodeId,
    /// The new rights mask. `0` removes the grant.
    pub rights: u64,
    pub expected_acl_revision: u64,
}

/// What a `set_grant` writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantEffect {
    pub key: ResourceKey,
    /// The node whose grant changes.
    pub subject: NodeId,
    /// The resource row after the transition -- the existing row with its
    /// revisions advanced, or a lazily created one. Written whole, so the fence
    /// and the revision bump cannot be applied separately.
    pub row: ResourceRow,
    pub creates_resource: bool,
    /// `Some(rights)` to insert or update the grant, `None` to remove it.
    pub rights: Option<u64>,
    /// Whether this adds an ACL row that did not exist.
    pub creates_acl_row: bool,
    /// Whether the current holder was fenced (§8.3).
    pub fenced: bool,
}

/// The outcome of deciding one `set_grant`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Apply(Box<GrantEffect>),
    /// The command is legal and changes nothing.
    ///
    /// Returns `6 GrantUpdated` with the revision **unmoved**: re-issuing an
    /// identical grant, or removing a grant that is not there, must not
    /// invalidate every other client's `expected_acl_revision` for a command
    /// that changed nothing.
    NoOp {
        acl_revision: u64,
    },
    Refuse(RefusalReason),
    RefuseShapeless(ShapeLimit),
}

impl Decision {
    /// The outcome code a successful decision reports. Both success shapes are
    /// `6 GrantUpdated`; a client distinguishes them by the revision.
    pub fn outcome(&self) -> Option<Outcome> {
        match self {
            Decision::Apply(_) | Decision::NoOp { .. } => Some(Outcome::GrantUpdated),
            Decision::Refuse(_) | Decision::RefuseShapeless(_) => None,
        }
    }
}

/// Decide one `set_grant` against committed state.
///
/// Check order is normative wherever it changes the answer; each ordering
/// decision is argued at its site.
pub fn decide(state: &State, actor: &NodeId, action: &GrantAction) -> Decision {
    // Shape first: a malformed identity or an undefined rights bit is a
    // request that was never well posed, and §8.1 rejects unknown bits rather
    // than ignoring them -- a grant this voter cannot evaluate is not forward
    // compatibility.
    let key = match action.resource.validate() {
        Ok(key) => key,
        Err(limit) => return Decision::RefuseShapeless(limit),
    };
    if let Some(limit) = shape::check_rights(action.rights) {
        return Decision::RefuseShapeless(limit);
    }

    // Authorization is **group administrator**, not resource rights. §8.1:
    // administrators may change ACL/admin/membership state but receive no
    // implicit resource rights, and holding rights on a resource confers no
    // power to grant them.
    if !state.admins.contains(actor) {
        return Decision::Refuse(RefusalReason::PermissionChanged);
    }

    let existing_resource = state.resources.get(&key);
    let current_revision = existing_resource.map_or(0, |r| r.acl_revision);

    // The revision check runs **before** any capacity decision. A stale request
    // was never going to apply, so reporting a capacity reason for it would
    // send the client to free rows it does not need.
    if action.expected_acl_revision != current_revision {
        return Decision::Refuse(RefusalReason::AclRevisionMismatch);
    }

    let existing_rights = state.acl.get(&(key.clone(), action.subject)).copied();
    let removing = action.rights == 0;

    // Change detection precedes every capacity and ceiling check, because a
    // command that changes nothing cannot exhaust anything. The spike's two
    // preflight/apply disagreements were both here: at the ACL-revision ceiling
    // a no-op still succeeds, and `rights = 0` on an absent resource still
    // succeeds at a full resource cap.
    let changes = if removing {
        existing_rights.is_some()
    } else {
        existing_rights != Some(action.rights)
    };
    if !changes {
        return Decision::NoOp {
            acl_revision: current_revision,
        };
    }

    // §8.3: does this fence the current holder? Only the *current holder*
    // losing a sustaining right does. Losing only `READ`/`ACQUIRE` does not,
    // and neither does changing a non-holder's rights.
    let fences = existing_resource.is_some_and(|row| {
        row.held
            && row.holder == Some(action.subject)
            && existing_rights.is_some_and(|before| {
                let after = action.rights;
                before & LEASE_SUSTAINING_RIGHTS & !after != 0
            })
    });

    // All of the command's counters, together, before any of them moves. A
    // fence advances three; one that advanced the epoch but not the lease
    // revision is not a legal state, so a per-counter check that stopped
    // halfway would leave one.
    let mut counters = vec![current_revision];
    if fences {
        let row = existing_resource.expect("a fence implies a resource row");
        counters.push(row.epoch);
        counters.push(row.lease_revision);
    }
    if counters.iter().any(|c| *c >= MAX_COUNTER) {
        return Decision::Refuse(RefusalReason::CounterExhausted);
    }

    let creates_resource = existing_resource.is_none() && !removing;
    let creates_acl_row = existing_rights.is_none() && !removing;

    // Capacity. Removal is exempt from all of it: it is safety-reducing, and
    // gating it would make ACL exhaustion permanent -- which would in turn make
    // `23`'s non-terminality false. The exemption is scoped to **capacity
    // only**; removal is still refused above by a stale revision, an exhausted
    // counter, authorization, and the §8.2.1 classifier.
    if !removing {
        // Terminal before retryable: answering `23` when `22` also holds would
        // tell the client to free an ACL row and retry, and every retry would
        // hit the resource cap again.
        if creates_resource && state.resources.len() as u64 >= MAX_RESOURCES_PER_GROUP {
            return Decision::Refuse(RefusalReason::ResourceSlotsExhausted);
        }
        if creates_acl_row {
            let group_full = state.acl.len() as u64 >= MAX_ACL_ROWS_PER_GROUP;
            let resource_full = grants_on(state, &key) >= MAX_GRANTS_PER_RESOURCE;
            // Both are ACL-slot exhaustion and both are cleared the same way,
            // by removing a grant on that resource, so both are retryable and
            // both report `23`.
            if group_full || resource_full {
                return Decision::Refuse(RefusalReason::AclSlotsExhausted);
            }
        }
        if !growth_fits(state, &key, creates_resource, creates_acl_row) {
            return Decision::Refuse(RefusalReason::StateBudgetExhausted);
        }
    }

    // Build the whole post-state at once. `apply_acl_change` is one value
    // rather than three call sites because partial application is not a legal
    // state.
    let mut row = existing_resource.cloned().unwrap_or_default();
    row.acl_revision = current_revision + 1;
    if fences {
        row.held = false;
        row.holder = None;
        row.epoch += 1;
        row.lease_revision += 1;
    }

    Decision::Apply(Box::new(GrantEffect {
        key,
        subject: action.subject,
        row,
        creates_resource,
        rights: (!removing).then_some(action.rights),
        creates_acl_row,
        fenced: fences,
    }))
}

/// How many grants exist on one resource.
fn grants_on(state: &State, key: &ResourceKey) -> u64 {
    state.acl.keys().filter(|(k, _)| k == key).count() as u64
}

/// Would the rows this command adds fit the growth budget?
fn growth_fits(
    state: &State,
    key: &ResourceKey,
    creates_resource: bool,
    creates_acl_row: bool,
) -> bool {
    let (ns, k) = key_parts(key);
    let mut added = 0u64;
    if creates_resource {
        // A lazily created resource carries an empty value.
        added += limits::charge_resource(ns, k, 0);
    }
    if creates_acl_row {
        added += limits::charge_acl(ns, k);
    }
    if added == 0 {
        return true;
    }
    state.growth_bytes.saturating_add(added) <= limits::GROWTH_BUDGET
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::model::limits::{
        ALL_RIGHTS, GROWTH_BUDGET, RIGHT_ACQUIRE, RIGHT_MUTATE, RIGHT_READ, RIGHT_RELEASE,
        RIGHT_RENEW,
    };
    use crate::coordination::reason::Reason;

    const ADMIN: NodeId = [1u8; 32];
    const SUBJECT: NodeId = [2u8; 32];
    const OTHER: NodeId = [3u8; 32];

    fn raw() -> RawResourceKey {
        RawResourceKey::new("ns", b"k".to_vec())
    }

    fn key() -> ResourceKey {
        raw().validate().expect("valid")
    }

    /// A state with one administrator and nothing else.
    fn admin_state() -> State {
        let mut s = State::default();
        s.admins.insert(ADMIN);
        s.admin_revision = 1;
        s.growth_bytes = s.recompute_growth();
        s
    }

    fn action(rights: u64, expected_acl_revision: u64) -> GrantAction {
        GrantAction {
            resource: raw(),
            subject: SUBJECT,
            rights,
            expected_acl_revision,
        }
    }

    fn effect(d: Decision) -> GrantEffect {
        match d {
            Decision::Apply(e) => *e,
            other => panic!("expected an applied grant, got {other:?}"),
        }
    }

    fn reason(d: Decision) -> Reason {
        match d {
            Decision::Refuse(r) => r.get(),
            other => panic!("expected a reason-coded refusal, got {other:?}"),
        }
    }

    /// A resource held by `holder`, with `rights` granted to `SUBJECT`.
    fn held_by(state: &mut State, holder: NodeId, rights: u64, acl_revision: u64) {
        state.resources.insert(
            key(),
            ResourceRow {
                epoch: 4,
                held: true,
                holder: Some(holder),
                ttl_ms: 30_000,
                lease_revision: 2,
                value_revision: 0,
                value: Vec::new(),
                acl_revision,
            },
        );
        state.acl.insert((key(), SUBJECT), rights);
        state.growth_bytes = state.recompute_growth();
    }

    // ── Creation ────────────────────────────────────────────────────────────

    /// A grant on a missing resource creates **both** rows: the resource in
    /// §9.1's initial state, and the ACL row. The revision is `1`, not `0` --
    /// initialization is followed by the grant transition, so a freshly granted
    /// resource at revision `0` is a state the protocol cannot produce.
    #[test]
    fn a_grant_on_a_missing_resource_creates_both_rows_at_revision_one() {
        let s = admin_state();
        let e = effect(decide(&s, &ADMIN, &action(ALL_RIGHTS, 0)));

        assert!(e.creates_resource);
        assert!(e.creates_acl_row);
        assert_eq!(e.rights, Some(ALL_RIGHTS));
        assert_eq!(e.row.acl_revision, 1);
        // §9.1's initial state, otherwise untouched.
        assert_eq!(e.row.epoch, 0);
        assert!(!e.row.held);
        assert_eq!(e.row.holder, None);
        assert_eq!(e.row.value_revision, 0);
        assert!(e.row.value.is_empty());
        assert!(!e.fenced);
    }

    /// `rights = 0` on an absent resource is a pure no-op: it creates nothing,
    /// **not even the resource**. Lazily creating one here would make removal --
    /// which can never be refused for capacity -- an unbounded channel past the
    /// resource cap.
    #[test]
    fn removal_on_an_absent_resource_creates_nothing() {
        let s = admin_state();
        let d = decide(&s, &ADMIN, &action(0, 0));
        assert_eq!(d, Decision::NoOp { acl_revision: 0 });
        assert_eq!(d.outcome(), Some(Outcome::GrantUpdated));
    }

    /// "Always succeeds" is scoped to **capacity**. Removal is still refused by
    /// a stale revision.
    #[test]
    fn removal_is_still_refused_by_a_stale_revision() {
        let mut s = admin_state();
        held_by(&mut s, OTHER, ALL_RIGHTS, 3);
        assert_eq!(
            reason(decide(&s, &ADMIN, &action(0, 2))),
            Reason::AclRevisionMismatch
        );
    }

    // ── Authorization ───────────────────────────────────────────────────────

    /// Administrator, not resource rights. Holding every right on a resource
    /// confers no power to grant them.
    #[test]
    fn only_a_group_administrator_may_grant() {
        let mut s = admin_state();
        s.acl.insert((key(), OTHER), ALL_RIGHTS);
        s.resources.insert(key(), ResourceRow::default());
        s.growth_bytes = s.recompute_growth();

        assert_eq!(
            reason(decide(&s, &OTHER, &action(RIGHT_READ, 0))),
            Reason::PermissionChanged,
            "every right on the resource is not the power to grant"
        );
        assert!(matches!(
            decide(&s, &ADMIN, &action(RIGHT_READ, 0)),
            Decision::Apply(_)
        ));
    }

    #[test]
    fn an_undefined_rights_bit_is_a_shapeless_refusal() {
        let s = admin_state();
        assert!(matches!(
            decide(&s, &ADMIN, &action(0x40, 0)),
            Decision::RefuseShapeless(ShapeLimit::RightsBits)
        ));
        assert!(matches!(
            decide(&s, &ADMIN, &action(ALL_RIGHTS | 0x80, 0)),
            Decision::RefuseShapeless(ShapeLimit::RightsBits)
        ));
    }

    // ── Revisions ───────────────────────────────────────────────────────────

    /// A revision advances exactly when ACL state actually changes.
    #[test]
    fn the_revision_advances_only_when_the_grant_actually_changes() {
        let mut s = admin_state();
        held_by(&mut s, OTHER, RIGHT_READ, 5);

        // Same rights again: no-op, revision unmoved.
        assert_eq!(
            decide(&s, &ADMIN, &action(RIGHT_READ, 5)),
            Decision::NoOp { acl_revision: 5 }
        );

        // Different rights: advances.
        let e = effect(decide(&s, &ADMIN, &action(RIGHT_READ | RIGHT_ACQUIRE, 5)));
        assert_eq!(e.row.acl_revision, 6);
        assert!(!e.creates_acl_row, "updating a row creates none");
    }

    /// Removing a grant that is not there is a no-op too.
    #[test]
    fn removing_an_absent_grant_is_a_no_op() {
        let mut s = admin_state();
        s.resources.insert(
            key(),
            ResourceRow {
                acl_revision: 7,
                ..Default::default()
            },
        );
        s.growth_bytes = s.recompute_growth();

        assert_eq!(
            decide(&s, &ADMIN, &action(0, 7)),
            Decision::NoOp { acl_revision: 7 }
        );
    }

    /// Removing a grant that *is* there advances the revision.
    #[test]
    fn removing_a_present_grant_advances_the_revision() {
        let mut s = admin_state();
        held_by(&mut s, OTHER, RIGHT_READ, 5);
        let e = effect(decide(&s, &ADMIN, &action(0, 5)));
        assert_eq!(e.rights, None, "the grant row is removed");
        assert_eq!(e.row.acl_revision, 6);
    }

    // ── §8.3 holder fence ───────────────────────────────────────────────────

    /// Losing a sustaining right fences the holder in the **same** applied
    /// command: holder cleared, epoch and lease revision advanced, ACL revision
    /// recorded. Without this a holder stripped of `RENEW` keeps its lease --
    /// the opposite of what revocation is for.
    #[test]
    fn losing_a_sustaining_right_fences_the_holder_in_the_same_command() {
        for lost in [RIGHT_RENEW, RIGHT_MUTATE, RIGHT_RELEASE] {
            let mut s = admin_state();
            held_by(&mut s, SUBJECT, ALL_RIGHTS, 5);

            let e = effect(decide(&s, &ADMIN, &action(ALL_RIGHTS & !lost, 5)));
            assert!(e.fenced, "losing {lost:#x} must fence");
            assert!(!e.row.held);
            assert_eq!(e.row.holder, None);
            assert_eq!(e.row.epoch, 5, "epoch advanced");
            assert_eq!(e.row.lease_revision, 3, "lease revision advanced");
            assert_eq!(e.row.acl_revision, 6, "and the ACL revision too");
        }
    }

    /// `rights = 0` removes every right, so it fences as well.
    #[test]
    fn removing_every_right_from_the_holder_fences_it() {
        let mut s = admin_state();
        held_by(&mut s, SUBJECT, ALL_RIGHTS, 5);
        let e = effect(decide(&s, &ADMIN, &action(0, 5)));
        assert!(e.fenced);
        assert_eq!(e.row.holder, None);
    }

    /// Losing only `READ` or `ACQUIRE` does not terminate an existing lease --
    /// neither sustains one.
    #[test]
    fn losing_only_read_or_acquire_does_not_fence() {
        for lost in [RIGHT_READ, RIGHT_ACQUIRE] {
            let mut s = admin_state();
            held_by(&mut s, SUBJECT, ALL_RIGHTS, 5);

            let e = effect(decide(&s, &ADMIN, &action(ALL_RIGHTS & !lost, 5)));
            assert!(!e.fenced, "losing {lost:#x} must not fence");
            assert!(e.row.held, "the lease survives");
            assert_eq!(e.row.holder, Some(SUBJECT));
            assert_eq!(e.row.epoch, 4, "epoch unmoved");
            assert_eq!(e.row.lease_revision, 2, "lease revision unmoved");
            assert_eq!(e.row.acl_revision, 6, "but the ACL revision still advances");
        }
    }

    /// Changing a **non-holder's** rights never fences, even when the right lost
    /// is a sustaining one.
    #[test]
    fn changing_a_non_holders_rights_never_fences() {
        let mut s = admin_state();
        // OTHER holds the lease; SUBJECT merely has rights.
        held_by(&mut s, OTHER, ALL_RIGHTS, 5);

        let e = effect(decide(&s, &ADMIN, &action(RIGHT_READ, 5)));
        assert!(!e.fenced);
        assert!(e.row.held, "the actual holder keeps its lease");
        assert_eq!(e.row.holder, Some(OTHER));
        assert_eq!(e.row.epoch, 4);
        assert_eq!(e.row.acl_revision, 6, "the ACL revision still advances");
    }

    /// Granting a right the subject did not have never fences, whatever it is.
    #[test]
    fn adding_rights_never_fences() {
        let mut s = admin_state();
        held_by(&mut s, SUBJECT, RIGHT_READ, 5);
        let e = effect(decide(&s, &ADMIN, &action(ALL_RIGHTS, 5)));
        assert!(!e.fenced);
        assert!(e.row.held);
    }

    // ── Counter ceilings ────────────────────────────────────────────────────

    /// The ACL revision at the ceiling refuses the whole command, leaving ACL
    /// and resource state unchanged.
    #[test]
    fn an_acl_revision_at_the_ceiling_refuses_the_command() {
        let mut s = admin_state();
        held_by(&mut s, OTHER, RIGHT_READ, MAX_COUNTER);
        assert_eq!(
            reason(decide(&s, &ADMIN, &action(ALL_RIGHTS, MAX_COUNTER))),
            Reason::CounterExhausted
        );
    }

    /// A fence advances three counters, so **any** of them at the ceiling
    /// refuses -- the epoch and the lease revision as much as the ACL revision.
    #[test]
    fn a_fence_checks_all_three_of_its_counters() {
        for (epoch, lease) in [(MAX_COUNTER, 2u64), (4, MAX_COUNTER)] {
            let mut s = admin_state();
            s.resources.insert(
                key(),
                ResourceRow {
                    epoch,
                    held: true,
                    holder: Some(SUBJECT),
                    lease_revision: lease,
                    acl_revision: 5,
                    ..Default::default()
                },
            );
            s.acl.insert((key(), SUBJECT), ALL_RIGHTS);
            s.growth_bytes = s.recompute_growth();

            assert_eq!(
                reason(decide(&s, &ADMIN, &action(RIGHT_READ, 5))),
                Reason::CounterExhausted,
                "epoch {epoch}, lease {lease}"
            );
        }
    }

    /// A no-op succeeds at the ceiling: it advances no counter, so there is
    /// nothing to exhaust. This is one of the two states where the spike's
    /// separate preflight implementation disagreed with apply.
    #[test]
    fn a_no_op_succeeds_even_at_the_counter_ceiling() {
        let mut s = admin_state();
        held_by(&mut s, OTHER, RIGHT_READ, MAX_COUNTER);
        assert_eq!(
            decide(&s, &ADMIN, &action(RIGHT_READ, MAX_COUNTER)),
            Decision::NoOp {
                acl_revision: MAX_COUNTER
            }
        );
    }

    // ── Capacity ────────────────────────────────────────────────────────────

    fn fill_resources(s: &mut State, n: u64) {
        for i in 0..n {
            s.resources.insert(
                ResourceKey::new("f", i.to_be_bytes().to_vec()).expect("valid"),
                ResourceRow::default(),
            );
        }
        s.growth_bytes = s.recompute_growth();
    }

    /// Resource-slot exhaustion wins when the command needs a new resource:
    /// terminal before retryable.
    #[test]
    fn the_resource_cap_wins_when_a_new_resource_is_needed() {
        let mut s = admin_state();
        fill_resources(&mut s, MAX_RESOURCES_PER_GROUP);
        // Make the ACL cap and the budget bind too, so the precedence is what
        // decides rather than the only condition present.
        s.growth_bytes = GROWTH_BUDGET;

        assert_eq!(
            reason(decide(&s, &ADMIN, &action(ALL_RIGHTS, 0))),
            Reason::ResourceSlotsExhausted
        );
    }

    /// A grant on an **existing** resource is unaffected by the resource cap:
    /// it creates no resource row.
    #[test]
    fn an_existing_resource_is_unaffected_by_the_resource_cap() {
        let mut s = admin_state();
        fill_resources(&mut s, MAX_RESOURCES_PER_GROUP);
        s.resources.insert(key(), ResourceRow::default());
        s.growth_bytes = s.recompute_growth();

        assert!(matches!(
            decide(&s, &ADMIN, &action(ALL_RIGHTS, 0)),
            Decision::Apply(_)
        ));
    }

    /// The per-resource cap is enforced, and reports `23` like the group-wide
    /// one: both are cleared by removing a grant on that resource.
    #[test]
    fn the_two_hundred_and_fifty_seventh_grant_on_a_resource_is_refused() {
        let mut s = admin_state();
        s.resources.insert(
            key(),
            ResourceRow {
                acl_revision: MAX_GRANTS_PER_RESOURCE,
                ..Default::default()
            },
        );
        for i in 0..MAX_GRANTS_PER_RESOURCE {
            let mut subject = [0u8; 32];
            subject[..8].copy_from_slice(&i.to_be_bytes());
            s.acl.insert((key(), subject), RIGHT_READ);
        }
        s.growth_bytes = s.recompute_growth();
        assert_eq!(grants_on(&s, &key()), MAX_GRANTS_PER_RESOURCE);

        // A 257th subject: refused.
        let rev = MAX_GRANTS_PER_RESOURCE;
        assert_eq!(
            reason(decide(&s, &ADMIN, &action(RIGHT_READ, rev))),
            Reason::AclSlotsExhausted
        );

        // Updating one of the existing 256 creates no row, so it is unaffected.
        let mut existing = [0u8; 32];
        existing[..8].copy_from_slice(&0u64.to_be_bytes());
        let update = GrantAction {
            resource: raw(),
            subject: existing,
            rights: ALL_RIGHTS,
            expected_acl_revision: rev,
        };
        assert!(matches!(decide(&s, &ADMIN, &update), Decision::Apply(_)));

        // And removing one frees a slot, so the 257th then fits.
        let removal = GrantAction {
            resource: raw(),
            subject: existing,
            rights: 0,
            expected_acl_revision: rev,
        };
        let removed = effect(decide(&s, &ADMIN, &removal));
        s.acl.remove(&(key(), existing));
        s.resources.get_mut(&key()).unwrap().acl_revision = removed.row.acl_revision;
        s.growth_bytes = s.recompute_growth();

        assert!(matches!(
            decide(&s, &ADMIN, &action(RIGHT_READ, removed.row.acl_revision)),
            Decision::Apply(_)
        ));
    }

    /// Grant removal is never refused for capacity, at any cap.
    #[test]
    fn removal_is_never_refused_for_capacity() {
        let mut s = admin_state();
        held_by(&mut s, OTHER, ALL_RIGHTS, 5);
        fill_resources(&mut s, MAX_RESOURCES_PER_GROUP);
        s.growth_bytes = GROWTH_BUDGET;

        let e = effect(decide(&s, &ADMIN, &action(0, 5)));
        assert_eq!(e.rights, None);
        assert_eq!(e.row.acl_revision, 6);
    }

    /// The byte budget binds only after the two slot caps, and only for a
    /// command that adds rows.
    #[test]
    fn the_byte_budget_is_checked_last() {
        let mut s = admin_state();
        s.resources.insert(key(), ResourceRow::default());
        s.growth_bytes = GROWTH_BUDGET;

        assert_eq!(
            reason(decide(&s, &ADMIN, &action(RIGHT_READ, 0))),
            Reason::StateBudgetExhausted
        );
    }

    // ── Cross-cutting ───────────────────────────────────────────────────────

    /// Both rows or neither: a refused `set_grant` produces no effect at all,
    /// so there is no shape in which the ACL row lands without the resource.
    #[test]
    fn a_refused_grant_yields_no_partial_effect() {
        let mut s = admin_state();
        fill_resources(&mut s, MAX_RESOURCES_PER_GROUP);

        let d = decide(&s, &ADMIN, &action(ALL_RIGHTS, 0));
        assert!(
            matches!(d, Decision::Refuse(_)),
            "a refusal carries no effect to apply partially"
        );
    }

    /// The decision is a pure function of `(state, actor, action)` and mutates
    /// nothing.
    #[test]
    fn deciding_twice_gives_the_same_answer() {
        let mut s = admin_state();
        held_by(&mut s, SUBJECT, ALL_RIGHTS, 5);
        let a = action(RIGHT_READ, 5);
        assert_eq!(decide(&s, &ADMIN, &a), decide(&s, &ADMIN, &a));
        assert_eq!(s.resources.get(&key()).unwrap().acl_revision, 5);
    }

    /// No refusal names a reserved reason or `None` -- structural, since
    /// `RefusalReason` has no constant for either.
    #[test]
    fn no_refusal_names_a_reserved_reason() {
        let mut s = admin_state();
        held_by(&mut s, SUBJECT, ALL_RIGHTS, 5);
        for rights in [0, RIGHT_READ, ALL_RIGHTS, 0x40] {
            for expected in [0u64, 5, 9] {
                if let Decision::Refuse(r) = decide(&s, &ADMIN, &action(rights, expected)) {
                    assert!(!r.get().is_reserved());
                    assert_ne!(r.get(), Reason::None);
                }
            }
        }
    }
}
