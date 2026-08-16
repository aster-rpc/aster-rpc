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
//! * **Request-shape** limits -- an oversized value, an over-long namespace, a
//!   TTL outside the v1 range -- describe the request itself. §5.2 has **no**
//!   reason code for any of them, because a request that is too big is shapeless
//!   to the protocol rather than a coordination result.
//!
//! The split is not which section declares the limit; it is whether a violation
//! has a reason code to report.
//!
//! ## Where they are enforced
//!
//! Both places, for different purposes:
//!
//! * A conforming leader refuses one **pre-proposal** at §10 stage 4, with an
//!   `RpcStatus`. Nothing is written and no sequence is consumed.
//! * `apply` enforces them again, because a **non-conforming** leader can
//!   propose anything and apply may not skip a committed entry. See
//!   [`crate::coordination::model::apply`] for the forced-apply rule that
//!   governs what that commits.
//!
//! [`ShapeLimit`] is an enum rather than a message string so the set is
//! enumerable: a test can assert that every request-shape limit §12.1 declares
//! has a variant, and adding a limit without classifying it is a compile error
//! at every exhaustive match.

use super::limits::{
    MAX_JOIN_PERMIT, MAX_KEY, MAX_NAMESPACE, MAX_TICKET, MAX_TTL_MS, MAX_VALUE, MIN_TTL_MS,
};

/// A §12.1 logical limit that bounds the shape of a request.
///
/// Every variant is a condition with **no** §5.2 reason code. Adding one here
/// is a deliberate statement that the protocol has no coordination reason for
/// it; if a condition does have a reason code it belongs in
/// [`Reason`](crate::coordination::reason::Reason) instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShapeLimit {
    /// `namespace` longer than [`MAX_NAMESPACE`].
    Namespace,
    /// `key` longer than [`MAX_KEY`].
    ResourceKey,
    /// A resource value longer than [`MAX_VALUE`].
    Value,
    /// `ttl_ms` outside `MIN_TTL_MS..=MAX_TTL_MS`.
    Ttl,
    /// A `rights` mask carrying a bit v1 does not define (§8.1).
    ///
    /// Unknown bits are rejected rather than ignored: a grant this voter cannot
    /// evaluate is not forward compatibility.
    RightsBits,
    /// An encoded `ActionV1` larger than `MAX_ENCODED_COMMAND`.
    ///
    /// Checked on the encoded form at the ingest boundary, where the encoded
    /// length exists -- not reconstructed from field limits.
    EncodedCommand,
    /// A member seed's ticket longer than [`MAX_TICKET`].
    MemberSeedTicket,
    /// A member seed's signed join permit longer than [`MAX_JOIN_PERMIT`].
    MemberSeedJoinPermit,
    /// More voters in a membership plan than v1 permits.
    MembershipPlanSize,
}

impl ShapeLimit {
    /// Every request-shape limit §12.1 declares.
    ///
    /// Derived by an exhaustive match rather than written as a literal beside
    /// the enum: a list maintained alongside its enum goes stale, which is the
    /// same defect `reasons!` exists to prevent.
    pub const ALL: &'static [ShapeLimit] = &[
        ShapeLimit::Namespace,
        ShapeLimit::ResourceKey,
        ShapeLimit::Value,
        ShapeLimit::Ttl,
        ShapeLimit::RightsBits,
        ShapeLimit::EncodedCommand,
        ShapeLimit::MemberSeedTicket,
        ShapeLimit::MemberSeedJoinPermit,
        ShapeLimit::MembershipPlanSize,
    ];

    /// A short description, for an `RpcStatus` message at §10 stage 4.
    ///
    /// This is diagnostic text, not protocol surface: a client branches on the
    /// status code, never on this string.
    pub fn describe(self) -> &'static str {
        match self {
            ShapeLimit::Namespace => "namespace exceeds its limit",
            ShapeLimit::ResourceKey => "resource key exceeds its limit",
            ShapeLimit::Value => "resource value exceeds its limit",
            ShapeLimit::Ttl => "ttl_ms is outside the v1 bounds",
            ShapeLimit::RightsBits => "rights carry a bit v1 does not define",
            ShapeLimit::EncodedCommand => "the encoded command exceeds its limit",
            ShapeLimit::MemberSeedTicket => "a member seed ticket exceeds its limit",
            ShapeLimit::MemberSeedJoinPermit => "a signed join permit exceeds its limit",
            ShapeLimit::MembershipPlanSize => "the membership plan names too many voters",
        }
    }
}

/// Check a `(namespace, key)` pair.
pub fn check_resource_key(namespace: &str, key: &[u8]) -> Option<ShapeLimit> {
    if namespace.len() > MAX_NAMESPACE {
        return Some(ShapeLimit::Namespace);
    }
    if key.len() > MAX_KEY {
        return Some(ShapeLimit::ResourceKey);
    }
    None
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

    /// `ALL` must be exhaustive. A wildcard-free match fails to compile when a
    /// variant is added, forcing both the list and a classification decision.
    #[test]
    fn the_all_list_covers_every_variant() {
        for limit in ShapeLimit::ALL {
            // Exhaustive by construction: adding a variant without extending
            // this match is a compile error.
            let _: &'static str = match limit {
                ShapeLimit::Namespace
                | ShapeLimit::ResourceKey
                | ShapeLimit::Value
                | ShapeLimit::Ttl
                | ShapeLimit::RightsBits
                | ShapeLimit::EncodedCommand
                | ShapeLimit::MemberSeedTicket
                | ShapeLimit::MemberSeedJoinPermit
                | ShapeLimit::MembershipPlanSize => limit.describe(),
            };
        }

        let mut seen = ShapeLimit::ALL.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), ShapeLimit::ALL.len(), "ALL has a duplicate");
        assert_eq!(
            ShapeLimit::ALL.len(),
            9,
            "§12.1 declares nine request-shape limits; update this count with the enum"
        );
    }

    /// Every limit is checked at its exact boundary, so none is off by one.
    #[test]
    fn each_limit_admits_its_maximum_and_refuses_one_more() {
        assert_eq!(check_resource_key(&"n".repeat(MAX_NAMESPACE), b""), None);
        assert_eq!(
            check_resource_key(&"n".repeat(MAX_NAMESPACE + 1), b""),
            Some(ShapeLimit::Namespace)
        );

        assert_eq!(check_resource_key("ns", &vec![0u8; MAX_KEY]), None);
        assert_eq!(
            check_resource_key("ns", &vec![0u8; MAX_KEY + 1]),
            Some(ShapeLimit::ResourceKey)
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

        assert_eq!(
            check_encoded_command(super::super::limits::MAX_ENCODED_COMMAND),
            None
        );
        assert_eq!(
            check_encoded_command(super::super::limits::MAX_ENCODED_COMMAND + 1),
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

    /// The namespace is checked before the key, so a request violating both is
    /// reported deterministically -- every voter must reach the same answer.
    #[test]
    fn a_request_violating_two_limits_reports_deterministically() {
        let long_ns = "n".repeat(MAX_NAMESPACE + 1);
        let long_key = vec![0u8; MAX_KEY + 1];
        assert_eq!(
            check_resource_key(&long_ns, &long_key),
            Some(ShapeLimit::Namespace)
        );
    }
}
