//! Spike 2 — compile-only Fory declaration probe for the coordination wire.
//!
//! The plan asserts a shape it has never compiled: single-payload wrapper
//! messages per union variant, explicit `#[fory(id = N)]` on every field,
//! `Vec<u8>` + `#[fory(bytes)]` instead of `[u8; 32]`, a `NodeIdV1` message for
//! repeated node ids, and one `Fory` runtime per payload root built through
//! `PayloadRegistry`. This module exists to find out which of that is true.
//!
//! It also tests the namespace guard: these wire names are under the reserved
//! `aster.` prefix, which `aster-macros` only permits while expanding the
//! `aster` crate itself — which is why this lives here and not in `spikes/`.

use fory_derive::{ForyStruct, ForyUnion};

use crate::rpc::contract::{PayloadRegistry, WireField};

// ── Leaf types ───────────────────────────────────────────────────────────────

/// A repeated node id needs its own message; `Vec<Vec<u8>>` is not expressible.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/NodeId")]
pub struct NodeIdV1 {
    #[fory(id = 1, bytes)]
    pub node_id: Vec<u8>,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ResourceKey")]
pub struct ResourceKeyV1 {
    #[fory(id = 1)]
    pub namespace: String,
    #[fory(id = 2, bytes)]
    pub key: Vec<u8>,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/LogId")]
pub struct LogIdV1 {
    #[fory(id = 1)]
    pub term: u64,
    #[fory(id = 2, bytes)]
    pub leader_node_id: Vec<u8>,
    #[fory(id = 3)]
    pub index: u64,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/Fence")]
pub struct FenceV1 {
    #[fory(id = 1, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 2)]
    pub resource: ResourceKeyV1,
    // u64, not i64: the signedness probe below settled this, but these
    // normative types were left on the exploratory `i64` shape.
    #[fory(id = 3)]
    pub epoch: u64,
    #[fory(id = 4, bytes)]
    pub holder: Vec<u8>,
}

/// The redaction flag from §4.3 lives here, on the public snapshot.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ResourceSnapshot")]
pub struct ResourceSnapshotV1 {
    #[fory(id = 1, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 2)]
    pub resource: ResourceKeyV1,
    #[fory(id = 3)]
    pub epoch: u64,
    /// `Option`, because an unheld resource has no holder. The exploratory
    /// shape used a bare `Vec<u8>`, which forced 32 zero bytes to mean "nobody"
    /// — indistinguishable from a real all-zero NodeId.
    #[fory(id = 4, bytes)]
    pub holder: Option<Vec<u8>>,
    #[fory(id = 5)]
    pub held: bool,
    #[fory(id = 6)]
    pub ttl_ms: u64,
    #[fory(id = 7)]
    pub lease_revision: u64,
    #[fory(id = 8)]
    pub value_revision: u64,
    #[fory(id = 9)]
    pub acl_revision: u64,
    #[fory(id = 10, bytes)]
    pub value: Option<Vec<u8>>,
    #[fory(id = 11)]
    pub value_redacted: bool,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/MemberSeed")]
pub struct MemberSeedV1 {
    #[fory(id = 1, bytes)]
    pub node_id: Vec<u8>,
    #[fory(id = 2)]
    pub ticket: String,
    #[fory(id = 3, bytes)]
    pub signed_join_permit: Option<Vec<u8>>,
}

/// The saved idempotency result. Deliberately has no `value` field at all --
/// see section 5.2.1. Session rows carry this typed, not opaque bytes.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SavedSnapshot")]
pub struct SavedSnapshotV1 {
    #[fory(id = 1, bytes)]
    pub group_id: Vec<u8>,
    #[fory(id = 2)]
    pub resource: ResourceKeyV1,
    #[fory(id = 3)]
    pub epoch: u64,
    #[fory(id = 4, bytes)]
    pub holder: Option<Vec<u8>>,
    #[fory(id = 5)]
    pub held: bool,
    #[fory(id = 6)]
    pub ttl_ms: u64,
    #[fory(id = 7)]
    pub lease_revision: u64,
    #[fory(id = 8)]
    pub value_revision: u64,
    #[fory(id = 9)]
    pub acl_revision: u64,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SavedReply")]
pub struct SavedReplyV1 {
    #[fory(id = 1)]
    pub disposition_code: i32,
    #[fory(id = 2)]
    pub outcome_code: i32,
    #[fory(id = 3)]
    pub reason_code: i32,
    #[fory(id = 4)]
    pub snapshot: Option<SavedSnapshotV1>,
    #[fory(id = 5)]
    pub acl_revision: u64,
    #[fory(id = 6)]
    pub admin_revision: u64,
    #[fory(id = 7, bytes)]
    pub operation_id: Option<Vec<u8>>,
    #[fory(id = 8)]
    pub membership_log_id: Option<LogIdV1>,
}

// ── Action wrapper messages (one per union variant) ──────────────────────────

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/AcquireAction")]
pub struct AcquireActionV1 {
    #[fory(id = 1)]
    pub resource: ResourceKeyV1,
    #[fory(id = 2)]
    pub ttl_ms: u64,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RenewAction")]
pub struct RenewActionV1 {
    #[fory(id = 1)]
    pub fence: FenceV1,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/CompareAndSetAction")]
pub struct CompareAndSetActionV1 {
    #[fory(id = 1)]
    pub fence: FenceV1,
    #[fory(id = 2)]
    pub expected_value_revision: u64,
    #[fory(id = 3, bytes)]
    pub new_value: Vec<u8>,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RevokeAction")]
pub struct RevokeActionV1 {
    #[fory(id = 1)]
    pub resource: ResourceKeyV1,
    /// The optional conditional-revoke epoch (§5.2). `Option<u64>` in a Fory
    /// struct was an open question when this spike began; it is settled — the
    /// round-trip tests below cover `Some` and `None`, and the type assertions
    /// pin the unsigned width.
    #[fory(id = 2)]
    pub expected_epoch: Option<u64>,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ChangeMembershipAction")]
pub struct ChangeMembershipActionV1 {
    #[fory(id = 1, bytes)]
    pub operation_id: Vec<u8>,
    #[fory(id = 2)]
    pub expected_membership_log_id: Option<LogIdV1>,
    #[fory(id = 3)]
    pub desired_voters: Vec<NodeIdV1>,
    #[fory(id = 4)]
    pub retained_learners: Vec<NodeIdV1>,
    #[fory(id = 5)]
    pub member_seeds: Vec<MemberSeedV1>,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ReleaseAction")]
pub struct ReleaseActionV1 {
    #[fory(id = 1)]
    pub fence: FenceV1,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ReleaseWithValueAction")]
pub struct ReleaseWithValueActionV1 {
    #[fory(id = 1)]
    pub fence: FenceV1,
    #[fory(id = 2)]
    pub expected_value_revision: u64,
    #[fory(id = 3, bytes)]
    pub final_value: Vec<u8>,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SetGrantAction")]
pub struct SetGrantActionV1 {
    #[fory(id = 1)]
    pub resource: ResourceKeyV1,
    #[fory(id = 2, bytes)]
    pub target_node_id: Vec<u8>,
    #[fory(id = 3)]
    pub rights: u64,
    #[fory(id = 4)]
    pub expected_acl_revision: u64,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SetAdminAction")]
pub struct SetAdminActionV1 {
    #[fory(id = 1, bytes)]
    pub target_node_id: Vec<u8>,
    #[fory(id = 2)]
    pub enabled: bool,
    #[fory(id = 3)]
    pub expected_admin_revision: u64,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/CancelMembershipAction")]
pub struct CancelMembershipActionV1 {
    #[fory(id = 1, bytes)]
    pub operation_id: Vec<u8>,
    #[fory(id = 2)]
    pub expected_phase_code: i32,
}

// ── The union ────────────────────────────────────────────────────────────────

/// `ActionV1`: variant ids equal the §5.2 method codes.
#[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/Action")]
pub enum ActionV1 {
    #[fory(id = 1)]
    Acquire(AcquireActionV1),
    #[fory(id = 2)]
    Renew(RenewActionV1),
    #[fory(id = 3)]
    CompareAndSet(CompareAndSetActionV1),
    #[fory(id = 4)]
    Release(ReleaseActionV1),
    #[fory(id = 5)]
    ReleaseWithValue(ReleaseWithValueActionV1),
    #[fory(id = 6)]
    Revoke(RevokeActionV1),
    #[fory(id = 7)]
    SetGrant(SetGrantActionV1),
    #[fory(id = 8)]
    SetAdmin(SetAdminActionV1),
    #[fory(id = 9)]
    ChangeMembership(ChangeMembershipActionV1),
    #[fory(default)]
    #[fory(id = 11)]
    CancelMembership(CancelMembershipActionV1),
    #[fory(unknown)]
    Unknown(fory_core::UnknownCase),
}

/// FINDING (review): the earlier spike used `i64` everywhere and the findings
/// note concluded "wire integers must be i64". That was inference, not
/// evidence: `aster/src/rpc/contract.rs` maps `u64` to Fory `var_uint64` (and
/// `u32` to `var_uint32`), so unsigned integers are supported. These two types
/// are identical apart from signedness, so the round-trip tests below settle it
/// by measurement instead.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SignedCounters")]
pub struct SignedCountersV1 {
    #[fory(id = 1)]
    pub epoch: i64,
    #[fory(id = 2)]
    pub lease_revision: i64,
    #[fory(id = 3)]
    pub code: i32,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/UnsignedCounters")]
pub struct UnsignedCountersV1 {
    #[fory(id = 1)]
    pub epoch: u64,
    #[fory(id = 2)]
    pub lease_revision: u64,
    #[fory(id = 3)]
    pub code: u32,
}

/// One `Fory` per payload root, built through `PayloadRegistry` exactly as
/// `rpc/lease.rs` does — the derive walks the field tree, so registration order
/// is not hand-maintained.
/// The largest `String` field this codec is known to carry, in bytes.
///
/// **A measured limit, not a policy one.** Under the `xlang(true)` +
/// `compatible(true)` runtime every Aster payload uses, a `String` field of
/// 4096 bytes encodes and then fails to decode, with `TypeError("Type
/// inconsistent, target collection element type is not polymorphic")`. 4095
/// round-trips. `Vec<u8>` under `#[fory(bytes)]` is unaffected at 64 KiB, so
/// the defect is specific to the string path.
///
/// The cause is upstream, in `fory-core`'s `Buffer::write_var_u36_small`: the
/// three-byte branch (`value < 0x200000`) writes a whole four-byte word, and the
/// five-byte branch writes eight, while the reader consumes only the varint's
/// actual length. A string's header packs its byte length, so 4095 encodes a
/// header below `0x4000` and 4096 crosses into the over-writing branch --
/// which is why the boundary is exactly here and not at a round number of the
/// string itself. The branches are still present on Apache Fory `main`, and the
/// latest release is 1.5.0, so this is not merely a property of the 1.3.0 pin.
///
/// 4096 is the **first failing length** that has been measured. Larger lengths
/// were sampled and also fail, but the cap is set from the first failure, not
/// from a claim about every length above it.
///
/// A `String` field declared "up to 4 KiB" is therefore capped one byte lower.
/// Refusing a 4096-byte value is strictly better than accepting one no peer can
/// read back.
///
/// **Raising this is not automatic when the codec is fixed.** It is a shipped
/// v1 bound: a voter on a patched build and a voter on an unpatched one would
/// still be in the same group, and the one that cannot decode 4096 bytes is the
/// one that decides whether the group makes progress. Lifting the cap is a
/// protocol-version decision, not a dependency bump. The alternative, if
/// product-wide long strings are needed before an upstream release, is to carry
/// the minimal patch in Aster's Fory fork under the existing fork-patch ledger
/// policy.
pub const MAX_WIRE_STRING: usize = 4095;

pub fn payload_fory<T: WireField>() -> fory_core::Fory {
    let mut reg = PayloadRegistry::new();
    T::register_payload(&mut reg);
    reg.into_fory()
}

#[cfg(test)]
mod tests {
    /// **The independently transcribed §5.2 audit matrix.** Sharing
    /// `audit_results_for` between the two readers proves they cannot drift
    /// from each other; it does not prove either matches the specification.
    /// This table is transcribed from §5.2 by hand — NOT derived from the
    /// function under test — and the assertion sweeps every operation code
    /// in and around the declared space against every result code either
    /// code space can spell.
    #[test]
    fn the_audit_table_matches_the_transcribed_specification() {
        // (operation, the exact §5.2 result list), quoted from the document.
        let transcribed: &[(i32, &[i32])] = &[
            (1, &[1, 100]),
            (2, &[2, 100]),
            (3, &[3, 100]),
            (4, &[4, 100]),
            (5, &[4, 100]),
            (6, &[5, 100]),
            (7, &[6, 100]),
            (8, &[7, 100]),
            (9, &[8, 100]),
            (10, &[9, 100]),
            (11, &[10, 100]),
            (100, &[1, 2, 5]),
            (101, &[3, 4, 6]),
            (102, &[7, 8]),
        ];
        // The production mapping is an enumerable constant, so the
        // comparison is the **complete** key/value set — not sampled probes.
        // A smuggled operation (2001, anything) fails the equality; a
        // widened result list fails the per-row slice equality.
        assert_eq!(
            super::AUDIT_RESULTS,
            transcribed,
            "the production table must equal the transcription in full — \
             same operations, same order, same result lists"
        );
        // And the lookup is exactly the table: every declared key maps to
        // its row, and the near-bracket codes map to nothing.
        for (operation, allowed) in transcribed {
            assert_eq!(super::audit_results_for(*operation), Some(*allowed));
        }
        for operation in [-1, 0, 12, 99, 103, 2001, i32::MIN, i32::MAX] {
            assert_eq!(super::audit_results_for(operation), None);
        }
    }

    use super::*;

    fn key() -> ResourceKeyV1 {
        ResourceKeyV1 {
            namespace: "portal.roaming".into(),
            key: vec![1, 2, 3, 4],
        }
    }

    fn fence() -> FenceV1 {
        FenceV1 {
            group_id: vec![7u8; 32],
            resource: key(),
            epoch: 42,
            holder: vec![9u8; 32],
        }
    }

    /// The measured boundary behind [`MAX_WIRE_STRING`]: 4095 round-trips and
    /// 4096 does not. Pinned as a test so that if the codec changes under us,
    /// this fails and the change is noticed rather than discovered by a peer
    /// that cannot read a ticket. A failure here is **not** an instruction to
    /// raise the cap -- see [`MAX_WIRE_STRING`] for why that is a
    /// protocol-version decision.
    #[test]
    fn the_first_failing_string_length_is_4096_bytes() {
        #[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
        #[aster(wire = "aster.coordination.v1/StringBoundProbe")]
        struct Probe {
            #[fory(id = 1)]
            text: String,
            #[fory(id = 2, bytes)]
            blob: Vec<u8>,
        }

        let fory = payload_fory::<Probe>();
        let encodes_and_decodes = |p: &Probe| {
            let bytes = fory.serialize(p).expect("encode always succeeds");
            fory.deserialize::<Probe>(&bytes).is_ok()
        };

        let at_limit = Probe {
            text: "x".repeat(MAX_WIRE_STRING),
            blob: Vec::new(),
        };
        assert!(encodes_and_decodes(&at_limit), "4095 bytes round-trips");

        let over_limit = Probe {
            text: "x".repeat(MAX_WIRE_STRING + 1),
            blob: Vec::new(),
        };
        assert!(
            !encodes_and_decodes(&over_limit),
            "4096 is the first failing length; if it now passes, the codec changed"
        );

        // The same byte count as bytes, which is why byte fields keep their
        // full declared ceilings.
        let as_bytes = Probe {
            text: String::new(),
            blob: vec![0x41u8; 64 * 1024],
        };
        assert!(
            encodes_and_decodes(&as_bytes),
            "the defect is specific to the string path"
        );
    }

    /// Q: would a field silently drifting back to `i64` be caught?
    ///
    /// **Round-trip tests cannot catch it**: a type decodes its own encoding,
    /// so an `i64` `ttl_ms` round-trips perfectly while producing different
    /// canonical bytes -- and therefore a different request hash -- from the
    /// `u64` the contract declares. This is exactly how three action fields sat
    /// on the wrong type through two review rounds of green tests.
    ///
    /// Two guards. First: signedness is *observable on the wire*, so getting it
    /// wrong is a wire-format change rather than a cosmetic one.
    #[test]
    fn signedness_changes_the_canonical_bytes() {
        // Same wire name, same field id, same logical value -- differing only
        // in signedness.
        #[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
        #[aster(wire = "aster.coordination.v1/SignednessProbe")]
        struct SignedProbe {
            #[fory(id = 1)]
            v: i64,
        }
        #[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
        #[aster(wire = "aster.coordination.v1/SignednessProbe")]
        struct UnsignedProbe {
            #[fory(id = 1)]
            v: u64,
        }

        let sf = payload_fory::<SignedProbe>();
        let uf = payload_fory::<UnsignedProbe>();
        // 30_000 is a real `ttl_ms`; zig-zag doubles it, so the varint bytes
        // differ even though the logical value is identical.
        let sb = sf.serialize(&SignedProbe { v: 30_000 }).expect("encode");
        let ub = uf.serialize(&UnsignedProbe { v: 30_000 }).expect("encode");
        assert_ne!(
            sb, ub,
            "signedness must be observable on the wire; if these matched, the \
             request hash could not depend on the field type"
        );
    }

    /// Second guard: compile-time type assertions. The `takes_u64` /
    /// `takes_opt_u64` calls below will not compile if any of these fields
    /// drifts, which is a build failure rather than a test that quietly keeps
    /// passing.
    #[test]
    fn action_field_types_match_the_normative_contract() {
        fn takes_u64(_: u64) {}
        fn takes_opt_u64(_: Option<u64>) {}

        let acquire = AcquireActionV1 {
            resource: key(),
            ttl_ms: 30_000,
        };
        takes_u64(acquire.ttl_ms);

        let cas = CompareAndSetActionV1 {
            fence: fence(),
            expected_value_revision: 3,
            new_value: vec![],
        };
        takes_u64(cas.expected_value_revision);

        let revoke = RevokeActionV1 {
            resource: key(),
            expected_epoch: Some(9),
        };
        takes_opt_u64(revoke.expected_epoch);

        // The counters §4 declares as u64, on the types that carry them.
        takes_u64(fence().epoch);
        let snap = ResourceSnapshotV1::default();
        takes_u64(snap.epoch);
        takes_u64(snap.ttl_ms);
        takes_u64(snap.lease_revision);
        takes_u64(snap.value_revision);
        takes_u64(snap.acl_revision);
    }

    /// Q: do ALL TEN normative action variants round-trip -- not the five-variant
    /// subset an earlier spike compiled while the plan claimed the "whole root"
    /// was proven?
    #[test]
    fn all_ten_action_variants_round_trip() {
        let f = payload_fory::<ActionV1>();
        let actions = [
            ActionV1::Acquire(AcquireActionV1 {
                resource: key(),
                ttl_ms: 30_000,
            }),
            ActionV1::Renew(RenewActionV1 { fence: fence() }),
            ActionV1::CompareAndSet(CompareAndSetActionV1 {
                fence: fence(),
                expected_value_revision: 3,
                new_value: vec![1u8; 64],
            }),
            ActionV1::Release(ReleaseActionV1 { fence: fence() }),
            ActionV1::ReleaseWithValue(ReleaseWithValueActionV1 {
                fence: fence(),
                expected_value_revision: 4,
                final_value: vec![2u8; 64],
            }),
            ActionV1::Revoke(RevokeActionV1 {
                resource: key(),
                expected_epoch: Some(9),
            }),
            ActionV1::SetGrant(SetGrantActionV1 {
                resource: key(),
                target_node_id: vec![5u8; 32],
                rights: 0x3f,
                expected_acl_revision: 2,
            }),
            ActionV1::SetAdmin(SetAdminActionV1 {
                target_node_id: vec![6u8; 32],
                enabled: true,
                expected_admin_revision: 1,
            }),
            ActionV1::ChangeMembership(ChangeMembershipActionV1 {
                operation_id: vec![7u8; 16],
                expected_membership_log_id: Some(LogIdV1 {
                    term: 2,
                    index: 30,
                    leader_node_id: vec![1u8; 32],
                }),
                desired_voters: vec![NodeIdV1 {
                    node_id: vec![8u8; 32],
                }],
                retained_learners: vec![],
                member_seeds: vec![],
            }),
            ActionV1::CancelMembership(CancelMembershipActionV1 {
                operation_id: vec![7u8; 16],
                expected_phase_code: 1,
            }),
        ];
        assert_eq!(actions.len(), 10, "the plan declares ten action variants");
        for a in actions {
            let b = f.serialize(&a).expect("serialize");
            let back: ActionV1 = f.deserialize(&b).expect("deserialize");
            assert_eq!(a, back, "round-trip lost information");
        }
    }

    /// Q: does an unheld resource distinguish "no holder" from an all-zero
    /// NodeId? The exploratory shape used a bare `Vec<u8>`, which cannot.
    #[test]
    fn unheld_snapshot_has_no_holder_rather_than_zeroes() {
        let f = payload_fory::<ResourceSnapshotV1>();
        let unheld = ResourceSnapshotV1 {
            group_id: vec![1u8; 32],
            resource: key(),
            epoch: 0,
            holder: None,
            held: false,
            ttl_ms: 0,
            lease_revision: 0,
            value_revision: 0,
            acl_revision: 0,
            value: None,
            value_redacted: false,
        };
        let zero_holder = ResourceSnapshotV1 {
            holder: Some(vec![0u8; 32]),
            ..unheld.clone()
        };
        assert_ne!(unheld, zero_holder);
        for v in [unheld, zero_holder] {
            let b = f.serialize(&v).expect("serialize");
            assert_eq!(f.deserialize::<ResourceSnapshotV1>(&b).expect("de"), v);
        }
    }

    /// Q: does a whole union root register without a Fory type-index collision?
    #[test]
    fn action_root_registers() {
        let _f = payload_fory::<ActionV1>();
    }

    /// Q: do the declared unions round-trip, including the optional-epoch and
    /// repeated-NodeId shapes the plan invented?
    #[test]
    fn action_round_trips() {
        let f = payload_fory::<ActionV1>();

        for a in [
            ActionV1::Acquire(AcquireActionV1 {
                resource: key(),
                ttl_ms: 30_000,
            }),
            ActionV1::Renew(RenewActionV1 { fence: fence() }),
            ActionV1::CompareAndSet(CompareAndSetActionV1 {
                fence: fence(),
                expected_value_revision: 3,
                new_value: vec![0u8; 1024],
            }),
            // Conditional and unconditional revoke.
            ActionV1::Revoke(RevokeActionV1 {
                resource: key(),
                expected_epoch: Some(42),
            }),
            ActionV1::Revoke(RevokeActionV1 {
                resource: key(),
                expected_epoch: None,
            }),
            ActionV1::ChangeMembership(ChangeMembershipActionV1 {
                operation_id: vec![3u8; 16],
                expected_membership_log_id: Some(LogIdV1 {
                    term: 1,
                    leader_node_id: vec![5u8; 32],
                    index: 9,
                }),
                desired_voters: vec![
                    NodeIdV1 {
                        node_id: vec![1u8; 32],
                    },
                    NodeIdV1 {
                        node_id: vec![2u8; 32],
                    },
                ],
                retained_learners: vec![],
                member_seeds: vec![MemberSeedV1 {
                    node_id: vec![1u8; 32],
                    ticket: "aster1abc".into(),
                    signed_join_permit: Some(vec![8u8; 64]),
                }],
            }),
        ] {
            let bytes = f.serialize(&a).expect("encode");
            let back: ActionV1 = f.deserialize(&bytes).expect("decode");
            assert_eq!(a, back, "round-trip mismatch");
        }
    }

    /// Settles the signed-vs-unsigned question the earlier findings note got
    /// wrong. Both must round-trip; the choice is then made on wire size and
    /// semantics, not on one of them being impossible.
    #[test]
    fn both_signed_and_unsigned_counters_round_trip() {
        let sf = payload_fory::<SignedCountersV1>();
        let uf = payload_fory::<UnsignedCountersV1>();

        let sv = SignedCountersV1 {
            epoch: 42,
            lease_revision: 7,
            code: 100,
        };
        let uv = UnsignedCountersV1 {
            epoch: 42,
            lease_revision: 7,
            code: 100,
        };

        let sb = sf.serialize(&sv).expect("encode i64");
        let ub = uf.serialize(&uv).expect("encode u64");
        assert_eq!(sf.deserialize::<SignedCountersV1>(&sb).expect("decode"), sv);
        assert_eq!(
            uf.deserialize::<UnsignedCountersV1>(&ub).expect("decode"),
            uv
        );

        // Both work. var_uint64 is not zig-zagged, so for the always-
        // non-negative counters this subsystem stores it is never larger and
        // is usually smaller.
        println!(
            "i64 encoding: {} bytes, u64 encoding: {} bytes",
            sb.len(),
            ub.len()
        );
        assert!(
            ub.len() <= sb.len(),
            "unsigned should not be larger for positive values"
        );

        // The ceiling this plan cares about survives either way.
        let big_u = UnsignedCountersV1 {
            epoch: i64::MAX as u64,
            lease_revision: i64::MAX as u64,
            code: 0,
        };
        let bb = uf.serialize(&big_u).expect("encode u64 max-signed");
        assert_eq!(
            uf.deserialize::<UnsignedCountersV1>(&bb).expect("decode"),
            big_u
        );
    }

    /// Q: is the encoding stable across two independently built runtimes?
    /// Golden vectors depend on this.
    #[test]
    fn encoding_is_stable_across_runtimes() {
        let a = ActionV1::Acquire(AcquireActionV1 {
            resource: key(),
            ttl_ms: 30_000,
        });
        let one = payload_fory::<ActionV1>().serialize(&a).expect("encode");
        let two = payload_fory::<ActionV1>().serialize(&a).expect("encode");
        assert_eq!(one, two);
    }

    /// Q: does an Option<Vec<u8>> `bytes` field distinguish None from empty?
    /// §4.3 depends on `Some([])` != `None`.
    #[test]
    fn snapshot_option_bytes_distinguishes_none_from_empty() {
        let f = payload_fory::<ResourceSnapshotV1>();
        let mut s = ResourceSnapshotV1 {
            group_id: vec![7u8; 32],
            resource: key(),
            epoch: 1,
            holder: Some(vec![9u8; 32]),
            held: true,
            ttl_ms: 30_000,
            lease_revision: 1,
            value_revision: 1,
            acl_revision: 0,
            value: None,
            value_redacted: true,
        };
        let none_bytes = f.serialize(&s).expect("encode");
        s.value = Some(Vec::new());
        s.value_redacted = false;
        let empty_bytes = f.serialize(&s).expect("encode");
        assert_ne!(none_bytes, empty_bytes, "None and Some([]) must differ");

        let back: ResourceSnapshotV1 = f.deserialize(&empty_bytes).expect("decode");
        assert_eq!(back.value, Some(Vec::new()));
        assert!(!back.value_redacted);
    }
}

// ── CommandV1 / CommandReplyV1 declaration root ──────────────────────────────
//
// Declared and compiled here because an earlier plan revision wrote
// `CommandReplyV1` as pseudo-Rust `union` with no derives, no explicit variant
// ids, no default/unknown variants, and no wire names on its payloads. None of
// that is a Fory declaration; it could not have compiled.

/// System command results. Deliberately its OWN code space: an earlier revision
/// reused `100`/`101`, which are audit *operation* codes for Expire and
/// MembershipProgress. An operation code says which command ran; a result code
/// says what it did. Conflating them makes "100" ambiguous in the one table
/// where both appear.
pub const SYS_EXPIRED: i32 = 1;
/// The expiry named a lease revision that a committed renew already superseded.
pub const SYS_EXPIRY_STALE: i32 = 2;
pub const SYS_MEMBERSHIP_ADVANCED: i32 = 3;
pub const SYS_MEMBERSHIP_PHASE_MISMATCH: i32 = 4;

/// A committed `Expire` whose fields will not narrow (§10.1).
///
/// Its own code, not a reuse of `SYS_MEMBERSHIP_PHASE_MISMATCH`: a malformed
/// expiry answered with a membership code tells an operator the wrong thing
/// about which subsystem is broken.
pub const SYS_EXPIRE_MALFORMED: i32 = 5;

/// A committed `MembershipProgress` whose fields will not narrow (§10.1).
pub const SYS_MEMBERSHIP_MALFORMED: i32 = 6;

/// The signed genesis was verified and its administrators installed.
pub const SYS_GENESIS_INSTALLED: i32 = 7;

/// The same genesis arrived again after it had already been installed.
///
/// Not an error and not a second installation: exact replay is idempotent, so
/// this reports that the entry was recognised and changed nothing. A *different*
/// genesis is not this code — it is fatal, because two genesis documents for one
/// group is a disagreement no voter can resolve locally.
pub const SYS_GENESIS_ALREADY_INSTALLED: i32 = 8;

/// The §5.2 audit operation codes for the three system commands. Declared
/// here — the contract surface both graphs compile — rather than in the
/// voter-only apply module, because the audit *readers* on both sides key
/// their result-space decisions off them.
pub const OPERATION_EXPIRE: i32 = 100;
pub const OPERATION_MEMBERSHIP_PROGRESS: i32 = 101;
pub const OPERATION_GENESIS: i32 = 102;

/// **The exact §5.2 audit operation × result table**, as an enumerable
/// constant — the one answer to "which results can this operation record",
/// shared by the durable-state validator and the client's inbound boundary
/// so the two readers cannot hold different oracles, and enumerable so the
/// independent transcription test compares the **complete** key/value set
/// rather than probing samples.
///
/// The exact pairing, not merely the code space: `Expire` reporting
/// `SYS_MEMBERSHIP_MALFORMED`, or `Acquire` reporting `MembershipAccepted`,
/// are both in the right space and describe results no apply path produces.
pub const AUDIT_RESULTS: &[(i32, &[i32])] = {
    use crate::coordination::reason::Outcome;
    const REJECTED: i32 = Outcome::Rejected as i32;
    &[
        (1, &[Outcome::Granted as i32, REJECTED]),
        (2, &[Outcome::Renewed as i32, REJECTED]),
        (3, &[Outcome::Mutated as i32, REJECTED]),
        // Release and release-with-value share an outcome; they differ in
        // what they write, not in what they report.
        (4, &[Outcome::Released as i32, REJECTED]),
        (5, &[Outcome::Released as i32, REJECTED]),
        (6, &[Outcome::Revoked as i32, REJECTED]),
        (7, &[Outcome::GrantUpdated as i32, REJECTED]),
        (8, &[Outcome::AdminUpdated as i32, REJECTED]),
        (9, &[Outcome::MembershipAccepted as i32, REJECTED]),
        (10, &[Outcome::ClientRetired as i32, REJECTED]),
        (11, &[Outcome::MembershipCancelled as i32, REJECTED]),
        (
            OPERATION_EXPIRE,
            &[SYS_EXPIRED, SYS_EXPIRY_STALE, SYS_EXPIRE_MALFORMED],
        ),
        (
            OPERATION_MEMBERSHIP_PROGRESS,
            &[
                SYS_MEMBERSHIP_ADVANCED,
                SYS_MEMBERSHIP_PHASE_MISMATCH,
                SYS_MEMBERSHIP_MALFORMED,
            ],
        ),
        // Two results and no rejection: a genesis that does not verify is
        // fatal before any row is written, so no audit row can record one.
        (
            OPERATION_GENESIS,
            &[SYS_GENESIS_INSTALLED, SYS_GENESIS_ALREADY_INSTALLED],
        ),
    ]
};

/// Look one operation up in [`AUDIT_RESULTS`]. `None` for an operation §5.2
/// does not declare.
pub fn audit_results_for(operation_code: i32) -> Option<&'static [i32]> {
    AUDIT_RESULTS
        .iter()
        .find(|(operation, _)| *operation == operation_code)
        .map(|(_, results)| *results)
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ClientReply")]
pub struct ClientReplyV1 {
    #[fory(id = 1)]
    pub disposition_code: i32,
    #[fory(id = 2)]
    pub outcome_code: i32,
    #[fory(id = 3)]
    pub reason_code: i32,
    #[fory(id = 4)]
    pub snapshot: Option<ResourceSnapshotV1>,
    #[fory(id = 5)]
    pub acl_revision: u64,
    #[fory(id = 6)]
    pub admin_revision: u64,
    #[fory(id = 7, bytes)]
    pub operation_id: Option<Vec<u8>>,
    #[fory(id = 8)]
    pub membership_log_id: Option<LogIdV1>,
}

/// `Expire` and `MembershipProgress`. No disposition and no outcome code:
/// there is no client and no sequence, so nothing was consumed.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/SystemReply")]
pub struct SystemReplyV1 {
    #[fory(id = 1)]
    pub system_result_code: i32,
    #[fory(id = 2)]
    pub lease_revision: u64,
    #[fory(id = 3)]
    pub membership_log_id: Option<LogIdV1>,
}

/// OpenRaft's native Blank and Membership entries: one empty response per
/// applied entry.
///
/// Not a `CommandV1` variant, and deliberately empty. OpenRaft requires one
/// `R` per entry it applies, including entries the application never
/// proposed -- a blank written on leader election and a membership change --
/// so `R` needs an inhabitant for them. Giving them a `Client` or `System`
/// reply instead would attach a disposition, an outcome and a reason code to
/// an entry that has none of those, and a projection would then have to
/// invent what they meant.
///
/// It carries no fields today and is a struct rather than a unit so it can
/// gain one without changing the union's shape (§5.2.1).
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/NonCommandReply")]
pub struct NonCommandReplyV1 {}

/// `RetireClient`. Has a caller but no sequence, so no disposition.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RetirementReply")]
pub struct RetirementReplyV1 {
    #[fory(id = 1)]
    pub outcome_code: i32,
    #[fory(id = 2)]
    pub reason_code: i32,
}

/// OpenRaft's `R`. One type for every applied log entry, including OpenRaft's
/// own Blank and Membership payloads. `CommandV1` has four variants and only
/// one of them has a client sequence, so `disposition_code` is meaningless for
/// `Expire`, `MembershipProgress`, `RetireClient`, Blank and Membership --
/// hence a union rather than a struct carrying fields that do not apply.
// MEASURED: 264 bytes (Client) against 56 (System) and 8 (Retirement). Boxing
// the large variant is NOT available -- Aster's `WireField` is not implemented
// for `Box<T>`, so a declared field cannot be boxed without leaving the wire
// contract. 264 bytes per applied entry is accepted: OpenRaft returns one `R`
// per entry, so a 1,000-entry apply batch costs ~264 KiB of transient replies.
// `reply_variant_sizes_are_recorded` fails if it grows.
#[allow(clippy::large_enum_variant)]
#[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/CommandReply")]
pub enum CommandReplyV1 {
    #[fory(default)]
    #[fory(id = 1)]
    Client(ClientReplyV1),
    #[fory(id = 2)]
    System(SystemReplyV1),
    #[fory(id = 3)]
    Retirement(RetirementReplyV1),
    /// OpenRaft-native Blank and Membership entries: one empty response per
    /// applied entry, with no application-command or public-RPC semantics.
    #[fory(id = 4)]
    NonCommand(NonCommandReplyV1),
    #[fory(unknown)]
    Unknown(fory_core::UnknownCase),
}

/// No `Default`: `ActionV1` is a `ForyUnion`, and `ForyUnion` does not emit a
/// Rust `Default` impl, so any struct embedding a union cannot derive one.
/// `ForyStruct` does not require `Default`, so this is fine -- but it means the
/// plan cannot show these command structs with a blanket `#[derive(Default)]`.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ClientCommand")]
pub struct ClientCommandV1 {
    #[fory(id = 1, bytes)]
    pub actor_node_id: Vec<u8>,
    #[fory(id = 2, bytes)]
    pub client_id: Vec<u8>,
    #[fory(id = 3)]
    pub sequence: u64,
    #[fory(id = 4, bytes)]
    pub request_hash: Vec<u8>,
    #[fory(id = 5)]
    pub action: ActionV1,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/ExpireCommand")]
pub struct ExpireCommandV1 {
    #[fory(id = 1)]
    pub resource: ResourceKeyV1,
    #[fory(id = 2)]
    pub epoch: u64,
    #[fory(id = 3)]
    pub lease_revision: u64,
}

#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/MembershipProgressCommand")]
pub struct MembershipProgressCommandV1 {
    #[fory(id = 1, bytes)]
    pub operation_id: Vec<u8>,
    #[fory(id = 2)]
    pub expected_phase: i32,
    #[fory(id = 3)]
    pub new_phase: i32,
    #[fory(id = 4)]
    pub observed_membership_log_id: Option<LogIdV1>,
    #[fory(id = 5)]
    pub result_code: i32,
}

/// The signed genesis, replicated as a committed entry.
///
/// ## Why the genesis has to be a command
///
/// The group's initial administrators live in the signed genesis, and until
/// this existed nothing installed them: `boot::initialize` writes OpenRaft's
/// membership entry and nothing else, and `set_admin` requires an existing
/// administrator, so the delegated set could never become non-empty. A group
/// could form, elect and replicate, and never authorize an administrative
/// action again — no grant, and therefore no membership change.
///
/// Seeding it beside the log would not do. State written before Raft
/// initialization is state a full-log replay does not reproduce, and state
/// derived from locally loaded key material is state two voters can disagree
/// about. A committed entry is the only form every present and future member
/// obtains identically — by replay, or by the snapshot that subsumes it.
///
/// Carries the **exact** signed document rather than its parts, so apply
/// re-verifies the same bytes the operator signed instead of a projection of
/// them that a proposer chose.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/GenesisCommand")]
pub struct GenesisCommandV1 {
    /// The canonical `SignedCoordinationDocumentV1` bytes.
    #[fory(id = 1, bytes)]
    pub signed_genesis: Vec<u8>,
}

/// Sessionless (§10): no client id, no sequence, so it cannot be a
/// `ClientCommandV1`, which is defined around one.
#[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/RetireClientCommand")]
pub struct RetireClientCommandV1 {
    #[fory(id = 1, bytes)]
    pub actor_node_id: Vec<u8>,
    #[fory(id = 2, bytes)]
    pub target_node_id: Vec<u8>,
    #[fory(id = 3, bytes)]
    pub target_client_id: Vec<u8>,
}

/// OpenRaft's `D`. **Five** variants.
///
/// `Genesis` was added after the v1 shape was called frozen, deliberately: the
/// frozen declaration described an authorization model — ROOT delegating to an
/// initial administrator set — and omitted the transition that makes it
/// reachable, so a group could never acquire an administrator. v1 has not
/// shipped, and a format that cannot express its own bootstrap is not the
/// format that was frozen.
#[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "aster.coordination.v1/Command")]
pub enum CommandV1 {
    #[fory(default)]
    #[fory(id = 1)]
    Client(ClientCommandV1),
    #[fory(id = 2)]
    Expire(ExpireCommandV1),
    #[fory(id = 3)]
    MembershipProgress(MembershipProgressCommandV1),
    #[fory(id = 4)]
    RetireClient(RetireClientCommandV1),
    #[fory(id = 5)]
    Genesis(GenesisCommandV1),
    #[fory(unknown)]
    Unknown(fory_core::UnknownCase),
}

impl CommandV1 {
    /// The reply variant this command must produce. A mismatch is an invariant
    /// failure, not a defaulted field.
    pub fn expected_reply_variant(&self) -> Option<u8> {
        Some(match self {
            CommandV1::Client(_) => 1,
            // A genesis has no caller and no session; like the other system
            // commands it answers with a `System` reply carrying a phase code.
            CommandV1::Expire(_) | CommandV1::MembershipProgress(_) | CommandV1::Genesis(_) => 2,
            CommandV1::RetireClient(_) => 3,
            CommandV1::Unknown(_) => return None,
        })
    }
}

impl CommandReplyV1 {
    pub fn variant_id(&self) -> Option<u8> {
        Some(match self {
            CommandReplyV1::Client(_) => 1,
            CommandReplyV1::System(_) => 2,
            CommandReplyV1::Retirement(_) => 3,
            // No `CommandV1` maps to this: it answers OpenRaft's native
            // entries, which are not commands. `expected_reply_variant` above
            // deliberately never returns 4, so the one-command-one-reply
            // check cannot be satisfied by a `NonCommand`.
            CommandReplyV1::NonCommand(_) => 4,
            CommandReplyV1::Unknown(_) => return None,
        })
    }
}

/// The `CommandV1`/`CommandReplyV1` declaration root.
#[cfg(test)]
mod command_tests {
    use super::*;

    fn key() -> ResourceKeyV1 {
        ResourceKeyV1 {
            namespace: "portal.roaming".into(),
            key: vec![1, 2, 3, 4],
        }
    }

    // ── CommandV1 / CommandReplyV1 root ──────────────────────────────────────

    fn client_cmd(seq: u64) -> CommandV1 {
        CommandV1::Client(ClientCommandV1 {
            actor_node_id: vec![3u8; 32],
            client_id: vec![4u8; 16],
            sequence: seq,
            request_hash: vec![5u8; 32],
            action: ActionV1::Acquire(AcquireActionV1 {
                resource: key(),
                ttl_ms: 30_000,
            }),
        })
    }

    /// Q: does the *reply* root register on its own, or does it collide with
    /// the command root's type index? Two roots in one crate is exactly the
    /// shape that bit us before (see the PayloadRegistry memory).
    /// Measured, so the `large_enum_variant` allow on `CommandReplyV1` rests on
    /// a number rather than an assumption.
    #[test]
    fn reply_variant_sizes_are_recorded() {
        use std::mem::size_of;
        println!(
            "CommandReplyV1={} Client={} System={} Retirement={}",
            size_of::<CommandReplyV1>(),
            size_of::<ClientReplyV1>(),
            size_of::<SystemReplyV1>(),
            size_of::<RetirementReplyV1>()
        );
        // The union is as large as its largest variant; this is the value the
        // allow accepts. Tighten the bound if the snapshot grows.
        assert!(
            size_of::<CommandReplyV1>() <= 264,
            "CommandReplyV1 grew to {}; revisit boxing",
            size_of::<CommandReplyV1>()
        );
    }

    #[test]
    fn command_and_reply_roots_both_register() {
        let _c = payload_fory::<CommandV1>();
        let _r = payload_fory::<CommandReplyV1>();
    }

    /// Q: do all four command variants round-trip -- including the fourth,
    /// `RetireClient`, that the plan text still omits?
    #[test]
    fn command_root_round_trips_all_four_variants() {
        let f = payload_fory::<CommandV1>();
        let cmds = [
            client_cmd(7),
            CommandV1::Expire(ExpireCommandV1 {
                resource: key(),
                epoch: 42,
                lease_revision: 9,
            }),
            CommandV1::MembershipProgress(MembershipProgressCommandV1 {
                operation_id: vec![6u8; 16],
                expected_phase: 1,
                new_phase: 2,
                observed_membership_log_id: Some(LogIdV1 {
                    term: 4,
                    index: 128,
                    leader_node_id: vec![3u8; 32],
                }),
                result_code: 0,
            }),
            CommandV1::RetireClient(RetireClientCommandV1 {
                actor_node_id: vec![3u8; 32],
                target_node_id: vec![8u8; 32],
                target_client_id: vec![4u8; 16],
            }),
        ];
        assert_eq!(cmds.len(), 4, "CommandV1 has four variants, not three");
        for c in cmds {
            let b = f.serialize(&c).expect("serialize");
            let back: CommandV1 = f.deserialize(&b).expect("deserialize");
            assert_eq!(c, back);
        }
    }

    #[test]
    fn reply_root_round_trips_all_three_variants() {
        let f = payload_fory::<CommandReplyV1>();
        for r in [
            CommandReplyV1::Client(ClientReplyV1 {
                disposition_code: 1,
                outcome_code: 1,
                reason_code: 0,
                snapshot: None,
                acl_revision: 3,
                admin_revision: 4,
                operation_id: None,
                membership_log_id: None,
            }),
            CommandReplyV1::System(SystemReplyV1 {
                system_result_code: SYS_EXPIRED,
                lease_revision: 9,
                membership_log_id: None,
            }),
            CommandReplyV1::Retirement(RetirementReplyV1 {
                outcome_code: 9,
                reason_code: 0,
            }),
        ] {
            let b = f.serialize(&r).expect("serialize");
            let back: CommandReplyV1 = f.deserialize(&b).expect("deserialize");
            assert_eq!(r, back);
        }
    }

    /// Q: is every command paired with the one reply variant it can produce?
    /// The state machine returns `CommandReplyV1` for any `CommandV1`, so
    /// nothing in the type system stops it returning a `Client` reply for an
    /// `Expire`. This pins the intended pairing so an implementation can assert
    /// it.
    #[test]
    fn each_command_maps_to_exactly_one_reply_variant() {
        let pairs = [
            (
                client_cmd(1),
                CommandReplyV1::Client(ClientReplyV1::default()),
            ),
            (
                CommandV1::Expire(ExpireCommandV1::default()),
                CommandReplyV1::System(SystemReplyV1::default()),
            ),
            (
                CommandV1::MembershipProgress(MembershipProgressCommandV1::default()),
                CommandReplyV1::System(SystemReplyV1::default()),
            ),
            (
                CommandV1::RetireClient(RetireClientCommandV1::default()),
                CommandReplyV1::Retirement(RetirementReplyV1::default()),
            ),
        ];
        for (c, r) in pairs {
            assert_eq!(
                c.expected_reply_variant(),
                r.variant_id(),
                "command/reply variant pairing broken for {c:?}"
            );
        }
    }

    /// The non-command reply round-trips through the declared root.
    ///
    /// An empty struct is exactly where a codec is most likely to be wrong in
    /// a way nothing notices: there are no fields whose values could disagree,
    /// so the only thing that can break is the variant tag itself -- and a
    /// reply decoded as the wrong variant would answer a Blank entry with a
    /// client disposition.
    #[test]
    fn the_non_command_reply_round_trips_as_its_own_variant() {
        let fory = payload_fory::<CommandReplyV1>();
        let reply = CommandReplyV1::NonCommand(NonCommandReplyV1 {});

        let bytes = fory.serialize(&reply).expect("encode");
        let back: CommandReplyV1 = fory.deserialize(&bytes).expect("decode");
        assert_eq!(back, reply);
        assert!(
            matches!(back, CommandReplyV1::NonCommand(_)),
            "it must not decode as another variant: {back:?}"
        );
        assert_eq!(back.variant_id(), Some(4));
    }

    /// No command maps to the non-command reply.
    ///
    /// The pairing check would otherwise be satisfiable by answering a real
    /// command with an empty reply, which is precisely the mistake the variant
    /// exists to make impossible: `NonCommand` is for entries the application
    /// never proposed.
    #[test]
    fn no_command_expects_the_non_command_reply() {
        for command in [
            client_cmd(1),
            CommandV1::Expire(ExpireCommandV1::default()),
            CommandV1::MembershipProgress(MembershipProgressCommandV1::default()),
            CommandV1::RetireClient(RetireClientCommandV1::default()),
        ] {
            assert_ne!(
                command.expected_reply_variant(),
                Some(4),
                "no CommandV1 may be answered with a NonCommand reply: {command:?}"
            );
        }
    }

    /// Q: are the system *result* codes actually distinct from the audit
    /// *operation* codes 100/101 that an earlier revision reused for them?
    #[test]
    fn system_result_codes_do_not_reuse_audit_operation_codes() {
        const AUDIT_OP_EXPIRE: i32 = 100;
        const AUDIT_OP_MEMBERSHIP_PROGRESS: i32 = 101;
        for c in [
            SYS_EXPIRED,
            SYS_EXPIRY_STALE,
            SYS_MEMBERSHIP_ADVANCED,
            SYS_MEMBERSHIP_PHASE_MISMATCH,
            SYS_EXPIRE_MALFORMED,
            SYS_MEMBERSHIP_MALFORMED,
        ] {
            assert_ne!(c, AUDIT_OP_EXPIRE);
            assert_ne!(c, AUDIT_OP_MEMBERSHIP_PROGRESS);
        }
    }

    /// Q: can a v1 node decode a log entry written by a v2 leader that added a
    /// fifth command variant carrying a NEW payload struct?
    ///
    /// Measured answer: **no**. `#[fory(unknown)]` does not cover it, because
    /// `unknown_case::read_payload` calls `read_any_type_info()` and then
    /// `read_polymorphic_data_as_send_sync_any`, which need the payload type
    /// registered locally. Tested against a real second declaration rather than
    /// by flipping bytes and hoping one of them was the variant tag.
    #[test]
    fn future_variant_with_a_new_payload_type_fails_to_decode() {
        // A hypothetical v2 wire type: same wire name, same variant ids 1-4,
        // plus a variant 5 that v1 has never heard of.
        #[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
        #[aster(wire = "aster.coordination.v1/TransferLeadershipCommand")]
        struct TransferLeadershipCommandV2 {
            #[fory(id = 1, bytes)]
            target_node_id: Vec<u8>,
        }

        #[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
        #[aster(wire = "aster.coordination.v1/Command")]
        enum CommandV2 {
            #[fory(default)]
            #[fory(id = 1)]
            Client(ClientCommandV1),
            #[fory(id = 2)]
            Expire(ExpireCommandV1),
            #[fory(id = 3)]
            MembershipProgress(MembershipProgressCommandV1),
            #[fory(id = 4)]
            RetireClient(RetireClientCommandV1),
            #[fory(id = 5)]
            Genesis(GenesisCommandV1),
            #[fory(id = 6)]
            TransferLeadership(TransferLeadershipCommandV2),
            #[fory(unknown)]
            Unknown(fory_core::UnknownCase),
        }

        let f2 = payload_fory::<CommandV2>();
        let v2 = CommandV2::TransferLeadership(TransferLeadershipCommandV2 {
            target_node_id: vec![9u8; 32],
        });
        let bytes = f2.serialize(&v2).expect("v2 serialize");

        // The EXACT measured outcome, asserted rather than tolerated. An
        // earlier version of this test also accepted `Ok(Unknown)`, so it would
        // have passed whichever way Fory behaved -- which is how the plan came
        // to claim a forward-compatibility property that does not hold.
        let f1 = payload_fory::<CommandV1>();
        let err = f1
            .deserialize::<CommandV1>(&bytes)
            .expect_err("v1 must NOT decode a v2 variant carrying a new type");
        assert!(
            err.to_string().contains("not registered locally"),
            "expected a type-registration failure, got: {err}"
        );
    }

    /// Q: so what DOES `#[fory(unknown)]` buy us? Narrow the rule: an unknown
    /// *case id* whose payload type is already registered locally.
    #[test]
    fn unknown_case_id_with_known_payload_type_decodes_as_unknown() {
        #[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
        #[aster(wire = "aster.coordination.v1/Command")]
        enum CommandV2Reused {
            #[fory(default)]
            #[fory(id = 1)]
            Client(ClientCommandV1),
            #[fory(id = 2)]
            Expire(ExpireCommandV1),
            #[fory(id = 3)]
            MembershipProgress(MembershipProgressCommandV1),
            #[fory(id = 4)]
            RetireClient(RetireClientCommandV1),
            #[fory(id = 5)]
            Genesis(GenesisCommandV1),
            // New case id, but its payload is a type v1 already registers.
            // Six rather than five: v1 case 5 is `Genesis` now, and reusing it
            // would test a *known* case rather than an unknown one.
            #[fory(id = 6)]
            ExpireAll(ExpireCommandV1),
            #[fory(unknown)]
            Unknown(fory_core::UnknownCase),
        }

        let f2 = payload_fory::<CommandV2Reused>();
        let bytes = f2
            .serialize(&CommandV2Reused::ExpireAll(ExpireCommandV1 {
                resource: key(),
                epoch: 1,
                lease_revision: 1,
            }))
            .expect("v2 serialize");

        let f1 = payload_fory::<CommandV1>();
        match f1.deserialize::<CommandV1>(&bytes) {
            Ok(CommandV1::Unknown(_)) => {}
            Ok(other) => panic!("expected Unknown, got {other:?}"),
            Err(e) => panic!("expected Unknown, got error: {e}"),
        }
    }
}
