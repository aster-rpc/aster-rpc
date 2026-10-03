//! The client's one **reply-validity** boundary (the fifth and sixth
//! reviews' P0s).
//!
//! Ownership — protocol, group, client id, sequence, request hash — says a
//! reply is addressed to this entry. It does not say the reply is a §5.2
//! reply: a decodable frame can claim `Granted` with no snapshot, pair a
//! disposition with a reason no answer path produces (`Consumed` +
//! `21 ActiveClientCap` claims a session was created by a path that never
//! creates one), decorate an acquire with a membership operation id, name a
//! snapshot about some other resource, or smuggle malformed nested
//! identifiers. This module holds the full, **request-aware** verdict for
//! all nineteen Store methods, shared verbatim by live delivery
//! (`driver::decode_and_check`), the journal's persist transitions, and the
//! journal reopen — one validator per reply shape, no spelling of "live
//! replies are less validated than saved ones".
//!
//! ## The mutation matrix
//!
//! A mutation reply's `(disposition, outcome, reason)` triple is checked
//! against the three answer paths that exist (§5.2, §8.2.1, §10):
//!
//! * a **success** is `Consumed` with reason `0`, and its outcome is the one
//!   the pending method can produce;
//! * a **`Consumed` rejection** carries reason `0` only as §10.1's shapeless
//!   pairing, and otherwise a reason apply can save
//!   ([`Reason::may_be_saved_rejection`]);
//! * a **`NotConsumed` rejection** carries a classifier or admission reason
//!   ([`Reason::may_be_not_consumed`]) — never `0`, never a consumed
//!   conflict, which §8.2.1 forbids answering pre-write precisely because it
//!   would wedge the client on a stale expectation.
//!
//! The auxiliary fields are method-correlated: a membership success echoes
//! the request's operation id and nothing else ever carries one; the
//! revision fields are `0` exactly where §5.2.1 says "not applicable".
//!
//! ## Reads, statuses and streams
//!
//! Every typed reply implements [`ValidReply`] against its own request
//! type: identity echoes (probe targets, snapshot resources, membership
//! operation ids), §5.2 code domains (lifecycle, role, profile, fatal,
//! intent phase, audit operation/result), ordering (sorted, unique,
//! disjoint sets), and the §12.1/§11.4 byte ceilings on the recovery
//! stream. The stream-level invariants — audit count/order/cursor, chunk
//! sizes — live at the consuming loops in `client::mod`, bounded by
//! `wire::MAX_EXPORT_CHUNK_BYTES`.

use crate::coordination::contract::{FenceV1, LogIdV1, ResourceKeyV1, ResourceSnapshotV1};
use crate::coordination::model::limits::{rights_are_known, MAX_COUNTER, MAX_VALUE};
use crate::coordination::reason::{Outcome, Reason};
use crate::coordination::store::methods::{spec, MutationKind};
use crate::coordination::store::wire::{
    AclStatusReplyWireV1, AclStatusRequestV1, AcquireRequestV1, AdminStatusReplyWireV1,
    AdminStatusRequestV1, AuditRecordWireV1, CancelMembershipRequestV1, ChangeMembershipRequestV1,
    CompareAndSetRequestV1, GroupStatusReplyWireV1, GroupStatusRequestV1,
    MembershipStatusReplyWireV1, MembershipStatusRequestV1, MutationReplyWireV1,
    ProbeClientReplyWireV1, ProbeClientRequestV1, ReadReplyWireV1, ReadRequestV1,
    RecoveryExportManifestWireV1, RecoveryExportRequestV1, ReleaseRequestV1,
    ReleaseWithValueRequestV1, RenewRequestV1, RetireClientReplyWireV1, RetireClientRequestV1,
    RevokeRequestV1, SetAdminRequestV1, SetGrantRequestV1, STORE_PROTOCOL_VERSION,
};

/// What a mutation reply must be validated **against**: the entry it claims
/// to answer, and the world it claims to be from.
pub(crate) struct MutationContext<'a> {
    pub(crate) group_id: &'a [u8],
    /// The local actor — the §5.2 holder every grant/renewal/CAS this client
    /// commits must name.
    pub(crate) local_actor: &'a [u8],
    pub(crate) method: &'a str,
    /// The exact pending request frame, decoded per method for correlation.
    pub(crate) request: &'a [u8],
    pub(crate) client_id: &'a [u8],
    pub(crate) sequence: u64,
    pub(crate) request_hash: &'a [u8],
}

fn decode<T>(bytes: &[u8]) -> Result<T, String>
where
    T: crate::rpc::contract::WireField + fory_core::Serializer<Target = T> + 'static,
{
    crate::coordination::contract::payload_fory::<T>()
        .deserialize(bytes)
        .map_err(|_| "the pending request no longer decodes as its method's type".to_string())
}

/// The decoded facts of one pending request — everything its reply must be
/// correlated against, per method. Decoding is driven by the §5.2 method
/// table's [`MutationKind`] column, so a method the table does not classify
/// as a sequenced mutation cannot be validated at all, and a new variant is
/// a compile error in the exhaustive matches below.
pub(crate) enum RequestFacts {
    Acquire {
        resource: ResourceKeyV1,
        ttl_ms: u64,
    },
    Renew {
        fence: FenceV1,
    },
    CompareAndSet {
        fence: FenceV1,
        expected_value_revision: u64,
        new_value: Vec<u8>,
    },
    Release {
        fence: FenceV1,
    },
    ReleaseWithValue {
        fence: FenceV1,
        expected_value_revision: u64,
        final_value: Vec<u8>,
    },
    Revoke {
        resource: ResourceKeyV1,
        expected_epoch: Option<u64>,
    },
    SetGrant {
        resource: ResourceKeyV1,
        target_node_id: Vec<u8>,
        rights: u64,
        expected_acl_revision: u64,
    },
    SetAdmin {
        target_node_id: Vec<u8>,
        expected_admin_revision: u64,
    },
    ChangeMembership {
        operation_id: Vec<u8>,
        expected_membership_log_id: Option<LogIdV1>,
        desired_voters: Vec<Vec<u8>>,
        retained_learners: Vec<Vec<u8>>,
        member_seeds: Vec<crate::coordination::contract::MemberSeedV1>,
    },
    CancelMembership {
        operation_id: Vec<u8>,
        expected_phase_code: i32,
    },
}

impl RequestFacts {
    /// The §5.2 mutation this request is.
    fn kind(&self) -> MutationKind {
        match self {
            RequestFacts::Acquire { .. } => MutationKind::Acquire,
            RequestFacts::Renew { .. } => MutationKind::Renew,
            RequestFacts::CompareAndSet { .. } => MutationKind::CompareAndSet,
            RequestFacts::Release { .. } => MutationKind::Release,
            RequestFacts::ReleaseWithValue { .. } => MutationKind::ReleaseWithValue,
            RequestFacts::Revoke { .. } => MutationKind::Revoke,
            RequestFacts::SetGrant { .. } => MutationKind::SetGrant,
            RequestFacts::SetAdmin { .. } => MutationKind::SetAdmin,
            RequestFacts::ChangeMembership { .. } => MutationKind::ChangeMembership,
            RequestFacts::CancelMembership { .. } => MutationKind::CancelMembership,
        }
    }

    /// The resource this request names, for snapshot correlation. `None` for
    /// the methods whose replies carry no resource snapshot.
    fn resource(&self) -> Option<&ResourceKeyV1> {
        match self {
            RequestFacts::Acquire { resource, .. }
            | RequestFacts::Revoke { resource, .. }
            | RequestFacts::SetGrant { resource, .. } => Some(resource),
            RequestFacts::Renew { fence }
            | RequestFacts::CompareAndSet { fence, .. }
            | RequestFacts::Release { fence }
            | RequestFacts::ReleaseWithValue { fence, .. } => Some(&fence.resource),
            RequestFacts::SetAdmin { .. }
            | RequestFacts::ChangeMembership { .. }
            | RequestFacts::CancelMembership { .. } => None,
        }
    }

    /// The fence this request presents, if it is a fenced operation.
    fn fence(&self) -> Option<&FenceV1> {
        match self {
            RequestFacts::Renew { fence }
            | RequestFacts::CompareAndSet { fence, .. }
            | RequestFacts::Release { fence }
            | RequestFacts::ReleaseWithValue { fence, .. } => Some(fence),
            _ => None,
        }
    }

    /// The operation id a membership success must echo.
    fn operation_id(&self) -> Option<&[u8]> {
        match self {
            RequestFacts::ChangeMembership { operation_id, .. }
            | RequestFacts::CancelMembership { operation_id, .. } => Some(operation_id),
            _ => None,
        }
    }
}

/// Decode one pending request per the §5.2 method table.
pub(crate) fn decode_facts(method: &str, request: &[u8]) -> Result<RequestFacts, String> {
    let Some(kind) = spec(method).and_then(|m| m.mutation) else {
        return Err(format!("'{method}' is not a sequenced mutation method"));
    };
    Ok(match kind {
        MutationKind::Acquire => {
            let r = decode::<AcquireRequestV1>(request)?;
            RequestFacts::Acquire {
                resource: r.resource,
                ttl_ms: r.ttl_ms,
            }
        }
        MutationKind::Renew => RequestFacts::Renew {
            fence: decode::<RenewRequestV1>(request)?.fence,
        },
        MutationKind::CompareAndSet => {
            let r = decode::<CompareAndSetRequestV1>(request)?;
            RequestFacts::CompareAndSet {
                fence: r.fence,
                expected_value_revision: r.expected_value_revision,
                new_value: r.new_value,
            }
        }
        MutationKind::Release => RequestFacts::Release {
            fence: decode::<ReleaseRequestV1>(request)?.fence,
        },
        MutationKind::ReleaseWithValue => {
            let r = decode::<ReleaseWithValueRequestV1>(request)?;
            RequestFacts::ReleaseWithValue {
                fence: r.fence,
                expected_value_revision: r.expected_value_revision,
                final_value: r.final_value,
            }
        }
        MutationKind::Revoke => {
            let r = decode::<RevokeRequestV1>(request)?;
            RequestFacts::Revoke {
                resource: r.resource,
                expected_epoch: r.expected_epoch,
            }
        }
        MutationKind::SetGrant => {
            let r = decode::<SetGrantRequestV1>(request)?;
            RequestFacts::SetGrant {
                resource: r.resource,
                target_node_id: r.target_node_id,
                rights: r.rights,
                expected_acl_revision: r.expected_acl_revision,
            }
        }
        MutationKind::SetAdmin => {
            let r = decode::<SetAdminRequestV1>(request)?;
            RequestFacts::SetAdmin {
                target_node_id: r.target_node_id,
                expected_admin_revision: r.expected_admin_revision,
            }
        }
        MutationKind::ChangeMembership => {
            let r = decode::<ChangeMembershipRequestV1>(request)?;
            RequestFacts::ChangeMembership {
                operation_id: r.operation_id,
                expected_membership_log_id: r.expected_membership_log_id,
                desired_voters: r.desired_voters.into_iter().map(|n| n.node_id).collect(),
                retained_learners: r.retained_learners.into_iter().map(|n| n.node_id).collect(),
                member_seeds: r.member_seeds,
            }
        }
        MutationKind::CancelMembership => {
            let r = decode::<CancelMembershipRequestV1>(request)?;
            RequestFacts::CancelMembership {
                operation_id: r.operation_id,
                expected_phase_code: r.expected_phase_code,
            }
        }
    })
}

/// The **outbound** half of the per-method specification: is this a request
/// the client may send at all?
///
/// Every **deterministic** request-shape rule runs here, before anything is
/// journaled — because a journaled request that can only earn a stage-4
/// status denial is a permanent wedge: §10 forbids substituting bytes at a
/// sent sequence, so the driver could only resend the same impossible frame
/// forever. That covers the §4.2 namespace grammar (not merely lengths),
/// §12.1 leaf and TTL bounds, counter ceilings on every expectation a reply
/// successor is computed from, the §9 membership-plan shape (ordering, role
/// disjointness, the profile-independent voter counts, seed bounds,
/// seed-in-plan, canonical tickets), the §7.3 phase domain, and the two
/// fence facts no reply can repair — the minting group and this actor's
/// hold.
///
/// The rules that depend on replicated state — the profile's exact voter
/// count, ROOT membership, which seeds need permits and whether they
/// verify — cannot be decided from the request alone; `change_membership`
/// closes them by running the shared §9 `plan_shape` against a validated
/// linearizable status cut plus the operator-sealed authority, and the
/// voter's fence-before-shape ordering guarantees the plan is judged
/// against exactly the state it was proven against (or answered with the
/// typed `13`).
pub(crate) fn validate_request(
    group_id: &[u8],
    local_actor: &[u8],
    facts: &RequestFacts,
) -> Result<(), String> {
    use crate::coordination::model::shape;

    let counter = |what: &str, value: u64| {
        if value > MAX_COUNTER {
            Err(format!("{what} is past the §12.1 counter ceiling"))
        } else {
            Ok(())
        }
    };

    if let Some(resource) = facts.resource() {
        // The full §4.2 grammar, via the identity type's own constructor —
        // not a length check. An invalid namespace is shapeless to the
        // protocol and can only be denied.
        shape::validate_resource_key(&resource.namespace, &resource.key)
            .map_err(|limit| limit.describe().to_string())?;
    }
    if let Some(fence) = facts.fence() {
        if fence.group_id != group_id {
            return Err("the fence names another coordination group".to_string());
        }
        if fence.holder != local_actor {
            return Err("the fence's holder is not this client's actor".to_string());
        }
        counter("the fence's epoch", fence.epoch)?;
    }
    match facts {
        RequestFacts::Acquire { ttl_ms, .. } => {
            crate::coordination::store::wire::ttl_bounds(*ttl_ms)
                .map_err(|invalid| invalid.describe())?;
        }
        RequestFacts::CompareAndSet {
            expected_value_revision,
            new_value,
            ..
        } => {
            if new_value.len() > MAX_VALUE {
                return Err("the new value exceeds the §12.1 bound".to_string());
            }
            counter("the expected value revision", *expected_value_revision)?;
        }
        RequestFacts::ReleaseWithValue {
            expected_value_revision,
            final_value,
            ..
        } => {
            if final_value.len() > MAX_VALUE {
                return Err("the final value exceeds the §12.1 bound".to_string());
            }
            counter("the expected value revision", *expected_value_revision)?;
        }
        RequestFacts::Revoke { expected_epoch, .. } => {
            if let Some(expected) = expected_epoch {
                counter("the expected epoch", *expected)?;
            }
        }
        RequestFacts::SetGrant {
            target_node_id,
            rights,
            expected_acl_revision,
            ..
        } => {
            if target_node_id.len() != 32 {
                return Err("the grant target is not a 32-byte node id".to_string());
            }
            if !rights_are_known(*rights) {
                return Err("the rights carry a bit v1 does not define".to_string());
            }
            counter("the expected ACL revision", *expected_acl_revision)?;
        }
        RequestFacts::SetAdmin {
            target_node_id,
            expected_admin_revision,
            ..
        } => {
            if target_node_id.len() != 32 {
                return Err("the admin target is not a 32-byte node id".to_string());
            }
            counter("the expected admin revision", *expected_admin_revision)?;
        }
        RequestFacts::ChangeMembership {
            operation_id,
            expected_membership_log_id,
            desired_voters,
            retained_learners,
            member_seeds,
        } => {
            if operation_id.len() != 16 {
                return Err("the operation id is not 16 bytes".to_string());
            }
            if let Some(id) = expected_membership_log_id {
                check_log_id("the expected membership log id", id)?;
            }
            validate_membership_plan(desired_voters, retained_learners, member_seeds)?;
        }
        RequestFacts::CancelMembership {
            operation_id,
            expected_phase_code,
        } => {
            if operation_id.len() != 16 {
                return Err("the operation id is not 16 bytes".to_string());
            }
            if !domains::PHASE.contains(expected_phase_code) {
                return Err(format!(
                    "phase {expected_phase_code} is not a §7.3 intent phase"
                ));
            }
        }
        RequestFacts::Renew { .. } | RequestFacts::Release { .. } => {}
    }
    Ok(())
}

/// The deterministic subset of the model's §9 `plan_shape`: everything a
/// plan can violate without consulting replicated state, via the same
/// shared `shape` functions the model runs, so the two cannot disagree
/// about what these rules mean.
fn validate_membership_plan(
    desired_voters: &[Vec<u8>],
    retained_learners: &[Vec<u8>],
    member_seeds: &[crate::coordination::contract::MemberSeedV1],
) -> Result<(), String> {
    use crate::coordination::model::shape;

    let narrow = |what: &str, ids: &[Vec<u8>]| -> Result<Vec<[u8; 32]>, String> {
        ids.iter()
            .map(|id| {
                <[u8; 32]>::try_from(id.as_slice())
                    .map_err(|_| format!("{what} contains a malformed node id"))
            })
            .collect()
    };
    let voters = narrow("the desired voter list", desired_voters)?;
    let learners = narrow("the retained learner list", retained_learners)?;

    // §9's profile-independent envelope: no §7.1 profile legalizes zero
    // voters, four, or more than five; the profile's exact rule is
    // replicated state and stays the group's.
    if !matches!(voters.len(), 1 | 2 | 3 | 5) {
        return Err("no §7.1 profile permits this desired voter count".to_string());
    }
    if learners.len() > 2 {
        return Err("a plan retains at most two learners (§9)".to_string());
    }
    if !shape::sorted_unique(&voters) || !shape::sorted_unique(&learners) {
        return Err("a plan's node vectors must be sorted and unique (§9)".to_string());
    }
    if voters.iter().any(|v| learners.contains(v)) {
        return Err("a node cannot be both a voter and a learner (§9)".to_string());
    }
    let seed_ids: Vec<[u8; 32]> = member_seeds
        .iter()
        .map(|seed| {
            <[u8; 32]>::try_from(seed.node_id.as_slice())
                .map_err(|_| "a member seed names a malformed node id".to_string())
        })
        .collect::<Result<_, _>>()?;
    if !shape::sorted_unique(&seed_ids) {
        return Err("the member seeds must be sorted and unique (§9)".to_string());
    }
    for (seed, id) in member_seeds.iter().zip(&seed_ids) {
        crate::coordination::store::wire::member_seed_bounds(seed)
            .map_err(|invalid| invalid.describe())?;
        if !voters.contains(id) && !learners.contains(id) {
            return Err("a member seed names a node the plan does not contain (§9)".to_string());
        }
        shape::check_seed_ticket(seed.ticket.as_bytes(), id)
            .map_err(|limit| limit.describe().to_string())?;
    }
    Ok(())
}

fn check_log_id(what: &str, id: &LogIdV1) -> Result<(), String> {
    if id.leader_node_id.len() != 32 {
        return Err(format!("{what} names a malformed leader node id"));
    }
    Ok(())
}

/// The full §5.2 verdict on one mutation reply against its entry.
pub(crate) fn validate_mutation_reply(
    cx: &MutationContext<'_>,
    reply: &MutationReplyWireV1,
) -> Result<(), String> {
    // Ownership: this entry's answer and no other's.
    if reply.protocol_version != STORE_PROTOCOL_VERSION {
        return Err("the reply carries another protocol version".to_string());
    }
    if reply.group_id != cx.group_id {
        return Err("the reply names another coordination group".to_string());
    }
    if reply.client_id != cx.client_id
        || reply.sequence != cx.sequence
        || reply.request_hash != cx.request_hash
    {
        return Err(
            "the reply does not echo this entry's client id, sequence and request hash".to_string(),
        );
    }

    // Disposition, outcome and reason must be §5.2 values that an answer
    // path can actually pair.
    if reply.disposition_code != 1 && reply.disposition_code != 2 {
        return Err(format!(
            "disposition {} is not a §5.2 disposition",
            reply.disposition_code
        ));
    }
    let Some(outcome) = Outcome::from_code(reply.outcome_code) else {
        return Err(format!(
            "outcome {} is not a §5.2 outcome",
            reply.outcome_code
        ));
    };
    let Some(reason) = Reason::from_code(reply.reason_code) else {
        return Err(format!("reason {} is not a §5.2 reason", reply.reason_code));
    };
    // No separate reserved-code clause: the reserved `20` is in neither
    // saveable nor NotConsumed-answerable list and a success demands reason
    // 0, so the pair rules below refuse it on every path. The campaign
    // proved an explicit clause here unfalsifiable — shadowed on all inputs.
    if outcome != Outcome::Rejected {
        // A success carrying a refusal reason — a terminal one included — is
        // neither a success nor a refusal.
        if reason != Reason::None {
            return Err("a non-rejection outcome must carry reason 0".to_string());
        }
        if reply.disposition_code != 1 {
            return Err("a success must be Consumed".to_string());
        }
    } else {
        // §5.2 has exactly three rejection paths, and each pins the pair.
        match (reply.disposition_code, reason) {
            // §10.1: the one Consumed pairing with no reason — a committed
            // logical request-shape violation.
            (1, Reason::None) => {}
            // An apply-time refusal, saved. The classifier-only reasons are
            // never saved: a Consumed `21 ActiveClientCap` claims a session
            // was created by a path that never creates one, and acknowledging
            // it would advance this journal past a sequence the group still
            // expects.
            (1, saved) if saved.may_be_saved_rejection() => {}
            (1, saved) => {
                return Err(format!(
                    "reason {} is never saved with a Consumed rejection",
                    saved.code()
                ));
            }
            // A pre-write answer: §8.2.1 classifier or §10 stage-3 admission.
            // A consumed conflict here is the wedge §8.2.1 forbids, and
            // reason 0 has no NotConsumed pairing at all.
            (2, refused) if refused.may_be_not_consumed() => {}
            (2, refused) => {
                return Err(format!(
                    "reason {} cannot answer NotConsumed",
                    refused.code()
                ));
            }
            _ => unreachable!("dispositions were narrowed above"),
        }
    }

    // Method correlation, from the §5.2 method table: this method's success
    // or a rejection — nothing else.
    let facts = decode_facts(cx.method, cx.request)?;
    let success_code = facts.kind().success_outcome_code();
    if reply.outcome_code != success_code && outcome != Outcome::Rejected {
        return Err(format!(
            "outcome {} does not answer method '{}'",
            reply.outcome_code, cx.method
        ));
    }

    // Snapshot presence follows the outcome: the five register successes
    // carry one; `GrantUpdated` carries the granted row's snapshot when the
    // row exists (apply builds it that way) and a rejection may carry the
    // pre-state, so both are optional; the admin and membership successes
    // carry none.
    let register_success = matches!(
        outcome,
        Outcome::Granted
            | Outcome::Renewed
            | Outcome::Mutated
            | Outcome::Released
            | Outcome::Revoked
    );
    let never_carries = matches!(
        outcome,
        Outcome::AdminUpdated | Outcome::MembershipAccepted | Outcome::MembershipCancelled
    );
    if register_success && reply.snapshot.is_none() {
        return Err(format!(
            "outcome {} requires a resource snapshot and the reply carries none",
            reply.outcome_code
        ));
    }
    if never_carries && reply.snapshot.is_some() {
        return Err(format!(
            "outcome {} carries no resource snapshot, yet the reply has one",
            reply.outcome_code
        ));
    }

    if let Some(snapshot) = &reply.snapshot {
        validate_snapshot(cx, &facts, outcome, snapshot)?;
    }

    // Auxiliary fields are method-correlated, not merely well-formed
    // (§5.2.1). Apply writes each into exactly one reply family and zeros
    // it everywhere else, so a value outside its family is not this
    // protocol's reply.
    let membership_success = matches!(
        outcome,
        Outcome::MembershipAccepted | Outcome::MembershipCancelled
    );
    match &reply.operation_id {
        Some(op) => {
            if !membership_success {
                return Err(format!(
                    "outcome {} carries no operation id, yet the reply has one",
                    reply.outcome_code
                ));
            }
            let expected = facts
                .operation_id()
                .ok_or_else(|| "a membership success must echo an operation id".to_string())?;
            if op != expected {
                return Err("the reply's operation id does not echo the request".to_string());
            }
        }
        None => {
            if membership_success {
                return Err(format!(
                    "outcome {} must carry the request's operation id",
                    reply.outcome_code
                ));
            }
        }
    }
    if reply.membership_log_id.is_some() && !membership_success {
        return Err(format!(
            "outcome {} carries no membership log id, yet the reply has one",
            reply.outcome_code
        ));
    }
    if let Some(id) = &reply.membership_log_id {
        check_log_id("the reply's membership log id", id)?;
    }
    // `admin_revision`: correlated to the request on AdminUpdated — apply
    // fences on `expected_admin_revision`, so a success reports either the
    // advanced revision or, for a no-op, the expected one it echoed — the
    // observed revision on a rejection, and `0` everywhere else.
    // Reply-level counters obey the §12.1 ceiling before any successor is
    // computed from them: apply can never write past `MAX_COUNTER`, and a
    // revision above it is not a value any checked successor produced.
    if reply.acl_revision > MAX_COUNTER {
        return Err("the reply's ACL revision is past the counter ceiling".to_string());
    }
    if reply.admin_revision > MAX_COUNTER {
        return Err("the reply's admin revision is past the counter ceiling".to_string());
    }
    match outcome {
        Outcome::AdminUpdated => {
            let RequestFacts::SetAdmin {
                expected_admin_revision,
                ..
            } = &facts
            else {
                return Err("outcome 7 answers only set_admin".to_string());
            };
            let advanced = expected_admin_revision
                .checked_add(1)
                .filter(|next| *next <= MAX_COUNTER);
            if reply.admin_revision != *expected_admin_revision
                && Some(reply.admin_revision) != advanced
            {
                return Err(
                    "an AdminUpdated revision does not follow from the request's fence".to_string(),
                );
            }
        }
        Outcome::Rejected => {}
        _ => {
            if reply.admin_revision != 0 {
                return Err(format!(
                    "outcome {} carries no admin revision, yet the reply has one",
                    reply.outcome_code
                ));
            }
        }
    }
    // `acl_revision`: apply copies it from the same row the snapshot was
    // taken from, so the two never disagree; without a snapshot it is the
    // grant echo (GrantUpdated) or `0`. A GrantUpdated additionally follows
    // from the request's `expected_acl_revision` fence: the advanced
    // revision, or the echoed one for a no-op re-issue.
    match (&reply.snapshot, outcome) {
        (Some(snapshot), _) => {
            if reply.acl_revision != snapshot.acl_revision {
                return Err("the reply's ACL revision contradicts its snapshot".to_string());
            }
        }
        (None, Outcome::GrantUpdated | Outcome::Rejected) => {}
        (None, _) => {
            if reply.acl_revision != 0 {
                return Err(format!(
                    "outcome {} carries no ACL revision, yet the reply has one",
                    reply.outcome_code
                ));
            }
        }
    }
    if outcome == Outcome::GrantUpdated {
        let RequestFacts::SetGrant {
            expected_acl_revision,
            ..
        } = &facts
        else {
            return Err("outcome 6 answers only set_grant".to_string());
        };
        let advanced = expected_acl_revision
            .checked_add(1)
            .filter(|next| *next <= MAX_COUNTER);
        if reply.acl_revision != *expected_acl_revision && Some(reply.acl_revision) != advanced {
            return Err("a GrantUpdated revision does not follow from the request".to_string());
        }
    }
    Ok(())
}

/// The nested snapshot: identity echoed from the request, §12.1 leaf
/// bounds, and the §9.1 postconditions of the outcome that carried it.
fn validate_snapshot(
    cx: &MutationContext<'_>,
    facts: &RequestFacts,
    outcome: Outcome,
    snapshot: &ResourceSnapshotV1,
) -> Result<(), String> {
    if snapshot.group_id != cx.group_id {
        return Err("the snapshot names another coordination group".to_string());
    }
    if let Some(expected) = facts.resource() {
        if snapshot.resource != *expected {
            return Err("the snapshot names another resource than the request".to_string());
        }
    }
    check_snapshot_bounds(snapshot)?;
    // Outcome-specific facts: a grant/renewal/write this client committed
    // is held by this client; a release/revoke ends with a free row.
    match outcome {
        Outcome::Granted | Outcome::Renewed | Outcome::Mutated => {
            if snapshot.holder.as_deref() != Some(cx.local_actor) {
                return Err(format!(
                    "outcome {} must leave this client holding the register",
                    outcome.code()
                ));
            }
        }
        Outcome::Released | Outcome::Revoked if snapshot.held => {
            return Err(format!(
                "outcome {} must leave the register free",
                outcome.code()
            ));
        }
        _ => {}
    }
    // The §9.1 success postconditions, correlated to the decoded request.
    // Each is a fact apply's transition arithmetic guarantees, so a success
    // that violates one is a reply no honest apply produced — an acquire
    // granted at a TTL apply can never set, a CAS whose value revision did
    // not advance from the request's expectation, a release that left the
    // fenced epoch unbumped.
    if outcome.code() == facts.kind().success_outcome_code() {
        match facts {
            RequestFacts::Acquire { ttl_ms, .. } => {
                if snapshot.ttl_ms != *ttl_ms {
                    return Err("a grant's TTL must equal the requested TTL".to_string());
                }
                if snapshot.epoch == 0 || snapshot.lease_revision == 0 {
                    return Err("a grant advances the epoch and lease revision".to_string());
                }
            }
            RequestFacts::Renew { fence } => {
                if snapshot.epoch != fence.epoch {
                    return Err("a renewal never moves the fenced epoch".to_string());
                }
            }
            RequestFacts::CompareAndSet {
                fence,
                expected_value_revision,
                new_value,
            } => {
                if snapshot.epoch != fence.epoch {
                    return Err("a write never moves the fenced epoch".to_string());
                }
                check_written_value(snapshot, new_value)?;
                if Some(snapshot.value_revision) != expected_value_revision.checked_add(1) {
                    return Err(
                        "a write advances the value revision from the request's expectation"
                            .to_string(),
                    );
                }
            }
            RequestFacts::Release { fence } => {
                if Some(snapshot.epoch) != fence.epoch.checked_add(1) {
                    return Err("a release advances the fenced epoch by one".to_string());
                }
            }
            RequestFacts::ReleaseWithValue {
                fence,
                expected_value_revision,
                final_value,
            } => {
                if Some(snapshot.epoch) != fence.epoch.checked_add(1) {
                    return Err("a release advances the fenced epoch by one".to_string());
                }
                check_written_value(snapshot, final_value)?;
                if Some(snapshot.value_revision) != expected_value_revision.checked_add(1) {
                    return Err(
                        "a write advances the value revision from the request's expectation"
                            .to_string(),
                    );
                }
            }
            RequestFacts::Revoke { expected_epoch, .. } => match expected_epoch {
                Some(expected) => {
                    if Some(snapshot.epoch) != expected.checked_add(1) {
                        return Err(
                            "a fenced revoke advances the expected epoch by one".to_string()
                        );
                    }
                }
                None => {
                    if snapshot.epoch == 0 {
                        return Err("a revoke advances the epoch".to_string());
                    }
                }
            },
            RequestFacts::SetGrant { .. }
            | RequestFacts::SetAdmin { .. }
            | RequestFacts::ChangeMembership { .. }
            | RequestFacts::CancelMembership { .. } => {}
        }
    }
    Ok(())
}

/// A written value's echo: a success snapshot that carries readable bytes
/// must carry **exactly** the bytes this request wrote. §4.3 legitimately
/// withholds them — a replay is redacted, and a caller without `READ` gets
/// `None` — but visible bytes that differ from what the command wrote are
/// not this command's result.
fn check_written_value(snapshot: &ResourceSnapshotV1, written: &[u8]) -> Result<(), String> {
    match &snapshot.value {
        Some(value) if value != written => {
            Err("the snapshot's value is not the value this request wrote".to_string())
        }
        _ => Ok(()),
    }
}

/// The §12.1 leaf bounds every snapshot obeys, mutation and read alike: a
/// snapshot naming an out-of-bounds identity, an oversized value, a TTL no
/// grant can set, an exhausted counter, or a self-contradictory hold is not
/// a projection of any legal row.
fn check_snapshot_bounds(snapshot: &ResourceSnapshotV1) -> Result<(), String> {
    crate::coordination::store::wire::resource_bounds(&snapshot.resource)
        .map_err(|invalid| invalid.describe())?;
    match &snapshot.holder {
        Some(holder) if holder.len() != 32 => {
            return Err("the snapshot's holder is not a 32-byte node id".to_string());
        }
        _ => {}
    }
    if snapshot.held != snapshot.holder.is_some() {
        return Err("the snapshot's held flag contradicts its holder".to_string());
    }
    if snapshot.value_redacted && snapshot.value.is_some() {
        return Err("a redacted value cannot carry bytes".to_string());
    }
    if let Some(value) = &snapshot.value {
        if value.len() > MAX_VALUE {
            return Err("the snapshot's value exceeds the §12.1 bound".to_string());
        }
    }
    // A held row's TTL was set by a bounds-checked grant; an unheld row
    // retains its last TTL or the never-written zero.
    if snapshot.held {
        crate::coordination::store::wire::ttl_bounds(snapshot.ttl_ms)
            .map_err(|invalid| invalid.describe())?;
    } else if snapshot.ttl_ms != 0
        && crate::coordination::store::wire::ttl_bounds(snapshot.ttl_ms).is_err()
    {
        return Err("the snapshot's TTL is outside the §12.1 bounds".to_string());
    }
    for (what, counter) in [
        ("epoch", snapshot.epoch),
        ("lease revision", snapshot.lease_revision),
        ("value revision", snapshot.value_revision),
        ("ACL revision", snapshot.acl_revision),
    ] {
        if counter > MAX_COUNTER {
            return Err(format!("the snapshot's {what} is past the counter ceiling"));
        }
    }
    Ok(())
}

// ── The retirement pair (§10) ───────────────────────────────────────────────

/// The frozen verdict on a `retire_client` reply to a **self-targeted**
/// request — the only kind this client sends, from the closing corridor and
/// from `retire_other_client` alike.
///
/// Exactly two answers exist: `(9 ClientRetired, 0)` and `(100 Rejected,
/// 18 RetainedSessionCapReached)`. The model's third pair, `(100, 12
/// PermissionChanged)`, answers a *cross-actor* retirement without
/// administrative authority; a self-targeted request is always authorized,
/// so receiving it — or any other combination, such as an outcome 9 dressed
/// with reason 18, or a success outcome smuggling the fatal reason — is not
/// this group's answer to this request and must be resent, not honored.
pub(crate) fn validate_retire_reply(
    group_id: &[u8],
    reply: &RetireClientReplyWireV1,
) -> Result<(), String> {
    if reply.protocol_version != STORE_PROTOCOL_VERSION {
        return Err("the reply carries another protocol version".to_string());
    }
    if reply.group_id != group_id {
        return Err("the reply names another coordination group".to_string());
    }
    match (reply.outcome_code, reply.reason_code) {
        (9, 0) => Ok(()),
        (100, code) if code == Reason::RetainedSessionCapReached.code() => Ok(()),
        (outcome, reason) => Err(format!(
            "({outcome}, {reason}) is not a §10 answer to a self-targeted retirement"
        )),
    }
}

// ── Read/status/stream validity, request-aware ──────────────────────────────

/// §5.2 code domains that live voter-side. The authoritative declarations
/// (`runtime::lifecycle`, `model::membership`) do not compile in the
/// `coordination-client` graph, so the domains are restated here and pinned
/// against the originals by `domain_tests` under the voter feature.
mod domains {
    /// §7.6 lifecycle codes: Bootstrapping 1 … Fatal 6.
    pub(super) const LIFECYCLE: std::ops::RangeInclusive<i32> = 1..=6;
    pub(super) const LIFECYCLE_FATAL: i32 = 6;
    /// §5.2 local roles: None 0, Learner 1, Voter 2.
    pub(super) const ROLE: std::ops::RangeInclusive<i32> = 0..=2;
    /// §5.2 fatal codes: 0 not fatal, then one per `FatalCause` variant.
    pub(super) const FATAL: std::ops::RangeInclusive<i32> = 0..=4;
    /// §7.1 deployment profiles: Prod 1, DevSingle 2, DevEven 3.
    pub(super) const PROFILE: [i32; 3] = [1, 2, 3];
    /// §7.3 intent phases: Pending 1 … Cancelled 7.
    pub(super) const PHASE: std::ops::RangeInclusive<i32> = 1..=7;
    /// The §7.3 phases that may carry a completion result code.
    pub(super) const PHASE_TERMINAL: [i32; 3] = [4, 5, 7];
}

fn check_sorted_unique_32(what: &str, ids: &[Vec<u8>]) -> Result<(), String> {
    if ids.iter().any(|id| id.len() != 32) {
        return Err(format!("{what} contains a malformed node id"));
    }
    if ids.windows(2).any(|w| w[0] >= w[1]) {
        return Err(format!("{what} is not sorted and unique"));
    }
    Ok(())
}

fn check_read_snapshot(group_id: &[u8], snapshot: &ResourceSnapshotV1) -> Result<(), String> {
    if snapshot.group_id != group_id {
        return Err("the snapshot names another coordination group".to_string());
    }
    check_snapshot_bounds(snapshot)
}

/// Request-aware validity for one typed reply, run after the outer
/// protocol/group envelope. **No default implementation**: every reply type
/// the client accepts must spell out its own verdict against the request
/// it answers, so a new method cannot silently ship unvalidated.
pub(crate) trait ValidReply {
    type Request;
    fn validate_reply(&self, group_id: &[u8], request: &Self::Request) -> Result<(), String>;
}

impl ValidReply for ReadReplyWireV1 {
    type Request = ReadRequestV1;
    fn validate_reply(&self, group_id: &[u8], request: &Self::Request) -> Result<(), String> {
        check_read_snapshot(group_id, &self.snapshot)?;
        if self.snapshot.resource != request.resource {
            return Err("the snapshot names another resource than the request".to_string());
        }
        // A linearizable read is always a first attempt by a caller that
        // proved `READ`: the server answers `Some(value)` — empty bytes for
        // a never-written register — and never redacts. `None` or a
        // redaction flag here is a replay artifact no read path produces.
        if self.snapshot.value_redacted {
            return Err("a linearizable read is never redacted".to_string());
        }
        if self.snapshot.value.is_none() {
            return Err("a linearizable read always carries the register's value".to_string());
        }
        Ok(())
    }
}

impl ValidReply for AclStatusReplyWireV1 {
    type Request = AclStatusRequestV1;
    fn validate_reply(&self, _group_id: &[u8], request: &Self::Request) -> Result<(), String> {
        if self.acl_snapshot.resource != request.resource {
            return Err("the ACL snapshot names another resource than the request".to_string());
        }
        let subjects: Vec<Vec<u8>> = self
            .acl_snapshot
            .grants
            .iter()
            .map(|grant| grant.subject_node_id.clone())
            .collect();
        check_sorted_unique_32("the ACL grant list", &subjects)?;
        for grant in &self.acl_snapshot.grants {
            if !rights_are_known(grant.rights) {
                return Err("a grant carries a rights bit v1 does not define".to_string());
            }
        }
        Ok(())
    }
}

impl ValidReply for AdminStatusReplyWireV1 {
    type Request = AdminStatusRequestV1;
    fn validate_reply(&self, _group_id: &[u8], _request: &Self::Request) -> Result<(), String> {
        let ids: Vec<Vec<u8>> = self
            .admin_snapshot
            .administrators
            .iter()
            .map(|node| node.node_id.clone())
            .collect();
        check_sorted_unique_32("the administrator list", &ids)?;
        // §8.1: the last administrator can never be removed, so a committed
        // group always has at least one.
        if ids.is_empty() {
            return Err("a group never has an empty administrator set".to_string());
        }
        Ok(())
    }
}

impl ValidReply for MembershipStatusReplyWireV1 {
    type Request = MembershipStatusRequestV1;
    fn validate_reply(&self, _group_id: &[u8], request: &Self::Request) -> Result<(), String> {
        for (what, id) in [
            (
                "the expected membership log id",
                &self.expected_membership_log_id,
            ),
            (
                "the observed membership log id",
                &self.observed_membership_log_id,
            ),
        ] {
            if let Some(id) = id {
                check_log_id(what, id)?;
            }
        }
        match &self.operation_id {
            None => {
                // No intent matched: every intent field is at its zero.
                if self.status_code != 0 {
                    return Err("a matchless status must carry status code 0".to_string());
                }
                if self.expected_membership_log_id.is_some() {
                    return Err("a matchless status carries no expected log id".to_string());
                }
                if !self.desired_voters.is_empty() || !self.retained_learners.is_empty() {
                    return Err("a matchless status carries no plan sets".to_string());
                }
                if self.error_code != 0 {
                    return Err("a matchless status must carry error code 0".to_string());
                }
            }
            Some(op) => {
                if op.len() != 16 {
                    return Err("the intent's operation id is not 16 bytes".to_string());
                }
                // §5.2: asked about one intent, answered about that intent
                // or none — never a different one.
                if let Some(asked) = &request.operation_id {
                    if op != asked {
                        return Err(
                            "the status answers a different operation than asked".to_string()
                        );
                    }
                }
                if !domains::PHASE.contains(&self.status_code) {
                    return Err(format!(
                        "status code {} is not a §7.3 intent phase",
                        self.status_code
                    ));
                }
                // A completion result is recorded when the intent becomes
                // terminal and never before.
                if self.error_code != 0 && !domains::PHASE_TERMINAL.contains(&self.status_code) {
                    return Err("a non-terminal intent carries no completion code".to_string());
                }
                let voters: Vec<Vec<u8>> = self
                    .desired_voters
                    .iter()
                    .map(|n| n.node_id.clone())
                    .collect();
                let learners: Vec<Vec<u8>> = self
                    .retained_learners
                    .iter()
                    .map(|n| n.node_id.clone())
                    .collect();
                check_sorted_unique_32("the desired voter list", &voters)?;
                check_sorted_unique_32("the retained learner list", &learners)?;
                if learners.iter().any(|l| voters.contains(l)) {
                    return Err("the plan's voter and learner sets overlap".to_string());
                }
            }
        }
        Ok(())
    }
}

impl ValidReply for ProbeClientReplyWireV1 {
    type Request = ProbeClientRequestV1;
    fn validate_reply(&self, _group_id: &[u8], request: &Self::Request) -> Result<(), String> {
        // The probe answers about exactly the session asked about.
        if self.target_node_id != request.target_node_id
            || self.target_client_id != request.target_client_id
        {
            return Err("the probe answers about another session than asked".to_string());
        }
        if self.exists {
            // A session row always records the 32-byte hash of its last
            // request, and only Consumed results are ever saved.
            if self.request_hash.len() != 32 {
                return Err("an existing session's request hash is 32 bytes".to_string());
            }
            if self.disposition_code != 0 && self.disposition_code != 1 {
                return Err(
                    "a probe reports a saved result as disposition 1, or 0 for none".to_string(),
                );
            }
        } else {
            // No session was ever created: §5.2 zeros every remaining field,
            // and residue here is an existence oracle the reply did not earn.
            if self.last_sequence != 0
                || !self.request_hash.is_empty()
                || self.disposition_code != 0
                || self.retired
            {
                return Err("an absent session carries no residue".to_string());
            }
        }
        Ok(())
    }
}

impl ValidReply for GroupStatusReplyWireV1 {
    type Request = GroupStatusRequestV1;
    fn validate_reply(&self, _group_id: &[u8], _request: &Self::Request) -> Result<(), String> {
        if self.local_node_id.len() != 32 {
            return Err("the status names a malformed local node id".to_string());
        }
        if let Some(leader) = &self.leader_node_id {
            if leader.len() != 32 {
                return Err("the status names a malformed leader node id".to_string());
            }
        }
        let voters: Vec<Vec<u8>> = self.voters.iter().map(|n| n.node_id.clone()).collect();
        let learners: Vec<Vec<u8>> = self.learners.iter().map(|n| n.node_id.clone()).collect();
        check_sorted_unique_32("the voter list", &voters)?;
        check_sorted_unique_32("the learner list", &learners)?;
        if learners.iter().any(|l| voters.contains(l)) {
            return Err("the voter and learner lists overlap".to_string());
        }
        for (what, id) in [
            ("the last log id", &self.last_log_id),
            ("the committed log id", &self.committed_log_id),
            ("the applied log id", &self.applied_log_id),
            ("the membership log id", &self.membership_log_id),
            ("the audit floor", &self.audit_floor),
        ] {
            if let Some(id) = id {
                check_log_id(what, id)?;
            }
        }
        if self.genesis_hash.len() != 32 {
            return Err("the status's genesis hash is not 32 bytes".to_string());
        }
        if self.authority_public_key.len() != 32 {
            return Err("the status's authority key is not 32 bytes".to_string());
        }
        if self.authority_domain.is_empty() {
            return Err("the status carries no authority domain".to_string());
        }
        if let Some(op) = &self.pending_operation_id {
            if op.len() != 16 {
                return Err("the pending operation id is not 16 bytes".to_string());
            }
        }
        // §5.2 code domains, and the one cross-field fact both codes are
        // sampled from the same lifecycle view: Fatal and a fatal cause
        // appear together or not at all.
        if !domains::LIFECYCLE.contains(&self.lifecycle_code) {
            return Err(format!(
                "lifecycle code {} is not a §7.6 code",
                self.lifecycle_code
            ));
        }
        if !domains::ROLE.contains(&self.local_role_code) {
            return Err(format!(
                "role code {} is not a §5.2 local role",
                self.local_role_code
            ));
        }
        if !domains::FATAL.contains(&self.fatal_code) {
            return Err(format!("fatal code {} is not a §5.2 code", self.fatal_code));
        }
        if (self.lifecycle_code == domains::LIFECYCLE_FATAL) != (self.fatal_code != 0) {
            return Err("the lifecycle and fatal codes contradict each other".to_string());
        }
        if !domains::PROFILE.contains(&self.deployment_profile_code) {
            return Err(format!(
                "profile code {} is not a §7.1 deployment profile",
                self.deployment_profile_code
            ));
        }
        if let Some(recovered) = &self.recovered_from {
            for (what, bytes) in [
                ("source group id", &recovered.source_group_id),
                ("source genesis hash", &recovered.source_genesis_hash),
                (
                    "source authority public key",
                    &recovered.source_authority_public_key,
                ),
                ("export hash", &recovered.export_hash),
            ] {
                if bytes.len() != 32 {
                    return Err(format!("the status's recovery {what} is not 32 bytes"));
                }
            }
            if recovered.source_authority_domain.is_empty() {
                return Err("the status's recovery provenance carries no domain".to_string());
            }
            if let Some(id) = &recovered.source_last_applied {
                check_log_id("the recovery source last-applied id", id)?;
            }
        }
        Ok(())
    }
}

/// One audit record's own validity; the stream-level invariants — ascending
/// order, the strictly-after cursor, the requested limit — live at the
/// consuming loop, which sees the records in sequence.
pub(crate) fn validate_audit_record(record: &AuditRecordWireV1) -> Result<(), String> {
    check_log_id("the audit record's log id", &record.log_id)?;
    if let Some(actor) = &record.actor_node_id {
        if actor.len() != 32 {
            return Err("the audit record names a malformed actor".to_string());
        }
    }
    // The exact §5.2 operation × result pairing, from the one shared table
    // (`contract::audit_results_for`) the durable-state validator also
    // reads. Not merely the code space: an acquire recording
    // `MembershipAccepted` is in the right space and is still a result no
    // apply path produces.
    let Some(allowed) = crate::coordination::contract::audit_results_for(record.operation_code)
    else {
        return Err(format!(
            "operation code {} is not a §5.2 audit operation",
            record.operation_code
        ));
    };
    if !allowed.contains(&record.result_code) {
        return Err(format!(
            "operation {} cannot record result {}",
            record.operation_code, record.result_code
        ));
    }
    if let Some(resource) = &record.resource {
        crate::coordination::store::wire::resource_bounds(resource)
            .map_err(|invalid| invalid.describe())?;
    }
    Ok(())
}

impl ValidReply for AuditRecordWireV1 {
    type Request = ();
    fn validate_reply(&self, _group_id: &[u8], _request: &Self::Request) -> Result<(), String> {
        validate_audit_record(self)
    }
}

impl ValidReply for RecoveryExportManifestWireV1 {
    type Request = RecoveryExportRequestV1;
    fn validate_reply(&self, _group_id: &[u8], _request: &Self::Request) -> Result<(), String> {
        if self.blake3_checksum.len() != 32 {
            return Err("the manifest's checksum is not 32 bytes".to_string());
        }
        if let Some(id) = &self.applied_log_id {
            check_log_id("the manifest's applied log id", id)?;
        }
        // §11.4: the canonical body is bounded by the state budgets it
        // exports. A manifest above the ceiling is a demand to buffer what
        // no group can hold, refused before a byte is written toward it.
        if self.total_len > crate::coordination::rowstream::MAX_SNAPSHOT_BODY_BYTES {
            return Err(format!(
                "the manifest claims {} bytes, over the §11.4 body ceiling",
                self.total_len
            ));
        }
        Ok(())
    }
}

impl ValidReply for RetireClientReplyWireV1 {
    type Request = RetireClientRequestV1;
    fn validate_reply(&self, group_id: &[u8], _request: &Self::Request) -> Result<(), String> {
        validate_retire_reply(group_id, self)
    }
}

/// The voter-side declarations the [`domains`] module restates, compared
/// where both graphs compile so the restatement cannot drift.
#[cfg(all(test, feature = "coordination"))]
mod domain_tests {
    use super::domains;

    #[test]
    fn the_restated_domains_match_their_authoritative_declarations() {
        use crate::coordination::model::membership as m;
        use crate::coordination::model::shape;
        use crate::coordination::runtime::lifecycle::LifecycleCode;

        let lifecycle: Vec<i32> = LifecycleCode::ALL.iter().map(|code| code.code()).collect();
        assert_eq!(lifecycle, (1..=6).collect::<Vec<i32>>());
        assert_eq!(domains::LIFECYCLE, 1..=6);
        assert_eq!(domains::LIFECYCLE_FATAL, LifecycleCode::Fatal.code());

        assert_eq!(
            [
                shape::PROFILE_PROD,
                shape::PROFILE_DEV_SINGLE,
                shape::PROFILE_DEV_EVEN
            ],
            domains::PROFILE
        );

        // Role and fatal domains, against their declarations.
        use crate::coordination::runtime::lifecycle::{FatalCause, LocalRole};
        for (role, code) in [
            (LocalRole::None, 0),
            (LocalRole::Learner, 1),
            (LocalRole::Voter, 2),
        ] {
            assert_eq!(role.code(), code);
            assert!(domains::ROLE.contains(&code));
        }
        assert_eq!(domains::ROLE, 0..=2);
        let causes = [
            FatalCause::Storage(String::new()),
            FatalCause::Raft(String::new()),
            FatalCause::ProfileViolation {
                profile: 1,
                voters: 0,
            },
            FatalCause::IdentityMismatch(String::new()),
        ];
        for cause in &causes {
            assert!(
                domains::FATAL.contains(&cause.code()) && cause.code() != 0,
                "{cause:?}"
            );
        }
        assert_eq!(
            *domains::FATAL.end(),
            causes
                .iter()
                .map(|cause| cause.code())
                .max()
                .expect("codes"),
            "the fatal domain ends at the last declared cause"
        );

        let phases = [
            m::PHASE_PENDING,
            m::PHASE_ADDING_LEARNERS,
            m::PHASE_CHANGING_VOTERS,
            m::PHASE_COMPLETED,
            m::PHASE_FAILED,
            m::PHASE_CANCEL_REQUESTED,
            m::PHASE_CANCELLED,
        ];
        assert_eq!(phases.to_vec(), (1..=7).collect::<Vec<i32>>());
        assert_eq!(domains::PHASE, 1..=7);
        assert_eq!(
            domains::PHASE_TERMINAL,
            [m::PHASE_COMPLETED, m::PHASE_FAILED, m::PHASE_CANCELLED]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::contract::{payload_fory, FenceV1, NodeIdV1};
    use crate::coordination::store::wire::{
        AclGrantWireV1, AclSnapshotWireV1, AdminSnapshotWireV1, AuditStreamRequestV1,
    };

    const GROUP: [u8; 32] = [0xAA; 32];
    const ACTOR: [u8; 32] = [0x0B; 32];
    const OTHER: [u8; 32] = [0x0D; 32];
    const CLIENT: [u8; 16] = [0x0C; 16];
    const HASH: [u8; 32] = [0x21; 32];
    const OP: [u8; 16] = [0x0E; 16];

    // ── Oracles quoted from the specification, not from the predicates ─────
    //
    // Deriving these lists from `Reason`'s methods would prove only that the
    // validator agrees with itself; the lists are §8.2.1's refusing rows ∪
    // §10 stage 3, and the apply-saveable complement, transcribed.

    /// Reasons a `NotConsumed` rejection may carry.
    const NOT_CONSUMED: [i32; 13] = [8, 9, 10, 11, 15, 18, 19, 21, 22, 23, 24, 25, 26];
    /// Nonzero reasons a `Consumed` rejection may carry.
    const SAVED: [i32; 19] = [
        1, 2, 3, 4, 5, 6, 7, 12, 13, 14, 15, 16, 17, 19, 22, 23, 24, 25, 26,
    ];

    fn resource() -> ResourceKeyV1 {
        ResourceKeyV1 {
            namespace: "matrix.test".into(),
            key: b"register".to_vec(),
        }
    }

    fn encode<T>(value: &T) -> Vec<u8>
    where
        T: crate::rpc::contract::WireField + fory_core::Serializer<Target = T> + 'static,
    {
        payload_fory::<T>().serialize(value).expect("encodes")
    }

    fn prefix() -> (i32, Vec<u8>, Vec<u8>, u64) {
        (STORE_PROTOCOL_VERSION, GROUP.to_vec(), CLIENT.to_vec(), 1)
    }

    fn fence_v1() -> FenceV1 {
        FenceV1 {
            group_id: GROUP.to_vec(),
            resource: resource(),
            epoch: 1,
            holder: ACTOR.to_vec(),
        }
    }

    /// One well-formed request per sequenced method — the pending frames the
    /// matrix decodes and correlates against.
    fn sequenced_request(method: &str) -> Vec<u8> {
        let (protocol_version, group_id, client_id, sequence) = prefix();
        match method {
            "acquire" => encode(&AcquireRequestV1 {
                protocol_version,
                group_id,
                client_id,
                sequence,
                resource: resource(),
                ttl_ms: 30_000,
            }),
            "renew" => encode(&RenewRequestV1 {
                protocol_version,
                group_id,
                client_id,
                sequence,
                fence: fence_v1(),
            }),
            "compare_and_set" => encode(&CompareAndSetRequestV1 {
                protocol_version,
                group_id,
                client_id,
                sequence,
                fence: fence_v1(),
                expected_value_revision: 0,
                new_value: b"v".to_vec(),
            }),
            "release" => encode(&ReleaseRequestV1 {
                protocol_version,
                group_id,
                client_id,
                sequence,
                fence: fence_v1(),
            }),
            "release_with_value" => encode(&ReleaseWithValueRequestV1 {
                protocol_version,
                group_id,
                client_id,
                sequence,
                fence: fence_v1(),
                expected_value_revision: 0,
                final_value: Vec::new(),
            }),
            "revoke" => encode(&RevokeRequestV1 {
                protocol_version,
                group_id,
                client_id,
                sequence,
                resource: resource(),
                expected_epoch: Some(1),
            }),
            "set_grant" => encode(&SetGrantRequestV1 {
                protocol_version,
                group_id,
                client_id,
                sequence,
                resource: resource(),
                target_node_id: OTHER.to_vec(),
                rights: 0x01,
                expected_acl_revision: 3,
            }),
            "set_admin" => encode(&SetAdminRequestV1 {
                protocol_version,
                group_id,
                client_id,
                sequence,
                target_node_id: OTHER.to_vec(),
                enabled: true,
                expected_admin_revision: 1,
            }),
            "change_membership" => encode(&ChangeMembershipRequestV1 {
                protocol_version,
                group_id,
                client_id,
                sequence,
                operation_id: OP.to_vec(),
                expected_membership_log_id: None,
                desired_voters: Vec::new(),
                retained_learners: Vec::new(),
                ..Default::default()
            }),
            "cancel_membership" => encode(&CancelMembershipRequestV1 {
                protocol_version,
                group_id,
                client_id,
                sequence,
                operation_id: OP.to_vec(),
                expected_phase_code: 1,
            }),
            other => panic!("{other} is not a sequenced method"),
        }
    }

    const SEQUENCED: [&str; 10] = [
        "acquire",
        "renew",
        "compare_and_set",
        "release",
        "release_with_value",
        "revoke",
        "set_grant",
        "set_admin",
        "change_membership",
        "cancel_membership",
    ];

    fn cx<'a>(method: &'a str, request: &'a [u8]) -> MutationContext<'a> {
        MutationContext {
            group_id: &GROUP,
            local_actor: &ACTOR,
            method,
            request,
            client_id: &CLIENT,
            sequence: 1,
            request_hash: &HASH,
        }
    }

    fn held_snapshot() -> ResourceSnapshotV1 {
        ResourceSnapshotV1 {
            group_id: GROUP.to_vec(),
            resource: resource(),
            epoch: 1,
            holder: Some(ACTOR.to_vec()),
            held: true,
            ttl_ms: 30_000,
            lease_revision: 1,
            value_revision: 0,
            acl_revision: 3,
            value: Some(Vec::new()),
            value_redacted: false,
        }
    }

    fn free_snapshot() -> ResourceSnapshotV1 {
        ResourceSnapshotV1 {
            holder: None,
            held: false,
            ..held_snapshot()
        }
    }

    fn owned_reply(disposition: i32, outcome: i32, reason: i32) -> MutationReplyWireV1 {
        MutationReplyWireV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            client_id: CLIENT.to_vec(),
            sequence: 1,
            request_hash: HASH.to_vec(),
            disposition_code: disposition,
            outcome_code: outcome,
            reason_code: reason,
            ..Default::default()
        }
    }

    /// A reply satisfying the **method's own** §9.1 success postconditions
    /// when the triple is that method's success, and the rejection shape
    /// otherwise — so the matrix verdict is about the pair and correlation
    /// rules, never about a missing snapshot or a stray auxiliary field.
    fn well_shaped(
        method: &str,
        disposition: i32,
        outcome: i32,
        reason: i32,
    ) -> MutationReplyWireV1 {
        let mut reply = owned_reply(disposition, outcome, reason);
        let success = spec(method)
            .and_then(|m| m.mutation)
            .expect("a sequenced method")
            .success_outcome_code();
        if outcome == success {
            match method {
                "acquire" => {
                    reply.snapshot = Some(held_snapshot());
                    reply.acl_revision = 3;
                }
                "renew" => {
                    reply.snapshot = Some(held_snapshot());
                    reply.acl_revision = 3;
                }
                "compare_and_set" => {
                    reply.snapshot = Some(ResourceSnapshotV1 {
                        value_revision: 1,
                        // §4.3: a readable success snapshot echoes exactly
                        // the bytes the request wrote.
                        value: Some(b"v".to_vec()),
                        ..held_snapshot()
                    });
                    reply.acl_revision = 3;
                }
                "release" => {
                    reply.snapshot = Some(ResourceSnapshotV1 {
                        epoch: 2,
                        ..free_snapshot()
                    });
                    reply.acl_revision = 3;
                }
                "release_with_value" => {
                    reply.snapshot = Some(ResourceSnapshotV1 {
                        epoch: 2,
                        value_revision: 1,
                        ..free_snapshot()
                    });
                    reply.acl_revision = 3;
                }
                "revoke" => {
                    reply.snapshot = Some(ResourceSnapshotV1 {
                        epoch: 2,
                        ..free_snapshot()
                    });
                    reply.acl_revision = 3;
                }
                "set_grant" => {
                    reply.snapshot = Some(free_snapshot());
                    reply.acl_revision = 3;
                }
                "set_admin" => reply.admin_revision = 2,
                "change_membership" | "cancel_membership" => {
                    reply.operation_id = Some(OP.to_vec());
                }
                other => panic!("{other} is not a sequenced method"),
            }
        }
        reply
    }

    /// **The exhaustive §5.2 pair matrix.** Every disposition × outcome ×
    /// reason (0–26, the reserved 20, and out-of-range codes) against an
    /// acquire entry, each row's verdict from the quoted spec lists alone.
    #[test]
    fn the_mutation_pair_matrix_is_exhaustive() {
        for method in SEQUENCED {
            let request = sequenced_request(method);
            let success = spec(method)
                .and_then(|m| m.mutation)
                .expect("a sequenced method")
                .success_outcome_code();
            for disposition in [0, 1, 2, 3] {
                for outcome in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 99, 100] {
                    for reason in (0..=27).chain([-1, 20]) {
                        let legal = match (disposition, outcome, reason) {
                            // The method's one success, Consumed, no reason.
                            (1, o, 0) if o == success => true,
                            // §10.1 shapeless, saved conflicts and refusals.
                            (1, 100, 0) => true,
                            (1, 100, r) => SAVED.contains(&r),
                            // Pre-write answers.
                            (2, 100, r) => NOT_CONSUMED.contains(&r),
                            _ => false,
                        };
                        let reply = well_shaped(method, disposition, outcome, reason);
                        let verdict = validate_mutation_reply(&cx(method, &request), &reply);
                        assert_eq!(
                            verdict.is_ok(),
                            legal,
                            "({disposition}, {outcome}, {reason}) on {method}: {verdict:?}"
                        );
                    }
                }
            }
        }
    }

    /// The reviewer's counterexample, end to end: a Consumed rejection with
    /// `21 ActiveClientCap` claims a session no path creates — accepting it
    /// would advance the journal past a sequence the group still expects.
    #[test]
    fn a_consumed_active_client_cap_is_refused() {
        let request = sequenced_request("acquire");
        let reply = well_shaped("acquire", 1, 100, 21);
        let why = validate_mutation_reply(&cx("acquire", &request), &reply)
            .expect_err("Consumed + ActiveClientCap");
        assert!(why.contains("never saved"), "{why}");
        // And the wedge inverse: a consumed conflict answered pre-write.
        let reply = well_shaped("acquire", 2, 100, 6);
        let why = validate_mutation_reply(&cx("acquire", &request), &reply)
            .expect_err("NotConsumed + AclRevisionMismatch");
        assert!(why.contains("NotConsumed"), "{why}");
    }

    /// Auxiliary fields are method-correlated: each forbidden decoration is
    /// refused, and the legal decorations pass.
    #[test]
    fn auxiliary_fields_follow_their_reply_family() {
        let acquire = sequenced_request("acquire");
        let change = sequenced_request("change_membership");
        let admin = sequenced_request("set_admin");

        // A grant dressed with a membership operation id.
        let mut reply = well_shaped("acquire", 1, 1, 0);
        reply.operation_id = Some(OP.to_vec());
        assert!(validate_mutation_reply(&cx("acquire", &acquire), &reply).is_err());

        // A grant dressed with a membership log id.
        let mut reply = well_shaped("acquire", 1, 1, 0);
        reply.membership_log_id = Some(LogIdV1 {
            term: 1,
            leader_node_id: vec![0x11; 32],
            index: 4,
        });
        assert!(validate_mutation_reply(&cx("acquire", &acquire), &reply).is_err());

        // A grant dressed with an admin revision.
        let mut reply = well_shaped("acquire", 1, 1, 0);
        reply.admin_revision = 7;
        assert!(validate_mutation_reply(&cx("acquire", &acquire), &reply).is_err());

        // The reply-level ACL revision contradicting the snapshot's.
        let mut reply = well_shaped("acquire", 1, 1, 0);
        reply.acl_revision = 9;
        assert!(validate_mutation_reply(&cx("acquire", &acquire), &reply).is_err());

        // A membership success that fails to echo, or echoes another id.
        let mut reply = well_shaped("change_membership", 1, 8, 0);
        reply.operation_id = None;
        assert!(validate_mutation_reply(&cx("change_membership", &change), &reply).is_err());
        let mut reply = well_shaped("change_membership", 1, 8, 0);
        reply.operation_id = Some(vec![0x77; 16]);
        assert!(validate_mutation_reply(&cx("change_membership", &change), &reply).is_err());
        // The echo itself passes, with its optional membership log id.
        let mut reply = well_shaped("change_membership", 1, 8, 0);
        reply.membership_log_id = Some(LogIdV1 {
            term: 1,
            leader_node_id: vec![0x11; 32],
            index: 4,
        });
        assert!(validate_mutation_reply(&cx("change_membership", &change), &reply).is_ok());

        // AdminUpdated must report the committed revision, and does.
        let mut reply = well_shaped("set_admin", 1, 7, 0);
        reply.admin_revision = 0;
        assert!(validate_mutation_reply(&cx("set_admin", &admin), &reply).is_err());
        let reply = well_shaped("set_admin", 1, 7, 0);
        assert!(validate_mutation_reply(&cx("set_admin", &admin), &reply).is_ok());
    }

    /// **The frozen retirement pair table.** Every combination the wire can
    /// spell, and exactly two survive for a self-targeted request.
    #[test]
    fn the_retirement_pair_table_is_exhaustive() {
        for outcome in [0, 1, 7, 9, 10, 100, 101] {
            for reason in [0, 1, 11, 12, 18, 20, 24, 26, 99] {
                let reply = RetireClientReplyWireV1 {
                    protocol_version: STORE_PROTOCOL_VERSION,
                    group_id: GROUP.to_vec(),
                    outcome_code: outcome,
                    reason_code: reason,
                };
                let legal = matches!((outcome, reason), (9, 0) | (100, 18));
                assert_eq!(
                    validate_retire_reply(&GROUP, &reply).is_ok(),
                    legal,
                    "({outcome}, {reason})"
                );
            }
        }
        // The model's cross-actor pair confirms nothing self-targeted, and a
        // foreign world confirms nothing at all.
        let reply = RetireClientReplyWireV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: vec![0xEE; 32],
            outcome_code: 9,
            reason_code: 0,
        };
        assert!(validate_retire_reply(&GROUP, &reply).is_err());
    }

    // ── The read/status/stream tables ───────────────────────────────────────

    /// Each row: a valid base, then every forbidden class as a mutation of
    /// it — so a refusal is attributable to exactly the mutated fact.
    fn refused<T: ValidReply>(label: &str, request: &T::Request, reply: T) {
        assert!(
            reply.validate_reply(&GROUP, request).is_err(),
            "{label} must be refused"
        );
    }

    fn accepted<T: ValidReply>(label: &str, request: &T::Request, reply: T) {
        let verdict = reply.validate_reply(&GROUP, request);
        assert!(verdict.is_ok(), "{label}: {verdict:?}");
    }

    #[test]
    fn a_read_reply_is_validated_against_its_request() {
        let request = ReadRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            resource: resource(),
        };
        let valid = || ReadReplyWireV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            snapshot: ResourceSnapshotV1 {
                value: Some(b"payload".to_vec()),
                ..free_snapshot()
            },
        };
        accepted("a fresh read", &request, valid());

        let mut wrong = valid();
        wrong.snapshot.resource.key = b"other".to_vec();
        refused("a read about another resource", &request, wrong);

        let mut redacted = valid();
        redacted.snapshot.value = None;
        redacted.snapshot.value_redacted = true;
        refused("a redacted read", &request, redacted);

        let mut valueless = valid();
        valueless.snapshot.value = None;
        refused("a read withholding the value", &request, valueless);

        let mut contradictory = valid();
        contradictory.snapshot.held = true;
        refused("held with no holder", &request, contradictory);
    }

    #[test]
    fn an_acl_reply_is_validated_against_its_request() {
        let request = AclStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            resource: resource(),
        };
        let valid = || AclStatusReplyWireV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            acl_snapshot: AclSnapshotWireV1 {
                resource: resource(),
                acl_revision: 3,
                grants: vec![
                    AclGrantWireV1 {
                        subject_node_id: vec![0x01; 32],
                        rights: 0x01,
                    },
                    AclGrantWireV1 {
                        subject_node_id: vec![0x02; 32],
                        rights: 0x03,
                    },
                ],
            },
        };
        accepted("an ACL snapshot", &request, valid());

        let mut wrong = valid();
        wrong.acl_snapshot.resource.key = b"other".to_vec();
        refused("an ACL about another resource", &request, wrong);

        let mut unsorted = valid();
        unsorted.acl_snapshot.grants.swap(0, 1);
        refused("unsorted grants", &request, unsorted);

        let mut unknown = valid();
        unknown.acl_snapshot.grants[0].rights = u64::MAX;
        refused("an unknown rights bit", &request, unknown);
    }

    #[test]
    fn an_admin_reply_requires_a_sorted_nonempty_set() {
        let request = AdminStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
        };
        let valid = || AdminStatusReplyWireV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            admin_snapshot: AdminSnapshotWireV1 {
                admin_revision: 1,
                administrators: vec![NodeIdV1 {
                    node_id: vec![0x01; 32],
                }],
            },
        };
        accepted("an administrator set", &request, valid());

        let mut empty = valid();
        empty.admin_snapshot.administrators.clear();
        refused("an empty administrator set", &request, empty);

        let mut malformed = valid();
        malformed.admin_snapshot.administrators[0].node_id = vec![0x01; 31];
        refused("a malformed administrator id", &request, malformed);
    }

    #[test]
    fn a_membership_status_is_validated_against_its_request() {
        let asked = MembershipStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            operation_id: Some(OP.to_vec()),
        };
        let matched = || MembershipStatusReplyWireV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            operation_id: Some(OP.to_vec()),
            status_code: 2,
            expected_membership_log_id: None,
            observed_membership_log_id: None,
            desired_voters: vec![
                NodeIdV1 {
                    node_id: vec![0x01; 32],
                },
                NodeIdV1 {
                    node_id: vec![0x02; 32],
                },
            ],
            retained_learners: vec![NodeIdV1 {
                node_id: vec![0x03; 32],
            }],
            error_code: 0,
        };
        accepted("a matched intent", &asked, matched());

        let mut other = matched();
        other.operation_id = Some(vec![0x77; 16]);
        refused("an answer about another operation", &asked, other);

        let mut unknown_phase = matched();
        unknown_phase.status_code = 8;
        refused("an unknown phase", &asked, unknown_phase);

        let mut early_error = matched();
        early_error.error_code = 4;
        refused(
            "a completion code on a non-terminal phase",
            &asked,
            early_error,
        );

        let mut terminal = matched();
        terminal.status_code = 5;
        terminal.error_code = 4;
        accepted("a terminal completion code", &asked, terminal);

        let mut overlap = matched();
        overlap.retained_learners = overlap.desired_voters.clone();
        refused("overlapping plan sets", &asked, overlap);

        let mut unsorted = matched();
        unsorted.desired_voters.swap(0, 1);
        refused("unsorted desired voters", &asked, unsorted);

        // A matchless answer is all zeros, and residue is refused per field.
        let matchless = || MembershipStatusReplyWireV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            operation_id: None,
            status_code: 0,
            expected_membership_log_id: None,
            observed_membership_log_id: None,
            desired_voters: Vec::new(),
            retained_learners: Vec::new(),
            error_code: 0,
        };
        accepted("a matchless status", &asked, matchless());
        let mut residue = matchless();
        residue.status_code = 2;
        refused("a matchless status with a phase", &asked, residue);
        let mut residue = matchless();
        residue.desired_voters = vec![NodeIdV1 {
            node_id: vec![0x01; 32],
        }];
        refused("a matchless status with a plan", &asked, residue);
        let mut residue = matchless();
        residue.error_code = 3;
        refused("a matchless status with an error", &asked, residue);
    }

    #[test]
    fn a_probe_reply_is_validated_against_its_request() {
        let request = ProbeClientRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            target_node_id: OTHER.to_vec(),
            target_client_id: CLIENT.to_vec(),
        };
        let existing = || ProbeClientReplyWireV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            target_node_id: OTHER.to_vec(),
            target_client_id: CLIENT.to_vec(),
            exists: true,
            last_sequence: 4,
            request_hash: vec![0x21; 32],
            disposition_code: 1,
            retired: false,
        };
        accepted("an existing session", &request, existing());

        let mut other = existing();
        other.target_node_id = vec![0x99; 32];
        refused("a probe about another session", &request, other);

        let mut hashless = existing();
        hashless.request_hash = Vec::new();
        refused("an existing session with no hash", &request, hashless);

        let mut not_consumed = existing();
        not_consumed.disposition_code = 2;
        refused(
            "a probe claiming a saved NotConsumed",
            &request,
            not_consumed,
        );

        let absent = || ProbeClientReplyWireV1 {
            exists: false,
            last_sequence: 0,
            request_hash: Vec::new(),
            disposition_code: 0,
            retired: false,
            ..existing()
        };
        accepted("an absent session", &request, absent());
        for (label, residue) in [
            ("sequence residue", {
                let mut r = absent();
                r.last_sequence = 1;
                r
            }),
            ("hash residue", {
                let mut r = absent();
                r.request_hash = vec![0x21; 32];
                r
            }),
            ("disposition residue", {
                let mut r = absent();
                r.disposition_code = 1;
                r
            }),
            ("retired residue", {
                let mut r = absent();
                r.retired = true;
                r
            }),
        ] {
            refused(label, &request, residue);
        }
    }

    #[test]
    fn a_group_status_is_held_to_the_spec_domains() {
        let request = GroupStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
        };
        let valid = || GroupStatusReplyWireV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            lifecycle_code: 3,
            local_node_id: vec![0x01; 32],
            local_role_code: 2,
            leader_node_id: Some(vec![0x01; 32]),
            term: 1,
            last_log_id: None,
            committed_log_id: None,
            applied_log_id: None,
            deployment_profile_code: 1,
            membership_log_id: None,
            membership_joint: false,
            recovered_from: None,
            voters: vec![
                NodeIdV1 {
                    node_id: vec![0x01; 32],
                },
                NodeIdV1 {
                    node_id: vec![0x02; 32],
                },
            ],
            learners: Vec::new(),
            pending_operation_id: None,
            quorum_available: Some(true),
            fatal_code: 0,
            audit_floor: None,
            genesis_hash: vec![0x11; 32],
            authority_public_key: vec![0x42; 32],
            authority_domain: "matrix.test".into(),
        };
        accepted("a ready voter", &request, valid());

        for (label, mutate) in [
            (
                "lifecycle 0",
                Box::new(|r: &mut GroupStatusReplyWireV1| r.lifecycle_code = 0)
                    as Box<dyn Fn(&mut GroupStatusReplyWireV1)>,
            ),
            ("lifecycle 7", Box::new(|r| r.lifecycle_code = 7)),
            ("role 3", Box::new(|r| r.local_role_code = 3)),
            ("fatal 5", Box::new(|r| r.fatal_code = 5)),
            (
                "a fatal cause without Fatal",
                Box::new(|r| r.fatal_code = 2),
            ),
            ("Fatal without a cause", Box::new(|r| r.lifecycle_code = 6)),
            ("profile 0", Box::new(|r| r.deployment_profile_code = 0)),
            ("profile 4", Box::new(|r| r.deployment_profile_code = 4)),
            (
                "a 31-byte authority key",
                Box::new(|r| r.authority_public_key = vec![0x42; 31]),
            ),
            (
                "an empty authority domain",
                Box::new(|r| r.authority_domain = String::new()),
            ),
        ] {
            let mut reply = valid();
            mutate(&mut reply);
            refused(label, &request, reply);
        }

        // The coherent Fatal passes.
        let mut fatal = valid();
        fatal.lifecycle_code = 6;
        fatal.fatal_code = 2;
        accepted("a coherent Fatal", &request, fatal);
    }

    #[test]
    fn an_audit_record_is_held_to_the_spec_domains() {
        let _ = AuditStreamRequestV1::default();
        let valid = || AuditRecordWireV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            log_id: LogIdV1 {
                term: 1,
                leader_node_id: vec![0x11; 32],
                index: 9,
            },
            actor_node_id: Some(ACTOR.to_vec()),
            operation_code: 1,
            resource: Some(resource()),
            result_code: 1,
            epoch: 1,
            value_revision: 0,
        };
        accepted("a client audit row", &(), valid());

        let mut system = valid();
        system.operation_code = 100;
        system.result_code = 1;
        accepted("a system audit row", &(), system);

        for (label, mutate) in [
            (
                "operation 12",
                Box::new(|r: &mut AuditRecordWireV1| r.operation_code = 12)
                    as Box<dyn Fn(&mut AuditRecordWireV1)>,
            ),
            ("operation 99", Box::new(|r| r.operation_code = 99)),
            ("operation 103", Box::new(|r| r.operation_code = 103)),
            (
                "a client row with a non-outcome result",
                Box::new(|r| r.result_code = 11),
            ),
            (
                "a system row with an outcome result",
                Box::new(|r| {
                    r.operation_code = 101;
                    r.result_code = 100;
                }),
            ),
            (
                "an empty namespace",
                Box::new(|r| {
                    if let Some(resource) = r.resource.as_mut() {
                        resource.namespace = String::new();
                    }
                }),
            ),
        ] {
            let mut record = valid();
            mutate(&mut record);
            refused(label, &(), record);
        }
    }

    #[test]
    fn a_manifest_is_bounded_by_the_body_ceiling() {
        let request = RecoveryExportRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
        };
        let valid = || RecoveryExportManifestWireV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            total_len: crate::coordination::rowstream::MAX_SNAPSHOT_BODY_BYTES,
            blake3_checksum: vec![0x33; 32],
            applied_log_id: None,
        };
        accepted("a manifest at the ceiling", &request, valid());

        let mut over = valid();
        over.total_len += 1;
        refused("a manifest over the ceiling", &request, over);

        let mut checksum = valid();
        checksum.blake3_checksum = vec![0x33; 31];
        refused("a malformed checksum", &request, checksum);
    }

    /// **The §9.1 success postconditions, per method** — each deformation is
    /// a success apply's transition arithmetic can never produce, refused
    /// against the decoded request even though the reply is otherwise
    /// perfectly shaped.
    #[test]
    fn impossible_successes_are_refused_per_method() {
        type Deform = Box<dyn Fn(&mut MutationReplyWireV1)>;
        let deformations: Vec<(&str, Deform)> = vec![
            (
                "acquire",
                Box::new(|r| r.snapshot.as_mut().expect("snapshot").ttl_ms = 60_000),
            ),
            (
                "acquire",
                Box::new(|r| {
                    let s = r.snapshot.as_mut().expect("snapshot");
                    s.epoch = 0;
                }),
            ),
            (
                "renew",
                Box::new(|r| r.snapshot.as_mut().expect("snapshot").epoch = 2),
            ),
            (
                "compare_and_set",
                Box::new(|r| r.snapshot.as_mut().expect("snapshot").value_revision = 5),
            ),
            (
                "compare_and_set",
                Box::new(|r| r.snapshot.as_mut().expect("snapshot").epoch = 2),
            ),
            (
                "release",
                Box::new(|r| r.snapshot.as_mut().expect("snapshot").epoch = 1),
            ),
            (
                "release_with_value",
                Box::new(|r| r.snapshot.as_mut().expect("snapshot").value_revision = 9),
            ),
            (
                "revoke",
                Box::new(|r| r.snapshot.as_mut().expect("snapshot").epoch = 7),
            ),
            (
                "set_grant",
                Box::new(|r| {
                    // Coherent with its snapshot, so only the request's
                    // revision fence can refuse it.
                    r.acl_revision = 9;
                    r.snapshot.as_mut().expect("snapshot").acl_revision = 9;
                }),
            ),
            ("set_admin", Box::new(|r| r.admin_revision = 9)),
        ];
        for (method, deform) in &deformations {
            let request = sequenced_request(method);
            let success = spec(method)
                .and_then(|m| m.mutation)
                .expect("a sequenced method")
                .success_outcome_code();
            let mut reply = well_shaped(method, 1, success, 0);
            assert!(
                validate_mutation_reply(&cx(method, &request), &reply).is_ok(),
                "{method}: the well-shaped success is the baseline"
            );
            deform(&mut reply);
            assert!(
                validate_mutation_reply(&cx(method, &request), &reply).is_err(),
                "{method}: an impossible success must be refused"
            );
        }
    }

    /// The §12.1 leaf bounds on every snapshot: oversized values, illegal
    /// TTLs on held rows, exhausted counters, out-of-bounds identities.
    #[test]
    fn snapshot_leaf_bounds_are_enforced() {
        let request = sequenced_request("acquire");
        let base = || well_shaped("acquire", 1, 1, 0);

        let mut oversized = base();
        oversized.snapshot.as_mut().expect("snapshot").value = Some(vec![0u8; MAX_VALUE + 1]);
        assert!(validate_mutation_reply(&cx("acquire", &request), &oversized).is_err());

        let mut exhausted = base();
        exhausted
            .snapshot
            .as_mut()
            .expect("snapshot")
            .lease_revision = MAX_COUNTER + 1;
        assert!(validate_mutation_reply(&cx("acquire", &request), &exhausted).is_err());

        let mut bad_key = base();
        bad_key
            .snapshot
            .as_mut()
            .expect("snapshot")
            .resource
            .namespace = String::new();
        assert!(validate_mutation_reply(&cx("acquire", &request), &bad_key).is_err());
    }

    /// **The outbound half**: requests the client must refuse to send at
    /// all — an out-of-bounds TTL apply can never grant, a fence naming
    /// another group (§4.1) or another actor's hold.
    #[test]
    fn illegal_requests_are_refused_before_sending() {
        let ok = decode_facts("acquire", &sequenced_request("acquire")).expect("decodes");
        assert!(validate_request(&GROUP, &ACTOR, &ok).is_ok());

        let short_ttl = RequestFacts::Acquire {
            resource: resource(),
            ttl_ms: 1,
        };
        assert!(validate_request(&GROUP, &ACTOR, &short_ttl).is_err());

        let foreign_fence = RequestFacts::Renew {
            fence: FenceV1 {
                group_id: vec![0xEE; 32],
                ..fence_v1()
            },
        };
        assert!(validate_request(&GROUP, &ACTOR, &foreign_fence).is_err());

        let anothers_fence = RequestFacts::Renew {
            fence: FenceV1 {
                holder: OTHER.to_vec(),
                ..fence_v1()
            },
        };
        assert!(validate_request(&GROUP, &ACTOR, &anothers_fence).is_err());

        let oversized = RequestFacts::CompareAndSet {
            fence: fence_v1(),
            expected_value_revision: 0,
            new_value: vec![0u8; MAX_VALUE + 1],
        };
        assert!(validate_request(&GROUP, &ACTOR, &oversized).is_err());

        let unknown_rights = RequestFacts::SetGrant {
            resource: resource(),
            target_node_id: OTHER.to_vec(),
            rights: u64::MAX,
            expected_acl_revision: 0,
        };
        assert!(validate_request(&GROUP, &ACTOR, &unknown_rights).is_err());
    }

    /// **The wedge-prevention rules** (§10): every deterministic shape rule
    /// the group could only answer with a stage-4 status denial is refused
    /// here, before a sequence could be burned on it.
    #[test]
    fn shapeless_requests_are_refused_before_journaling() {
        // §4.2 grammar, not lengths: uppercase is in bounds and invalid.
        let bad_namespace = RequestFacts::Acquire {
            resource: ResourceKeyV1 {
                namespace: "NOT a namespace".into(),
                key: b"k".to_vec(),
            },
            ttl_ms: 30_000,
        };
        assert!(validate_request(&GROUP, &ACTOR, &bad_namespace).is_err());

        // Counter ceilings on everything a successor is computed from.
        let past_ceiling = RequestFacts::Renew {
            fence: FenceV1 {
                epoch: MAX_COUNTER + 1,
                ..fence_v1()
            },
        };
        assert!(validate_request(&GROUP, &ACTOR, &past_ceiling).is_err());
        let past_ceiling = RequestFacts::SetAdmin {
            target_node_id: OTHER.to_vec(),
            expected_admin_revision: u64::MAX,
        };
        assert!(validate_request(&GROUP, &ACTOR, &past_ceiling).is_err());

        // The §9 membership-plan shape, deterministic subset.
        let plan = |voters: Vec<Vec<u8>>, learners: Vec<Vec<u8>>| RequestFacts::ChangeMembership {
            operation_id: OP.to_vec(),
            expected_membership_log_id: None,
            desired_voters: voters,
            retained_learners: learners,
            member_seeds: Vec::new(),
        };
        let node = |b: u8| vec![b; 32];
        // A count no §7.1 profile permits.
        assert!(validate_request(
            &GROUP,
            &ACTOR,
            &plan(vec![node(1), node(2), node(3), node(4)], Vec::new())
        )
        .is_err());
        // Unsorted voters.
        assert!(
            validate_request(&GROUP, &ACTOR, &plan(vec![node(2), node(1)], Vec::new())).is_err()
        );
        // A node in both roles.
        assert!(validate_request(
            &GROUP,
            &ACTOR,
            &plan(vec![node(1), node(2), node(3)], vec![node(2)])
        )
        .is_err());
        // A seed for a node the plan does not contain, and a non-ticket.
        let with_seed = |seed_node: Vec<u8>, ticket: &str| RequestFacts::ChangeMembership {
            operation_id: OP.to_vec(),
            expected_membership_log_id: None,
            desired_voters: vec![node(1), node(2), node(3)],
            retained_learners: Vec::new(),
            member_seeds: vec![crate::coordination::contract::MemberSeedV1 {
                node_id: seed_node,
                ticket: ticket.into(),
                signed_join_permit: None,
            }],
        };
        assert!(validate_request(&GROUP, &ACTOR, &with_seed(node(9), "x")).is_err());
        assert!(validate_request(&GROUP, &ACTOR, &with_seed(node(1), "not a ticket")).is_err());
        // The sound plan passes: three sorted voters, a canonical seed.
        let candidate: [u8; 32] = node(1).try_into().expect("32 bytes");
        let ticket = crate::coordination::model::authority::canonical_candidate_ticket(&candidate)
            .expect("a canonical ticket");
        assert!(
            validate_request(&GROUP, &ACTOR, &with_seed(node(1), &ticket)).is_ok(),
            "the deterministic subset admits a sound plan"
        );

        // The §7.3 phase domain on a cancel.
        let bad_phase = RequestFacts::CancelMembership {
            operation_id: OP.to_vec(),
            expected_phase_code: 99,
        };
        assert!(validate_request(&GROUP, &ACTOR, &bad_phase).is_err());
    }

    /// A readable success snapshot echoes **exactly** the written bytes:
    /// different bytes of the same length are not this command's result.
    #[test]
    fn a_success_snapshot_must_echo_the_written_bytes() {
        let request = sequenced_request("compare_and_set");
        let mut reply = well_shaped("compare_and_set", 1, 3, 0);
        // Same length as the request's b"v", different byte.
        reply.snapshot.as_mut().expect("snapshot").value = Some(b"x".to_vec());
        assert!(validate_mutation_reply(&cx("compare_and_set", &request), &reply).is_err());
        // Legitimately withheld bytes still pass: no READ, or a replay.
        let mut withheld = well_shaped("compare_and_set", 1, 3, 0);
        withheld.snapshot.as_mut().expect("snapshot").value = None;
        assert!(validate_mutation_reply(&cx("compare_and_set", &request), &withheld).is_ok());
    }

    /// Reply-level counters obey the ceiling: a forged revision above
    /// `MAX_COUNTER` is refused even where nothing else bounds it — a
    /// **rejection**, whose observed revisions carry no request correlation,
    /// is the shape only the ceiling can refuse.
    #[test]
    fn reply_counters_are_bounded() {
        let request = sequenced_request("set_admin");
        let mut reply = well_shaped("set_admin", 1, 100, 7);
        reply.admin_revision = MAX_COUNTER + 1;
        assert!(validate_mutation_reply(&cx("set_admin", &request), &reply).is_err());

        let mut reply = well_shaped("set_grant", 1, 100, 6);
        reply.acl_revision = MAX_COUNTER + 1;
        assert!(
            validate_mutation_reply(&cx("set_grant", &sequenced_request("set_grant")), &reply)
                .is_err()
        );
    }

    /// The renew/fence request family decodes for correlation too — one
    /// spot-check that resource correlation reads through the fence.
    #[test]
    fn correlation_reads_through_a_fence() {
        let renew = encode(&RenewRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            client_id: CLIENT.to_vec(),
            sequence: 1,
            fence: FenceV1 {
                group_id: GROUP.to_vec(),
                resource: resource(),
                epoch: 1,
                holder: ACTOR.to_vec(),
            },
        });
        let mut reply = well_shaped("renew", 1, 2, 0);
        assert!(validate_mutation_reply(&cx("renew", &renew), &reply).is_ok());
        if let Some(s) = reply.snapshot.as_mut() {
            s.resource.key = b"other".to_vec();
        }
        assert!(validate_mutation_reply(&cx("renew", &renew), &reply).is_err());
    }
}
