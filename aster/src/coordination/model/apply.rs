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

use crate::coordination::reason::{Disposition, Outcome, Reason};

use super::classify::{classify, Classification};
use super::limits;
use super::resource::{self, Decision, ResourceAction};
use super::shape::ShapeLimit;
use super::state::{AuditRow, ClientId, NodeId, ResourceKey, SavedReply, Session, State};

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

    fn resource(&self) -> &ResourceKey {
        match self {
            Action::Resource(r) => r.resource(),
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
    Deny(Reason),
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
            reply: rejection(action.resource(), reason),
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
        Decision::Refuse(Reason::PermissionChanged) => {
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

fn rejection(resource: &ResourceKey, reason: Reason) -> SavedReply {
    SavedReply {
        outcome: Outcome::Rejected,
        reason,
        resource: Some(resource.clone()),
        epoch: 0,
        value_revision: 0,
        acl_revision: 0,
    }
}

/// Apply one committed client command.
///
/// Total by construction: every input produces a result and advances
/// `last_applied`. `log_index` and `ordinal` key the audit row so pruning the
/// oldest is deterministic.
pub fn apply_client_command(
    state: &mut State,
    log_index: u64,
    ordinal: u64,
    client: &ClientRef,
    action: &Action,
) -> ClientResult {
    let session = state.sessions.get(&(client.actor, client.client_id));
    let capacity = state.capacity_for(&client.actor);
    let classification = classify(session, client.sequence, &client.request_hash, capacity);

    let (result, writes_session) = match classification {
        // A replay re-applies nothing and returns the saved row verbatim.
        // Reconstructing it field by field is how the spike lost a committed
        // `19`, which replayed as "rejected, no cause".
        Classification::Replay { saved, .. } => {
            return ClientResult {
                disposition: Disposition::Consumed,
                reply: saved,
            }
        }

        // A classifier refusal writes no session row -- "creating nothing" in
        // the capacity case means no session or business row. It still writes
        // its audit row and advances `last_applied`, because a committed entry
        // may never be silently skipped.
        Classification::Refuse { reason, .. } => (
            ClientResult {
                disposition: Disposition::NotConsumed,
                reply: rejection(action.resource(), reason),
            },
            false,
        ),

        Classification::Unresolved { .. } => (apply_decided(state, client, action), true),
    };

    if writes_session {
        write_session(state, client, Some(result.reply.clone()));
    }
    write_audit(
        state,
        log_index,
        ordinal,
        client,
        action,
        result.reply.outcome,
    );
    state.last_applied = log_index;
    result
}

/// The unresolved path: decide, then write whatever the decision says.
fn apply_decided(state: &mut State, client: &ClientRef, action: &Action) -> ClientResult {
    match action.decide(state, &client.actor) {
        Decision::Apply(effect) => {
            let key = action.resource().clone();
            let old_charge = state
                .resources
                .get(&key)
                .map(|row| limits::charge_resource(&key.namespace, &key.key, row.value.len()));
            let new_charge =
                limits::charge_resource(&key.namespace, &key.key, effect.row.value.len());
            state.growth_bytes = state.growth_bytes + new_charge - old_charge.unwrap_or(0);
            state.resources.insert(key.clone(), effect.row.clone());

            ClientResult {
                disposition: Disposition::Consumed,
                reply: SavedReply {
                    outcome: effect.outcome,
                    reason: Reason::None,
                    resource: Some(key),
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
            reply: rejection(action.resource(), reason),
        },

        // The forced-apply case. Business state is untouched; the metadata
        // below is what consumes the sequence.
        Decision::RefuseShapeless(_) => ClientResult {
            disposition: Disposition::Consumed,
            reply: rejection(action.resource(), Reason::None),
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
            .map(ResourceKey::as_charge_parts),
    )
}

/// Append the audit row. Every applied entry writes one, whether it succeeded
/// or was rejected.
fn write_audit(
    state: &mut State,
    log_index: u64,
    ordinal: u64,
    client: &ClientRef,
    action: &Action,
    outcome: Outcome,
) {
    let resource = Some(action.resource().clone());
    let row = AuditRow {
        actor: client.actor,
        operation_code: action.operation_code(),
        resource: resource.clone(),
        result: outcome,
    };
    state.reserve_bytes +=
        limits::charge_audit(resource.as_ref().map(ResourceKey::as_charge_parts));
    state.audit.insert((log_index, ordinal), row);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::model::limits::{ALL_RIGHTS, MAX_VALUE, RIGHT_ACQUIRE};
    use crate::coordination::model::resource::Fence;
    use crate::coordination::model::state::ResourceRow;

    const ACTOR: NodeId = [1u8; 32];
    const CLIENT: ClientId = [9u8; 16];
    const HASH: [u8; 32] = [0xAA; 32];

    fn key() -> ResourceKey {
        ResourceKey::new("ns", b"k".to_vec())
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
            resource: key(),
            ttl_ms: 30_000,
        })
    }

    /// An acquire whose value is irrelevant but whose namespace is over-long.
    fn oversized_value() -> Action {
        Action::Resource(ResourceAction::CompareAndSet {
            fence: Fence {
                resource: key(),
                epoch: 1,
                holder: ACTOR,
            },
            expected_value_revision: 0,
            new_value: vec![0u8; MAX_VALUE + 1],
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

    // ── Stage 4: a conforming leader denies pre-proposal, status-only ───────

    /// Every request-shape limit a resource action can violate is denied
    /// pre-proposal as a status, never as a typed reply -- there is no reason
    /// code to put in one.
    #[test]
    fn preflight_denies_every_shape_limit_as_a_status_only() {
        let s = held_state();
        let cases: Vec<(ShapeLimit, Action)> = vec![
            (
                ShapeLimit::Namespace,
                Action::Resource(ResourceAction::Acquire {
                    resource: ResourceKey::new(
                        "n".repeat(limits::MAX_NAMESPACE + 1),
                        b"k".to_vec(),
                    ),
                    ttl_ms: 30_000,
                }),
            ),
            (
                ShapeLimit::ResourceKey,
                Action::Resource(ResourceAction::Acquire {
                    resource: ResourceKey::new("ns", vec![0u8; limits::MAX_KEY + 1]),
                    ttl_ms: 30_000,
                }),
            ),
            (ShapeLimit::Value, oversized_value()),
            (
                ShapeLimit::Ttl,
                Action::Resource(ResourceAction::Acquire {
                    resource: key(),
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

    /// A conforming leader writes nothing when it denies at stage 4.
    #[test]
    fn a_stage_four_denial_writes_nothing() {
        let s = held_state();
        let before = s.clone();
        let answer = preflight(&s, &client(1), &oversized_value());
        assert!(matches!(answer, Admission::Status(_)));
        assert_eq!(s.sessions.len(), before.sessions.len());
        assert_eq!(s.audit.len(), before.audit.len());
        assert_eq!(s.resources, before.resources);
    }

    /// Authorization is a stage-4 status pre-proposal, though apply saves it as
    /// `12 PermissionChanged`.
    #[test]
    fn an_unauthorized_request_is_a_stage_four_status() {
        let mut s = State::default();
        s.acl.insert((key(), ACTOR), ALL_RIGHTS & !RIGHT_ACQUIRE);
        assert_eq!(
            preflight(&s, &client(1), &acquire()),
            Admission::Status(StatusDenial::NotAuthorized)
        );
    }

    // ── Forced apply ────────────────────────────────────────────────────────

    /// A non-conforming leader proposes a shape violation. Apply is total: it
    /// commits `Consumed + 100 + 0` rather than skipping the entry or halting.
    #[test]
    fn a_committed_shape_violation_commits_rejected_with_no_reason() {
        let mut s = held_state();
        let result = apply_client_command(&mut s, 7, 0, &client(1), &oversized_value());

        assert_eq!(result.disposition, Disposition::Consumed);
        assert_eq!(result.reply.outcome, Outcome::Rejected);
        assert_eq!(result.reply.reason, Reason::None);
        assert!(result.is_forced_shape_rejection());
        assert!(result.is_terminal(), "Consumed is always terminal");
    }

    /// Forced apply mutates no business state -- but persists its metadata
    /// atomically, which is what consumes the sequence.
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

        apply_client_command(&mut s, 7, 0, &client(1), &oversized_value());

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

        // And the metadata that consumes the sequence.
        let session = s.sessions.get(&(ACTOR, CLIENT)).expect("session row");
        assert_eq!(session.last_sequence, 1);
        assert_eq!(session.request_hash, HASH);
        assert!(session.last_result.is_some(), "the rejection is saved");
        assert_eq!(s.audit.len(), 1, "an audit row is written");
        assert_eq!(s.last_applied, 7);
        assert!(
            s.counters_match_recomputation(),
            "the reserve counter must track the rows it funded"
        );
    }

    /// The saved rejection replays exactly, as a typed reply -- never as an
    /// `RpcStatus`. The client that lost its response learns the same thing the
    /// first attempt told it.
    #[test]
    fn a_forced_rejection_replays_exactly_as_a_typed_reply() {
        let mut s = held_state();
        let first = apply_client_command(&mut s, 7, 0, &client(1), &oversized_value());

        // Preflight now resolves it from the session table.
        match preflight(&s, &client(1), &oversized_value()) {
            Admission::Resolved(replayed) => {
                assert_eq!(*replayed, first, "the replay must be byte-identical");
            }
            other => panic!("a consumed sequence must resolve, got {other:?}"),
        }

        // And so does a second apply, without re-applying anything.
        let audit_before = s.audit.len();
        let second = apply_client_command(&mut s, 8, 0, &client(1), &oversized_value());
        assert_eq!(second, first);
        assert_eq!(
            s.audit.len(),
            audit_before,
            "a replay re-applies nothing at all"
        );
    }

    // ── (100, 0) uniqueness ─────────────────────────────────────────────────

    /// `(Rejected, None)` is legal **only** for a committed shape violation.
    /// Every other rejection carries a defined, nonzero coordination reason.
    #[test]
    fn only_a_shape_violation_rejects_with_no_reason() {
        // A representative rejection from each reason-carrying path.
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
                        resource: key(),
                        epoch: 1,
                        holder: ACTOR,
                    },
                    expected_value_revision: 99,
                    new_value: b"v".to_vec(),
                }),
            ),
        ];

        for (name, mut s, action) in cases {
            let result = apply_client_command(&mut s, 1, 0, &client(1), &action);
            assert_eq!(result.reply.outcome, Outcome::Rejected, "{name}");
            assert_ne!(
                result.reply.reason,
                Reason::None,
                "{name} must retain its defined coordination reason"
            );
            assert!(!result.is_forced_shape_rejection(), "{name}");
        }
    }

    /// A successful outcome never carries a reason, and a rejection is the only
    /// outcome that may carry one.
    #[test]
    fn a_successful_outcome_carries_no_reason() {
        let mut s = granted();
        let result = apply_client_command(&mut s, 1, 0, &client(1), &acquire());
        assert_eq!(result.reply.outcome, Outcome::Granted);
        assert_eq!(result.reply.reason, Reason::None);
        assert!(
            !result.is_forced_shape_rejection(),
            "success is not a rejection, whatever its reason field"
        );
    }

    // ── Projection ──────────────────────────────────────────────────────────

    /// Exactly the four §10 admission reasons are denied pre-proposal; every
    /// other reason-coded refusal is admitted so it reaches the log.
    #[test]
    fn only_the_four_admission_reasons_are_denied_before_proposal() {
        for reason in Reason::ALL {
            if reason.is_reserved() || *reason == Reason::PermissionChanged {
                continue;
            }
            let projected = project(Decision::Refuse(*reason));
            if reason.is_admission_denial() {
                assert_eq!(
                    projected,
                    Admission::Deny(*reason),
                    "{reason:?} is a stage-3 admission denial"
                );
            } else {
                assert_eq!(
                    projected,
                    Admission::Admit,
                    "{reason:?} must be proposed so it can commit a saved result"
                );
            }
        }
    }

    /// The case the spike broke twice: a stale revision must be **admitted**,
    /// not denied, or a client that may not change its request retries forever.
    #[test]
    fn a_stale_revision_is_admitted_rather_than_denied() {
        assert_eq!(
            project(Decision::Refuse(Reason::AclRevisionMismatch)),
            Admission::Admit
        );
    }

    /// Session resolution runs before every denial, so a committed result is
    /// never stranded behind a permanent authorization failure.
    #[test]
    fn a_committed_result_is_returned_even_after_the_grant_is_revoked() {
        let mut s = granted();
        let first = apply_client_command(&mut s, 1, 0, &client(1), &acquire());
        assert_eq!(first.reply.outcome, Outcome::Granted);

        // The actor's grant is revoked, then it retries the lost response.
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
        let result = apply_client_command(&mut s, 3, 0, &client(1), &acquire());

        assert_eq!(result.disposition, Disposition::Consumed);
        assert_eq!(result.reply.outcome, Outcome::Granted);
        assert_eq!(result.reply.epoch, 1);

        let row = s.resources.get(&key()).expect("the row was created");
        assert!(row.held);
        assert_eq!(row.holder, Some(ACTOR));
        assert_eq!(s.last_applied, 3);
        assert!(s.counters_match_recomputation());
    }

    /// A classifier refusal writes no session row, but still writes its audit
    /// row and advances `last_applied` -- a committed entry may never be
    /// silently skipped.
    #[test]
    fn a_classifier_refusal_writes_audit_but_no_session_row() {
        let mut s = granted();
        // Sequence 5 with no session: row 13, a gap.
        let result = apply_client_command(&mut s, 4, 0, &client(5), &acquire());

        assert_eq!(result.disposition, Disposition::NotConsumed);
        assert_eq!(result.reply.reason, Reason::SequenceGap);
        assert!(s.sessions.is_empty(), "no session row is created");
        assert_eq!(s.audit.len(), 1, "the applied entry still leaves a trace");
        assert_eq!(s.last_applied, 4);
        assert!(s.counters_match_recomputation());
    }

    /// Counters stay honest across an arbitrary history -- the property snapshot
    /// install and log replay depend on.
    #[test]
    fn counters_equal_recomputation_after_a_mixed_history() {
        let mut s = granted();
        let mut index = 0u64;
        let mut sequence = 1u64;

        for step in 0..40u64 {
            index += 1;
            let action = match step % 4 {
                0 => acquire(),
                1 => Action::Resource(ResourceAction::CompareAndSet {
                    fence: Fence {
                        resource: key(),
                        epoch: s.resources.get(&key()).map_or(0, |r| r.epoch),
                        holder: ACTOR,
                    },
                    expected_value_revision: s
                        .resources
                        .get(&key())
                        .map_or(0, |r| r.value_revision),
                    new_value: vec![step as u8; (step % 7) as usize],
                }),
                2 => Action::Resource(ResourceAction::Renew {
                    fence: Fence {
                        resource: key(),
                        epoch: s.resources.get(&key()).map_or(0, |r| r.epoch),
                        holder: ACTOR,
                    },
                }),
                _ => Action::Resource(ResourceAction::Release {
                    fence: Fence {
                        resource: key(),
                        epoch: s.resources.get(&key()).map_or(0, |r| r.epoch),
                        holder: ACTOR,
                    },
                }),
            };
            let mut c = client(sequence);
            c.request_hash = [step as u8; 32];
            let result = apply_client_command(&mut s, index, 0, &c, &action);
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
