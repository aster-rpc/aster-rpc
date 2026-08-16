//! `retire_client` (§10) -- **sessionless**.
//!
//! No client id of its own, no sequence, no disposition. It is authorized by
//! transport identity plus dedicated retirement logic, not by the §8.2.1
//! sequence classifier -- which exists to resolve a sequence, and has nothing
//! to resolve here.
//!
//! Being sessionless does not make it un-replicated: it creates or converts a
//! durable row, so it is a committed state-machine command like any other.
//!
//! ## Why it is sessionless
//!
//! An earlier revision made it a sequenced mutation, which then had to forbid a
//! client retiring the session it was calling from -- and that restriction
//! created a capacity bootstrap problem: a client with no usable session could
//! not issue the command that would free one. Sessionless, a client may retire
//! its own last working id, and an actor with no usable session at all can still
//! call it.
//!
//! ## Idempotent, not exactly-once
//!
//! Raft ordering makes the state transition idempotent and orders a delayed
//! target command strictly before or after retirement. But retries may
//! legitimately produce additional audit rows, because there is no sequence to
//! deduplicate on -- only `(target_actor, target_client_id)`.

use super::limits::MAX_RETAINED_SESSIONS_PER_ACTOR;
use super::state::{Capacity, ClientId, NodeId, Session, State};
use crate::coordination::reason::{Outcome, RefusalReason};

/// A `retire_client` command (§10). Deliberately not an `Action`: the action
/// union is defined around a mutation prefix this command does not have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetireCommand {
    /// The caller, from immutable transport identity.
    pub actor: NodeId,
    /// The actor whose session is retired.
    pub target_actor: NodeId,
    pub target_client_id: ClientId,
}

/// What a `retire_client` writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetireEffect {
    /// The session exists and becomes a tombstone, **retaining** its last
    /// sequence, hash and saved result.
    ///
    /// Retention is what lets a retry of a sequence that already committed
    /// still get its result. Replacing it with a terminal marker would make the
    /// journal unresolvable.
    Convert,
    /// No session exists, so a tombstone is created with `last_sequence = 0`
    /// and no saved result.
    ///
    /// Deliberately not a no-op: a caller retiring an id it believes is gone
    /// must be able to make that true going forward, and a no-op would leave
    /// sequence 1 available to a straggler. `last_sequence = 0` with no saved
    /// result means every sequence, including 1, meets the `ClientRetired` path
    /// -- and `0` is refused outright by §8.2.1 row 1, so the tombstone cannot
    /// be matched against itself.
    CreateTombstone,
    /// Already retired: no change, and still `9 ClientRetired`.
    AlreadyRetired,
}

/// The outcome of deciding one `retire_client`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Apply(RetireEffect),
    Refuse(RefusalReason),
}

impl Decision {
    /// A sessionless command has no disposition, so terminality is a property
    /// of the reason alone.
    pub fn outcome(&self) -> Outcome {
        match self {
            Decision::Apply(_) => Outcome::ClientRetired,
            Decision::Refuse(_) => Outcome::Rejected,
        }
    }
}

/// Decide one `retire_client` against committed state.
pub fn decide(state: &State, command: &RetireCommand) -> Decision {
    // §10: an actor may retire only sessions whose `target_node_id` equals its
    // own authenticated NodeId; a group administrator may retire any session.
    let authorized = command.actor == command.target_actor || state.admins.contains(&command.actor);
    if !authorized {
        return Decision::Refuse(RefusalReason::PermissionChanged);
    }

    match state
        .sessions
        .get(&(command.target_actor, command.target_client_id))
    {
        Some(session) if session.retired => Decision::Apply(RetireEffect::AlreadyRetired),
        // Converting an existing row is always legal: it adds no row, so no cap
        // can refuse it. That is what keeps retirement available as the
        // recovery step §10 prescribes.
        Some(_) => Decision::Apply(RetireEffect::Convert),
        None => {
            // Creating a tombstone consumes retained capacity, and at the cap
            // the answer is the terminal `18` with **no row created**. This is
            // the case that makes reason 18's recovery "surface and stop"
            // rather than "retire and rotate": the tombstone needs the same row
            // the refusal is about, so retirement cannot rescue it either.
            let capacity = state.capacity_for(&command.target_actor);
            if retained_exhausted_for_new_row(&capacity) {
                return Decision::Refuse(RefusalReason::RetainedSessionCapReached);
            }
            Decision::Apply(RetireEffect::CreateTombstone)
        }
    }
}

/// Retained capacity for a **new** row, at both scopes.
///
/// Note it consults retained only. The *active* cap does not apply: a tombstone
/// is not an active session, so creating one cannot exhaust it -- and gating
/// retirement on active capacity would be exactly backwards, since retirement
/// is what frees active capacity.
fn retained_exhausted_for_new_row(capacity: &Capacity) -> bool {
    capacity.retained_group >= super::limits::MAX_RETAINED_SESSIONS_PER_GROUP
        || capacity.retained_actor >= MAX_RETAINED_SESSIONS_PER_ACTOR
}

/// Apply a decided retirement to the session table.
///
/// Returns the row to store, or `None` when nothing changes. The caller owns
/// the reserve accounting, because only it knows the previous charge.
pub fn effect_row(
    state: &State,
    command: &RetireCommand,
    effect: &RetireEffect,
) -> Option<Session> {
    let key = (command.target_actor, command.target_client_id);
    match effect {
        RetireEffect::AlreadyRetired => None,
        RetireEffect::Convert => {
            let existing = state.sessions.get(&key)?;
            Some(Session {
                retired: true,
                ..existing.clone()
            })
        }
        RetireEffect::CreateTombstone => Some(Session {
            last_sequence: 0,
            request_hash: [0u8; 32],
            last_result: None,
            retired: true,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::model::limits::{
        MAX_ACTIVE_CLIENTS_PER_ACTOR, MAX_RETAINED_SESSIONS_PER_GROUP,
    };
    use crate::coordination::model::state::{ResourceKey, SavedReply};
    use crate::coordination::reason::Reason;

    const ACTOR: NodeId = [1u8; 32];
    const ADMIN: NodeId = [9u8; 32];
    const OTHER: NodeId = [2u8; 32];
    const CLIENT: ClientId = [7u8; 16];

    fn saved() -> SavedReply {
        SavedReply {
            outcome: Outcome::Granted,
            reason: Reason::None,
            resource: Some(ResourceKey::new("ns", b"k".to_vec()).expect("valid")),
            epoch: 3,
            value_revision: 1,
            acl_revision: 2,
            admin_revision: 0,
        }
    }

    fn with_session(retired: bool) -> State {
        let mut s = State::default();
        s.sessions.insert(
            (ACTOR, CLIENT),
            Session {
                last_sequence: 5,
                request_hash: [0xAA; 32],
                last_result: Some(saved()),
                retired,
            },
        );
        s.reserve_bytes = s.recompute_reserve();
        s
    }

    fn command(actor: NodeId) -> RetireCommand {
        RetireCommand {
            actor,
            target_actor: ACTOR,
            target_client_id: CLIENT,
        }
    }

    // ── The §10 transition table, row by row ────────────────────────────────

    /// Exists, not retired: becomes a tombstone **retaining** its last
    /// sequence, hash and saved result. That retention is what lets a retry of
    /// a sequence that already committed still get its result.
    #[test]
    fn an_existing_session_becomes_a_tombstone_retaining_its_result() {
        let s = with_session(false);
        let d = decide(&s, &command(ACTOR));
        assert_eq!(d, Decision::Apply(RetireEffect::Convert));
        assert_eq!(d.outcome(), Outcome::ClientRetired);

        let row = effect_row(&s, &command(ACTOR), &RetireEffect::Convert).expect("a row");
        assert!(row.retired);
        assert_eq!(row.last_sequence, 5, "the sequence is retained");
        assert_eq!(row.request_hash, [0xAA; 32], "and the hash");
        assert_eq!(row.last_result, Some(saved()), "and the saved result");
    }

    /// Already retired: no change, still `9 ClientRetired` -- idempotent.
    #[test]
    fn retiring_an_already_retired_session_is_idempotent() {
        let s = with_session(true);
        let d = decide(&s, &command(ACTOR));
        assert_eq!(d, Decision::Apply(RetireEffect::AlreadyRetired));
        assert_eq!(d.outcome(), Outcome::ClientRetired);
        assert_eq!(
            effect_row(&s, &command(ACTOR), &RetireEffect::AlreadyRetired),
            None,
            "nothing is written"
        );
    }

    /// Does not exist: a tombstone is created with `last_sequence = 0` and no
    /// saved result, so no future sequence can ever be accepted for that pair.
    #[test]
    fn retiring_an_absent_session_creates_a_zero_sequence_tombstone() {
        let s = State::default();
        assert_eq!(
            decide(&s, &command(ACTOR)),
            Decision::Apply(RetireEffect::CreateTombstone)
        );

        let row = effect_row(&s, &command(ACTOR), &RetireEffect::CreateTombstone).expect("a row");
        assert!(row.retired);
        assert_eq!(row.last_sequence, 0);
        assert_eq!(row.last_result, None);
        assert_eq!(row.request_hash, [0u8; 32]);
    }

    /// The point of creating rather than no-op'ing: sequence 1 must not remain
    /// available to a straggler. Verified through the classifier, which is what
    /// a straggler actually meets.
    #[test]
    fn a_zero_sequence_tombstone_closes_every_future_sequence() {
        use crate::coordination::model::classify::{classify, Classification};

        let s = State::default();
        let row = effect_row(&s, &command(ACTOR), &RetireEffect::CreateTombstone).expect("a row");
        let capacity = s.capacity_for(&ACTOR);

        for sequence in [1u64, 2, 100] {
            match classify(Some(&row), sequence, &[0xAA; 32], capacity) {
                Classification::Refuse { reason, .. } => {
                    assert_eq!(reason, Reason::ClientRetired, "sequence {sequence}")
                }
                other => panic!("sequence {sequence} must be refused, got {other:?}"),
            }
        }
    }

    /// Retained capacity exhausted: no change, no row created, terminal `18`.
    /// This is the case that makes reason 18's recovery "surface and stop" --
    /// the tombstone needs the very row the refusal is about.
    #[test]
    fn retirement_at_the_retained_cap_creates_nothing_and_is_terminal() {
        let mut s = State::default();
        for i in 0..MAX_RETAINED_SESSIONS_PER_ACTOR {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&i.to_be_bytes());
            s.sessions.insert(
                (ACTOR, id),
                Session {
                    last_sequence: 1,
                    request_hash: [0u8; 32],
                    last_result: None,
                    retired: true,
                },
            );
        }

        let d = decide(&s, &command(ACTOR));
        match d {
            Decision::Refuse(r) => {
                assert_eq!(r.get(), Reason::RetainedSessionCapReached);
                assert!(r.is_terminal_when_not_consumed());
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(d.outcome(), Outcome::Rejected);
    }

    /// Converting an existing row is always legal: it adds no row, so no cap
    /// refuses it. Retirement must stay available as §10's recovery step.
    #[test]
    fn converting_an_existing_session_is_legal_even_at_the_retained_cap() {
        let mut s = with_session(false);
        for i in 0..MAX_RETAINED_SESSIONS_PER_ACTOR {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&i.to_be_bytes());
            id[15] = 1;
            s.sessions.insert(
                (ACTOR, id),
                Session {
                    last_sequence: 1,
                    request_hash: [0u8; 32],
                    last_result: None,
                    retired: true,
                },
            );
        }
        assert!(s.capacity_for(&ACTOR).retained_exhausted());
        assert_eq!(
            decide(&s, &command(ACTOR)),
            Decision::Apply(RetireEffect::Convert)
        );
    }

    /// The **active** cap never refuses a retirement: a tombstone is not an
    /// active session, and gating on it would be backwards, since retirement is
    /// what frees active capacity.
    #[test]
    fn a_full_active_cap_never_refuses_a_retirement() {
        let mut s = State::default();
        for i in 0..MAX_ACTIVE_CLIENTS_PER_ACTOR {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&i.to_be_bytes());
            s.sessions.insert(
                (ACTOR, id),
                Session {
                    last_sequence: 1,
                    request_hash: [0u8; 32],
                    last_result: None,
                    retired: false,
                },
            );
        }
        assert!(s.capacity_for(&ACTOR).active_exhausted());
        assert!(!s.capacity_for(&ACTOR).retained_exhausted());
        assert_eq!(
            decide(&s, &command(ACTOR)),
            Decision::Apply(RetireEffect::CreateTombstone)
        );
    }

    /// The group-wide retained cap refuses too, not only the per-actor one.
    #[test]
    fn the_group_retained_cap_also_refuses_a_new_tombstone() {
        let mut s = State::default();
        for i in 0..MAX_RETAINED_SESSIONS_PER_GROUP {
            let mut actor = [0u8; 32];
            actor[..8].copy_from_slice(&i.to_be_bytes());
            s.sessions.insert(
                (actor, CLIENT),
                Session {
                    last_sequence: 1,
                    request_hash: [0u8; 32],
                    last_result: None,
                    retired: true,
                },
            );
        }
        match decide(&s, &command(ACTOR)) {
            Decision::Refuse(r) => assert_eq!(r.get(), Reason::RetainedSessionCapReached),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // ── Authorization ───────────────────────────────────────────────────────

    /// An actor may retire only its own sessions; an administrator may retire
    /// any.
    #[test]
    fn only_the_target_actor_or_an_administrator_may_retire() {
        let mut s = with_session(false);
        s.admins.insert(ADMIN);

        // Own session: allowed.
        assert!(matches!(decide(&s, &command(ACTOR)), Decision::Apply(_)));
        // Administrator: allowed.
        assert!(matches!(decide(&s, &command(ADMIN)), Decision::Apply(_)));
        // A stranger: refused.
        match decide(&s, &command(OTHER)) {
            Decision::Refuse(r) => assert_eq!(r.get(), Reason::PermissionChanged),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A client may retire its **own** last working id: sessionless means there
    /// is no self-retirement problem to legislate against.
    #[test]
    fn a_client_may_retire_the_session_it_would_otherwise_be_calling_from() {
        let s = with_session(false);
        assert_eq!(
            decide(&s, &command(ACTOR)),
            Decision::Apply(RetireEffect::Convert),
            "no sequence is consumed, so there is nothing to bootstrap"
        );
    }

    /// Deduplication is on `(target_actor, target_client_id)` -- retiring a
    /// different client id of the same actor is a different command.
    #[test]
    fn retirement_is_scoped_to_one_actor_and_client_pair() {
        let s = with_session(false);
        let other_pair = RetireCommand {
            actor: ACTOR,
            target_actor: ACTOR,
            target_client_id: [8u8; 16],
        };
        assert_eq!(
            decide(&s, &other_pair),
            Decision::Apply(RetireEffect::CreateTombstone),
            "a different client id has no session of its own"
        );
    }

    #[test]
    fn deciding_twice_gives_the_same_answer() {
        let s = with_session(false);
        assert_eq!(decide(&s, &command(ACTOR)), decide(&s, &command(ACTOR)));
    }
}
