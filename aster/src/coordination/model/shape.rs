//! Request-shape limits: the §12.1 logical bounds that are properties of the
//! **submitted request** rather than of replicated state.
//!
//! ## Why these are their own category
//!
//! §12.1 declares two kinds of limit that read alike and behave differently:
//!
//! * **State** limits -- resources per group, ACL rows per group, grants per
//!   resource -- describe how full the group is. A violation is a coordination
//!   outcome and has a §5.2 reason code (`22`, `23`).
//! * **Request-shape** limits -- an invalid resource identity, an oversized
//!   value, a TTL outside the v1 range -- describe the request itself. §5.2 has
//!   **no** reason code for any of them, because a malformed request is
//!   shapeless to the protocol rather than a coordination result.
//!
//! The split is not which section declares the limit; it is whether a violation
//! has a reason code to report.
//!
//! ## Shape is validity, not merely size
//!
//! An earlier version of this module checked maxima only, which accepted empty
//! keys, empty namespaces, uppercase and non-ASCII namespaces, and punctuation
//! §4.2 forbids. That was not a smaller check than the contract's -- it was a
//! different one, and it let identities into bounded metadata that
//! [`coordination::ids::ResourceKey`](crate::coordination::ids::ResourceKey)
//! would later reject. [`validate_resource_key`] delegates to that type, which
//! is the only constructor for a durable key.
//!
//! ## Where they are enforced
//!
//! Both places, for different purposes:
//!
//! * A conforming leader refuses one **pre-proposal** at §10 stage 4, with an
//!   `RpcStatus`. Nothing is written and no sequence is consumed.
//! * `apply` enforces them again, because a **non-conforming** leader can
//!   propose anything and apply may not skip a committed entry. See §10.1 and
//!   [`crate::coordination::model::apply`] for the forced-apply rule.

use crate::coordination::ids::{IdError, ResourceKey};

use super::limits::{MAX_JOIN_PERMIT, MAX_TICKET, MAX_TTL_MS, MAX_VALUE, MIN_TTL_MS};

/// Generates the [`ShapeLimit`] enum, its `ALL` list, and its descriptions from
/// one declaration -- the same reason `reasons!` exists.
///
/// An earlier version kept `ALL` as a literal beside the enum with a comment
/// claiming it was derived. It was not: adding a variant left the list stale
/// and every "every limit" test silently narrower.
macro_rules! shape_limits {
    ($($(#[$m:meta])* $name:ident => $describe:expr),* $(,)?) => {
        /// A §12.1 logical limit that bounds the shape of a request.
        ///
        /// Every variant is a condition with **no** §5.2 reason code. Adding one
        /// here is a deliberate statement that the protocol has no coordination
        /// reason for it; a condition that does have a reason code belongs in
        /// [`Reason`](crate::coordination::reason::Reason) instead.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum ShapeLimit {
            $($(#[$m])* $name),*
        }

        impl ShapeLimit {
            /// Every request-shape limit §12.1 declares.
            pub const ALL: &'static [ShapeLimit] = &[$(ShapeLimit::$name),*];

            /// A short description, for an `RpcStatus` message at §10 stage 4.
            ///
            /// Diagnostic text, not protocol surface: a client branches on the
            /// status code, never on this string.
            pub fn describe(self) -> &'static str {
                match self {
                    $(ShapeLimit::$name => $describe),*
                }
            }
        }
    };
}

shape_limits! {
    /// The namespace is not a valid §4.2 namespace: empty, over 64 bytes, or
    /// carrying a byte outside `[a-z0-9._-]` (or a leading `.`/`-`/`_`).
    Namespace => "namespace is not a valid coordination namespace",
    /// The key is not a valid §4.2 key: empty or over 512 bytes.
    ResourceKey => "resource key is not a valid coordination key",
    /// A resource value longer than `MAX_VALUE`.
    Value => "resource value exceeds its limit",
    /// `ttl_ms` outside `MIN_TTL_MS..=MAX_TTL_MS`.
    Ttl => "ttl_ms is outside the v1 bounds",
    /// A `rights` mask carrying a bit v1 does not define (§8.1).
    ///
    /// Unknown bits are rejected rather than ignored: a grant this voter cannot
    /// evaluate is not forward compatibility.
    RightsBits => "rights carry a bit v1 does not define",
    /// An encoded `ActionV1` larger than `MAX_ENCODED_COMMAND`.
    ///
    /// Checked on the encoded form at the ingest boundary, where the encoded
    /// length exists -- not reconstructed from field limits.
    EncodedCommand => "the encoded command exceeds its limit",
    /// A member seed's ticket longer than `MAX_TICKET`.
    MemberSeedTicket => "a member seed ticket exceeds its limit",
    /// A member seed's signed join permit longer than `MAX_JOIN_PERMIT`.
    MemberSeedJoinPermit => "a signed join permit exceeds its limit",
    /// A `Vec<u8>` node id that is not exactly 32 bytes.
    NodeIdWidth => "a node id is not 32 bytes",
    /// A `Vec<u8>` client or operation id that is not exactly 16 bytes.
    ClientIdWidth => "a client or operation id is not 16 bytes",
    /// A `request_hash` that is not exactly 32 bytes.
    RequestHashWidth => "the request hash is not 32 bytes",
    /// The `request_hash` is well-formed but is **not** the canonical hash of
    /// the action it accompanies (§5.2.1).
    RequestHashMismatch => "the request hash does not match its action",
    /// A membership plan outside §9's shape: not 3 or 5 desired voters, more
    /// than two new voter identities, or more than two retained learners.
    MembershipPlanSize => "the membership plan is outside the v1 shape",
    MembershipPlanOrder => "a membership plan node vector is not sorted and unique",
    MemberSeedUnknownNode => "a member seed names a node the plan does not contain",
    MemberSeedPermit => "a member seed's join permit does not match its node's status",
}

/// Validate a raw `(namespace, key)` into the durable key type.
///
/// This is the **only** way a resource identity enters state, a saved reply, or
/// an audit row. Returning the validated value rather than a boolean is what
/// stops a caller from validating and then storing the raw input anyway.
pub fn validate_resource_key(namespace: &str, key: &[u8]) -> Result<ResourceKey, ShapeLimit> {
    ResourceKey::new(namespace, key).map_err(|e| match e {
        IdError::NamespaceLength { .. } | IdError::NamespaceCharset => ShapeLimit::Namespace,
        IdError::KeyLength { .. } => ShapeLimit::ResourceKey,
        // The fixed-width and hex errors cannot arise from `ResourceKey::new`;
        // mapping them to the key limit keeps this total without inventing a
        // variant for an unreachable case.
        IdError::Length { .. } | IdError::HexLength { .. } | IdError::HexDigit => {
            ShapeLimit::ResourceKey
        }
    })
}

/// Check a resource value.
pub fn check_value(value: &[u8]) -> Option<ShapeLimit> {
    (value.len() > MAX_VALUE).then_some(ShapeLimit::Value)
}

/// Check a lease TTL.
pub fn check_ttl(ttl_ms: u64) -> Option<ShapeLimit> {
    (!(MIN_TTL_MS..=MAX_TTL_MS).contains(&ttl_ms)).then_some(ShapeLimit::Ttl)
}

/// Check a rights mask (§8.1).
pub fn check_rights(rights: u64) -> Option<ShapeLimit> {
    (!super::limits::rights_are_known(rights)).then_some(ShapeLimit::RightsBits)
}

/// Check an encoded command length.
pub fn check_encoded_command(len: usize) -> Option<ShapeLimit> {
    (len > super::limits::MAX_ENCODED_COMMAND).then_some(ShapeLimit::EncodedCommand)
}

/// §12.1 voter-count bounds for a membership plan (§9).
pub const MIN_VOTERS: usize = 3;
pub const MAX_VOTERS: usize = 5;
pub const MAX_RETAINED_LEARNERS: usize = 2;

/// Check a membership plan's shape.
///
/// A production plan targets exactly 3 or 5 voters -- never an even set, which
/// has no majority advantage and a worse failure mode -- and retains at most two
/// learners. Larger replacements use successive committed plans.
pub fn check_membership_plan(voters: usize, retained_learners: usize) -> Option<ShapeLimit> {
    let sized = voters == MIN_VOTERS || voters == MAX_VOTERS;
    (!sized || retained_learners > MAX_RETAINED_LEARNERS).then_some(ShapeLimit::MembershipPlanSize)
}

/// Is a node-id vector sorted and free of duplicates?
///
/// Both properties at once, and deliberately not "sort it and compare lengths":
/// §9 declares these vectors **sorted unique**, so an unsorted one is a
/// different request from its sorted spelling and must be refused rather than
/// normalised. Two spellings of one plan would hash differently while meaning
/// the same thing, which is exactly what the request hash exists to prevent.
pub fn sorted_unique(ids: &[[u8; 32]]) -> bool {
    ids.windows(2).all(|w| w[0] < w[1])
}

/// Check a member seed's ticket and join permit.
pub fn check_member_seed(ticket_len: usize, join_permit_len: Option<usize>) -> Option<ShapeLimit> {
    if ticket_len > MAX_TICKET {
        return Some(ShapeLimit::MemberSeedTicket);
    }
    if join_permit_len.is_some_and(|len| len > MAX_JOIN_PERMIT) {
        return Some(ShapeLimit::MemberSeedJoinPermit);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::model::limits::{MAX_ENCODED_COMMAND, MAX_KEY, MAX_NAMESPACE};

    #[test]
    fn the_all_list_is_generated_and_has_no_duplicates() {
        let mut seen = ShapeLimit::ALL.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), ShapeLimit::ALL.len());
        assert_eq!(ShapeLimit::ALL.len(), 16, "update this count with the enum");
        // Every variant has its own description; a shared one would make two
        // different failures read identically in a status message.
        let mut descriptions: Vec<_> = ShapeLimit::ALL.iter().map(|l| l.describe()).collect();
        descriptions.sort_unstable();
        descriptions.dedup();
        assert_eq!(descriptions.len(), ShapeLimit::ALL.len());
    }

    /// The findings this module previously accepted: a maxima-only check let
    /// every one of these through into bounded durable metadata.
    #[test]
    fn an_invalid_identity_is_rejected_even_when_it_is_within_the_maxima() {
        let cases: Vec<(&str, &str, &[u8], ShapeLimit)> = vec![
            ("empty namespace", "", b"k", ShapeLimit::Namespace),
            ("empty key", "ns", b"", ShapeLimit::ResourceKey),
            ("uppercase namespace", "NS", b"k", ShapeLimit::Namespace),
            ("unicode namespace", "nsé", b"k", ShapeLimit::Namespace),
            (
                "byte outside the charset",
                "ns/sub",
                b"k",
                ShapeLimit::Namespace,
            ),
            ("space", "ns ns", b"k", ShapeLimit::Namespace),
            // `.`, `-` and `_` are legal *inside* a namespace but not as the
            // first byte, which keeps a leading-dot name from looking like a
            // hidden file wherever it is written to disk. One case per byte,
            // and the legal-in-the-tail counterpart below.
            ("leading dot", ".hidden", b"k", ShapeLimit::Namespace),
            ("leading dash", "-ns", b"k", ShapeLimit::Namespace),
            ("leading underscore", "_ns", b"k", ShapeLimit::Namespace),
        ];
        for (name, ns, key, expected) in cases {
            assert_eq!(
                validate_resource_key(ns, key),
                Err(expected),
                "{name} must be refused"
            );
        }
    }

    /// The counterpart to the leading-byte cases: the same three bytes are
    /// legal after the first, so the rule is about position rather than about
    /// the characters.
    #[test]
    fn dot_dash_and_underscore_are_legal_after_the_first_byte() {
        assert!(validate_resource_key("a.b-c_d", b"k").is_ok());
    }

    #[test]
    fn a_valid_identity_validates_and_preserves_its_bytes() {
        let k = validate_resource_key("ns.sub-1_a", b"\x00\xff any bytes").expect("valid");
        assert_eq!(k.namespace(), "ns.sub-1_a");
        assert_eq!(k.key(), b"\x00\xff any bytes");
    }

    /// Every limit is checked at its exact boundary, so none is off by one.
    #[test]
    fn each_limit_admits_its_maximum_and_refuses_one_more() {
        assert!(validate_resource_key(&"n".repeat(MAX_NAMESPACE), b"k").is_ok());
        assert_eq!(
            validate_resource_key(&"n".repeat(MAX_NAMESPACE + 1), b"k"),
            Err(ShapeLimit::Namespace)
        );

        assert!(validate_resource_key("ns", &vec![0u8; MAX_KEY]).is_ok());
        assert_eq!(
            validate_resource_key("ns", &vec![0u8; MAX_KEY + 1]),
            Err(ShapeLimit::ResourceKey)
        );

        assert_eq!(check_value(&vec![0u8; MAX_VALUE]), None);
        assert_eq!(
            check_value(&vec![0u8; MAX_VALUE + 1]),
            Some(ShapeLimit::Value)
        );

        assert_eq!(check_ttl(MIN_TTL_MS), None);
        assert_eq!(check_ttl(MAX_TTL_MS), None);
        assert_eq!(check_ttl(MIN_TTL_MS - 1), Some(ShapeLimit::Ttl));
        assert_eq!(check_ttl(MAX_TTL_MS + 1), Some(ShapeLimit::Ttl));
        assert_eq!(check_ttl(0), Some(ShapeLimit::Ttl));

        assert_eq!(check_rights(super::super::limits::ALL_RIGHTS), None);
        assert_eq!(check_rights(0), None, "removing every right is legal");
        assert_eq!(check_rights(0x40), Some(ShapeLimit::RightsBits));

        assert_eq!(check_encoded_command(MAX_ENCODED_COMMAND), None);
        assert_eq!(
            check_encoded_command(MAX_ENCODED_COMMAND + 1),
            Some(ShapeLimit::EncodedCommand)
        );

        assert_eq!(check_member_seed(MAX_TICKET, Some(MAX_JOIN_PERMIT)), None);
        assert_eq!(
            check_member_seed(MAX_TICKET + 1, None),
            Some(ShapeLimit::MemberSeedTicket)
        );
        assert_eq!(
            check_member_seed(0, Some(MAX_JOIN_PERMIT + 1)),
            Some(ShapeLimit::MemberSeedJoinPermit)
        );
        assert_eq!(check_member_seed(0, None), None, "a permit is optional");
    }

    /// The namespace is reported before the key, so a request violating both is
    /// answered deterministically -- every voter must reach the same answer.
    #[test]
    fn a_request_violating_two_limits_reports_deterministically() {
        assert_eq!(
            validate_resource_key(&"n".repeat(MAX_NAMESPACE + 1), &vec![0u8; MAX_KEY + 1]),
            Err(ShapeLimit::Namespace)
        );
    }
}
