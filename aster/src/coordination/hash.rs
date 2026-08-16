//! The canonical action hash (§5.2.1).
//!
//! ```text
//! BLAKE3( "aster-coordination/action/v1\0" || fory_canonical(ActionV1) )
//! ```
//!
//! ## What the hash covers, and what it deliberately does not
//!
//! The variant id is **inside** `fory_canonical(ActionV1)`, so the method is
//! covered without a separately framed code -- which also removes the
//! width/endianness question a framed code would raise.
//!
//! It covers neither the transport actor, nor the client id, nor the sequence.
//! Those are bound by the session key, and including them would make an
//! otherwise identical retry hash differently -- defeating the one thing the
//! hash exists for, which is recognising a retry of the same request at the same
//! sequence.
//!
//! The trailing NUL matches the domain-separation convention in
//! `core/src/attestation.rs`.
//!
//! ## `Unknown` is never hashable
//!
//! An action that decodes to `Unknown` is rejected before the hash is computed.
//! The forward-compatibility carrier exists so a newer peer's payload does not
//! corrupt a decoder, **not** so an unrecognised action can travel: hashing one
//! would let it be proposed, and applying it would diverge the voters, since no
//! two versions agree on what it means.
//!
//! ## Why the field types are part of the hash
//!
//! Fory encodes an `i64` and a `u64` differently, so a declaration that drifted
//! in signedness alone would produce different canonical bytes for the same
//! logical value -- and therefore a different hash. Round-trip tests cannot see
//! that class of defect at all, because a type decodes its own encoding
//! perfectly. `contract.rs` carries a dedicated signedness test for exactly this
//! reason.

use crate::coordination::contract::{payload_fory, ActionV1};

/// The §5.2.1 domain separator, trailing NUL included.
pub const ACTION_DOMAIN: &[u8] = b"aster-coordination/action/v1\0";

/// Why an action could not be hashed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashError {
    /// The action decoded to the forward-compatibility carrier. It is not
    /// hashable, not proposable, and not appliable.
    UnknownVariant,
    /// The canonical encoding failed. `Fory::serialize` is fallible, so any
    /// claim of an infallible encode is wrong.
    Encode,
}

/// The canonical encoding of an action -- the hash preimage after the domain
/// separator.
///
/// Exposed so golden vectors can pin the *bytes*, not merely the digest. A
/// digest-only vector cannot distinguish "the encoding changed" from "the hash
/// function changed".
pub fn canonical_bytes(action: &ActionV1) -> Result<Vec<u8>, HashError> {
    if matches!(action, ActionV1::Unknown(_)) {
        return Err(HashError::UnknownVariant);
    }
    payload_fory::<ActionV1>()
        .serialize(action)
        .map_err(|_| HashError::Encode)
}

/// The canonical request hash for `action`.
pub fn request_hash(action: &ActionV1) -> Result<[u8; 32], HashError> {
    let encoded = canonical_bytes(action)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(ACTION_DOMAIN);
    hasher.update(&encoded);
    Ok(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::contract::{
        AcquireActionV1, CompareAndSetActionV1, FenceV1, ResourceKeyV1, RevokeActionV1,
        SetAdminActionV1,
    };

    fn key() -> ResourceKeyV1 {
        ResourceKeyV1 {
            namespace: "portal.roaming".into(),
            key: vec![1, 2, 3, 4],
        }
    }

    fn acquire() -> ActionV1 {
        ActionV1::Acquire(AcquireActionV1 {
            resource: key(),
            ttl_ms: 30_000,
        })
    }

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The same action always hashes the same way, across independently built
    /// runtimes -- the property every voter's agreement rests on.
    #[test]
    fn the_hash_is_stable_for_one_action() {
        let a = request_hash(&acquire()).expect("hashable");
        let b = request_hash(&acquire()).expect("hashable");
        assert_eq!(a, b);
    }

    /// Different actions hash differently, including ones differing only in a
    /// single field.
    #[test]
    fn distinct_actions_hash_distinctly() {
        let longer_ttl = ActionV1::Acquire(AcquireActionV1 {
            resource: key(),
            ttl_ms: 30_001,
        });
        let other_key = ActionV1::Acquire(AcquireActionV1 {
            resource: ResourceKeyV1 {
                namespace: "portal.roaming".into(),
                key: vec![1, 2, 3, 5],
            },
            ttl_ms: 30_000,
        });

        let base = request_hash(&acquire()).expect("hashable");
        assert_ne!(base, request_hash(&longer_ttl).expect("hashable"));
        assert_ne!(base, request_hash(&other_key).expect("hashable"));
    }

    /// The variant id is inside the canonical bytes, so two actions carrying
    /// identical payload fields under different methods hash differently
    /// **without** a separately framed method code.
    #[test]
    fn the_method_is_covered_without_a_framed_code() {
        let revoke = ActionV1::Revoke(RevokeActionV1 {
            resource: key(),
            expected_epoch: None,
        });
        let admin = ActionV1::SetAdmin(SetAdminActionV1 {
            target_node_id: vec![0u8; 32],
            enabled: true,
            expected_admin_revision: 0,
        });
        assert_ne!(
            request_hash(&revoke).expect("hashable"),
            request_hash(&admin).expect("hashable")
        );
    }

    /// `Unknown` is refused before the hash is computed. Hashing it would let
    /// an unrecognised action be proposed, and applying one diverges the voters.
    ///
    /// `UnknownCase` is built by the decoder and has no public constructor, so
    /// this reaches the carrier the only way production can: by decoding a
    /// newer peer's action whose variant id v1 does not know. The payload type
    /// is one v1 *does* know, which is the case the spike measured as decoding
    /// to `Unknown` rather than failing outright.
    #[test]
    fn an_unknown_action_is_not_hashable() {
        use fory_derive::ForyUnion;

        #[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
        #[aster(wire = "aster.coordination.v1/Action")]
        enum ActionV2 {
            #[fory(default)]
            #[fory(id = 1)]
            Acquire(AcquireActionV1),
            /// A method v1 has never heard of, carrying a payload it has.
            #[fory(id = 12)]
            Future(AcquireActionV1),
            #[fory(unknown)]
            Unknown(fory_core::UnknownCase),
        }

        let encoded = payload_fory::<ActionV2>()
            .serialize(&ActionV2::Future(AcquireActionV1 {
                resource: key(),
                ttl_ms: 30_000,
            }))
            .expect("v2 encodes");

        let decoded: ActionV1 = payload_fory::<ActionV1>()
            .deserialize(&encoded)
            .expect("v1 decodes a known payload type under an unknown case id");
        assert!(
            matches!(decoded, ActionV1::Unknown(_)),
            "expected the forward-compat carrier, got {decoded:?}"
        );

        assert_eq!(request_hash(&decoded), Err(HashError::UnknownVariant));
        assert_eq!(canonical_bytes(&decoded), Err(HashError::UnknownVariant));
    }

    /// The domain separator is part of the preimage, so an action's hash is not
    /// the bare hash of its encoding -- a value hashed in another context
    /// cannot be replayed as an action hash.
    #[test]
    fn the_domain_separator_is_part_of_the_preimage() {
        let encoded = canonical_bytes(&acquire()).expect("encodable");
        let undomained = *blake3::hash(&encoded).as_bytes();
        assert_ne!(request_hash(&acquire()).expect("hashable"), undomained);

        assert_eq!(ACTION_DOMAIN, b"aster-coordination/action/v1\0");
        assert_eq!(
            *ACTION_DOMAIN.last().unwrap(),
            0,
            "the trailing NUL is the convention in core/src/attestation.rs"
        );
    }

    // ── Golden vectors ──────────────────────────────────────────────────────

    /// One action per §5.2 method, in method-code order.
    fn all_ten_actions() -> Vec<(&'static str, ActionV1)> {
        use crate::coordination::contract::{
            CancelMembershipActionV1, ChangeMembershipActionV1, MemberSeedV1, NodeIdV1,
            ReleaseActionV1, ReleaseWithValueActionV1, RenewActionV1, SetGrantActionV1,
        };

        let fence = FenceV1 {
            group_id: vec![7u8; 16],
            resource: key(),
            epoch: 4,
            holder: vec![3u8; 32],
        };

        vec![
            ("acquire", acquire()),
            (
                "renew",
                ActionV1::Renew(RenewActionV1 {
                    fence: fence.clone(),
                }),
            ),
            (
                "compare_and_set",
                ActionV1::CompareAndSet(CompareAndSetActionV1 {
                    fence: fence.clone(),
                    expected_value_revision: 2,
                    new_value: b"payload".to_vec(),
                }),
            ),
            (
                "release",
                ActionV1::Release(ReleaseActionV1 {
                    fence: fence.clone(),
                }),
            ),
            (
                "release_with_value",
                ActionV1::ReleaseWithValue(ReleaseWithValueActionV1 {
                    fence,
                    expected_value_revision: 2,
                    final_value: b"final".to_vec(),
                }),
            ),
            (
                "revoke",
                ActionV1::Revoke(RevokeActionV1 {
                    resource: key(),
                    expected_epoch: Some(9),
                }),
            ),
            (
                "set_grant",
                ActionV1::SetGrant(SetGrantActionV1 {
                    resource: key(),
                    target_node_id: vec![0x11; 32],
                    rights: 0x3f,
                    expected_acl_revision: 5,
                }),
            ),
            (
                "set_admin",
                ActionV1::SetAdmin(SetAdminActionV1 {
                    target_node_id: vec![0xAB; 32],
                    enabled: true,
                    expected_admin_revision: 3,
                }),
            ),
            (
                "change_membership",
                ActionV1::ChangeMembership(ChangeMembershipActionV1 {
                    operation_id: vec![0x5A; 16],
                    expected_membership_log_id: None,
                    desired_voters: vec![
                        NodeIdV1 {
                            node_id: vec![1u8; 32],
                        },
                        NodeIdV1 {
                            node_id: vec![2u8; 32],
                        },
                        NodeIdV1 {
                            node_id: vec![3u8; 32],
                        },
                    ],
                    retained_learners: vec![NodeIdV1 {
                        node_id: vec![4u8; 32],
                    }],
                    member_seeds: vec![MemberSeedV1 {
                        node_id: vec![5u8; 32],
                        ticket: "aster1seed".into(),
                        signed_join_permit: Some(vec![9, 9, 9]),
                    }],
                }),
            ),
            (
                "cancel_membership",
                ActionV1::CancelMembership(CancelMembershipActionV1 {
                    operation_id: vec![0x5A; 16],
                    expected_phase_code: 2,
                }),
            ),
        ]
    }

    /// Checked-in **preimage bytes and hashes** for all ten actions.
    ///
    /// Both, not just the digest: a digest alone cannot distinguish "the
    /// encoding changed" from "the hash function changed", and it is the
    /// encoding that a Fory version bump, a signedness drift or a reordered
    /// field would move. Round-trip tests see none of those, because a type
    /// decodes its own encoding perfectly.
    ///
    /// Regenerating these is a **protocol change**: a client's journal holds
    /// these hashes and a voter compares them, so a changed vector means every
    /// in-flight request at that sequence stops matching. The failure message
    /// prints the replacement block, so the change is deliberate rather than a
    /// guess.
    #[test]
    fn golden_action_preimages_and_hashes_are_unchanged() {
        let mut actual: Vec<(String, String, String)> = Vec::new();
        for (name, action) in all_ten_actions() {
            let preimage = canonical_bytes(&action).expect("encodable");
            let hash = request_hash(&action).expect("hashable");
            actual.push((name.to_string(), to_hex(&preimage), to_hex(&hash)));
        }

        assert_eq!(actual.len(), 10, "§5.2 declares ten methods");

        let expected: Vec<(String, String, String)> = GOLDEN
            .iter()
            .map(|(n, p, h)| ((*n).to_string(), (*p).to_string(), (*h).to_string()))
            .collect();

        if actual != expected {
            let rendered: String = actual
                .iter()
                .map(|(n, p, h)| format!("        (\"{n}\", \"{p}\", \"{h}\"),\n"))
                .collect();
            panic!(
                "canonical action encodings or hashes changed. If the change is \
                 intended and recorded, update GOLDEN to:\n{rendered}"
            );
        }
    }

    /// Each golden hash really is BLAKE3 over the domain plus its own golden
    /// preimage, so the two halves cannot drift apart independently.
    #[test]
    fn each_golden_hash_is_the_digest_of_its_golden_preimage() {
        for (name, preimage_hex, hash_hex) in GOLDEN {
            let preimage: Vec<u8> = (0..preimage_hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&preimage_hex[i..i + 2], 16).expect("hex"))
                .collect();
            let mut hasher = blake3::Hasher::new();
            hasher.update(ACTION_DOMAIN);
            hasher.update(&preimage);
            assert_eq!(to_hex(hasher.finalize().as_bytes()), *hash_hex, "{name}");
        }
    }

    /// `(method, canonical preimage hex, hash hex)` for all ten actions.
    const GOLDEN: &[(&str, &str, &str)] = &[
        ("acquire", "01ff230017a08386368dec63054200926223f0471c88641a02641c6fcaea130053439a01001e022160dc529e4ed168e24200926223f0471c88641a02641c6fcaea29740285111274029a1cd0c80ec41eb0ea011e042080156928a9022fe24200926223f0471c88641a02641c6fcaea25f62493a91113aa2600c415c8293a706f7274616c2e726f616d696e670401020304", "bf533048f376807f4fbcf7a84824cb53cea933667065b415f24a95085dcaf81c"),
        ("renew", "01ff230017a08386368dec63054200926223f0471c88641a02641c6fcaea130053439a02001e021ed0fa77c097b372e14200926223f0471c88641a02641c6fcaea25f624692dd00a687340c41e1e041f206dd810040f7be44200926223f0471c88641a02641c6fcaea13948d1100cc0ec429c81ed0290410070707070707070707070707070707071e062080156928a9022fe24200926223f0471c88641a02641c6fcaea25f62493a91113aa2600c415c8293a706f7274616c2e726f616d696e670401020304200303030303030303030303030303030303030303030303030303030303030303", "c31adf33c89ae616f022482b8b4095a8e27631746d892c6c6c50d3e75c532d08"),
        ("compare_and_set", "01ff230017a08386368dec63054200926223f0471c88641a02641c6fcaea130053439a03001e022820b1a79ad33c52e34200926223f0471c88641a02641c6fcaea3e38718780888d1a1d8226d0499071a0c80ec41ecc29021e041f206dd810040f7be44200926223f0471c88641a02641c6fcaea13948d1100cc0ec429c81ed0290410070707070707070707070707070707071e062080156928a9022fe24200926223f0471c88641a02641c6fcaea25f62493a91113aa2600c415c8293a706f7274616c2e726f616d696e670401020304200303030303030303030303030303030303030303030303030303030303030303077061796c6f6164", "3b6b7ee92dfbb6675befe9f7e18df2a9216a24b8435f7db20bb2064e6c4f0b5b"),
        ("release", "01ff230017a08386368dec63054200926223f0471c88641a02641c6fcaea130053439a04001e021fd01be8f319dc09e14200926223f0471c88641a02641c6fcaea29762459012274029a1cd0c41e1e041f206dd810040f7be44200926223f0471c88641a02641c6fcaea13948d1100cc0ec429c81ed0290410070707070707070707070707070707071e062080156928a9022fe24200926223f0471c88641a02641c6fcaea25f62493a91113aa2600c415c8293a706f7274616c2e726f616d696e670401020304200303030303030303030303030303030303030303030303030303030303030303", "83ed6db50a5da026667c685dc7747b79c87616841ad1a8abbbafb78dbd29eb4c"),
        ("release_with_value", "01ff230017a08386368dec63054200926223f0471c88641a02641c6fcaea130053439a05001e022aa018dff4b83334e34200926223f0471c88641a02641c6fcaea45f62459012276c899fb502e84e8053439a0c80ec41ecc29021e041f206dd810040f7be44200926223f0471c88641a02641c6fcaea13948d1100cc0ec429c81ed0290410070707070707070707070707070707071e062080156928a9022fe24200926223f0471c88641a02641c6fcaea25f62493a91113aa2600c415c8293a706f7274616c2e726f616d696e6704010203042003030303030303030303030303030303030303030303030303030303030303030566696e616c", "c6f88fb49362d1b3c28aa4fe0f3a34293f004c3aa833f971fe74ccacb6b90fcd"),
        ("revoke", "01ff230017a08386368dec63054200926223f0471c88641a02641c6fcaea130053439a06001e022060ce3e28e12616e24200926223f0471c88641a02641c6fcaea257624ab944e8053439aca0ec41eff091e042080156928a9022fe24200926223f0471c88641a02641c6fcaea25f62493a91113aa2600c415c8293a706f7274616c2e726f616d696e670401020304", "3f15bfcbbf29b5bdea795c3d6be68d544fc613bf7fe9cc86062a531c4b422b0a"),
        ("set_grant", "01ff230017a08386368dec63054200926223f0471c88641a02641c6fcaea130053439a07001e022610aaa4e10c6d0be44200926223f0471c88641a02641c6fcaea2e5822702201a9b412641c68cc0ed00ec41ec8293f051e042080156928a9022fe24200926223f0471c88641a02641c6fcaea25f62493a91113aa2600c415c8293a706f7274616c2e726f616d696e670401020304201111111111111111111111111111111111111111111111111111111111111111", "169cc1fbf905c4d7f9c269b22f0d5c74f05ea8fab65648bb393bb11bb39705b7"),
        ("set_admin", "01ff230017a08386368dec63054200926223f0471c88641a02641c6fcaea130053439a08001e0224c0cd0497ba410be34200926223f0471c88641a02641c6fcaea2e58226d066106b412641c68c801cc0ec429010320abababababababababababababababababababababababababababababababab", "5df821a6b686e989c9688d01062e4b2d1d49be1aca9ea9f00febdbf27ff6ecc3"),
        ("change_membership", "01ff230017a08386368dec63054200926223f0471c88641a02641c6fcaea130053439a09001e0230f070a305b7bd36e54200926223f0471c88641a02641c6fcaea417447034c4eb08c092323a1fd00a68734c429ca1ecc1678d01678d41678105a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5afd03081e041a60c8b14607e62de14200926223f0471c88641a02641c6fcaea164e70624418c42920010101010101010101010101010101010101010101010101010101010101010120020202020202020202020202020202020202020202020202020202020202020220030303030303030303030303030303030303030303030303030303030303030301081e0520040404040404040404040404040404040404040404040404040404040404040401081e0621c0ea0570378066e34200926223f0471c88641a02641c6fcaea224c2180888d820818c429c815ce292005050505050505050505050505050505050505050505050505050505050505052a61737465723173656564ff03090909", "5e590b6ad9b4dda94245e61432c27c92cca2000780453e54896f31c5ed727434"),
        ("cancel_membership", "01ff230017a08386368dec63054200926223f0471c88641a02641c6fcaea130053439a0b001e022740bcdb71dad908e24200926223f0471c88641a02641c6fcaea4174406888beb08c092323a1fd00a68734c805c42904105a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a", "2c70d285b8a4edad2b4ae3607b4691a16fc99736e546fd676a7e464dd8b969b8"),
    ];
}
