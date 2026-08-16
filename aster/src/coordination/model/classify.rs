//! The §8.2.1 session classifier -- **one** function, shared by preflight and
//! apply.
//!
//! Two implementations of this decision is the defect the section exists to
//! prevent. The spike had exactly that, and the two disagreed immediately: a
//! saved replay returned "unresolved" from preflight, so a request already
//! answerable from the session table fell through toward proposal.
//!
//! ## Why session resolution precedes every pre-proposal denial
//!
//! ```text
//! A commits at sequence N          response lost
//! actor's grant is revoked
//! retry A  ->  preflight sees no ACL grant  ->  PermissionDenied, pre-proposal
//! ```
//!
//! A's result is sitting in the session table, but the retry never reaches apply
//! to find it. Retirement does not rescue this -- the tombstone preserves the
//! result and the retry is still refused a layer earlier. The sequence never
//! terminates and the client can never learn that its work committed. So the
//! leader resolves the session **first**, at or after the same linearizable
//! barrier `read_linearizable` takes.
//!
//! ## The table
//!
//! Rows are evaluated in order and the first match wins. Five orderings are
//! load-bearing and each has a test named for its consequence rather than its
//! row number; see [`Row`].

use crate::coordination::reason::{Disposition, Reason};

use super::limits::MAX_COUNTER;
use super::state::{Capacity, SavedReply, Session};

/// Which §8.2.1 row matched.
///
/// Carried in the result so tests can assert the **ordering** rules, not merely
/// the answer. Several rows produce the same reason by different routes -- rows
/// 1 and 5 both give `8 SequenceTooOld`, rows 6 and 12 both give `15` -- so a
/// test that checked only the reason could not tell a precedence regression from
/// correct behaviour.
///
/// These numbers are the specification's, not an implementation detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Row {
    /// 1: `sequence == 0`, the tombstone sentinel.
    SequenceZero = 1,
    /// 2: exact replay of the last sequence with a matching hash.
    Replay = 2,
    /// 3: retired, any other sequence or a hash mismatch.
    Retired = 3,
    /// 4: same sequence, different hash.
    HashConflict = 4,
    /// 5: `sequence < last_sequence`.
    TooOld = 5,
    /// 6: counter ceiling, existing session.
    CeilingExisting = 6,
    /// 7: exact next sequence. Unresolved.
    ExactNext = 7,
    /// 8: `sequence > last_sequence + 1`.
    Gap = 8,
    /// 9: no session, retained capacity exhausted.
    NoSessionRetainedFull = 9,
    /// 10: no session, retained available but active exhausted.
    NoSessionActiveFull = 10,
    /// 11: no session, capacity available, `sequence == 1`. Unresolved.
    NoSessionFirst = 11,
    /// 12: no session, capacity available, sequence above the ceiling.
    CeilingAbsent = 12,
    /// 13: no session, capacity available, `sequence > 1`.
    NoSessionGap = 13,
}

/// What the classifier decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Classification {
    /// Rows 7 and 11: the classifier has no answer. Fall through to the
    /// admission checks and then to apply.
    ///
    /// `creates_session` distinguishes row 11 (apply must create the session
    /// row) from row 7 (it must update an existing one). Apply needs to know
    /// without re-deriving it, and re-deriving is how two implementations of one
    /// decision start to drift.
    Unresolved { row: Row, creates_session: bool },

    /// Row 2: answered from the saved result. Always `Consumed`, always
    /// terminal.
    ///
    /// Boxed: `SavedReply` now carries a whole snapshot, so this variant is
    /// much larger than the other two and an unboxed enum would pay for it at
    /// every classification.
    ///
    /// The saved row is returned **verbatim**. Reconstructing a reply field by
    /// field is how the spike lost a committed `19 StateBudgetExhausted`, which
    /// replayed as reason `0` -- telling the client its rejection had no cause.
    Replay { row: Row, saved: Box<SavedReply> },

    /// Every refusing row. All are `NotConsumed`: the classifier answers before
    /// anything is written.
    Refuse { row: Row, reason: Reason },
}

impl Classification {
    /// The disposition §8.2.1's table assigns this result.
    pub fn disposition(&self) -> Option<Disposition> {
        match self {
            Classification::Unresolved { .. } => None,
            Classification::Replay { .. } => Some(Disposition::Consumed),
            Classification::Refuse { .. } => Some(Disposition::NotConsumed),
        }
    }

    /// Which row matched.
    pub fn row(&self) -> Row {
        match self {
            Classification::Unresolved { row, .. }
            | Classification::Replay { row, .. }
            | Classification::Refuse { row, .. } => *row,
        }
    }

    /// Is this answer terminal (§10)?
    pub fn is_terminal(&self) -> bool {
        match self {
            Classification::Unresolved { .. } => false,
            Classification::Replay { .. } => true,
            Classification::Refuse { reason, .. } => Disposition::NotConsumed.is_terminal(*reason),
        }
    }
}

/// Classify one `(sequence, request_hash)` against committed session state.
///
/// `session` is the row for `(actor, client_id)`, or `None` if there is none.
/// `capacity` is only consulted on the absent-session path.
pub fn classify(
    session: Option<&Session>,
    sequence: u64,
    request_hash: &[u8; 32],
    capacity: Capacity,
) -> Classification {
    // Row 1. `0` is the tombstone sentinel: retiring an absent session creates
    // a row with `last_sequence = 0`. If `0` were a valid client sequence it
    // would match row 2 or row 6 against that very tombstone, and a straggler
    // would apply against the row created to stop it. The first valid client
    // sequence is 1. This precedes everything, including the absent-session
    // rows -- sequence 0 is out of contract whether or not a session exists.
    if sequence == 0 {
        return Classification::Refuse {
            row: Row::SequenceZero,
            reason: Reason::SequenceTooOld,
        };
    }

    let Some(session) = session else {
        return classify_absent(sequence, capacity);
    };

    // Row 2, and it precedes row 3 deliberately. Retirement preserves the saved
    // result precisely so a retry of a sequence that already committed still
    // gets it. Answering `ClientRetired` to a client whose action *did* apply is
    // the one lie the journal cannot survive: it would record the sequence as
    // resolved-and-never-applied.
    if sequence == session.last_sequence && request_hash == &session.request_hash {
        if let Some(saved) = &session.last_result {
            return Classification::Replay {
                row: Row::Replay,
                saved: Box::new(saved.clone()),
            };
        }
    }

    // Row 3. Retirement dominates everything below it: once retired, the only
    // thing a session will ever answer is its one preserved result. An earlier
    // revision returned `SequenceConflict` for a retired session with a
    // mismatched hash -- a retryable-looking answer for a session that can never
    // accept anything again.
    if session.retired {
        return Classification::Refuse {
            row: Row::Retired,
            reason: Reason::ClientRetired,
        };
    }

    // Row 4.
    if sequence == session.last_sequence {
        return Classification::Refuse {
            row: Row::HashConflict,
            reason: Reason::SequenceConflict,
        };
    }

    // Row 5.
    if sequence < session.last_sequence {
        return Classification::Refuse {
            row: Row::TooOld,
            reason: Reason::SequenceTooOld,
        };
    }

    // Row 6, and it precedes row 7 so that `last + 1` is never computed at all.
    // Two defects live in an unguarded successor: an out-of-contract sequence
    // above the ceiling is accepted and stored, and because `last_sequence` is a
    // `u64` read back from storage, corrupt or hostile state at `u64::MAX`
    // panics in debug and **wraps to 0** in release -- where a stale sequence
    // then looks exact-next.
    if session.last_sequence >= MAX_COUNTER || sequence > MAX_COUNTER {
        return Classification::Refuse {
            row: Row::CeilingExisting,
            reason: Reason::CounterExhausted,
        };
    }

    // Checked, even though row 6 has already excluded the overflow: the guard
    // and the arithmetic should not be separately correct by coincidence.
    let next = match session.last_sequence.checked_add(1) {
        Some(next) => next,
        None => {
            return Classification::Refuse {
                row: Row::CeilingExisting,
                reason: Reason::CounterExhausted,
            }
        }
    };

    // Row 7.
    if sequence == next {
        return Classification::Unresolved {
            row: Row::ExactNext,
            creates_session: false,
        };
    }

    // Row 8.
    Classification::Refuse {
        row: Row::Gap,
        reason: Reason::SequenceGap,
    }
}

/// Rows 9-13: no session row exists for `(actor, client_id)`.
///
/// "No row" is not the same as "sequence 0", and leaving these implicit is how
/// the capacity contradiction in §12.1 went unnoticed -- preflight sees this
/// path constantly, because a first request from a new client has nothing to
/// classify against.
fn classify_absent(sequence: u64, capacity: Capacity) -> Classification {
    // Row 9 precedes row 10. When both caps are full, `18` wins: `21` would
    // invite a retry against capacity nothing in v1 can free, which loops
    // forever. Only when retained capacity is available does an exhausted active
    // cap produce the retryable `21`.
    if capacity.retained_exhausted() {
        return Classification::Refuse {
            row: Row::NoSessionRetainedFull,
            reason: Reason::RetainedSessionCapReached,
        };
    }

    if capacity.active_exhausted() {
        return Classification::Refuse {
            row: Row::NoSessionActiveFull,
            reason: Reason::ActiveClientCap,
        };
    }

    // Row 12: the same ceiling as row 6, so a session cannot be *opened* above
    // it either.
    if sequence > MAX_COUNTER {
        return Classification::Refuse {
            row: Row::CeilingAbsent,
            reason: Reason::CounterExhausted,
        };
    }

    // Row 11.
    if sequence == 1 {
        return Classification::Unresolved {
            row: Row::NoSessionFirst,
            creates_session: true,
        };
    }

    // Row 13: a first request is always sequence 1.
    Classification::Refuse {
        row: Row::NoSessionGap,
        reason: Reason::SequenceGap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::model::limits::{
        MAX_ACTIVE_CLIENTS_PER_ACTOR, MAX_ACTIVE_CLIENTS_PER_GROUP,
        MAX_RETAINED_SESSIONS_PER_ACTOR, MAX_RETAINED_SESSIONS_PER_GROUP,
    };
    use crate::coordination::model::state::{ResourceKey, SavedSnapshot};
    use crate::coordination::reason::Outcome;

    const HASH_A: [u8; 32] = [0xAA; 32];
    const HASH_B: [u8; 32] = [0xBB; 32];

    fn room() -> Capacity {
        Capacity {
            retained_group: 0,
            retained_actor: 0,
            active_group: 0,
            active_actor: 0,
        }
    }

    fn saved(reason: Reason) -> SavedReply {
        SavedReply {
            outcome: if reason == Reason::None {
                Outcome::Granted
            } else {
                Outcome::Rejected
            },
            reason,
            snapshot: Some(SavedSnapshot {
                resource: ResourceKey::new("ns", b"k".to_vec()).expect("valid"),
                epoch: 7,
                holder: Some([5u8; 32]),
                held: true,
                ttl_ms: 30_000,
                lease_revision: 1,
                value_revision: 3,
                acl_revision: 2,
            }),
            acl_revision: 2,
            admin_revision: 0,
            operation_id: None,
            membership_log_id: None,
        }
    }

    fn session(last: u64, hash: [u8; 32], result: Option<SavedReply>, retired: bool) -> Session {
        Session {
            last_sequence: last,
            request_hash: hash,
            last_result: result,
            retired,
        }
    }

    // ── Row 1 ───────────────────────────────────────────────────────────────

    /// Sequence 0 is the tombstone sentinel, so it is refused whether or not a
    /// session exists -- including against the very tombstone that retiring an
    /// absent session creates.
    #[test]
    fn sequence_zero_never_applies_against_the_tombstone_created_to_stop_it() {
        let tombstone = session(0, [0u8; 32], None, true);
        for existing in [None, Some(&tombstone)] {
            let c = classify(existing, 0, &[0u8; 32], room());
            assert_eq!(c.row(), Row::SequenceZero);
            assert!(matches!(
                c,
                Classification::Refuse {
                    reason: Reason::SequenceTooOld,
                    ..
                }
            ));
        }
    }

    // ── Row 2 before row 3 ──────────────────────────────────────────────────

    /// The one lie the journal cannot survive: a retired client whose action
    /// *did* commit must still get its saved result, not `ClientRetired`.
    #[test]
    fn a_retired_session_still_replays_the_sequence_that_committed() {
        let s = session(5, HASH_A, Some(saved(Reason::None)), true);
        let c = classify(Some(&s), 5, &HASH_A, room());
        assert_eq!(c.row(), Row::Replay, "row 2 must precede row 3");
        match c {
            Classification::Replay { saved, .. } => {
                assert_eq!(saved.outcome, Outcome::Granted);
                assert_eq!(saved.snapshot.as_ref().unwrap().epoch, 7);
            }
            other => panic!("expected a replay, got {other:?}"),
        }
    }

    /// A committed rejection replays with its **saved reason**, verbatim. The
    /// spike reconstructed the reply field by field with `Reason::None`, so a
    /// committed `19` replayed as "rejected, no cause" -- and the test compared
    /// only the outcome code, which is 100 for every rejection, so it passed.
    #[test]
    fn a_committed_rejection_replays_with_its_saved_reason_not_a_blank_one() {
        let s = session(3, HASH_A, Some(saved(Reason::StateBudgetExhausted)), false);
        match classify(Some(&s), 3, &HASH_A, room()) {
            Classification::Replay { saved, .. } => {
                assert_eq!(saved.reason, Reason::StateBudgetExhausted);
                assert_eq!(saved.outcome, Outcome::Rejected);
                // The revisions a client resyncs from after losing its response.
                let snap = saved.snapshot.as_ref().unwrap();
                assert_eq!((snap.epoch, snap.value_revision), (7, 3));
                assert_eq!(saved.acl_revision, 2);
            }
            other => panic!("expected a replay, got {other:?}"),
        }
    }

    /// A replay is `Consumed` and terminal whatever it saved.
    #[test]
    fn a_replay_is_consumed_and_terminal() {
        let s = session(1, HASH_A, Some(saved(Reason::StateBudgetExhausted)), false);
        let c = classify(Some(&s), 1, &HASH_A, room());
        assert_eq!(c.disposition(), Some(Disposition::Consumed));
        assert!(c.is_terminal());
    }

    // ── Row 3 ───────────────────────────────────────────────────────────────

    /// Retirement dominates: every other sequence, and every hash mismatch,
    /// answers `ClientRetired` and is terminal. `SequenceConflict` here would be
    /// a retryable-looking answer for a session that can never accept anything.
    #[test]
    fn retirement_dominates_every_sequence_but_the_saved_one() {
        let s = session(5, HASH_A, Some(saved(Reason::None)), true);

        // Hash mismatch at the same sequence.
        let mismatch = classify(Some(&s), 5, &HASH_B, room());
        assert_eq!(mismatch.row(), Row::Retired);
        assert!(mismatch.is_terminal());

        // Older, newer, exact-next: all ClientRetired.
        for sequence in [1, 4, 6, 7, 100] {
            let c = classify(Some(&s), sequence, &HASH_A, room());
            assert_eq!(
                c.row(),
                Row::Retired,
                "sequence {sequence} against a retired session"
            );
            assert!(matches!(
                c,
                Classification::Refuse {
                    reason: Reason::ClientRetired,
                    ..
                }
            ));
            assert!(c.is_terminal());
        }
    }

    /// A tombstone for a session that never existed answers `ClientRetired` to
    /// sequence 1 -- which is what proves the delayed original can never commit.
    #[test]
    fn a_zero_sequence_tombstone_refuses_the_first_real_sequence() {
        let tombstone = session(0, [0u8; 32], None, true);
        let c = classify(Some(&tombstone), 1, &HASH_A, room());
        assert_eq!(c.row(), Row::Retired);
        assert!(c.is_terminal());
    }

    // ── Rows 4, 5, 7, 8 ─────────────────────────────────────────────────────

    #[test]
    fn the_sequence_comparison_rows_answer_as_the_table_says() {
        let s = session(5, HASH_A, Some(saved(Reason::None)), false);

        // Row 4: same sequence, different hash. A client may not change the
        // request at a sequence it has already sent.
        let conflict = classify(Some(&s), 5, &HASH_B, room());
        assert_eq!(conflict.row(), Row::HashConflict);
        assert!(!conflict.is_terminal());

        // Row 5.
        assert_eq!(classify(Some(&s), 4, &HASH_A, room()).row(), Row::TooOld);

        // Row 7: unresolved, updating an existing row.
        match classify(Some(&s), 6, &HASH_A, room()) {
            Classification::Unresolved {
                row,
                creates_session,
            } => {
                assert_eq!(row, Row::ExactNext);
                assert!(!creates_session);
            }
            other => panic!("expected unresolved, got {other:?}"),
        }

        // Row 8.
        assert_eq!(classify(Some(&s), 7, &HASH_A, room()).row(), Row::Gap);
        assert_eq!(classify(Some(&s), 999, &HASH_A, room()).row(), Row::Gap);
    }

    /// Rows 4, 5 and 8 are all retryable; only 3 and 6 are terminal on this
    /// path.
    #[test]
    fn only_retirement_and_the_ceiling_are_terminal_for_an_existing_session() {
        let s = session(5, HASH_A, None, false);
        for (sequence, hash, terminal) in [
            (5u64, HASH_B, false), // row 4
            (4, HASH_A, false),    // row 5
            (7, HASH_A, false),    // row 8
        ] {
            assert_eq!(
                classify(Some(&s), sequence, &hash, room()).is_terminal(),
                terminal
            );
        }
    }

    // ── Row 6 before row 7 ──────────────────────────────────────────────────

    /// The ceiling is checked before the successor is computed, so neither an
    /// out-of-contract sequence nor a `u64::MAX` overflow is representable.
    ///
    /// Row 6 sits **after** rows 4 and 5, so it is reached only on the
    /// `sequence > last_sequence` path -- which is exactly the path where
    /// `last + 1` would otherwise be computed. A stale or equal sequence is
    /// answered by the more specific sequence rows first; see
    /// [`a_ceiling_exhausted_session_still_answers_stale_sequences_by_sequence`].
    #[test]
    fn the_counter_ceiling_is_checked_before_the_successor_is_computed() {
        // A client whose session sits at the ceiling asks for its next
        // sequence. This is the reachable case: it is what a well-behaved
        // client does after committing `MAX_COUNTER`.
        let at = session(MAX_COUNTER, HASH_A, None, false);
        let c = classify(Some(&at), MAX_COUNTER + 1, &HASH_A, room());
        assert_eq!(c.row(), Row::CeilingExisting);
        assert!(c.is_terminal(), "15 CounterExhausted is terminal");

        // One below: still usable, so the ceiling is not off by one.
        let below = session(MAX_COUNTER - 1, HASH_A, None, false);
        assert_eq!(
            classify(Some(&below), MAX_COUNTER, &HASH_A, room()).row(),
            Row::ExactNext
        );

        // An out-of-contract sequence above the ceiling against a healthy
        // session is refused rather than accepted and stored.
        let healthy = session(5, HASH_A, None, false);
        let above = classify(Some(&healthy), MAX_COUNTER + 1, &HASH_A, room());
        assert_eq!(above.row(), Row::CeilingExisting);
        assert!(above.is_terminal());
    }

    /// Rows 4 and 5 precede row 6, so a session at the ceiling answers a stale
    /// or repeated sequence by **sequence**, not by the ceiling.
    ///
    /// This is the table's ordering and it is the right answer: a stale sequence
    /// arriving at an exhausted session is a straggler or duplicate, and
    /// `SequenceTooOld` says so precisely. The client's own next request --
    /// `last + 1` -- is the one that gets the terminal `15`, which is what stops
    /// it. Written down because "the ceiling is checked first" is easy to
    /// over-read as "the ceiling outranks every other row".
    #[test]
    fn a_ceiling_exhausted_session_still_answers_stale_sequences_by_sequence() {
        let at = session(MAX_COUNTER, HASH_A, None, false);

        assert_eq!(
            classify(Some(&at), 5, &HASH_A, room()).row(),
            Row::TooOld,
            "a straggler is answered as a straggler"
        );
        assert_eq!(
            classify(Some(&at), MAX_COUNTER, &HASH_B, room()).row(),
            Row::HashConflict,
            "a changed request at an already-sent sequence is a conflict"
        );
        // And the client's real next request is what terminates it.
        assert!(classify(Some(&at), MAX_COUNTER + 1, &HASH_A, room()).is_terminal());
    }

    /// Corrupt state at `u64::MAX`: in release an unguarded `last + 1` wraps to
    /// 0, and a stale sequence then looks exact-next. Neither the panic nor the
    /// wrap is reachable.
    ///
    /// `u64::MAX` is above the contract ceiling, so no legal transition produces
    /// it -- this is a storage-corruption case. Every sequence is then `<= last`
    /// and answered by rows 2, 4 or 5, so the successor is never computed and
    /// the wrap has no path. What must never happen is a stale sequence being
    /// treated as exact-next, which is what the wrap would produce.
    #[test]
    fn corrupt_u64_max_state_never_treats_a_stale_sequence_as_exact_next() {
        let corrupt = session(u64::MAX, HASH_A, None, false);
        for sequence in [1u64, 5, MAX_COUNTER, u64::MAX] {
            let c = classify(Some(&corrupt), sequence, &HASH_A, room());
            assert!(
                !matches!(c, Classification::Unresolved { .. }),
                "sequence {sequence} against corrupt u64::MAX state must not be \
                 admitted; a wrapped successor would make it look exact-next"
            );
        }
        // Specifically: sequence 0 after a wrap would be the exact-next match.
        // It is refused by row 1 regardless.
        assert_eq!(
            classify(Some(&corrupt), 0, &HASH_A, room()).row(),
            Row::SequenceZero
        );
    }

    // ── Row 9 before row 10 ─────────────────────────────────────────────────

    /// The precedence no single-cap test can reach: with **both** caps full the
    /// answer is the terminal `18`, not the retryable `21`. Answering `21` would
    /// invite retries against capacity nothing in v1 can free.
    #[test]
    fn both_caps_full_answers_the_terminal_18_not_the_retryable_21() {
        let both_full = Capacity {
            retained_group: MAX_RETAINED_SESSIONS_PER_GROUP,
            retained_actor: MAX_RETAINED_SESSIONS_PER_ACTOR,
            active_group: MAX_ACTIVE_CLIENTS_PER_GROUP,
            active_actor: MAX_ACTIVE_CLIENTS_PER_ACTOR,
        };
        let c = classify(None, 1, &HASH_A, both_full);
        assert_eq!(c.row(), Row::NoSessionRetainedFull);
        assert!(matches!(
            c,
            Classification::Refuse {
                reason: Reason::RetainedSessionCapReached,
                ..
            }
        ));
        assert!(c.is_terminal());
    }

    /// Only when retained capacity is available does an exhausted active cap
    /// produce the retryable `21` -- retiring a session frees an active slot
    /// without freeing a retained row.
    #[test]
    fn an_exhausted_active_cap_with_retained_room_is_retryable() {
        let active_only = Capacity {
            retained_group: 10,
            retained_actor: 5,
            active_group: MAX_ACTIVE_CLIENTS_PER_GROUP,
            active_actor: MAX_ACTIVE_CLIENTS_PER_ACTOR,
        };
        let c = classify(None, 1, &HASH_A, active_only);
        assert_eq!(c.row(), Row::NoSessionActiveFull);
        assert!(matches!(
            c,
            Classification::Refuse {
                reason: Reason::ActiveClientCap,
                ..
            }
        ));
        assert!(!c.is_terminal(), "retiring a session frees an active slot");
    }

    /// Either scope exhausting retained is enough for row 9, so one actor
    /// cannot be starved by the group and vice versa.
    #[test]
    fn retained_exhaustion_at_either_scope_reaches_row_nine() {
        let group_only = Capacity {
            retained_group: MAX_RETAINED_SESSIONS_PER_GROUP,
            retained_actor: 0,
            active_group: 0,
            active_actor: 0,
        };
        let actor_only = Capacity {
            retained_group: 0,
            retained_actor: MAX_RETAINED_SESSIONS_PER_ACTOR,
            active_group: 0,
            active_actor: 0,
        };
        for cap in [group_only, actor_only] {
            assert_eq!(
                classify(None, 1, &HASH_A, cap).row(),
                Row::NoSessionRetainedFull
            );
        }
    }

    // ── Rows 11, 12, 13 ─────────────────────────────────────────────────────

    #[test]
    fn a_first_request_is_unresolved_and_creates_the_session() {
        match classify(None, 1, &HASH_A, room()) {
            Classification::Unresolved {
                row,
                creates_session,
            } => {
                assert_eq!(row, Row::NoSessionFirst);
                assert!(creates_session, "apply must create the row");
            }
            other => panic!("expected unresolved, got {other:?}"),
        }
    }

    /// A first request is always sequence 1; anything higher is a gap.
    #[test]
    fn an_absent_session_cannot_be_opened_above_sequence_one() {
        for sequence in [2u64, 3, 1_000] {
            let c = classify(None, sequence, &HASH_A, room());
            assert_eq!(c.row(), Row::NoSessionGap);
            assert!(!c.is_terminal());
        }
    }

    /// Row 12: the ceiling applies to a session that does not exist yet, so one
    /// cannot be opened above it either.
    #[test]
    fn an_absent_session_cannot_be_opened_above_the_counter_ceiling() {
        let c = classify(None, MAX_COUNTER + 1, &HASH_A, room());
        assert_eq!(c.row(), Row::CeilingAbsent);
        assert!(c.is_terminal());

        // And the ceiling is checked after capacity, so a full group still
        // answers 18 -- the terminal reason that proves the session can never
        // be created at all.
        let full = Capacity {
            retained_group: MAX_RETAINED_SESSIONS_PER_GROUP,
            retained_actor: 0,
            active_group: 0,
            active_actor: 0,
        };
        assert_eq!(
            classify(None, MAX_COUNTER + 1, &HASH_A, full).row(),
            Row::NoSessionRetainedFull
        );
    }

    // ── Cross-cutting ───────────────────────────────────────────────────────

    /// Every refusal the classifier can return is `NotConsumed`: it answers
    /// before anything is written.
    #[test]
    fn every_classifier_refusal_is_not_consumed() {
        let retired = session(5, HASH_A, Some(saved(Reason::None)), true);
        let normal = session(5, HASH_A, None, false);
        let full = Capacity {
            retained_group: MAX_RETAINED_SESSIONS_PER_GROUP,
            retained_actor: 0,
            active_group: 0,
            active_actor: 0,
        };

        let cases = [
            classify(None, 0, &HASH_A, room()),
            classify(Some(&retired), 9, &HASH_A, room()),
            classify(Some(&normal), 5, &HASH_B, room()),
            classify(Some(&normal), 1, &HASH_A, room()),
            classify(Some(&normal), 99, &HASH_A, room()),
            classify(None, 1, &HASH_A, full),
            classify(None, 5, &HASH_A, room()),
        ];
        for c in cases {
            assert!(matches!(c, Classification::Refuse { .. }));
            assert_eq!(c.disposition(), Some(Disposition::NotConsumed));
        }
    }

    /// Terminality here must agree with §10's rule rather than being decided
    /// twice. Reached through the classifier, not by calling the reason
    /// predicate directly.
    #[test]
    fn classifier_terminality_agrees_with_the_section_10_rule() {
        let retired = session(5, HASH_A, None, true);
        let ceiling = session(MAX_COUNTER, HASH_A, None, false);
        let normal = session(5, HASH_A, None, false);
        let retained_full = Capacity {
            retained_group: MAX_RETAINED_SESSIONS_PER_GROUP,
            retained_actor: 0,
            active_group: 0,
            active_actor: 0,
        };
        let active_full = Capacity {
            retained_group: 0,
            retained_actor: 0,
            active_group: MAX_ACTIVE_CLIENTS_PER_GROUP,
            active_actor: 0,
        };

        // The three terminal classifier reasons: 11, 15, 18. The ceiling case
        // asks for `last + 1`, which is the only path that reaches row 6.
        for c in [
            classify(Some(&retired), 9, &HASH_A, room()),
            classify(Some(&ceiling), MAX_COUNTER + 1, &HASH_A, room()),
            classify(None, 1, &HASH_A, retained_full),
        ] {
            assert!(c.is_terminal(), "{c:?} must be terminal");
        }

        // And the retryable ones: 8, 9, 10, 21.
        for c in [
            classify(Some(&normal), 4, &HASH_A, room()),
            classify(Some(&normal), 99, &HASH_A, room()),
            classify(Some(&normal), 5, &HASH_B, room()),
            classify(None, 1, &HASH_A, active_full),
        ] {
            assert!(!c.is_terminal(), "{c:?} must be retryable");
        }
    }

    /// The exact-next rows are the only unresolved ones, and they are the only
    /// ones with no disposition -- nothing has been decided yet.
    #[test]
    fn only_the_two_exact_next_rows_are_unresolved() {
        let s = session(5, HASH_A, None, false);
        let unresolved = [
            classify(Some(&s), 6, &HASH_A, room()),
            classify(None, 1, &HASH_A, room()),
        ];
        for c in unresolved {
            assert!(matches!(c, Classification::Unresolved { .. }));
            assert_eq!(c.disposition(), None);
            assert!(!c.is_terminal());
        }
    }

    /// A session with an exact sequence and hash match but **no saved result**
    /// is not a replay: nothing was saved to return. It falls through to row 4,
    /// because the request is at a sequence already consumed.
    #[test]
    fn a_matching_hash_with_no_saved_result_is_a_conflict_not_a_replay() {
        let s = session(5, HASH_A, None, false);
        let c = classify(Some(&s), 5, &HASH_A, room());
        assert_eq!(c.row(), Row::HashConflict);
    }
}
