//! The two reserved coordination services: `aster.coordination.Store` v1
//! (§5.2) and `aster.coordination.Raft` v1 (§5.3).
//!
//! Slice 0 reserves them; it does not implement them. What a reservation buys
//! is not a placeholder:
//!
//! * **The names are taken.** `(service, version)` is the registry key, so a
//!   later slice adding the real dispatcher cannot silently collide with an
//!   application service that squatted the name in the meantime.
//! * **The frame caps are installed.** `max_frame_size` is read by the
//!   dispatcher's `FrameLimitResolver` *before* a body frame is read, so the
//!   §12.4 seam is enforced for these services from the moment they are
//!   registered -- with no handler in existence. A cap that only appears once
//!   the handler does would leave the whole implementation window on the 16 MiB
//!   default.
//! * **`iroh_only` is on.** A voter's identity is its authority (§3.3), so
//!   neither service is reachable over a transport that cannot authenticate the
//!   peer. Declaring it here rather than in the eventual handler means no
//!   revision of this subsystem can forget it.
//!
//! Every declared method answers `UNIMPLEMENTED` with a message naming the
//! reservation. That is deliberately distinguishable from the generic
//! `"method '<svc>.<m>' not implemented"` that dispatch emits for a method
//! outside the table: the two say different things -- "this is reserved and
//! coming" versus "no such method" -- and the tests depend on telling them
//! apart to prove the reserved dispatcher actually ran.

use crate::rpc::{Call, RpcStatus, ServiceDispatch, StatusCode};

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

/// Finish `call` with the reserved-service trailer.
async fn reserved(service: &str, method: &str, call: Call) {
    let _ = call
        .finish(&RpcStatus::error(
            StatusCode::Unimplemented,
            format!("{service} v1 reserves '{method}'; it is not implemented yet"),
        ))
        .await;
}

/// The reserved `aster.coordination.Store` v1 service.
///
/// Registered by
/// [`builtin_coordination`](crate::rpc::AsterServerBuilder::builtin_coordination)
/// and by nothing else, so a server that did not ask to host coordination does
/// not answer for the name at all.
pub struct ReservedStore;

#[async_trait::async_trait]
impl ServiceDispatch for ReservedStore {
    fn name(&self) -> &str {
        STORE_SERVICE
    }

    fn version(&self) -> i32 {
        STORE_VERSION
    }

    fn methods(&self) -> &[&str] {
        STORE_METHODS
    }

    fn iroh_only(&self) -> bool {
        true
    }

    /// 2 MiB for every method, including one not in the table.
    ///
    /// Returning `None` for an unrecognised method would hand it the 16 MiB
    /// global default -- a larger allocation for a method this service does not
    /// have than for one it does. The resolver runs on header bytes before
    /// dispatch has rejected anything, so this is the only place that ordering
    /// can be made safe.
    fn max_frame_size(&self, _method: &str) -> Option<u32> {
        Some(STORE_FRAME_MAX)
    }

    async fn dispatch(&self, method: &str, call: Call) {
        reserved(STORE_SERVICE, method, call).await;
    }
}

/// The reserved `aster.coordination.Raft` v1 service.
pub struct ReservedRaft;

#[async_trait::async_trait]
impl ServiceDispatch for ReservedRaft {
    fn name(&self) -> &str {
        RAFT_SERVICE
    }

    fn version(&self) -> i32 {
        RAFT_VERSION
    }

    fn methods(&self) -> &[&str] {
        RAFT_METHODS
    }

    fn iroh_only(&self) -> bool {
        true
    }

    /// 8 MiB for the unary pair, 2 MiB for snapshot chunks and for anything
    /// unrecognised (see [`ReservedStore::max_frame_size`] on why the fallback
    /// is the smaller cap rather than the global default).
    fn max_frame_size(&self, method: &str) -> Option<u32> {
        match method {
            "vote" | "append_entries" => Some(RAFT_UNARY_FRAME_MAX),
            _ => Some(SNAPSHOT_CHUNK_FRAME_MAX),
        }
    }

    async fn dispatch(&self, method: &str, call: Call) {
        reserved(RAFT_SERVICE, method, call).await;
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

    /// The caps are asserted as literal byte counts rather than against the
    /// constants they came from -- an oracle that reads the same constant
    /// proves only that the constant equals itself.
    #[test]
    fn the_declared_frame_caps_are_the_sizes_section_12_3_fixes() {
        assert_eq!(ReservedStore.max_frame_size("acquire"), Some(2_097_152));
        assert_eq!(ReservedRaft.max_frame_size("vote"), Some(8_388_608));
        assert_eq!(
            ReservedRaft.max_frame_size("append_entries"),
            Some(8_388_608)
        );
        assert_eq!(
            ReservedRaft.max_frame_size("install_snapshot"),
            Some(2_097_152),
            "a snapshot chunk must not inherit the 8 MiB unary cap"
        );
    }

    /// An unrecognised method must land on the service's smallest cap, never on
    /// the 16 MiB global default.
    #[test]
    fn an_unrecognised_method_gets_the_smaller_cap_not_the_global_default() {
        assert_eq!(
            ReservedStore.max_frame_size("no_such_method"),
            Some(2_097_152)
        );
        assert_eq!(
            ReservedRaft.max_frame_size("no_such_method"),
            Some(2_097_152)
        );
    }

    #[test]
    fn both_reserved_services_are_iroh_only() {
        assert!(ReservedStore.iroh_only());
        assert!(ReservedRaft.iroh_only());
    }
}
