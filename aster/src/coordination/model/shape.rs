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
    /// The plan names ROOT as a voter or a retained learner.
    ///
    /// Refused for the reason a genesis naming ROOT as an initial voter is:
    /// a member runs its identity's secret as its Aster endpoint secret on a
    /// live node, and ROOT's secret is the deployment's offline trust anchor.
    /// Shapeless rather than reason-coded because it does not depend on
    /// replicated state — ROOT is ROOT at every revision.
    MembershipPlanRoot => "a membership plan names the deployment ROOT as a member",
    MemberSeedUnknownNode => "a member seed names a node the plan does not contain",
    MemberSeedPermit => "a member seed's join permit does not match its node's status",
    MemberSeedTicketEmpty => "a member seed carries no ticket",
    MemberSeedTicketInvalid => "a member seed ticket is not a canonical Aster ticket",
    MemberSeedTicketCredential => "a member seed ticket carries a credential",
    MemberSeedTicketNode => "a member seed ticket is for another node",
    MemberSeedTicketNonCanonical => "a member seed ticket is not its own canonical encoding",
    MembershipNewVoters => "a membership plan adds more than two new voters at once",
}

/// The `retain` flag §7.3 step 4 passes to `change_membership`.
///
/// **False, normatively.** OpenRaft's `retain = true` turns every member absent
/// from the new voter set into a learner, so a plan that removes voters adds
/// learners the plan never named — and the projection below, which counts the
/// nodes a plan *introduces*, cannot see them.
///
/// The counterexample is a shrink: five voters and six stranded learners,
/// cleaning up to three voters. It introduces no node, so the projection is six
/// and admission accepts it. Under `retain = true` the two departing voters
/// become learners, the committed uniform membership carries eight, and the
/// storage layer — correctly — refuses to persist a membership it could not
/// reopen, on every voter at once.
///
/// `retain = false` removes them instead, which is also what §9 already
/// expresses: a departing voter that should stay is named in the plan's
/// `retained_learners`, and is then a current member rather than an
/// introduction. Nothing is lost and the projection becomes exact.
///
/// A constant rather than an argument, for the same reason the storage codec
/// version is: a controller that could be *asked* to retain could commit a
/// membership no voter can load.
///
/// **It has no shipping consumer yet.** Slice 2 declares it and only Slice 2's
/// tests reference it; §7.3's controller is Slice 3's, and its gate carries the
/// obligation to read this rather than pass a literal. Until then this pins the
/// decision, not the call.
///
/// `retain = false` alone is also not sufficient. It removes a departing voter
/// even when the plan names it in `retained_learners`, so §7.3 step 5 must
/// **replace** the node map with the exact voters plus retained learners rather
/// than add before it removes -- see
/// `step_five_replaces_the_node_map_rather_than_adding_first`.
pub const MEMBERSHIP_RETAIN: bool = false;

/// The learners a plan's execution would transiently hold.
///
/// Not a [`ShapeLimit`]: exceeding the envelope is a property of the *group*
/// rather than of the request, so §10 puts it at stage 3 with a reason code
/// (`25 MembershipLearnerEnvelope`) rather than at stage 4. This is the
/// arithmetic; `membership::decide_change` is where the refusal is raised.
///
/// §7.3 step 2 adds every node the plan names that is not already a member, and
/// step 5 removes the learners it is not retaining -- so the peak is the
/// learners already present *plus* the ones being added, before either
/// promotion or removal happens.
///
/// Passing the current learners rather than assuming the retained two is the
/// whole point. A change that fails during `AddingLearners` leaves its
/// transient learners in the node map with no intent active, so the next plan
/// starts from six rather than two; without this the envelope ratcheted, and a
/// second plan produced a membership the store applied and could not reopen.
///
/// This counts introductions only, which is exact **because**
/// [`MEMBERSHIP_RETAIN`] is false: a departing voter is removed rather than
/// demoted, so no learner appears that the plan did not name.
pub fn projected_learners(
    current_learners: &std::collections::BTreeSet<[u8; 32]>,
    current_members: &std::collections::BTreeSet<[u8; 32]>,
    desired_voters: &[[u8; 32]],
    retained_learners: &[[u8; 32]],
) -> usize {
    let missing = desired_voters
        .iter()
        .chain(retained_learners)
        .filter(|node| !current_members.contains(*node));
    current_learners
        .iter()
        .chain(missing)
        .collect::<std::collections::BTreeSet<_>>()
        .len()
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

/// The learners a membership can name while a change is being applied.
///
/// Three groups, each bounded by a cap already declared, and the sum is what a
/// node map has to hold mid-change rather than a fourth constant:
///
/// * the retained learners the change is **replacing**, which §9 removes only
///   at step 5, after the joint transition;
/// * the **new voters**, which the controller adds as learners at step 2 and
///   promotes at step 4;
/// * the retained learners the change is **arriving at**.
///
/// So six, from `2 + MAX_NEW_VOTERS + 2`. §9 bounds retained and transient
/// learners at two *each*, and a plan may legally replace both learners while
/// adding both new voters — nothing in §9 forbids it, and forbidding it would
/// be a rule this implementation invented. An earlier envelope of four read the
/// retained bound as the total and made that plan's own mid-change state
/// unloadable.
pub const MAX_LEARNER_ENVELOPE: usize =
    MAX_RETAINED_LEARNERS + MAX_NEW_VOTERS + MAX_RETAINED_LEARNERS;

/// §12.1: at most two **new** voter identities per change.
///
/// The bound §12.1's `max_encoded_command` arithmetic assumes, and the reason a
/// `ChangeMembership` is bounded at all: each new voter carries a ticket and a
/// signed join permit. It also keeps a single change from replacing a quorum
/// outright, which is a membership change no surviving voter voted for.
pub const MAX_NEW_VOTERS: usize = 2;

/// Check a membership plan's shape.
///
/// A production plan targets exactly 3 or 5 voters -- never an even set, which
/// has no majority advantage and a worse failure mode -- and retains at most two
/// learners. Larger replacements use successive committed plans.
pub fn check_membership_plan(
    voters: usize,
    retained_learners: usize,
    deployment_profile_code: i32,
) -> Option<ShapeLimit> {
    if retained_learners > MAX_RETAINED_LEARNERS {
        return Some(ShapeLimit::MembershipPlanSize);
    }
    profile_permits_voters(deployment_profile_code, voters)
        .then_some(ShapeLimit::MembershipPlanSize)
}

/// §7.1's deployment profiles, as the exhaustive predicate apply uses.
///
/// Returns `true` when the count is **not** permitted, matching the `Option`
/// idiom of the other shape checks. An unknown profile code permits nothing: a
/// group whose genesis names a profile this build does not implement must not
/// fall back to the production rule and quietly accept a membership its
/// operators did not ask for.
pub fn profile_permits_voters(deployment_profile_code: i32, voters: usize) -> bool {
    let permitted = match deployment_profile_code {
        PROFILE_PROD => voters == MIN_VOTERS || voters == MAX_VOTERS,
        PROFILE_DEV_SINGLE => voters == 1,
        PROFILE_DEV_EVEN => voters == 2,
        _ => false,
    };
    !permitted
}

/// §7.1 profile 1: production. Exactly 3 or 5 voters.
pub const PROFILE_PROD: i32 = 1;
/// §7.1 profile 2: a single-voter development group.
pub const PROFILE_DEV_SINGLE: i32 = 2;
/// §7.1 profile 3: a two-voter development group, deliberately even -- it has
/// no fault tolerance and exists to make the even-quorum path testable.
pub const PROFILE_DEV_EVEN: i32 = 3;

/// Every profile §7.1 declares.
///
/// Listed so a stored code can be checked for membership in the set, which
/// `profile_permits_voters` cannot answer on its own: it reports whether a
/// *count* is permitted, and an unknown profile permits no count -- so a group
/// that has committed no membership yet slips past it entirely.
pub const PROFILES: [i32; 3] = [PROFILE_PROD, PROFILE_DEV_SINGLE, PROFILE_DEV_EVEN];

/// How many voters in `desired` are not already committed members?
pub fn new_voters(desired: &[[u8; 32]], current: &std::collections::BTreeSet<[u8; 32]>) -> usize {
    desired.iter().filter(|v| !current.contains(*v)).count()
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

/// Validate one member-seed ticket (§9).
///
/// Decoded on every voter from the committed command, so the verdict is
/// replicated rather than taken on a controller's word.
pub(crate) fn check_seed_ticket(ticket: &[u8], node: &[u8; 32]) -> Result<(), ShapeLimit> {
    if ticket.is_empty() {
        return Err(ShapeLimit::MemberSeedTicketEmpty);
    }
    let text = std::str::from_utf8(ticket).map_err(|_| ShapeLimit::MemberSeedTicketInvalid)?;
    let parsed = aster_transport_core::ticket::AsterTicket::from_base58_str(text)
        .map_err(|_| ShapeLimit::MemberSeedTicketInvalid)?;

    // **Parsing is not canonicity.** The core decoder ignores the header's
    // unused low bits and other reserved space, so a ticket with a reserved bit
    // set decodes cleanly to the same value -- which would give one route two
    // spellings, and put bytes into replicated state that no encoder produces.
    // Re-encoding and comparing is the check: the writer emits exactly one
    // encoding of a given value, so equality proves the input was that one.
    //
    // Done here rather than by tightening the core decoder: that decoder is
    // shared with every existing ticket consumer, and changing what it accepts
    // is a far wider change than this rule needs. A stricter rule at the point
    // where bytes enter replicated state is both narrower and stronger.
    let canonical = parsed
        .to_base58_string()
        .map_err(|_| ShapeLimit::MemberSeedTicketInvalid)?;
    if canonical != text {
        return Err(ShapeLimit::MemberSeedTicketNonCanonical);
    }

    // Open only: a route row is replicated group state that every voter and
    // every future snapshot reader holds, so a credential in it is a secret
    // distributed to everyone who can read the group.
    if !matches!(
        parsed.credential,
        None | Some(aster_transport_core::ticket::TicketCredential::Open)
    ) {
        return Err(ShapeLimit::MemberSeedTicketCredential);
    }
    // The ticket must be for the node it seeds. Without this a plan may add
    // node A while installing a route that dials node B.
    if &parsed.endpoint_id != node {
        return Err(ShapeLimit::MemberSeedTicketNode);
    }
    Ok(())
}

/// The structural half of the §7.1 join-permit validation, for callers that
/// hold no group authority — the §7.7 placement planner (§7.7.1: "every
/// supplied/provisioned seed passes the existing canonical ticket and permit
/// validation before the planner can emit `Plan`").
///
/// Mirrors [`model::authority`](crate::coordination::model::authority)'s
/// apply-time checks minus the two things a planner cannot hold: the pinned
/// authority key (so no signature verdict — a signature checked with the key
/// that came with the envelope proves nothing) and the group's own
/// domain/group-id/genesis bindings. Apply remains the decision of record;
/// this exists so a malformed or misbound permit is refused before a plan is
/// ever emitted, not discovered as a replicated refusal.
pub(crate) fn check_seed_permit(permit_bytes: &[u8], node: &[u8; 32]) -> Result<(), String> {
    use crate::coordination::contract::payload_fory;
    use crate::coordination::raft::{JoinPermitV1, SignedCoordinationDocumentV1};

    let envelope: SignedCoordinationDocumentV1 = payload_fory::<SignedCoordinationDocumentV1>()
        .deserialize(permit_bytes)
        .map_err(|_| "the permit envelope does not decode".to_string())?;
    if envelope.format_version != 1 {
        return Err(format!(
            "permit envelope format {} is not version 1",
            envelope.format_version
        ));
    }
    if envelope.authority_public_key.len() != 32 || envelope.signature.len() != 64 {
        return Err("the permit envelope's key or signature has the wrong width".to_string());
    }

    let permit: JoinPermitV1 = payload_fory::<JoinPermitV1>()
        .deserialize(&envelope.document_bytes)
        .map_err(|_| "the permit document does not decode".to_string())?;
    // §7.1: decode then re-encode must reproduce the signed bytes exactly.
    let reencoded = payload_fory::<JoinPermitV1>()
        .serialize(&permit)
        .map_err(|_| "the permit document does not re-encode".to_string())?;
    if reencoded != envelope.document_bytes {
        return Err("the permit document is not the canonical encoding".to_string());
    }

    if permit.format_version != 1 {
        return Err(format!(
            "permit format {} is not version 1",
            permit.format_version
        ));
    }
    if permit.group_id.len() != 32
        || permit.genesis_hash.len() != 32
        || permit.candidate_node_id.len() != 32
        || permit.join_nonce.len() != 32
        || permit.authority_domain.is_empty()
        // 128 is `authority::MAX_AUTHORITY_DOMAIN_BYTES`, restated because
        // that module is voter-gated and this check compiles for the client.
        || permit.authority_domain.len() > 128
        || !permit.authority_domain.is_ascii()
    {
        return Err("a permit field has the wrong width".to_string());
    }
    if permit.bootstrap_senders.is_empty()
        || permit
            .bootstrap_senders
            .iter()
            .any(|n| n.node_id.len() != 32)
        || permit
            .bootstrap_senders
            .windows(2)
            .any(|w| w[0].node_id >= w[1].node_id)
    {
        return Err("the permit's bootstrap senders are empty, unsorted, or malformed".to_string());
    }

    // The bindings a planner can check: this permit is for this seed's node,
    // via the same canonical-ticket rule the seed's own ticket passes.
    if permit.candidate_node_id.as_slice() != node.as_slice() {
        return Err("the permit names a different candidate node".to_string());
    }
    check_seed_ticket(permit.candidate_ticket.as_bytes(), node)
        .map_err(|limit| format!("the permit's candidate ticket is invalid: {limit:?}"))?;
    Ok(())
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
        assert_eq!(ShapeLimit::ALL.len(), 23, "update this count with the enum");
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
