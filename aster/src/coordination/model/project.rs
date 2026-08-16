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

use crate::coordination::contract::{ClientReplyV1, ResourceKeyV1, ResourceSnapshotV1};
use crate::coordination::reason::Disposition;

use super::state::{ResourceRow, SavedReply, State};

/// Whether this reply is a first attempt or an answer replayed from the session
/// table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// The command applied now: the live row is available, so the value is
    /// returned.
    FirstAttempt,
    /// Answered from a saved result: the value was never stored, so it is
    /// reported as redacted rather than as absent.
    Replay,
}

/// Project a model reply onto the public `ClientReplyV1`.
///
/// `state` supplies the live resource row for a first attempt. On a replay it
/// is not consulted for the value at all -- reading the *current* value and
/// returning it as though it were the one the command produced would be worse
/// than redacting, because the client could not tell the two apart.
pub fn client_reply(
    state: &State,
    group_id: &[u8],
    reply: &SavedReply,
    disposition: Disposition,
    provenance: Provenance,
) -> ClientReplyV1 {
    let snapshot = reply.resource.as_ref().map(|key| {
        let live = state.resources.get(key);
        let (value, value_redacted) = match provenance {
            Provenance::FirstAttempt => (live.map(|row| row.value.clone()), false),
            Provenance::Replay => (None, true),
        };
        ResourceSnapshotV1 {
            group_id: group_id.to_vec(),
            resource: ResourceKeyV1 {
                namespace: key.namespace().to_string(),
                key: key.key().to_vec(),
            },
            epoch: reply.epoch,
            holder: live.and_then(|row| row.holder.map(|h| h.to_vec())),
            held: live.is_some_and(|row| row.held),
            ttl_ms: live.map_or(0, |row| row.ttl_ms),
            lease_revision: live.map_or(0, |row| row.lease_revision),
            value_revision: reply.value_revision,
            acl_revision: reply.acl_revision,
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
        operation_id: None,
        membership_log_id: None,
    }
}

/// The live row for a resource, if any -- exposed so a caller can tell "the
/// resource has no value" from "the value is redacted" without reaching into
/// `State`.
pub fn live_row<'a>(state: &'a State, reply: &SavedReply) -> Option<&'a ResourceRow> {
    reply.resource.as_ref().and_then(|k| state.resources.get(k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::model::state::ResourceKey;
    use crate::coordination::reason::{Outcome, Reason};

    const GROUP: [u8; 16] = [0xAB; 16];

    fn key() -> ResourceKey {
        ResourceKey::new("ns", b"k".to_vec()).expect("valid")
    }

    fn state_with_value(value: &[u8]) -> State {
        let mut s = State::default();
        s.resources.insert(
            key(),
            ResourceRow {
                epoch: 4,
                held: true,
                holder: Some([1u8; 32]),
                ttl_ms: 30_000,
                lease_revision: 2,
                value_revision: 3,
                value: value.to_vec(),
                acl_revision: 1,
            },
        );
        s.growth_bytes = s.recompute_growth();
        s
    }

    fn reply() -> SavedReply {
        SavedReply {
            outcome: Outcome::Mutated,
            reason: Reason::None,
            resource: Some(key()),
            epoch: 4,
            value_revision: 3,
            acl_revision: 1,
            admin_revision: 0,
        }
    }

    /// Row 1: a first attempt returns the value, unredacted.
    #[test]
    fn a_first_attempt_returns_the_value() {
        let s = state_with_value(b"payload");
        let out = client_reply(
            &s,
            &GROUP,
            &reply(),
            Disposition::Consumed,
            Provenance::FirstAttempt,
        );
        let snap = out.snapshot.expect("a snapshot");
        assert_eq!(snap.value, Some(b"payload".to_vec()));
        assert!(!snap.value_redacted);
    }

    /// Row 2: an empty value is `Some([])`, not `None` -- distinguishable from
    /// a redacted one because Fory encodes the two differently.
    #[test]
    fn an_empty_value_is_present_and_not_redacted() {
        let s = state_with_value(b"");
        let out = client_reply(
            &s,
            &GROUP,
            &reply(),
            Disposition::Consumed,
            Provenance::FirstAttempt,
        );
        let snap = out.snapshot.expect("a snapshot");
        assert_eq!(snap.value, Some(Vec::new()));
        assert!(!snap.value_redacted);
    }

    /// Row 3: a replay carries `value = None, value_redacted = true`. The bytes
    /// were never saved, so there is nothing to return -- and returning the
    /// *current* value instead would be worse, because the client could not
    /// tell it from the one its command produced.
    #[test]
    fn a_replay_is_redacted_and_never_returns_the_live_value() {
        let s = state_with_value(b"a-different-value-since");
        let out = client_reply(
            &s,
            &GROUP,
            &reply(),
            Disposition::Consumed,
            Provenance::Replay,
        );
        let snap = out.snapshot.expect("a snapshot");
        assert_eq!(snap.value, None);
        assert!(snap.value_redacted);
    }

    /// A replay preserves the outcome and the revisions -- everything except
    /// the value. That is what lets a client resync after losing a response.
    #[test]
    fn a_replay_preserves_the_outcome_and_revisions() {
        let s = state_with_value(b"x");
        let first = client_reply(
            &s,
            &GROUP,
            &reply(),
            Disposition::Consumed,
            Provenance::FirstAttempt,
        );
        let replayed = client_reply(
            &s,
            &GROUP,
            &reply(),
            Disposition::Consumed,
            Provenance::Replay,
        );

        assert_eq!(replayed.outcome_code, first.outcome_code);
        assert_eq!(replayed.reason_code, first.reason_code);
        assert_eq!(replayed.acl_revision, first.acl_revision);
        let (a, b) = (
            first.snapshot.expect("first"),
            replayed.snapshot.expect("replay"),
        );
        assert_eq!(b.epoch, a.epoch);
        assert_eq!(b.value_revision, a.value_revision);
        assert_eq!(b.acl_revision, a.acl_revision);
        // And only the value differs.
        assert_ne!(b.value, a.value);
        assert_ne!(b.value_redacted, a.value_redacted);
    }

    /// Row 4: a reply naming no resource -- a `set_admin`, a membership
    /// command, or a shapeless rejection -- has no snapshot, and
    /// `value_redacted` does not apply.
    #[test]
    fn a_reply_with_no_resource_has_no_snapshot() {
        let s = State::default();
        let mut r = reply();
        r.resource = None;
        let out = client_reply(&s, &GROUP, &r, Disposition::Consumed, Provenance::Replay);
        assert!(out.snapshot.is_none());
    }

    /// The disposition codes are §5.2's: `1 Consumed`, `2 NotConsumed`.
    #[test]
    fn the_disposition_codes_match_the_spec() {
        let s = state_with_value(b"x");
        for (disposition, code) in [(Disposition::Consumed, 1), (Disposition::NotConsumed, 2)] {
            let out = client_reply(&s, &GROUP, &reply(), disposition, Provenance::FirstAttempt);
            assert_eq!(out.disposition_code, code);
        }
    }

    /// An unheld resource reports no holder rather than a zero-byte sentinel.
    #[test]
    fn an_unheld_resource_reports_no_holder() {
        let mut s = state_with_value(b"x");
        let row = s.resources.get_mut(&key()).unwrap();
        row.held = false;
        row.holder = None;

        let out = client_reply(
            &s,
            &GROUP,
            &reply(),
            Disposition::Consumed,
            Provenance::FirstAttempt,
        );
        let snap = out.snapshot.expect("a snapshot");
        assert_eq!(snap.holder, None);
        assert!(!snap.held);
    }

    #[test]
    fn live_row_resolves_only_a_named_resource() {
        let s = state_with_value(b"x");
        assert!(live_row(&s, &reply()).is_some());
        let mut r = reply();
        r.resource = None;
        assert!(live_row(&s, &r).is_none());
    }
}
