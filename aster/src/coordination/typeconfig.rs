//! The OpenRaft type configuration (§15 Slice 0).
//!
//! This module exists to *hold the build honest*: it names the concrete types
//! OpenRaft will be instantiated with, so a mismatch is a compile error here
//! rather than a surprise in Slice 2 when storage lands.
//!
//! Two constraints the spike measured and this file depends on:
//!
//! - `AppData`, `AppDataResponse`, `NodeId` and `Node` are **blanket**
//!   implemented. Writing manual impls is `E0119`, so this file declares no
//!   impls for them and relies on the blanket bounds being satisfied.
//! - Those blanket bounds include `OptionalSerde`, which is a no-op only while
//!   openraft's `serde` feature is off. Enabling it anywhere in the graph
//!   silently adds `Serialize`/`Deserialize` bounds to `CommandV1` and
//!   `CommandReplyV1`, which they do not implement — Fory is the codec here.

use super::contract::{CommandReplyV1, CommandV1};
use super::ids::CoordinationNodeId;
use super::storage::CoordinationSnapshotFile;

/// The node record OpenRaft stores alongside each member id.
///
/// Empty in v1: addressing is iroh's job, and a `NodeId` is already an Ed25519
/// public key, so there is no endpoint to carry here. Declared as its own type
/// rather than `()` so a future version can add fields without changing the
/// type config's shape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoordinationNode;

openraft::declare_raft_types!(
    /// The v1 coordination type config.
    pub CoordinationTypeConfig:
        D = CommandV1,
        R = CommandReplyV1,
        NodeId = CoordinationNodeId,
        Node = CoordinationNode,
        Entry = openraft::Entry<CoordinationTypeConfig>,
        // File-backed, and role-safe. `generic-snapshot-data` reduces
        // OpenRaft's bound to `OptionalSend + 'static`, so the handle is
        // opaque to OpenRaft and every byte of I/O is Aster's — including the
        // ownership rules, which OpenRaft's own storage suite exercises by
        // moving one store's published handle into another store. A
        // `Cursor<Vec<u8>>` would additionally hold a whole snapshot body in
        // memory, which §11.4 forbids.
        SnapshotData = CoordinationSnapshotFile,
        AsyncRuntime = openraft::TokioRuntime,
);

/// `openraft::testing::Suite` bounds `C::NodeId: From<u64>` **on the impl**, so
/// a `cfg(test)` impl on the real id satisfies it and the conformance suite
/// runs against the *production* type config.
///
/// This is deliberately not a shipping impl. An earlier proposal was a parallel
/// `TestNodeId` and a second type config, which would have conformance-tested a
/// type the voter never uses. Because this is `cfg(test)`, no shipping path can
/// mint a NodeId from an integer — which matters, since a NodeId is an Ed25519
/// public key and an integer-derived one corresponds to no key at all.
///
/// The mapping is `u64` big-endian in the low 8 bytes, zero-padded.
#[cfg(test)]
impl From<u64> for CoordinationNodeId {
    fn from(n: u64) -> Self {
        let mut bytes = [0u8; 32];
        bytes[24..].copy_from_slice(&n.to_be_bytes());
        CoordinationNodeId::from_array(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The type config compiles at all — which is the point of the module. If
    /// openraft's blanket bounds ever stop covering these types, this fails to
    /// build rather than failing in Slice 2.
    #[test]
    fn type_config_is_instantiable() {
        fn assert_type_config<C: openraft::RaftTypeConfig>() {}
        assert_type_config::<CoordinationTypeConfig>();
    }

    /// The `From<u64>` mapping is big-endian in the low 8 bytes. Pinned because
    /// the conformance suite generates ids this way, and a little-endian or
    /// high-byte mapping would still "work" while making suite failures
    /// unreadable.
    #[test]
    fn test_only_node_id_mapping_is_big_endian_low_bytes() {
        let id = CoordinationNodeId::from(1u64);
        assert_eq!(id.as_bytes()[..24], [0u8; 24]);
        assert_eq!(id.as_bytes()[24..], [0, 0, 0, 0, 0, 0, 0, 1]);

        let id = CoordinationNodeId::from(u64::MAX);
        assert_eq!(id.as_bytes()[24..], [0xff; 8]);

        // Distinct integers give distinct ids, which the suite relies on to
        // tell members apart.
        assert_ne!(
            CoordinationNodeId::from(1u64),
            CoordinationNodeId::from(2u64)
        );
    }

    /// Guard the `serde` contract from §15: if openraft's `serde` feature is
    /// ever enabled in this graph, `CommandV1` would need `Serialize`, which it
    /// does not implement. This test states the expectation in code so the
    /// reason for the failure is discoverable when it happens.
    #[test]
    fn openraft_serde_is_off() {
        // `openraft::entry::Entry` is constructible without any serde bound
        // only while the feature is off. If this stops compiling, the feature
        // was turned on somewhere in the graph.
        fn requires_no_serde<T: openraft::AppData>() {}
        requires_no_serde::<CommandV1>();
        fn reply_requires_no_serde<T: openraft::AppDataResponse>() {}
        reply_requires_no_serde::<CommandReplyV1>();
    }
}
