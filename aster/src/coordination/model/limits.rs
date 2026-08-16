//! §12.1 logical limits, the rights bits from §8.1, and the declared charge
//! formulas.
//!
//! These are constants, not configuration: `apply()` checks them, so every
//! voter must compute the identical answer at the identical log index. A
//! host-tunable limit here would make apply host-dependent, which is the one
//! thing a replicated state machine cannot be. (Host-local admission limits are
//! §12.2 and live elsewhere; they never affect apply.)
//!
//! ## The charge is declared arithmetic, not an encoded size
//!
//! The budget is deliberately **not** the encoded size of the state. That would
//! be self-referential -- the counter lives inside the thing being measured --
//! and it would make the charge depend on encoder framing, so a Fory version
//! bump could change every voter's admission decisions. Instead each row has a
//! declared charge over its field lengths: a fixed over-estimate of its
//! overhead, plus every variable-length field it can carry.
//!
//! ## Partitioned by table, never by command
//!
//! An earlier revision charged a session row to growth when its client id was
//! new and to the reserve when it already existed. That is not reconstructible:
//! a row on disk does not record which command created it, so the counters were
//! not functions of the row set and a snapshot could not recompute them.
//! Sessions and audit are reserve; resources, ACL, admins, intents and routes
//! are growth -- every row, every time, whatever wrote it.

// ── Row size sub-limits ─────────────────────────────────────────────────────

/// Maximum namespace length (§12.1).
pub const MAX_NAMESPACE: usize = 64;
/// Maximum resource key length (§12.1).
pub const MAX_KEY: usize = 512;
/// Maximum resource value length (§12.1).
pub const MAX_VALUE: usize = 65_536;
/// Maximum ticket length inside a member seed (§12.1).
///
/// §12.1 says "up to 4 KiB"; the enforced value is one byte lower because a
/// ticket travels in a Fory `String` field, and this codec cannot decode one of
/// 4096 bytes or more. See [`crate::coordination::contract::MAX_WIRE_STRING`].
/// Accepting a ticket no voter can read back would be worse than refusing it.
pub const MAX_TICKET: usize = crate::coordination::contract::MAX_WIRE_STRING;
/// Maximum signed join permit length (§12.1).
pub const MAX_JOIN_PERMIT: usize = 8 * 1024;
/// Maximum encoded `ActionV1` (§12.1). Checked on the encoded form, not
/// reconstructed from field limits.
pub const MAX_ENCODED_COMMAND: usize = 256 * 1024;

/// The largest `(namespace, key)` any row can carry: the `KEY` term in every
/// charge formula.
pub const MAX_ROW_KEY: u64 = (MAX_NAMESPACE + MAX_KEY) as u64;

/// §5.2 stops client counters at `i64::MAX` with reason `15 CounterExhausted`.
///
/// The field is a `u64` on the wire, so this is a contract rule rather than a
/// type bound and has to be enforced explicitly. The spike found two defects
/// from not doing so: an out-of-contract sequence above the ceiling was accepted
/// and stored, and since `last` is a `u64` read back from storage, corrupt state
/// at `u64::MAX` panics in debug and **wraps to 0** in release -- where a stale
/// sequence then looks like the exact next one.
pub const MAX_COUNTER: u64 = i64::MAX as u64;

// ── Budgets ─────────────────────────────────────────────────────────────────

/// Aggregate canonical state budget (§12.1). The binding constraint; every
/// other cap is a sub-limit.
pub const GROWTH_BUDGET: u64 = 192 * 1024 * 1024;
/// Reserved apply-metadata budget (§12.1). Funds session and audit rows so a
/// committed rejection can always persist its own metadata.
pub const RESERVE_BUDGET: u64 = 128 * 1024 * 1024;
/// §12.2's host-local state-database high-water. Not an apply limit -- it
/// appears here only so the compile-time headroom assertion below can reference
/// the figure the budgets were sized against.
pub const LOCAL_HIGH_WATER: u64 = 384 * 1024 * 1024;

// ── Slot caps ───────────────────────────────────────────────────────────────

/// Resources per group (§12.1). Enforced at apply independently of the byte
/// budget: 100,000 small resources sit far below 192 MiB.
pub const MAX_RESOURCES_PER_GROUP: u64 = 100_000;
/// ACL rows per group (§12.1).
pub const MAX_ACL_ROWS_PER_GROUP: u64 = 100_000;
/// Grants per resource (§12.1). Serialized by that resource's own ACL revision,
/// so unlike the two group-global caps it has no cross-resource race.
pub const MAX_GRANTS_PER_RESOURCE: u64 = 256;
/// Group administrators (§12.1).
pub const MAX_ADMINS_PER_GROUP: u64 = 64;
/// Retained authoritative audit rows (§12.1).
pub const MAX_AUDIT_ROWS: u64 = 100_000;
/// Terminal membership intents retained (§12.1). Evicted deterministically when
/// a 65th is produced -- which is why reason `20 IntentSlotsExhausted` is
/// reserved and never emitted.
pub const MAX_RETAINED_INTENTS: u64 = 64;
/// Node routes retained (§12.1).
pub const MAX_RETAINED_ROUTES: u64 = 64;

/// Retained client sessions per group -- active **and** tombstoned (§12.1).
///
/// One cap covering both, not two. Capping active and tombstoned separately is
/// bypassable: create a session under the active cap, retire it (which adds no
/// row, so it was never capped), repeat -- permanent rows grow without bound and
/// the reserve proof is void.
pub const MAX_RETAINED_SESSIONS_PER_GROUP: u64 = 16_384;
/// Retained client sessions per actor -- active and tombstoned (§12.1).
pub const MAX_RETAINED_SESSIONS_PER_ACTOR: u64 = 256;
/// Active (non-retired) client ids per group (§12.1). A sub-cap inside
/// retained.
pub const MAX_ACTIVE_CLIENTS_PER_GROUP: u64 = 1_024;
/// Active (non-retired) client ids per actor (§12.1).
pub const MAX_ACTIVE_CLIENTS_PER_ACTOR: u64 = 8;

/// Minimum lease TTL (§12.1).
pub const MIN_TTL_MS: u64 = 5_000;
/// Maximum lease TTL (§12.1).
pub const MAX_TTL_MS: u64 = 300_000;

// ── Rights (§8.1) ───────────────────────────────────────────────────────────

/// Read the resource.
pub const RIGHT_READ: u64 = 0x01;
/// Acquire a lease.
pub const RIGHT_ACQUIRE: u64 = 0x02;
/// Renew a held lease.
pub const RIGHT_RENEW: u64 = 0x04;
/// Mutate the value.
pub const RIGHT_MUTATE: u64 = 0x08;
/// Release a held lease.
pub const RIGHT_RELEASE: u64 = 0x10;
/// Revoke another holder's lease.
pub const RIGHT_REVOKE: u64 = 0x20;

/// Every bit v1 defines. Any other bit is rejected (§8.1) -- an unknown bit is
/// not forward compatibility, it is a grant whose meaning this voter cannot
/// evaluate.
pub const ALL_RIGHTS: u64 =
    RIGHT_READ | RIGHT_ACQUIRE | RIGHT_RENEW | RIGHT_MUTATE | RIGHT_RELEASE | RIGHT_REVOKE;

/// §8.3: losing any of these fences an existing holder in the same applied
/// command. Losing only `READ` or `ACQUIRE` does not -- neither sustains a lease
/// already held.
pub const LEASE_SUSTAINING_RIGHTS: u64 = RIGHT_RENEW | RIGHT_MUTATE | RIGHT_RELEASE;

/// Does `rights` contain only bits v1 defines?
pub fn rights_are_known(rights: u64) -> bool {
    rights & !ALL_RIGHTS == 0
}

// ── Charges (§12.1), derived only from stored rows ──────────────────────────

/// The `KEY` term: `len(namespace) + len(key)`.
pub fn key_charge(namespace: &str, key: &[u8]) -> u64 {
    (namespace.len() + key.len()) as u64
}

/// `charge(resource row) = 256 + KEY + len(value)`.
pub fn charge_resource(namespace: &str, key: &[u8], value_len: usize) -> u64 {
    256 + key_charge(namespace, key) + value_len as u64
}

/// `charge(acl row) = 256 + KEY`.
pub fn charge_acl(namespace: &str, key: &[u8]) -> u64 {
    256 + key_charge(namespace, key)
}

/// `charge(admin row) = 128`.
pub fn charge_admin() -> u64 {
    128
}

/// `charge(intent row) = 1024 + len(encoded desired_plan)`.
pub fn charge_intent(encoded_plan_len: usize) -> u64 {
    1024 + encoded_plan_len as u64
}

/// `charge(route row) = 256 + len(ticket_bytes)`.
pub fn charge_route(ticket_len: usize) -> u64 {
    256 + ticket_len as u64
}

/// `charge(session row) = 256 + KEY`.
///
/// `KEY` comes from the resource key embedded in the row's saved reply, which is
/// a property of the **stored row** rather than of the command that wrote it --
/// the whole point of partitioning by table. A flat 320 bytes, as an earlier
/// revision had, cannot bound a `SavedSnapshotV1` carrying a 576-byte key.
pub fn charge_session(saved_key: Option<(&str, &[u8])>) -> u64 {
    256 + saved_key.map_or(0, |(ns, k)| key_charge(ns, k))
}

/// `charge(audit row) = 256 + KEY`.
pub fn charge_audit(resource: Option<(&str, &[u8])>) -> u64 {
    256 + resource.map_or(0, |(ns, k)| key_charge(ns, k))
}

/// The largest charge any reserve-funded row can carry: `256 + MAX_ROW_KEY`.
///
/// Session and audit rows share a charge formula, so one figure bounds both.
/// This is the multiplicand in the reserve worst case below.
pub const MAX_RESERVE_ROW_CHARGE: u64 = 256 + MAX_ROW_KEY;

/// The reserve must actually fit its worst case (§12.1):
///
/// ```text
/// audit     100,000 x (256 + 576) = 79.3 MiB
/// sessions   16,384 x (256 + 576) = 13.0 MiB
///                           total = 92.3 MiB <= 128 MiB
/// ```
const _RESERVE_FITS: () = assert!(
    (MAX_AUDIT_ROWS + MAX_RETAINED_SESSIONS_PER_GROUP) * MAX_RESERVE_ROW_CHARGE <= RESERVE_BUDGET,
    "the reserve budget cannot fund its own worst case"
);

/// Growth plus reserve must leave headroom under the local high-water for redb
/// overhead and a concurrent snapshot (§12.1: 320 MiB against 384 MiB).
const _BUDGETS_FIT_THE_STORE: () = assert!(
    GROWTH_BUDGET + RESERVE_BUDGET < LOCAL_HIGH_WATER,
    "growth + reserve leaves no headroom under the local high-water"
);

/// Active is a sub-cap inside retained, at both scopes.
const _ACTIVE_IS_INSIDE_RETAINED: () = assert!(
    MAX_ACTIVE_CLIENTS_PER_GROUP <= MAX_RETAINED_SESSIONS_PER_GROUP
        && MAX_ACTIVE_CLIENTS_PER_ACTOR <= MAX_RETAINED_SESSIONS_PER_ACTOR,
    "the active cap must sit inside the retained cap it is a sub-cap of"
);

#[cfg(test)]
mod tests {
    use super::*;

    /// Values transcribed from §12.1's table, so drift in the constants is
    /// caught against the specification rather than against themselves.
    #[test]
    fn the_constants_match_the_section_12_1_table() {
        assert_eq!(MAX_VALUE, 65_536);
        assert_eq!(MAX_KEY, 512);
        assert_eq!(MAX_NAMESPACE, 64);
        assert_eq!(
            MAX_TICKET, 4_095,
            "one byte under §12.1's 4 KiB, pinned by the codec's string limit"
        );
        assert_eq!(MAX_JOIN_PERMIT, 8_192);
        assert_eq!(MAX_ENCODED_COMMAND, 262_144);
        assert_eq!(GROWTH_BUDGET, 201_326_592);
        assert_eq!(RESERVE_BUDGET, 134_217_728);
        assert_eq!(MAX_RESOURCES_PER_GROUP, 100_000);
        assert_eq!(MAX_GRANTS_PER_RESOURCE, 256);
        assert_eq!(MAX_ACL_ROWS_PER_GROUP, 100_000);
        assert_eq!(MAX_ADMINS_PER_GROUP, 64);
        assert_eq!(MAX_AUDIT_ROWS, 100_000);
        assert_eq!(MAX_ACTIVE_CLIENTS_PER_ACTOR, 8);
        assert_eq!(MAX_ACTIVE_CLIENTS_PER_GROUP, 1_024);
        assert_eq!(MAX_RETAINED_SESSIONS_PER_ACTOR, 256);
        assert_eq!(MAX_RETAINED_SESSIONS_PER_GROUP, 16_384);
        assert_eq!(MAX_RETAINED_INTENTS, 64);
        assert_eq!(MAX_RETAINED_ROUTES, 64);
        assert_eq!((MIN_TTL_MS, MAX_TTL_MS), (5_000, 300_000));
        assert_eq!(MAX_ROW_KEY, 576);
    }

    /// The arithmetic §12.1 states, recomputed here in MiB so a regression is
    /// legible rather than a nine-digit byte count.
    #[test]
    fn the_reserve_fits_its_worst_case_with_the_stated_margin() {
        let per_row = 256 + MAX_ROW_KEY;
        let audit = MAX_AUDIT_ROWS * per_row;
        let sessions = MAX_RETAINED_SESSIONS_PER_GROUP * per_row;
        let total = audit + sessions;

        assert_eq!(per_row, 832);
        assert_eq!(audit / (1024 * 1024), 79, "audit is ~79.3 MiB");
        assert_eq!(sessions / (1024 * 1024), 13, "sessions are ~13.0 MiB");
        assert_eq!(total / (1024 * 1024), 92, "total is ~92.3 MiB");
        assert!(total <= RESERVE_BUDGET);
    }

    /// The regression these caps exist to prevent, pinned as arithmetic.
    ///
    /// The superseded model had **two** session caps -- 1,024 active plus 65,536
    /// tombstoned -- and asserted the reserve fitted. It does not:
    ///
    /// ```text
    /// audit     100,000            x 832 = 79.3 MiB
    /// sessions  (1,024 + 65,536)   x 832 = 52.8 MiB
    ///                              total = 132.2 MiB > 128 MiB
    /// ```
    ///
    /// Both halves of the change matter and the test covers them separately:
    /// the count came down to 16,384, *and* the two caps became one. The
    /// 65,536 figure alone is 131.3 MiB -- also over, but not the quoted number,
    /// because the old model added the active cap on top rather than nesting it
    /// inside. Keeping the failing arithmetic here means restoring either half
    /// is a test failure rather than a silent overcommit.
    #[test]
    fn the_superseded_two_cap_session_model_did_not_fit_the_reserve() {
        let per_row = 256 + MAX_ROW_KEY;
        let audit = MAX_AUDIT_ROWS * per_row;

        // The old model as it actually stood: separate active and tombstone
        // caps, summed.
        let old_sessions = (1_024 + 65_536) * per_row;
        let old_total = audit + old_sessions;
        assert_eq!(
            old_sessions / (1024 * 1024),
            52,
            "old sessions are ~52.8 MiB"
        );
        assert_eq!(
            old_total / (1024 * 1024),
            132,
            "the old total is ~132.2 MiB"
        );
        assert!(
            old_total > RESERVE_BUDGET,
            "the two-cap model overcommits the reserve by ~4.2 MiB"
        );

        // And the count alone, without the second cap, is still over.
        let retained_only = audit + 65_536 * per_row;
        assert_eq!(retained_only / (1024 * 1024), 131);
        assert!(
            retained_only > RESERVE_BUDGET,
            "65,536 retained sessions overcommit the reserve even as one cap"
        );
    }

    #[test]
    fn the_rights_bits_match_section_8_1() {
        assert_eq!(RIGHT_READ, 0x01);
        assert_eq!(RIGHT_ACQUIRE, 0x02);
        assert_eq!(RIGHT_RENEW, 0x04);
        assert_eq!(RIGHT_MUTATE, 0x08);
        assert_eq!(RIGHT_RELEASE, 0x10);
        assert_eq!(RIGHT_REVOKE, 0x20);
        assert_eq!(ALL_RIGHTS, 0x3f);
    }

    #[test]
    fn unknown_rights_bits_are_rejected() {
        assert!(rights_are_known(0));
        assert!(rights_are_known(ALL_RIGHTS));
        assert!(rights_are_known(RIGHT_READ | RIGHT_REVOKE));
        assert!(!rights_are_known(0x40), "the first undefined bit");
        assert!(!rights_are_known(ALL_RIGHTS | 0x80));
        assert!(!rights_are_known(u64::MAX));
    }

    /// §8.3: only `RENEW`, `MUTATE` and `RELEASE` sustain a held lease.
    #[test]
    fn only_the_three_sustaining_rights_fence_a_holder() {
        for right in [RIGHT_RENEW, RIGHT_MUTATE, RIGHT_RELEASE] {
            assert_ne!(LEASE_SUSTAINING_RIGHTS & right, 0);
        }
        for right in [RIGHT_READ, RIGHT_ACQUIRE, RIGHT_REVOKE] {
            assert_eq!(
                LEASE_SUSTAINING_RIGHTS & right,
                0,
                "losing this right must not fence a holder"
            );
        }
    }

    /// Charges asserted against the literal formulas in §12.1, computed by hand
    /// rather than by calling the functions under test.
    #[test]
    fn the_charge_formulas_match_the_declared_arithmetic() {
        assert_eq!(charge_resource("ns", b"key", 100), 256 + 2 + 3 + 100);
        assert_eq!(charge_acl("ns", b"key"), 256 + 2 + 3);
        assert_eq!(charge_admin(), 128);
        assert_eq!(charge_intent(500), 1024 + 500);
        assert_eq!(charge_route(4096), 256 + 4096);
        assert_eq!(charge_session(Some(("ns", b"key"))), 256 + 2 + 3);
        assert_eq!(charge_audit(Some(("ns", b"key"))), 256 + 2 + 3);
    }

    /// A session or audit row with no resource key still costs its fixed
    /// overhead. Charging zero would let an unbounded number of them ride free.
    #[test]
    fn a_row_with_no_resource_key_still_carries_its_fixed_overhead() {
        assert_eq!(charge_session(None), 256);
        assert_eq!(charge_audit(None), 256);
    }

    /// The session charge must include `KEY`, which is what the flat 320-byte
    /// charge of an earlier revision could not do.
    #[test]
    fn the_session_charge_bounds_a_maximum_key_where_a_flat_320_would_not() {
        let ns = "n".repeat(MAX_NAMESPACE);
        let key = vec![b'k'; MAX_KEY];
        let charge = charge_session(Some((&ns, &key)));
        assert_eq!(charge, 256 + MAX_ROW_KEY);
        assert!(
            charge > 320,
            "a 576-byte key does not fit the old flat 320-byte session charge"
        );
    }

    /// Every variable-length field a row can carry must appear in its charge,
    /// or the counter underestimates the store.
    #[test]
    fn every_variable_length_field_moves_its_rows_charge() {
        assert!(charge_resource("ns", b"k", 1) > charge_resource("ns", b"k", 0));
        assert!(charge_resource("ns", b"kk", 0) > charge_resource("ns", b"k", 0));
        assert!(charge_resource("nss", b"k", 0) > charge_resource("ns", b"k", 0));
        assert!(charge_acl("ns", b"kk") > charge_acl("ns", b"k"));
        assert!(charge_intent(1) > charge_intent(0));
        assert!(charge_route(1) > charge_route(0));
        assert!(charge_session(Some(("n", b""))) > charge_session(None));
        assert!(charge_audit(Some(("n", b""))) > charge_audit(None));
    }
}
