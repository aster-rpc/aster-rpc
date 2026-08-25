//! The client's one **reply-validity** boundary (the fifth review's P0).
//!
//! Ownership — protocol, group, client id, sequence, request hash — says a
//! reply is addressed to this entry. It does not say the reply is a §5.2
//! reply: a decodable frame can claim `Granted` with no snapshot, name a
//! snapshot about some other resource, contradict itself (`held` with no
//! holder, a terminal reason on a success, a redacted value carrying
//! bytes), or smuggle malformed nested identifiers. This module holds the
//! full, **request-aware** verdict, shared verbatim by live delivery
//! (`driver::decode_and_check`), the journal's persist transitions, and the
//! journal reopen — one validator, three call sites, no spelling of "live
//! replies are less validated than saved ones".
//!
//! The read/status half lives on [`ValidNested`], which every typed reply
//! the client accepts implements: nested identifier widths, ordering and
//! coherence, checked after the outer protocol/group envelope.

use crate::coordination::contract::{LogIdV1, ResourceKeyV1, ResourceSnapshotV1};
use crate::coordination::reason::{Outcome, Reason};
use crate::coordination::store::wire::{
    AclStatusReplyWireV1, AcquireRequestV1, AdminStatusReplyWireV1, AuditRecordWireV1,
    CompareAndSetRequestV1, GroupStatusReplyWireV1, MembershipStatusReplyWireV1,
    MutationReplyWireV1, ProbeClientReplyWireV1, ReadReplyWireV1, RecoveryExportManifestWireV1,
    ReleaseRequestV1, ReleaseWithValueRequestV1, RenewRequestV1, RetireClientReplyWireV1,
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

    // Disposition, outcome and reason must be §5.2 values that cohere.
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
    if reply.reason_code != 0 && Reason::from_code(reply.reason_code).is_none() {
        return Err(format!("reason {} is not a §5.2 reason", reply.reason_code));
    }
    if outcome != Outcome::Rejected {
        // A success carrying a refusal reason — a terminal one included — is
        // neither a success nor a refusal.
        if reply.reason_code != 0 {
            return Err("a non-rejection outcome must carry reason 0".to_string());
        }
        if reply.disposition_code != 1 {
            return Err("a success must be Consumed".to_string());
        }
    }

    // Method correlation: this method's success, a rejection, or the
    // tombstone's ClientRetired refusal — nothing else.
    let success = method_success(cx.method)?;
    let tombstone = outcome == Outcome::Rejected
        && reply.disposition_code == 2
        && reply.reason_code == Reason::ClientRetired.code();
    if outcome != success && outcome != Outcome::Rejected && !tombstone {
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

    if let Some(id) = &reply.membership_log_id {
        check_log_id("the reply's membership log id", id)?;
    }
    if let Some(op) = &reply.operation_id {
        if op.len() != 16 {
            return Err("the reply's operation id is not 16 bytes".to_string());
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

// ── Read/status nested validity ─────────────────────────────────────────────

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

/// Nested validity for one typed reply, run after the outer protocol/group
/// envelope. The default is "nothing nested to check"; every reply with
/// nested identifiers overrides it.
pub(crate) trait ValidNested {
    fn validate_nested(&self, group_id: &[u8]) -> Result<(), String> {
        let _ = group_id;
        Ok(())
    }
}

impl ValidNested for ReadReplyWireV1 {
    fn validate_nested(&self, group_id: &[u8]) -> Result<(), String> {
        check_read_snapshot(group_id, &self.snapshot)
    }
}

impl ValidNested for AclStatusReplyWireV1 {
    fn validate_nested(&self, _group_id: &[u8]) -> Result<(), String> {
        let subjects: Vec<Vec<u8>> = self
            .acl_snapshot
            .grants
            .iter()
            .map(|grant| grant.subject_node_id.clone())
            .collect();
        check_sorted_unique_32("the ACL grant list", &subjects)
    }
}

impl ValidNested for AdminStatusReplyWireV1 {
    fn validate_nested(&self, _group_id: &[u8]) -> Result<(), String> {
        let ids: Vec<Vec<u8>> = self
            .admin_snapshot
            .administrators
            .iter()
            .map(|node| node.node_id.clone())
            .collect();
        check_sorted_unique_32("the administrator list", &ids)
    }
}

impl ValidNested for MembershipStatusReplyWireV1 {
    fn validate_nested(&self, _group_id: &[u8]) -> Result<(), String> {
        if let Some(op) = &self.operation_id {
            if op.len() != 16 {
                return Err("the intent's operation id is not 16 bytes".to_string());
            }
        }
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
        if self
            .desired_voters
            .iter()
            .chain(self.retained_learners.iter())
            .any(|node| node.node_id.len() != 32)
        {
            return Err("the intent names a malformed node id".to_string());
        }
        Ok(())
    }
}

impl ValidNested for ProbeClientReplyWireV1 {
    fn validate_nested(&self, _group_id: &[u8]) -> Result<(), String> {
        if self.target_node_id.len() != 32 {
            return Err("the probe names a malformed target node id".to_string());
        }
        if self.target_client_id.len() != 16 {
            return Err("the probe names a malformed target client id".to_string());
        }
        if self.exists && !self.request_hash.is_empty() && self.request_hash.len() != 32 {
            return Err("the probe's request hash is neither absent nor 32 bytes".to_string());
        }
        Ok(())
    }
}

impl ValidNested for GroupStatusReplyWireV1 {
    fn validate_nested(&self, _group_id: &[u8]) -> Result<(), String> {
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
        Ok(())
    }
}

impl ValidNested for AuditRecordWireV1 {
    fn validate_nested(&self, _group_id: &[u8]) -> Result<(), String> {
        check_log_id("the audit record's log id", &self.log_id)?;
        if let Some(actor) = &self.actor_node_id {
            if actor.len() != 32 {
                return Err("the audit record names a malformed actor".to_string());
            }
        }
        Ok(())
    }
}

impl ValidNested for RecoveryExportManifestWireV1 {
    fn validate_nested(&self, _group_id: &[u8]) -> Result<(), String> {
        if self.blake3_checksum.len() != 32 {
            return Err("the manifest's checksum is not 32 bytes".to_string());
        }
        if let Some(id) = &self.applied_log_id {
            check_log_id("the manifest's applied log id", id)?;
        }
        Ok(())
    }
}

impl ValidNested for RetireClientReplyWireV1 {}
