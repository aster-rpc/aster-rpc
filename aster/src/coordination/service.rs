//! The two coordination services' contract constants: `aster.coordination.Store`
//! v1 (§5.2) and `aster.coordination.Raft` v1 (§5.3).
//!
//! The names, method tables and per-method frame caps are declared here, in
//! one place, and the live dispatchers honour them rather than restating them:
//! [`store::host`](crate::coordination::store::host) serves the Store and
//! [`network::service`](crate::coordination::network::service) serves the
//! Raft, and each takes its caps from [`store_frame_max`] and
//! [`raft_frame_max`] below. What the declaration buys:
//!
//! * **The names are taken.** `(service, version)` is the registry key, so an
//!   application service cannot squat a coordination name.
//! * **The frame caps are enforced before a body is read.** `max_frame_size`
//!   is read by the dispatcher's `FrameLimitResolver` *before* a body frame is
//!   read, so the §12.4 seam holds from the moment a service is registered.
//!   The caps are keyed per **method**, not per service: a service-wide 8 MiB
//!   would let a snapshot sender allocate four times its own declared cap.
//! * **`iroh_only` is on** for both. A voter's identity is its authority
//!   (§3.3), so neither service is reachable over a transport that cannot
//!   authenticate the peer; each live dispatcher asserts it in its own tests.
//!
//! Historically this module also carried two placeholder dispatchers that
//! answered every method `UNIMPLEMENTED` while the runtime did not exist. The
//! runtime has existed since Slice 3 and nothing registered them; they are
//! gone, and the live services' tests compare against these constants and
//! functions instead.

/// The reserved store service name (§5.2).
pub const STORE_SERVICE: &str = "aster.coordination.Store";
/// The reserved store service version.
pub const STORE_VERSION: i32 = 1;

/// The reserved Raft service name (§5.3). Voter-internal: applications never
/// call it.
pub const RAFT_SERVICE: &str = "aster.coordination.Raft";
/// The reserved Raft service version.
pub const RAFT_VERSION: i32 = 1;

/// Every method of `aster.coordination.Store` v1, in §5.2 table order.
///
/// Declared in full at Slice 0 because the method table is what dispatch uses
/// to reject an unknown method before a handler runs, and what the frame-limit
/// resolver keys on. A partial table would make an unimplemented-but-reserved
/// method indistinguishable from a typo.
pub const STORE_METHODS: &[&str] = &[
    "acquire",
    "renew",
    "read_linearizable",
    "compare_and_set",
    "release",
    "release_with_value",
    "revoke",
    "set_grant",
    "set_admin",
    "change_membership",
    "cancel_membership",
    "membership_status",
    "inspect_acl",
    "inspect_admins",
    "retire_client",
    "probe_client",
    "group_status",
    "audit_stream",
    "recovery_export",
];

/// Every method of `aster.coordination.Raft` v1 (§5.3).
pub const RAFT_METHODS: &[&str] = &["vote", "append_entries", "install_snapshot"];

/// Store request frame maximum (§12.3): 2 MiB.
///
/// Strictly above the largest payload it must carry -- a value-carrying
/// `CompareAndSet` reaches ~64 KiB and the 256 KiB `max_encoded_command` bound
/// covers every `ActionV1` variant -- so the frame cap is a containment rule,
/// not a second payload limit.
pub const STORE_FRAME_MAX: u32 = 2 * 1024 * 1024;

/// Raft unary frame maximum (§12.3): 8 MiB, for `vote` and `append_entries`.
pub const RAFT_UNARY_FRAME_MAX: u32 = 8 * 1024 * 1024;

/// Snapshot / export chunk frame maximum (§12.3): 2 MiB, for
/// `install_snapshot`.
///
/// This is why the §12.4 resolver is keyed per **method** and not per service:
/// a service-wide 8 MiB would let a snapshot sender allocate four times its own
/// declared cap.
pub const SNAPSHOT_CHUNK_FRAME_MAX: u32 = 2 * 1024 * 1024;

/// The Store's frame cap for `method`: 2 MiB for every method, including one
/// not in the table.
///
/// Returning `None` for an unrecognised method would hand it the 16 MiB
/// global default -- a larger allocation for a method this service does not
/// have than for one it does. The resolver runs on header bytes before
/// dispatch has rejected anything, so this is the only place that ordering
/// can be made safe.
pub fn store_frame_max(_method: &str) -> Option<u32> {
    Some(STORE_FRAME_MAX)
}

/// The Raft service's frame cap for `method`: 8 MiB for the unary pair,
/// 2 MiB for snapshot chunks and for anything unrecognised (see
/// [`store_frame_max`] on why the fallback is the smaller cap rather than the
/// global default).
pub fn raft_frame_max(method: &str) -> Option<u32> {
    match method {
        "vote" | "append_entries" => Some(RAFT_UNARY_FRAME_MAX),
        _ => Some(SNAPSHOT_CHUNK_FRAME_MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The §5.2 table has 19 methods and the §5.3 one has 3. Counting them is
    /// a weak check on its own; what it catches is a merge that drops or
    /// duplicates a row while the names still look plausible.
    #[test]
    fn the_method_tables_match_the_declared_contracts() {
        assert_eq!(STORE_METHODS.len(), 19, "§5.2 declares 19 store methods");
        assert_eq!(RAFT_METHODS.len(), 3, "§5.3 declares 3 raft methods");

        let mut sorted = STORE_METHODS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            STORE_METHODS.len(),
            "a duplicated method name silently shadows a contract row"
        );
    }

    /// The §12.3 caps, per method, as literals: the constants are what the
    /// live services read, so a moved constant shows here as a moved number.
    #[test]
    fn the_frame_caps_are_the_declared_ones() {
        assert_eq!(store_frame_max("acquire"), Some(2_097_152));
        assert_eq!(raft_frame_max("vote"), Some(8_388_608));
        assert_eq!(raft_frame_max("append_entries"), Some(8_388_608));
        assert_eq!(raft_frame_max("install_snapshot"), Some(2_097_152));
    }

    /// An unknown method gets the *smaller* cap, never the global default.
    #[test]
    fn an_unknown_method_never_gets_the_global_default() {
        assert_eq!(store_frame_max("no_such_method"), Some(STORE_FRAME_MAX));
        assert_eq!(
            raft_frame_max("no_such_method"),
            Some(SNAPSHOT_CHUNK_FRAME_MAX)
        );
    }
}
