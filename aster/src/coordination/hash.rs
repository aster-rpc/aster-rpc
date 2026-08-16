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

    /// Checked-in hashes for one action per variant.
    ///
    /// These pin the **encoding**, not merely the code: a Fory version that
    /// changed field framing, a declaration whose signedness drifted, or a
    /// reordered field would all change these values while every round-trip
    /// test kept passing. Regenerating them is a protocol change, not a test
    /// fix -- a client's journal holds these hashes, and a voter compares them.
    #[test]
    fn golden_action_hashes_are_unchanged() {
        let cases: Vec<(&str, ActionV1)> = vec![
            ("acquire", acquire()),
            (
                "compare_and_set",
                ActionV1::CompareAndSet(CompareAndSetActionV1 {
                    fence: FenceV1 {
                        group_id: vec![7u8; 16],
                        resource: key(),
                        epoch: 4,
                        holder: vec![3u8; 32],
                    },
                    expected_value_revision: 2,
                    new_value: b"payload".to_vec(),
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
                "set_admin",
                ActionV1::SetAdmin(SetAdminActionV1 {
                    target_node_id: vec![0xAB; 32],
                    enabled: true,
                    expected_admin_revision: 3,
                }),
            ),
        ];

        let mut actual: Vec<(String, String)> = Vec::new();
        for (name, action) in &cases {
            let hash = request_hash(action).expect("hashable");
            actual.push(((*name).to_string(), to_hex(&hash)));
        }

        // Regenerate deliberately, never reflexively: print the block below and
        // paste it in only when the encoding change is intended and recorded.
        let expected: Vec<(String, String)> = GOLDEN
            .iter()
            .map(|(n, h)| ((*n).to_string(), (*h).to_string()))
            .collect();

        if actual != expected {
            let rendered: String = actual
                .iter()
                .map(|(n, h)| format!("    (\"{n}\", \"{h}\"),\n"))
                .collect();
            panic!(
                "canonical action hashes changed. If the encoding change is \
                 intended, update GOLDEN to:\n{rendered}"
            );
        }
    }

    /// One action per variant, so a change to any declaration is caught.
    const GOLDEN: &[(&str, &str)] = &[
        (
            "acquire",
            "bf533048f376807f4fbcf7a84824cb53cea933667065b415f24a95085dcaf81c",
        ),
        (
            "compare_and_set",
            "3b6b7ee92dfbb6675befe9f7e18df2a9216a24b8435f7db20bb2064e6c4f0b5b",
        ),
        (
            "revoke",
            "3f15bfcbbf29b5bdea795c3d6be68d544fc613bf7fe9cc86062a531c4b422b0a",
        ),
        (
            "set_admin",
            "5df821a6b686e989c9688d01062e4b2d1d49be1aca9ea9f00febdbf27ff6ecc3",
        ),
    ];
}
