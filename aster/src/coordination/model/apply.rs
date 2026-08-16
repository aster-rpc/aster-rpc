//! Preflight projection (§10 stages 2-4) and the committed apply path.
//!
//! One decision, two answers. [`preflight`] and [`apply_client_command`] call
//! the same `decide` functions; what differs is the **projection** -- whether a
//! refusal is denied before proposal or admitted so it reaches the log and
//! saves a result. A model that projects every refusal the same way is wrong in
//! one direction or the other: it either wedges the client on a conflict, or
//! burns a sequence for a condition that never reached the log.
//!
//! ## The forced-apply rule
//!
//! §10 stage 4 describes **conforming leader admission only**. A conforming
//! leader refuses a request-shape violation pre-proposal with an `RpcStatus`,
//! and nothing is written. A non-conforming leader can propose anything, and
//! apply may not skip a committed entry -- skipping diverges the voters, which
//! is the failure Raft exists to prevent. Halting instead would be worse: it
//! would hand a faulty leader an availability kill switch. Unknown commands are
//! fatal because voters cannot safely *interpret* them; a known bounds violation
//! is fully understood and can be rejected deterministically.
//!
//! So apply is total, and a committed request-shape violation commits:
//!
//! ```text
//! Consumed + 100 Rejected + 0 None
//! ```
//!
//! * **`Consumed`** because the entry committed. The result is saved and
//!   replayed exactly, like any other consumed sequence.
//! * **Reason `0`** truthfully: the frozen §5.2 reason space has no reason for
//!   this failure. Outcome `100` still identifies it as a rejection.
//!
//! Four invariants make that narrow rather than a loophole, and each has a test:
//!
//! 1. Forced apply mutates **no business state** -- no resource, ACL, admin,
//!    intent or route row -- but persists its saved reply, audit row, session
//!    sequence, applicable budget counters and `last_applied` atomically. That
//!    metadata is what consumes the sequence.
//! 2. `(Rejected, None)` is legal **only** for this case. Every other rejection
//!    carries a defined, nonzero coordination reason.
//! 3. A replay returns the saved typed reply, never an `RpcStatus`.
//! 4. Every logical request-shape limit is covered, not merely the obvious two.
//!    See [`ShapeLimit`](super::shape::ShapeLimit).
//!
//! ## Three things called "a replay", which behave differently
//!
//! Conflating them loses audit rows or double-charges the reserve:
//!
//! 1. **A preflight replay.** No entry was committed, so there is no audit row
//!    and `last_applied` does not move. [`preflight`] writes nothing at all.
//! 2. **A duplicate that was itself proposed and committed.** This happens
//!    normally -- two copies of a request preflight before either applies, so
//!    both reach the log -- and needs no faulty leader. It has no business or
//!    session effects, but it **is** an applied entry: it writes its audit row
//!    and advances `last_applied`, or the entry would be indistinguishable from
//!    one that was skipped.
//! 3. **Reapplication of the same log entry**, after a crash before
//!    `last_applied` was durable. Identified by `(log_term, log_index)`, the
//!    normative entry identity: the audit row already exists, so nothing is
//!    written and nothing is charged a second time. Content that differs at an
//!    existing identity is a divergence signal, not a duplicate.

use crate::coordination::reason::{Disposition, Outcome, Reason, RefusalReason};

use super::classify::{classify, Classification};
use super::limits;
use super::resource::{self, Decision, ResourceAction};
use super::shape::ShapeLimit;
use super::state::{
    key_parts, AuditRow, ClientId, NodeId, ResourceKey, SavedReply, Session, State,
};

/// The `(actor, client_id, sequence, request_hash)` mutation prefix every
/// client command carries, mirroring the wire type rather than passing four
/// loose arguments.
///
/// `actor` is never a request field: it is derived from the immutable Iroh
/// transport peer (§5.2). A request field that looks like a calling actor is a
/// contract error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientRef {
    pub actor: NodeId,
    pub client_id: ClientId,
    pub sequence: u64,
    pub request_hash: [u8; 32],
}

/// A client action. Resource actions today; grant and admin actions land with
/// their own modules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Resource(ResourceAction),
}

impl Action {
    /// The §5.2 method code, used as the audit operation code.
    pub fn operation_code(&self) -> i32 {
        match self {
            Action::Resource(r) => match r {
                ResourceAction::Acquire { .. } => 1,
                ResourceAction::Renew { .. } => 2,
                ResourceAction::CompareAndSet { .. } => 3,
                ResourceAction::Release { .. } => 4,
                ResourceAction::ReleaseWithValue { .. } => 5,
                ResourceAction::Revoke { .. } => 6,
            },
        }
    }

    /// The validated identity, or `None` when the request's identity is not
    /// well-formed.
    ///
    /// `None` is what keeps an unvalidated key out of bounded metadata: a saved
    /// reply and an audit row for a shapeless request carry no resource at all,
    /// rather than a copy of the malformed one. An oversized key stored in a
    /// session row would blow the 832-byte reserve charge the whole budget
    /// proof rests on, and would produce a row later validation rejects.
    fn validated_resource(&self) -> Option<ResourceKey> {
        match self {
            Action::Resource(r) => r.raw_resource().validate().ok(),
        }
    }

    fn decide(&self, state: &State, actor: &NodeId) -> Decision {
        match self {
            Action::Resource(r) => resource::decide(state, actor, r),
        }
    }
}

/// What a leader answers before proposing (§10 stages 2-4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Stage 2: the classifier resolved the sequence. Answered as a typed reply
    /// whatever the row -- including the non-terminal ones.
    Resolved(Box<ClientResult>),
    /// Stage 3: a reason-coded admission denial. Typed reply, `NotConsumed`,
    /// nothing written.
    ///
    /// A [`RefusalReason`], so a denial cannot be built carrying `None` or a
    /// reserved code.
    Deny(RefusalReason),
    /// Stage 4: a denial with no §5.2 reason code. An `RpcStatus`, never a
    /// typed reply -- there is no code to put in one.
    Status(StatusDenial),
    /// Propose it.
    Admit,
}

/// A stage-4 denial: no §5.2 reason code exists for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusDenial {
    /// The actor lacks the required right. At apply the same condition is
    /// `12 PermissionChanged`; pre-proposal §10 gives it no reason code.
    NotAuthorized,
    /// A request-shape bound.
    Shape(ShapeLimit),
}

/// A condition meaning the log or local state is inconsistent.
///
/// Not a rejection: there is no client answer for it. The runtime moves to
/// fatal rather than continuing, because continuing would mean writing state
/// that disagrees with a peer's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// An audit row already exists at this `(log_term, log_index)` carrying
    /// different content.
    ///
    /// The entry identity is normative, so the same identity must describe the
    /// same entry on every voter. Two different entries at one identity means
    /// this node's log and its applied state have diverged.
    AuditIdentityMismatch { log_term: u64, log_index: u64 },
}

/// The result of a committed client command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientResult {
    pub disposition: Disposition,
    pub reply: SavedReply,
}

impl ClientResult {
    /// Is this answer terminal (§10)?
    pub fn is_terminal(&self) -> bool {
        self.disposition.is_terminal(self.reply.reason)
    }

    /// Is this the one legal `(100 Rejected, 0 None)` pairing -- a committed
    /// request-shape violation?
    ///
    /// Every other rejection carries a defined, nonzero reason. Exposed so
    /// callers can assert the rule rather than restate it.
    pub fn is_forced_shape_rejection(&self) -> bool {
        self.reply.outcome == Outcome::Rejected && self.reply.reason == Reason::None
    }
}

/// Answer a request before proposing it (§10 stages 2-4).
///
/// Session resolution runs **first** and unconditionally: even a denial that
/// will be answered as a bare status must resolve against committed session
/// state, or a committed result is stranded behind a permanent authorization
/// failure and the sequence never terminates (§8.2.1).
pub fn preflight(state: &State, client: &ClientRef, action: &Action) -> Admission {
    let session = state.sessions.get(&(client.actor, client.client_id));
    let capacity = state.capacity_for(&client.actor);

    match classify(session, client.sequence, &client.request_hash, capacity) {
        // Stage 2. Every resolved row, terminal or not.
        Classification::Replay { saved, .. } => Admission::Resolved(Box::new(ClientResult {
            disposition: Disposition::Consumed,
            reply: saved,
        })),
        Classification::Refuse { reason, .. } => Admission::Resolved(Box::new(ClientResult {
            disposition: Disposition::NotConsumed,
            reply: rejection(action.validated_resource(), reason),
        })),
        Classification::Unresolved { .. } => project(action.decide(state, &client.actor)),
    }
}

/// Project an unresolved decision onto §10's stages 3 and 4.
fn project(decision: Decision) -> Admission {
    match decision {
        Decision::Apply(_) => Admission::Admit,
        Decision::RefuseShapeless(limit) => Admission::Status(StatusDenial::Shape(limit)),
        // Authorization has no §5.2 reason code pre-proposal (§10 stage 4),
        // even though apply saves it as `12 PermissionChanged`.
        Decision::Refuse(RefusalReason::PermissionChanged) => {
            Admission::Status(StatusDenial::NotAuthorized)
        }
        // Stage 3: exactly the four reason-coded admission denials.
        Decision::Refuse(reason) if reason.is_admission_denial() => Admission::Deny(reason),
        // Everything else is **admitted** precisely so it reaches the log and
        // commits a saved result. The clearest case is `6 AclRevisionMismatch`:
        // denied here it would be a non-terminal `NotConsumed`, and since a
        // client may never change the request at a sequence it has already
        // sent, it would retry the stale revision forever.
        Decision::Refuse(_) => Admission::Admit,
    }
}

/// Build a rejection reply.
///
/// `resource` is the **validated** identity or `None`; a raw action key never
/// reaches here. That applies to classifier refusals too -- they name a
/// sequence, not a resource, and copying an unvalidated key into one would put
/// it in the reserve-funded session table by a different door.
fn rejection(resource: Option<ResourceKey>, reason: Reason) -> SavedReply {
    SavedReply {
        outcome: Outcome::Rejected,
        reason,
        resource,
        epoch: 0,
        value_revision: 0,
        acl_revision: 0,
    }
}

/// Apply one committed client command.
///
/// `(log_term, log_index)` is the normative entry identity (§11.2). Reapplying
/// the same identity is idempotent; a different entry at an existing identity
/// is a [`Fault`].
///
/// Total by construction otherwise: every other input produces a result and
/// advances `last_applied`.
pub fn apply_client_command(
    state: &mut State,
    log_term: u64,
    log_index: u64,
    client: &ClientRef,
    action: &Action,
) -> Result<ClientResult, Fault> {
    let session = state.sessions.get(&(client.actor, client.client_id));
    let capacity = state.capacity_for(&client.actor);
    let classification = classify(session, client.sequence, &client.request_hash, capacity);

    let (result, writes_session) = match classification {
        // Case 2: a duplicate that was itself proposed and committed. It
        // re-applies nothing and returns the saved row **verbatim** --
        // reconstructing it field by field is how the spike lost a committed
        // `19`, which replayed as "rejected, no cause" -- but it is an applied
        // entry, so it still writes its audit row below.
        Classification::Replay { saved, .. } => (
            ClientResult {
                disposition: Disposition::Consumed,
                reply: saved,
            },
            false,
        ),

        // A classifier refusal writes no session row -- "creating nothing" in
        // the capacity case means no session or business row. It still writes
        // its audit row and advances `last_applied`.
        Classification::Refuse { reason, .. } => (
            ClientResult {
                disposition: Disposition::NotConsumed,
                reply: rejection(action.validated_resource(), reason),
            },
            false,
        ),

        Classification::Unresolved { .. } => (apply_decided(state, client, action), true),
    };

    // Case 3: reapplication. The audit row is the idempotence point, so this
    // must run before any counter moves -- and `apply_decided` above has
    // already run, which is safe only because a reapplied entry reaches an
    // identical decision from identical state. If it did not, the mismatch
    // below is exactly what catches it.
    let reapplied = write_audit(
        state,
        log_term,
        log_index,
        client,
        action,
        result.reply.outcome,
    )?;
    if reapplied {
        return Ok(result);
    }

    if writes_session {
        write_session(state, client, Some(result.reply.clone()));
    }
    // Never backwards: an out-of-order entry must not un-apply the log.
    state.last_applied = state.last_applied.max(log_index);
    Ok(result)
}

/// The unresolved path: decide, then write whatever the decision says.
fn apply_decided(state: &mut State, client: &ClientRef, action: &Action) -> ClientResult {
    match action.decide(state, &client.actor) {
        Decision::Apply(effect) => {
            let (ns, k) = key_parts(&effect.key);
            let old_charge = state
                .resources
                .get(&effect.key)
                .map(|row| limits::charge_resource(ns, k, row.value.len()));
            let new_charge = limits::charge_resource(ns, k, effect.row.value.len());
            state.growth_bytes = state.growth_bytes + new_charge - old_charge.unwrap_or(0);
            state
                .resources
                .insert(effect.key.clone(), effect.row.clone());

            ClientResult {
                disposition: Disposition::Consumed,
                reply: SavedReply {
                    outcome: effect.outcome,
                    reason: Reason::None,
                    resource: Some(effect.key),
                    epoch: effect.row.epoch,
                    value_revision: effect.row.value_revision,
                    acl_revision: effect.row.acl_revision,
                },
            }
        }

        // A reason-coded refusal that reached the log. Consumed, saved, and
        // terminal because it is saved -- even a `19` whose budget condition
        // later clears.
        Decision::Refuse(reason) => ClientResult {
            disposition: Disposition::Consumed,
            reply: rejection(action.validated_resource(), reason.into()),
        },

        // The forced-apply case (§10.1). Business state is untouched, and the
        // saved reply carries **no resource**: the identity may be exactly what
        // was malformed, and a shapeless key has no place in a bounded row.
        Decision::RefuseShapeless(_) => ClientResult {
            disposition: Disposition::Consumed,
            reply: rejection(action.validated_resource(), Reason::None),
        },
    }
}

/// Create or update the session row, maintaining the reserve counter.
fn write_session(state: &mut State, client: &ClientRef, result: Option<SavedReply>) {
    let key = (client.actor, client.client_id);
    let previous_charge = state.sessions.get(&key).map(session_charge);
    let row = Session {
        last_sequence: client.sequence,
        request_hash: client.request_hash,
        last_result: result,
        retired: state.sessions.get(&key).is_some_and(|s| s.retired),
    };
    state.reserve_bytes = state.reserve_bytes + session_charge(&row) - previous_charge.unwrap_or(0);
    state.sessions.insert(key, row);
}

fn session_charge(session: &Session) -> u64 {
    limits::charge_session(
        session
            .last_result
            .as_ref()
            .and_then(|r| r.resource.as_ref())
            .map(key_parts),
    )
}

/// Append the audit row, and report whether this entry had already been
/// applied.
///
/// Keyed by `(log_term, log_index)`, so a reapplied entry finds its own row
/// present, writes nothing, and is charged nothing. A row already present with
/// **different** content is a divergence signal rather than a duplicate.
fn write_audit(
    state: &mut State,
    log_term: u64,
    log_index: u64,
    client: &ClientRef,
    action: &Action,
    outcome: Outcome,
) -> Result<bool, Fault> {
    let resource = action.validated_resource();
    let row = AuditRow {
        actor: client.actor,
        operation_code: action.operation_code(),
        resource: resource.clone(),
        result: outcome,
    };

    match state.audit.get(&(log_term, log_index)) {
        Some(existing) if *existing == row => Ok(true),
        Some(_) => Err(Fault::AuditIdentityMismatch {
            log_term,
            log_index,
        }),
        None => {
            state.reserve_bytes += limits::charge_audit(resource.as_ref().map(key_parts));
            state.audit.insert((log_term, log_index), row);
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::model::limits::{ALL_RIGHTS, MAX_VALUE, RIGHT_ACQUIRE};
    use crate::coordination::model::resource::{Fence, RawResourceKey};
    use crate::coordination::model::state::ResourceRow;

    const ACTOR: NodeId = [1u8; 32];
    const CLIENT: ClientId = [9u8; 16];
    const HASH: [u8; 32] = [0xAA; 32];

    fn raw() -> RawResourceKey {
        RawResourceKey::new("ns", b"k".to_vec())
    }

    fn key() -> ResourceKey {
        raw().validate().expect("a valid fixture identity")
    }

    fn granted() -> State {
        let mut s = State::default();
        s.acl.insert((key(), ACTOR), ALL_RIGHTS);
        s.growth_bytes = s.recompute_growth();
        s.reserve_bytes = s.recompute_reserve();
        s
    }

    fn client(sequence: u64) -> ClientRef {
        ClientRef {
            actor: ACTOR,
            client_id: CLIENT,
            sequence,
            request_hash: HASH,
        }
    }

    fn acquire() -> Action {
        Action::Resource(ResourceAction::Acquire {
            resource: raw(),
            ttl_ms: 30_000,
        })
    }

    fn oversized_value() -> Action {
        Action::Resource(ResourceAction::CompareAndSet {
            fence: Fence {
                resource: raw(),
                epoch: 1,
                holder: ACTOR,
            },
            expected_value_revision: 0,
            new_value: vec![0u8; MAX_VALUE + 1],
        })
    }

    /// An action whose *identity* is malformed, so no validated key exists.
    fn malformed_identity() -> Action {
        Action::Resource(ResourceAction::Acquire {
            resource: RawResourceKey::new("NOT a namespace", vec![0u8; 2000]),
            ttl_ms: 30_000,
        })
    }

    fn held_state() -> State {
        let mut s = granted();
        s.resources.insert(
            key(),
            ResourceRow {
                epoch: 1,
                held: true,
                holder: Some(ACTOR),
                ttl_ms: 30_000,
                lease_revision: 1,
                ..Default::default()
            },
        );
        s.growth_bytes = s.recompute_growth();
        s
    }

    fn apply_ok(
        state: &mut State,
        term: u64,
        index: u64,
        client: &ClientRef,
        action: &Action,
    ) -> ClientResult {
        apply_client_command(state, term, index, client, action).expect("no fault")
    }

    // ── Stage 4: a conforming leader denies pre-proposal, status-only ───────

    /// Every request-shape limit a **resource action** can violate is denied
    /// pre-proposal as a status, never a typed reply -- there is no reason code
    /// to put in one.
    ///
    /// Scoped to resource actions deliberately: `EncodedCommand` is enforced at
    /// the ingest boundary where an encoded length exists, and the member-seed
    /// and membership-plan limits belong to `change_membership`, which is not
    /// modelled yet. Naming the scope keeps the test from implying it covers
    /// all nine.
    #[test]
    fn preflight_denies_every_resource_action_shape_limit_as_a_status_only() {
        let s = held_state();
        let cases: Vec<(ShapeLimit, Action)> = vec![
            (
                ShapeLimit::Namespace,
                Action::Resource(ResourceAction::Acquire {
                    resource: RawResourceKey::new("NOPE", b"k".to_vec()),
                    ttl_ms: 30_000,
                }),
            ),
            (
                ShapeLimit::ResourceKey,
                Action::Resource(ResourceAction::Acquire {
                    resource: RawResourceKey::new("ns", Vec::new()),
                    ttl_ms: 30_000,
                }),
            ),
            (ShapeLimit::Value, oversized_value()),
            (
                ShapeLimit::Ttl,
                Action::Resource(ResourceAction::Acquire {
                    resource: raw(),
                    ttl_ms: 1,
                }),
            ),
        ];

        for (limit, action) in cases {
            assert_eq!(
                preflight(&s, &client(1), &action),
                Admission::Status(StatusDenial::Shape(limit)),
                "{limit:?} must be a stage-4 status, not a typed reply"
            );
        }
    }

    #[test]
    fn a_stage_four_denial_writes_nothing() {
        let s = held_state();
        let before = s.clone();
        assert!(matches!(
            preflight(&s, &client(1), &oversized_value()),
            Admission::Status(_)
        ));
        assert_eq!(s.sessions.len(), before.sessions.len());
        assert_eq!(s.audit.len(), before.audit.len());
        assert_eq!(s.resources, before.resources);
    }

    #[test]
    fn an_unauthorized_request_is_a_stage_four_status() {
        let mut s = State::default();
        s.acl.insert((key(), ACTOR), ALL_RIGHTS & !RIGHT_ACQUIRE);
        assert_eq!(
            preflight(&s, &client(1), &acquire()),
            Admission::Status(StatusDenial::NotAuthorized)
        );
    }

    // ── Forced apply (§10.1) ────────────────────────────────────────────────

    #[test]
    fn a_committed_shape_violation_commits_rejected_with_no_reason() {
        let mut s = held_state();
        let result = apply_ok(&mut s, 1, 7, &client(1), &oversized_value());

        assert_eq!(result.disposition, Disposition::Consumed);
        assert_eq!(result.reply.outcome, Outcome::Rejected);
        assert_eq!(result.reply.reason, Reason::None);
        assert!(result.is_forced_shape_rejection());
        assert!(result.is_terminal(), "Consumed is always terminal");
    }

    #[test]
    fn forced_apply_mutates_no_business_state_but_advances_its_metadata() {
        let mut s = held_state();
        let business_before = (
            s.resources.clone(),
            s.acl.clone(),
            s.admins.clone(),
            s.intents.clone(),
            s.routes.clone(),
            s.growth_bytes,
        );

        apply_ok(&mut s, 1, 7, &client(1), &oversized_value());

        assert_eq!(
            (
                s.resources.clone(),
                s.acl.clone(),
                s.admins.clone(),
                s.intents.clone(),
                s.routes.clone(),
                s.growth_bytes,
            ),
            business_before,
            "no business row and no growth byte may move"
        );

        let session = s.sessions.get(&(ACTOR, CLIENT)).expect("session row");
        assert_eq!(session.last_sequence, 1);
        assert_eq!(session.request_hash, HASH);
        assert!(session.last_result.is_some(), "the rejection is saved");
        assert_eq!(s.audit.len(), 1, "an audit row is written");
        assert_eq!(s.last_applied, 7);
        assert!(s.counters_match_recomputation());
    }

    /// A malformed **identity** saves no resource at all, in the reply and in
    /// the audit row alike. Copying the raw key would put an unvalidated -- and
    /// possibly oversized -- identity into reserve-funded metadata, blowing the
    /// 832-byte charge the budget proof rests on.
    #[test]
    fn a_malformed_identity_is_never_copied_into_durable_metadata() {
        let mut s = granted();
        let result = apply_ok(&mut s, 1, 4, &client(1), &malformed_identity());

        assert!(result.is_forced_shape_rejection());
        assert_eq!(
            result.reply.resource, None,
            "a shapeless identity has no place in a saved reply"
        );

        let session = s.sessions.get(&(ACTOR, CLIENT)).expect("session row");
        assert_eq!(session.last_result.as_ref().unwrap().resource, None);

        let audit = s.audit.values().next().expect("audit row");
        assert_eq!(audit.resource, None, "nor in an audit row");

        // The charge is the bare row overhead, not the 2 KiB the request named.
        assert!(s.counters_match_recomputation());
        assert_eq!(s.reserve_bytes, 256 + 256, "one session row, one audit row");
    }

    /// A classifier refusal must not copy an unvalidated key either.
    #[test]
    fn a_classifier_refusal_carries_no_unvalidated_identity() {
        let mut s = granted();
        // Sequence 5 with no session: a gap, refused by the classifier before
        // any decision runs.
        let result = apply_ok(&mut s, 1, 4, &client(5), &malformed_identity());
        assert_eq!(result.reply.reason, Reason::SequenceGap);
        assert_eq!(result.reply.resource, None);
        assert_eq!(s.audit.values().next().unwrap().resource, None);
    }

    // ── The three replay cases ──────────────────────────────────────────────

    /// Case 1: a preflight replay commits nothing, so there is no audit row.
    #[test]
    fn a_preflight_replay_writes_no_audit_row() {
        let mut s = held_state();
        let first = apply_ok(&mut s, 1, 7, &client(1), &oversized_value());
        let audit_before = s.audit.len();

        match preflight(&s, &client(1), &oversized_value()) {
            Admission::Resolved(replayed) => assert_eq!(*replayed, first),
            other => panic!("a consumed sequence must resolve, got {other:?}"),
        }
        assert_eq!(s.audit.len(), audit_before, "preflight commits nothing");
        assert_eq!(s.last_applied, 7);
    }

    /// Case 2: a duplicate that was itself proposed and committed. No business
    /// or session effects -- but it **is** an applied entry, so it writes an
    /// audit row and advances `last_applied`.
    ///
    /// This needs no faulty leader: two copies of one request can preflight
    /// before either applies, so both reach the log.
    #[test]
    fn a_committed_duplicate_writes_its_own_audit_row_and_advances_last_applied() {
        let mut s = granted();
        let first = apply_ok(&mut s, 1, 7, &client(1), &acquire());
        let session_after_first = s.sessions.get(&(ACTOR, CLIENT)).cloned();
        let row_after_first = s.resources.get(&key()).cloned();

        // The duplicate lands at its own log position.
        let second = apply_ok(&mut s, 1, 8, &client(1), &acquire());

        assert_eq!(second, first, "the saved result is returned verbatim");
        assert_eq!(
            s.resources.get(&key()).cloned(),
            row_after_first,
            "no business effect"
        );
        assert_eq!(
            s.sessions.get(&(ACTOR, CLIENT)).cloned(),
            session_after_first,
            "no session effect"
        );
        assert_eq!(s.audit.len(), 2, "an applied entry always leaves a trace");
        assert_eq!(s.last_applied, 8);
        assert!(s.counters_match_recomputation());
    }

    /// Case 3: reapplying the **same** entry identity is idempotent -- no
    /// second audit row, and no second charge.
    #[test]
    fn reapplying_the_same_entry_identity_charges_nothing_twice() {
        let mut s = granted();
        let first = apply_ok(&mut s, 2, 7, &client(1), &acquire());
        let after = (
            s.audit.len(),
            s.reserve_bytes,
            s.growth_bytes,
            s.last_applied,
        );

        let again = apply_ok(&mut s, 2, 7, &client(1), &acquire());

        assert_eq!(again, first);
        assert_eq!(
            (
                s.audit.len(),
                s.reserve_bytes,
                s.growth_bytes,
                s.last_applied
            ),
            after,
            "reapplication must move no counter"
        );
        assert!(s.counters_match_recomputation());
    }

    /// The same index under a **different term** is a different entry, so it is
    /// not a reapplication.
    #[test]
    fn the_same_index_in_a_different_term_is_a_different_entry() {
        let mut s = granted();
        apply_ok(&mut s, 1, 7, &client(1), &acquire());
        apply_ok(&mut s, 2, 7, &client(1), &acquire());
        assert_eq!(s.audit.len(), 2, "(term, index) is the entry identity");
    }

    /// Different content at an existing identity means this node's log and its
    /// applied state disagree. That is a divergence signal, not a duplicate.
    #[test]
    fn different_content_at_an_existing_entry_identity_is_a_fault() {
        let mut s = granted();
        apply_ok(&mut s, 1, 7, &client(1), &acquire());

        let other_actor = ClientRef {
            actor: [7u8; 32],
            ..client(1)
        };
        assert_eq!(
            apply_client_command(&mut s, 1, 7, &other_actor, &acquire()),
            Err(Fault::AuditIdentityMismatch {
                log_term: 1,
                log_index: 7
            })
        );
    }

    /// `last_applied` never goes backwards.
    #[test]
    fn last_applied_never_regresses() {
        let mut s = granted();
        apply_ok(&mut s, 1, 9, &client(1), &acquire());
        assert_eq!(s.last_applied, 9);
        apply_ok(&mut s, 1, 3, &client(2), &acquire());
        assert_eq!(
            s.last_applied, 9,
            "an earlier index must not un-apply the log"
        );
    }

    #[test]
    fn a_forced_rejection_replays_exactly_as_a_typed_reply() {
        let mut s = held_state();
        let first = apply_ok(&mut s, 1, 7, &client(1), &oversized_value());

        match preflight(&s, &client(1), &oversized_value()) {
            Admission::Resolved(replayed) => assert_eq!(*replayed, first),
            other => panic!("a consumed sequence must resolve, got {other:?}"),
        }

        let second = apply_ok(&mut s, 1, 8, &client(1), &oversized_value());
        assert_eq!(second, first, "byte-identical, and never an RpcStatus");
    }

    // ── (100, 0) uniqueness ─────────────────────────────────────────────────

    #[test]
    fn only_a_shape_violation_rejects_with_no_reason() {
        let cases: Vec<(&str, State, Action)> = vec![
            (
                "authorization",
                {
                    let mut s = State::default();
                    s.acl.insert((key(), ACTOR), ALL_RIGHTS & !RIGHT_ACQUIRE);
                    s
                },
                acquire(),
            ),
            (
                "already held",
                {
                    let mut s = granted();
                    s.resources.insert(
                        key(),
                        ResourceRow {
                            epoch: 1,
                            held: true,
                            holder: Some([2u8; 32]),
                            ..Default::default()
                        },
                    );
                    s
                },
                acquire(),
            ),
            (
                "value revision",
                held_state(),
                Action::Resource(ResourceAction::CompareAndSet {
                    fence: Fence {
                        resource: raw(),
                        epoch: 1,
                        holder: ACTOR,
                    },
                    expected_value_revision: 99,
                    new_value: b"v".to_vec(),
                }),
            ),
        ];

        for (name, mut s, action) in cases {
            let result = apply_ok(&mut s, 1, 1, &client(1), &action);
            assert_eq!(result.reply.outcome, Outcome::Rejected, "{name}");
            assert_ne!(
                result.reply.reason,
                Reason::None,
                "{name} must retain its defined coordination reason"
            );
            assert!(!result.is_forced_shape_rejection(), "{name}");
        }
    }

    #[test]
    fn a_successful_outcome_carries_no_reason() {
        let mut s = granted();
        let result = apply_ok(&mut s, 1, 1, &client(1), &acquire());
        assert_eq!(result.reply.outcome, Outcome::Granted);
        assert_eq!(result.reply.reason, Reason::None);
        assert!(!result.is_forced_shape_rejection());
    }

    // ── Projection ──────────────────────────────────────────────────────────

    #[test]
    fn only_the_four_admission_reasons_are_denied_before_proposal() {
        for refusal in RefusalReason::ALL {
            if *refusal == RefusalReason::PermissionChanged {
                continue;
            }
            let projected = project(Decision::Refuse(*refusal));
            if refusal.is_admission_denial() {
                assert_eq!(
                    projected,
                    Admission::Deny(*refusal),
                    "{refusal:?} is a stage-3 admission denial"
                );
            } else {
                assert_eq!(
                    projected,
                    Admission::Admit,
                    "{refusal:?} must be proposed so it can commit a saved result"
                );
            }
        }
    }

    #[test]
    fn a_stale_revision_is_admitted_rather_than_denied() {
        assert_eq!(
            project(Decision::Refuse(RefusalReason::AclRevisionMismatch)),
            Admission::Admit
        );
    }

    #[test]
    fn a_committed_result_is_returned_even_after_the_grant_is_revoked() {
        let mut s = granted();
        let first = apply_ok(&mut s, 1, 1, &client(1), &acquire());
        assert_eq!(first.reply.outcome, Outcome::Granted);

        s.acl.remove(&(key(), ACTOR));

        match preflight(&s, &client(1), &acquire()) {
            Admission::Resolved(replayed) => assert_eq!(*replayed, first),
            other => panic!("the committed result must still be reachable, got {other:?}"),
        }
    }

    // ── Ordinary apply ──────────────────────────────────────────────────────

    #[test]
    fn a_successful_acquire_writes_the_row_and_keeps_the_counters_honest() {
        let mut s = granted();
        let result = apply_ok(&mut s, 1, 3, &client(1), &acquire());

        assert_eq!(result.disposition, Disposition::Consumed);
        assert_eq!(result.reply.outcome, Outcome::Granted);
        assert_eq!(result.reply.epoch, 1);

        let row = s.resources.get(&key()).expect("the row was created");
        assert!(row.held);
        assert_eq!(row.holder, Some(ACTOR));
        assert_eq!(s.last_applied, 3);
        assert!(s.counters_match_recomputation());
    }

    #[test]
    fn a_classifier_refusal_writes_audit_but_no_session_row() {
        let mut s = granted();
        let result = apply_ok(&mut s, 1, 4, &client(5), &acquire());

        assert_eq!(result.disposition, Disposition::NotConsumed);
        assert_eq!(result.reply.reason, Reason::SequenceGap);
        assert!(s.sessions.is_empty(), "no session row is created");
        assert_eq!(s.audit.len(), 1, "the applied entry still leaves a trace");
        assert_eq!(s.last_applied, 4);
        assert!(s.counters_match_recomputation());
    }

    #[test]
    fn counters_equal_recomputation_after_a_mixed_history() {
        let mut s = granted();
        let mut index = 0u64;
        let mut sequence = 1u64;

        for step in 0..40u64 {
            index += 1;
            let epoch = s.resources.get(&key()).map_or(0, |r| r.epoch);
            let value_revision = s.resources.get(&key()).map_or(0, |r| r.value_revision);
            let action = match step % 4 {
                0 => acquire(),
                1 => Action::Resource(ResourceAction::CompareAndSet {
                    fence: Fence {
                        resource: raw(),
                        epoch,
                        holder: ACTOR,
                    },
                    expected_value_revision: value_revision,
                    new_value: vec![step as u8; (step % 7) as usize],
                }),
                2 => Action::Resource(ResourceAction::Renew {
                    fence: Fence {
                        resource: raw(),
                        epoch,
                        holder: ACTOR,
                    },
                }),
                _ => Action::Resource(ResourceAction::Release {
                    fence: Fence {
                        resource: raw(),
                        epoch,
                        holder: ACTOR,
                    },
                }),
            };
            let mut c = client(sequence);
            c.request_hash = [step as u8; 32];
            let result = apply_ok(&mut s, 1, index, &c, &action);
            if result.disposition == Disposition::Consumed {
                sequence += 1;
            }
            assert!(
                s.counters_match_recomputation(),
                "counters drifted at step {step}"
            );
        }
    }
}
