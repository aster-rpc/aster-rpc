//! Projecting a model result onto the wire reply types (§5.2.1), including
//! value redaction (§4.3).
//!
//! ## Redaction is structural, not a rule someone remembers
//!
//! `SavedSnapshotV1` -- the snapshot stored in a session row -- has **no value
//! field at all**. So a replay physically cannot carry a register value: the
//! bytes were never saved. What the public `ResourceSnapshotV1` adds is
//! `value_redacted`, which tells the client *why* the value is absent rather
//! than leaving it ambiguous with "the resource has no value".
//!
//! That structure is what makes the reserve budget sound. If saved replies
//! carried values, every mutation would duplicate its value into the
//! reserve-funded session table, and a 16,384-row reserve sized at `256 + KEY`
//! per row would be wrong by up to 64 KiB a row.
//!
//! ## The four-way table (§4.3)
//!
//! | Case | `value` | `value_redacted` |
//! | --- | --- | --- |
//! | first attempt, resource has a value | `Some(bytes)` | `false` |
//! | first attempt, resource has no value | `Some(empty)` | `false` |
//! | replay | `None` | `true` |
//! | no resource in the reply at all | `None` | `false` |
//!
//! `Some([])` and `None` encode differently in Fory, which is what makes rows 2
//! and 3 distinguishable on the wire rather than collapsing into one.

use crate::coordination::contract::{
    ClientReplyV1, ResourceKeyV1, ResourceSnapshotV1, RetirementReplyV1, SystemReplyV1,
};
use crate::coordination::reason::{Disposition, Outcome, Reason};

use super::state::{LogId, SavedReply};

/// Whether this reply is a first attempt or an answer replayed from the session
/// table.
///
/// The distinction reaches the wire in exactly one field, `value_redacted`.
/// Everything else is identical, because everything else comes from the saved
/// snapshot either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// The command applied now, so the value it wrote is available.
    FirstAttempt,
    /// Answered from a saved result: the value was never stored, so it is
    /// reported as redacted rather than as absent.
    Replay,
}

/// Project a model reply onto the public `ClientReplyV1`.
///
/// **Reads no business state.** Every field but the value comes from the saved
/// snapshot, which was taken when the command applied. An earlier revision
/// rebuilt `holder`, `held`, `ttl_ms` and `lease_revision` from *current* state,
/// so a replay after a later mutation returned the saved revisions stapled to a
/// different holder -- a reply that never existed at any point in the log, and
/// one a client would act on as though it had.
///
/// `value` is the only argument the caller supplies, and only a first attempt
/// has one to give.
pub fn client_reply(
    group_id: &[u8],
    reply: &SavedReply,
    disposition: Disposition,
    provenance: Provenance,
    value: Option<&[u8]>,
) -> ClientReplyV1 {
    let snapshot = reply.snapshot.as_ref().map(|saved| {
        let (value, value_redacted) = match provenance {
            Provenance::FirstAttempt => (value.map(<[u8]>::to_vec), false),
            Provenance::Replay => (None, true),
        };
        ResourceSnapshotV1 {
            group_id: group_id.to_vec(),
            resource: ResourceKeyV1 {
                namespace: saved.resource.namespace().to_string(),
                key: saved.resource.key().to_vec(),
            },
            epoch: saved.epoch,
            holder: saved.holder.map(|h| h.to_vec()),
            held: saved.held,
            ttl_ms: saved.ttl_ms,
            lease_revision: saved.lease_revision,
            value_revision: saved.value_revision,
            acl_revision: saved.acl_revision,
            value,
            value_redacted,
        }
    });

    ClientReplyV1 {
        disposition_code: match disposition {
            Disposition::Consumed => 1,
            Disposition::NotConsumed => 2,
        },
        outcome_code: reply.outcome.code(),
        reason_code: reply.reason.code(),
        snapshot,
        acl_revision: reply.acl_revision,
        admin_revision: reply.admin_revision,
        operation_id: reply.operation_id.map(|id| id.to_vec()),
        membership_log_id: reply.membership_log_id.as_ref().map(Into::into),
    }
}

/// Project a system command's result (§5.3).
pub fn system_reply(
    system_result_code: i32,
    lease_revision: u64,
    membership_log_id: Option<&LogId>,
) -> SystemReplyV1 {
    SystemReplyV1 {
        system_result_code,
        lease_revision,
        membership_log_id: membership_log_id.map(Into::into),
    }
}

/// Project a retirement result (§5.3). No disposition: a sessionless command
/// consumes no sequence.
pub fn retirement_reply(outcome: Outcome, reason: Reason) -> RetirementReplyV1 {
    RetirementReplyV1 {
        outcome_code: outcome.code(),
        reason_code: reason.code(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::model::state::{ResourceKey, ResourceRow, SavedSnapshot};

    const GROUP: [u8; 16] = [0xAB; 16];
    const HOLDER: [u8; 32] = [1u8; 32];

    fn key() -> ResourceKey {
        ResourceKey::new("ns", b"k".to_vec()).expect("valid")
    }

    fn row() -> ResourceRow {
        ResourceRow {
            epoch: 4,
            held: true,
            holder: Some(HOLDER),
            ttl_ms: 30_000,
            lease_revision: 2,
            value_revision: 3,
            value: b"payload".to_vec(),
            acl_revision: 1,
        }
    }

    fn reply() -> SavedReply {
        SavedReply {
            outcome: Outcome::Mutated,
            reason: Reason::None,
            snapshot: Some(SavedSnapshot::of(key(), &row())),
            acl_revision: 1,
            admin_revision: 0,
            operation_id: None,
            membership_log_id: None,
        }
    }

    /// Row 1: a first attempt returns the value, unredacted.
    #[test]
    fn a_first_attempt_returns_the_value() {
        let out = client_reply(
            &GROUP,
            &reply(),
            Disposition::Consumed,
            Provenance::FirstAttempt,
            Some(b"payload"),
        );
        let snap = out.snapshot.expect("a snapshot");
        assert_eq!(snap.value, Some(b"payload".to_vec()));
        assert!(!snap.value_redacted);
    }

    /// Row 2: an empty value is `Some([])`, not `None` -- Fory encodes the two
    /// differently, which is what keeps them distinguishable.
    #[test]
    fn an_empty_value_is_present_and_not_redacted() {
        let out = client_reply(
            &GROUP,
            &reply(),
            Disposition::Consumed,
            Provenance::FirstAttempt,
            Some(b""),
        );
        let snap = out.snapshot.expect("a snapshot");
        assert_eq!(snap.value, Some(Vec::new()));
        assert!(!snap.value_redacted);
    }

    /// Row 3: a replay carries `value = None, value_redacted = true`.
    #[test]
    fn a_replay_is_redacted() {
        let out = client_reply(
            &GROUP,
            &reply(),
            Disposition::Consumed,
            Provenance::Replay,
            Some(b"whatever the caller passes"),
        );
        let snap = out.snapshot.expect("a snapshot");
        assert_eq!(snap.value, None, "a replay ignores any supplied value");
        assert!(snap.value_redacted);
    }

    /// **The exact-replay property.** Every field but the value comes from the
    /// saved snapshot, so a replay after arbitrary later mutations is identical
    /// to the first answer. An earlier revision rebuilt holder, held, TTL and
    /// lease revision from current state, so a replay returned the saved
    /// revisions stapled to a different holder -- a reply that never existed at
    /// any point in the log.
    #[test]
    fn a_replay_is_unaffected_by_later_mutations() {
        let saved = reply();

        // Whatever happens afterwards -- a new holder, a longer TTL, a bumped
        // lease revision -- projection has no access to it.
        let first = client_reply(
            &GROUP,
            &saved,
            Disposition::Consumed,
            Provenance::Replay,
            None,
        );
        let later = client_reply(
            &GROUP,
            &saved,
            Disposition::Consumed,
            Provenance::Replay,
            None,
        );
        assert_eq!(first, later);

        let snap = first.snapshot.expect("a snapshot");
        assert_eq!(snap.holder, Some(HOLDER.to_vec()));
        assert!(snap.held);
        assert_eq!(snap.ttl_ms, 30_000);
        assert_eq!(snap.lease_revision, 2);
        assert_eq!(snap.epoch, 4);
        assert_eq!(snap.value_revision, 3);
    }

    /// A first attempt and its replay differ in exactly one field.
    #[test]
    fn a_replay_differs_from_the_first_attempt_only_in_the_value() {
        let saved = reply();
        let first = client_reply(
            &GROUP,
            &saved,
            Disposition::Consumed,
            Provenance::FirstAttempt,
            Some(b"payload"),
        );
        let replayed = client_reply(
            &GROUP,
            &saved,
            Disposition::Consumed,
            Provenance::Replay,
            None,
        );

        assert_eq!(replayed.outcome_code, first.outcome_code);
        assert_eq!(replayed.reason_code, first.reason_code);
        assert_eq!(replayed.acl_revision, first.acl_revision);
        let (a, b) = (
            first.snapshot.expect("first"),
            replayed.snapshot.expect("replay"),
        );
        assert_eq!(
            ResourceSnapshotV1 {
                value: None,
                value_redacted: true,
                ..a.clone()
            },
            b,
            "only the value and its flag may differ"
        );
    }

    /// Row 4: a reply naming no resource -- `set_admin`, a membership command --
    /// has no snapshot.
    #[test]
    fn a_reply_with_no_resource_has_no_snapshot() {
        let mut r = reply();
        r.snapshot = None;
        let out = client_reply(&GROUP, &r, Disposition::Consumed, Provenance::Replay, None);
        assert!(out.snapshot.is_none());
    }

    /// A membership reply carries its operation id and membership log id, both
    /// of which §5.2.1 gives dedicated fields.
    #[test]
    fn a_membership_reply_carries_its_operation_and_log_ids() {
        let mut r = reply();
        r.snapshot = None;
        r.outcome = Outcome::MembershipCancelled;
        r.operation_id = Some([7u8; 16]);
        r.membership_log_id = Some(LogId::new(2, [9u8; 32], 11));

        let out = client_reply(
            &GROUP,
            &r,
            Disposition::Consumed,
            Provenance::FirstAttempt,
            None,
        );
        assert_eq!(out.operation_id, Some(vec![7u8; 16]));
        let id = out.membership_log_id.expect("a membership log id");
        assert_eq!((id.term, id.index), (2, 11));
        assert_eq!(id.leader_node_id, vec![9u8; 32]);
        assert_eq!(out.outcome_code, Outcome::MembershipCancelled.code());
    }

    #[test]
    fn the_disposition_codes_match_the_spec() {
        for (disposition, code) in [(Disposition::Consumed, 1), (Disposition::NotConsumed, 2)] {
            let out = client_reply(
                &GROUP,
                &reply(),
                disposition,
                Provenance::FirstAttempt,
                None,
            );
            assert_eq!(out.disposition_code, code);
        }
    }

    /// An unheld resource reports no holder rather than a zero-byte sentinel.
    #[test]
    fn an_unheld_resource_reports_no_holder() {
        let mut free = row();
        free.held = false;
        free.holder = None;
        let mut r = reply();
        r.snapshot = Some(SavedSnapshot::of(key(), &free));

        let out = client_reply(
            &GROUP,
            &r,
            Disposition::Consumed,
            Provenance::FirstAttempt,
            None,
        );
        let snap = out.snapshot.expect("a snapshot");
        assert_eq!(snap.holder, None);
        assert!(!snap.held);
    }

    /// A system reply carries the phase code, not a client outcome.
    #[test]
    fn a_system_reply_carries_its_phase_code() {
        let out = system_reply(
            crate::coordination::contract::SYS_EXPIRED,
            7,
            Some(&LogId::new(1, [3u8; 32], 4)),
        );
        assert_eq!(
            out.system_result_code,
            crate::coordination::contract::SYS_EXPIRED
        );
        assert_eq!(out.lease_revision, 7);
        assert!(out.membership_log_id.is_some());
    }

    /// A retirement reply has an outcome and a reason, and no disposition --
    /// a sessionless command consumes no sequence.
    #[test]
    fn a_retirement_reply_has_no_disposition() {
        let out = retirement_reply(Outcome::ClientRetired, Reason::None);
        assert_eq!(out.outcome_code, Outcome::ClientRetired.code());
        assert_eq!(out.reason_code, 0);
    }
}
