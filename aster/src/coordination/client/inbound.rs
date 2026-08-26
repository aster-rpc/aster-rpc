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

use crate::coordination::contract::{LogIdV1, ResourceKeyV1, ResourceSnapshotV1};
use crate::coordination::model::limits::rights_are_known;
use crate::coordination::reason::{Outcome, Reason};
use crate::coordination::store::wire::{
    AclStatusReplyWireV1, AclStatusRequestV1, AcquireRequestV1, AdminStatusReplyWireV1,
    AdminStatusRequestV1, AuditRecordWireV1, CancelMembershipRequestV1, ChangeMembershipRequestV1,
    CompareAndSetRequestV1, GroupStatusReplyWireV1, GroupStatusRequestV1,
    MembershipStatusReplyWireV1, MembershipStatusRequestV1, MutationReplyWireV1,
    ProbeClientReplyWireV1, ProbeClientRequestV1, ReadReplyWireV1, ReadRequestV1,
    RecoveryExportManifestWireV1, RecoveryExportRequestV1, ReleaseRequestV1,
    ReleaseWithValueRequestV1, RenewRequestV1, RetireClientReplyWireV1, RetireClientRequestV1,
    RevokeRequestV1, SetGrantRequestV1, STORE_PROTOCOL_VERSION,
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
    T: crate::rpc::contract::WireField + fory_core::Serializer + fory_core::ForyDefault + 'static,
{
    crate::coordination::contract::payload_fory::<T>()
        .deserialize(bytes)
        .map_err(|_| "the pending request no longer decodes as its method's type".to_string())
}

/// The resource the pending request names, for snapshot correlation.
/// `None` for the methods whose replies carry no resource snapshot.
fn expected_resource(method: &str, request: &[u8]) -> Result<Option<ResourceKeyV1>, String> {
    Ok(match method {
        "acquire" => Some(decode::<AcquireRequestV1>(request)?.resource),
        "renew" => Some(decode::<RenewRequestV1>(request)?.fence.resource),
        "compare_and_set" => Some(decode::<CompareAndSetRequestV1>(request)?.fence.resource),
        "release" => Some(decode::<ReleaseRequestV1>(request)?.fence.resource),
        "release_with_value" => Some(decode::<ReleaseWithValueRequestV1>(request)?.fence.resource),
        "revoke" => Some(decode::<RevokeRequestV1>(request)?.resource),
        "set_grant" => Some(decode::<SetGrantRequestV1>(request)?.resource),
        "set_admin" | "change_membership" | "cancel_membership" => None,
        other => return Err(format!("'{other}' is not a sequenced mutation method")),
    })
}

/// The operation id the pending request names — the one a membership
/// success must echo. `None` for every non-membership method.
fn expected_operation_id(method: &str, request: &[u8]) -> Result<Option<Vec<u8>>, String> {
    Ok(match method {
        "change_membership" => Some(decode::<ChangeMembershipRequestV1>(request)?.operation_id),
        "cancel_membership" => Some(decode::<CancelMembershipRequestV1>(request)?.operation_id),
        _ => None,
    })
}

/// The one success outcome each sequenced mutation may report.
fn method_success(method: &str) -> Result<Outcome, String> {
    Ok(match method {
        "acquire" => Outcome::Granted,
        "renew" => Outcome::Renewed,
        "compare_and_set" => Outcome::Mutated,
        "release" | "release_with_value" => Outcome::Released,
        "revoke" => Outcome::Revoked,
        "set_grant" => Outcome::GrantUpdated,
        "set_admin" => Outcome::AdminUpdated,
        "change_membership" => Outcome::MembershipAccepted,
        "cancel_membership" => Outcome::MembershipCancelled,
        other => return Err(format!("'{other}' is not a sequenced mutation method")),
    })
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
    if reason.is_reserved() {
        return Err(format!(
            "reason {} is reserved and never emitted",
            reply.reason_code
        ));
    }
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

    // Method correlation: this method's success, a rejection, or the
    // tombstone's ClientRetired refusal — nothing else.
    let success = method_success(cx.method)?;
    if outcome != success && outcome != Outcome::Rejected {
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
        validate_snapshot(cx, outcome, snapshot)?;
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
            let expected = expected_operation_id(cx.method, cx.request)?
                .ok_or_else(|| "a membership success must echo an operation id".to_string())?;
            if *op != expected {
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
    // `admin_revision`: the new committed revision on AdminUpdated, the
    // observed revision on a rejection, and `0` everywhere else.
    match outcome {
        Outcome::AdminUpdated => {
            if reply.admin_revision == 0 {
                return Err("an AdminUpdated reply must report the committed revision".to_string());
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
    // grant echo (GrantUpdated) or `0`.
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
    Ok(())
}

/// The nested snapshot: identity echoed from the request, and facts that
/// cannot contradict themselves or the outcome that carried them.
fn validate_snapshot(
    cx: &MutationContext<'_>,
    outcome: Outcome,
    snapshot: &ResourceSnapshotV1,
) -> Result<(), String> {
    if snapshot.group_id != cx.group_id {
        return Err("the snapshot names another coordination group".to_string());
    }
    if let Some(expected) = expected_resource(cx.method, cx.request)? {
        if snapshot.resource != expected {
            return Err("the snapshot names another resource than the request".to_string());
        }
    }
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
    /// §5.2 audit operation codes: 1–11 the mutation methods, 100 `Expire`,
    /// 101 `MembershipProgress`, 102 the committed genesis installation.
    pub(super) fn audit_operation(code: i32) -> bool {
        (1..=11).contains(&code) || (100..=102).contains(&code)
    }
    /// §5.3 system result codes: 1–8.
    pub(super) const SYSTEM_RESULT: std::ops::RangeInclusive<i32> = 1..=8;
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
    Ok(())
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
    if !domains::audit_operation(record.operation_code) {
        return Err(format!(
            "operation code {} is not a §5.2 audit operation",
            record.operation_code
        ));
    }
    // The result code space follows the operation family: a client command
    // records its public outcome, a system command its declared result.
    if record.operation_code <= 11 {
        if Outcome::from_code(record.result_code).is_none() {
            return Err(format!(
                "a client operation cannot record result {}",
                record.result_code
            ));
        }
    } else if !domains::SYSTEM_RESULT.contains(&record.result_code) {
        return Err(format!(
            "a system operation cannot record result {}",
            record.result_code
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
        SetAdminRequestV1,
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
        T: crate::rpc::contract::WireField
            + fory_core::Serializer
            + fory_core::ForyDefault
            + 'static,
    {
        payload_fory::<T>().serialize(value).expect("encodes")
    }

    fn acquire_request() -> Vec<u8> {
        encode(&AcquireRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            client_id: CLIENT.to_vec(),
            sequence: 1,
            resource: resource(),
            ttl_ms: 30_000,
        })
    }

    fn change_membership_request() -> Vec<u8> {
        encode(&ChangeMembershipRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            client_id: CLIENT.to_vec(),
            sequence: 1,
            operation_id: OP.to_vec(),
            expected_membership_log_id: None,
            desired_voters: Vec::new(),
            retained_learners: Vec::new(),
            ..Default::default()
        })
    }

    fn set_admin_request() -> Vec<u8> {
        encode(&SetAdminRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: GROUP.to_vec(),
            client_id: CLIENT.to_vec(),
            sequence: 1,
            target_node_id: OTHER.to_vec(),
            enabled: true,
            expected_admin_revision: 1,
        })
    }

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

    /// A structurally legal reply for the triple, so the matrix verdict is
    /// about the **pair rules**, never about a missing snapshot or a stray
    /// auxiliary field.
    fn well_shaped(
        method: &str,
        disposition: i32,
        outcome: i32,
        reason: i32,
    ) -> MutationReplyWireV1 {
        let mut reply = owned_reply(disposition, outcome, reason);
        match outcome {
            1..=3 => {
                reply.snapshot = Some(held_snapshot());
                reply.acl_revision = 3;
            }
            4..=6 => {
                reply.snapshot = Some(free_snapshot());
                reply.acl_revision = 3;
            }
            7 => reply.admin_revision = 2,
            8 | 10 if method == "change_membership" || method == "cancel_membership" => {
                reply.operation_id = Some(OP.to_vec());
            }
            _ => {}
        }
        reply
    }

    /// **The exhaustive §5.2 pair matrix.** Every disposition × outcome ×
    /// reason (0–26, the reserved 20, and out-of-range codes) against an
    /// acquire entry, each row's verdict from the quoted spec lists alone.
    #[test]
    fn the_mutation_pair_matrix_is_exhaustive() {
        let request = acquire_request();
        for disposition in [0, 1, 2, 3] {
            for outcome in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 99, 100] {
                for reason in (0..=27).chain([-1, 20]) {
                    let legal = match (disposition, outcome, reason) {
                        // The method's one success, Consumed, no reason.
                        (1, 1, 0) => true,
                        // §10.1 shapeless, saved conflicts and refusals.
                        (1, 100, 0) => true,
                        (1, 100, r) => SAVED.contains(&r),
                        // Pre-write answers.
                        (2, 100, r) => NOT_CONSUMED.contains(&r),
                        _ => false,
                    };
                    let reply = well_shaped("acquire", disposition, outcome, reason);
                    let verdict = validate_mutation_reply(&cx("acquire", &request), &reply);
                    assert_eq!(
                        verdict.is_ok(),
                        legal,
                        "({disposition}, {outcome}, {reason}) on acquire: {verdict:?}"
                    );
                }
            }
        }
    }

    /// The reviewer's counterexample, end to end: a Consumed rejection with
    /// `21 ActiveClientCap` claims a session no path creates — accepting it
    /// would advance the journal past a sequence the group still expects.
    #[test]
    fn a_consumed_active_client_cap_is_refused() {
        let request = acquire_request();
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
        let acquire = acquire_request();
        let change = change_membership_request();
        let admin = set_admin_request();

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

    /// The renew/fence request family decodes for correlation too — one
    /// spot-check that `expected_resource` reads through the fence.
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
