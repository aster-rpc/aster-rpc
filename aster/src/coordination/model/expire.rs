//! System `Expire` (§9.1, §9.2) -- deterministic, and deliberately not a client
//! action.
//!
//! ## Why expiry is a committed command rather than a timer
//!
//! A leader arms a monotonic timer for a lease's committed TTL and, on fire,
//! **proposes** `Expire(resource, epoch, lease_revision)`. It does not expire
//! anything locally. That is what makes an expiry-versus-renew race decidable:
//! the log orders the two, and every voter reaches the same answer from the same
//! committed state.
//!
//! `acquire` never takes a held row even when a leader believes the lease is
//! old, so the only way a lease ends without its holder releasing it is a
//! committed `Expire` or `revoke`.
//!
//! ## Stale timers are harmless applied rejections
//!
//! An `Expire` naming an epoch or lease revision that a committed renew has
//! already superseded is not an error -- it is the normal outcome of a timer
//! that fired while a renew was in flight. It applies as `2 ExpiryStale`,
//! changing nothing. Treating it as a fault would make ordinary races fatal.
//!
//! ## It cannot be refused for budget
//!
//! §12.1 lists `Expire` as safety-reducing: it must apply regardless of the
//! growth budget, or leases stop expiring exactly when the group is most
//! stressed. It also frees no growth bytes, because it preserves the value.

use crate::coordination::contract::{SYS_EXPIRED, SYS_EXPIRY_STALE};

use super::limits::MAX_COUNTER;
use super::state::{ResourceKey, ResourceRow, State};

/// A system `Expire` command (§9.1).
///
/// It carries the exact `(epoch, lease_revision)` the leader observed, so a
/// timer that fired against a superseded lease is identifiable rather than
/// merely late.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpireCommand {
    pub resource: ResourceKey,
    pub epoch: u64,
    pub lease_revision: u64,
}

/// What an `Expire` writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The lease matched exactly and is cleared.
    Apply { row: ResourceRow },
    /// Nothing matched, or a counter is exhausted. Changes nothing.
    Stale,
}

impl Decision {
    /// The §5.3 system result code.
    pub fn system_result_code(&self) -> i32 {
        match self {
            Decision::Apply { .. } => SYS_EXPIRED,
            Decision::Stale => SYS_EXPIRY_STALE,
        }
    }
}

/// Decide one `Expire` against committed state.
///
/// There is no authorization check and no session: a system command has no
/// caller. Its authority is that it reached the log, which only a leader can
/// arrange.
pub fn decide(state: &State, command: &ExpireCommand) -> Decision {
    let Some(row) = state.resources.get(&command.resource) else {
        return Decision::Stale;
    };

    // "Exact held lease" (§9.1): all three must match. A renew advances the
    // lease revision without touching the epoch, so comparing the epoch alone
    // would let a timer expire a lease that was renewed after it was armed --
    // precisely the race the lease revision exists to settle.
    if !row.held || row.epoch != command.epoch || row.lease_revision != command.lease_revision {
        return Decision::Stale;
    }

    // A counter at the ceiling cannot advance, so the expiry cannot apply. It
    // reports stale rather than a reason code: `SystemReplyV1` has no reason
    // field, and a system command has no client to report one to. The lease
    // remains held, which is the safe direction -- the alternative would be
    // clearing a holder without advancing the epoch that fences it.
    if row.epoch >= MAX_COUNTER || row.lease_revision >= MAX_COUNTER {
        return Decision::Stale;
    }

    let mut next = row.clone();
    next.held = false;
    next.holder = None;
    next.epoch += 1;
    next.lease_revision += 1;
    Decision::Apply { row: next }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ResourceKey {
        ResourceKey::new("ns", b"k".to_vec()).expect("valid")
    }

    fn held(epoch: u64, lease_revision: u64) -> State {
        let mut s = State::default();
        s.resources.insert(
            key(),
            ResourceRow {
                epoch,
                held: true,
                holder: Some([1u8; 32]),
                ttl_ms: 30_000,
                lease_revision,
                value_revision: 2,
                value: b"payload".to_vec(),
                acl_revision: 1,
            },
        );
        s.growth_bytes = s.recompute_growth();
        s
    }

    fn command(epoch: u64, lease_revision: u64) -> ExpireCommand {
        ExpireCommand {
            resource: key(),
            epoch,
            lease_revision,
        }
    }

    #[test]
    fn an_exact_match_clears_the_holder_and_advances_both_counters() {
        let s = held(4, 2);
        match decide(&s, &command(4, 2)) {
            Decision::Apply { row } => {
                assert!(!row.held);
                assert_eq!(row.holder, None);
                assert_eq!(row.epoch, 5);
                assert_eq!(row.lease_revision, 3);
                // The value survives: expiry ends a lease, it does not erase
                // what the lease guarded.
                assert_eq!(row.value, b"payload");
                assert_eq!(row.value_revision, 2);
            }
            other => panic!("expected an applied expiry, got {other:?}"),
        }
        assert_eq!(decide(&s, &command(4, 2)).system_result_code(), SYS_EXPIRED);
    }

    /// The race the lease revision exists to settle: a renew advances the lease
    /// revision without touching the epoch, so a timer armed before it must not
    /// expire the renewed lease.
    #[test]
    fn a_timer_armed_before_a_renew_is_stale_even_though_the_epoch_matches() {
        let renewed = held(4, 3);
        let d = decide(&renewed, &command(4, 2));
        assert_eq!(d, Decision::Stale);
        assert_eq!(d.system_result_code(), SYS_EXPIRY_STALE);
    }

    #[test]
    fn every_mismatch_is_stale_rather_than_an_error() {
        let s = held(4, 2);
        for (epoch, lease, why) in [
            (3u64, 2u64, "older epoch"),
            (5, 2, "newer epoch"),
            (4, 1, "older lease revision"),
            (4, 3, "newer lease revision"),
        ] {
            assert_eq!(decide(&s, &command(epoch, lease)), Decision::Stale, "{why}");
        }
    }

    /// An expiry against a lease that was already released is stale, not an
    /// error -- the ordinary outcome of a timer that fired after a release
    /// committed.
    #[test]
    fn expiring_a_free_row_is_stale() {
        let mut s = held(4, 2);
        let row = s.resources.get_mut(&key()).unwrap();
        row.held = false;
        row.holder = None;
        assert_eq!(decide(&s, &command(4, 2)), Decision::Stale);
    }

    #[test]
    fn expiring_an_absent_resource_is_stale() {
        let s = State::default();
        assert_eq!(decide(&s, &command(1, 1)), Decision::Stale);
    }

    /// A counter at the ceiling leaves the lease held rather than clearing a
    /// holder without advancing the epoch that fences it.
    #[test]
    fn a_counter_at_the_ceiling_leaves_the_lease_held() {
        for (epoch, lease) in [(MAX_COUNTER, 2u64), (4, MAX_COUNTER)] {
            let s = held(epoch, lease);
            assert_eq!(
                decide(&s, &command(epoch, lease)),
                Decision::Stale,
                "epoch {epoch}, lease {lease}"
            );
        }
    }

    /// One below the ceiling still expires, so the guard is not off by one.
    #[test]
    fn one_below_the_ceiling_still_expires() {
        let s = held(MAX_COUNTER - 1, MAX_COUNTER - 1);
        assert!(matches!(
            decide(&s, &command(MAX_COUNTER - 1, MAX_COUNTER - 1)),
            Decision::Apply { .. }
        ));
    }

    /// Deterministic: the same state and command always give the same answer,
    /// which is what lets every voter apply the entry identically.
    #[test]
    fn deciding_twice_gives_the_same_answer() {
        let s = held(4, 2);
        assert_eq!(decide(&s, &command(4, 2)), decide(&s, &command(4, 2)));
    }
}
