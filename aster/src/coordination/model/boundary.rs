//! The production boundary: `CommandV1` in, `CommandReplyV1` out.
//!
//! `typeconfig.rs` instantiates OpenRaft with `D = CommandV1` and
//! `R = CommandReplyV1`. Until this module existed, the model accepted its own
//! types and nothing converted between the two -- so "one apply path over all
//! production commands" was true of the model and false of the system. A
//! storage layer written against `apply_command` would have had to invent the
//! conversion itself, which is where the two would have drifted.
//!
//! ## Total, and fallible in exactly one direction
//!
//! [`apply_wire_command`] is **total**: every `CommandV1`, including one this
//! version cannot interpret, produces either a reply or a [`Fault`]. There is no
//! input for which a voter can decline to decide, because declining is
//! divergence.
//!
//! Decoding is fallible because the wire is wider than the model: `Vec<u8>`
//! where the model has `[u8; 32]`, a `String` namespace where it has a validated
//! one, an `i32` where it has an enum. A value that fails to narrow is not a
//! command this voter can apply -- and per §5.3.1 that is fatal, not skippable.

use crate::coordination::contract::{
    ActionV1, ClientCommandV1, CommandReplyV1, CommandV1, ExpireCommandV1,
    MembershipProgressCommandV1, RetireClientCommandV1,
};
use crate::coordination::reason::Disposition;

use super::admin::AdminAction;
use super::apply::{
    apply_command, Action, ClientRef, ClientResult, Command, CommandReply, Fault, ReplyShape,
    ShapelessCommand, OPERATION_EXPIRE, OPERATION_MEMBERSHIP_PROGRESS, OPERATION_RETIRE_CLIENT,
};
use super::expire::ExpireCommand;
use super::grant::GrantAction;
use super::limits::RIGHT_READ;
use super::membership::{
    CancelMembershipAction, ChangeMembershipAction, MembershipProgressCommand,
};
use super::project::{client_reply, retirement_reply, system_reply, Provenance};
use super::resource::{self, Fence, RawResourceKey, ResourceAction};
use super::retire::RetireCommand;
use super::shape::{self, ShapeLimit};
use super::state::{LogId, MemberSeed, MembershipPlan, NodeId, State};
use crate::coordination::contract::{SYS_EXPIRE_MALFORMED, SYS_MEMBERSHIP_MALFORMED};

/// Narrow a wire `Vec<u8>` to a 32-byte node id.
fn node_id(bytes: &[u8]) -> Result<NodeId, NarrowError> {
    bytes
        .try_into()
        .map_err(|_| NarrowError::MalformedField(ShapeLimit::NodeIdWidth))
}

fn client_id(bytes: &[u8]) -> Result<[u8; 16], NarrowError> {
    bytes
        .try_into()
        .map_err(|_| NarrowError::MalformedField(ShapeLimit::ClientIdWidth))
}

fn hash(bytes: &[u8]) -> Result<[u8; 32], NarrowError> {
    bytes
        .try_into()
        .map_err(|_| NarrowError::MalformedField(ShapeLimit::RequestHashWidth))
}

fn raw_key(k: &crate::coordination::contract::ResourceKeyV1) -> RawResourceKey {
    RawResourceKey::new(k.namespace.clone(), k.key.clone())
}

fn fence(f: &crate::coordination::contract::FenceV1) -> Result<Fence, NarrowError> {
    Ok(Fence {
        resource: raw_key(&f.resource),
        epoch: f.epoch,
        holder: node_id(&f.holder)?,
    })
}

fn log_id(id: &crate::coordination::contract::LogIdV1) -> Result<LogId, NarrowError> {
    LogId::try_from(id).map_err(|_| NarrowError::MalformedField(ShapeLimit::NodeIdWidth))
}

/// Why a wire action could not be narrowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NarrowError {
    /// The forward-compatibility carrier. This voter does not know what the
    /// action *is*, so §5.3.1 makes it fatal.
    UnknownVariant,
    /// A **known** action whose fields are ill-formed -- a node id that is not
    /// 32 bytes, a malformed log id.
    ///
    /// Deliberately not fatal. §10.1 makes a known bounds violation a
    /// deterministic rejection, and this is the same kind: every voter knows
    /// exactly what the command was meant to be and can refuse it identically.
    /// Halting instead would hand a faulty leader an availability kill switch
    /// through a malformed field, which is precisely the outcome §10.1 rejects.
    MalformedField(ShapeLimit),
}

/// Narrow a wire action to a model action.
pub fn action_from_wire(action: &ActionV1) -> Result<Action, NarrowError> {
    let field = |o: Option<NodeId>| o.ok_or(NarrowError::MalformedField(ShapeLimit::NodeIdWidth));
    let _ = &field;
    Ok(match action {
        ActionV1::Acquire(a) => Action::Resource(ResourceAction::Acquire {
            resource: raw_key(&a.resource),
            ttl_ms: a.ttl_ms,
        }),
        ActionV1::Renew(a) => Action::Resource(ResourceAction::Renew {
            fence: fence(&a.fence)?,
        }),
        ActionV1::CompareAndSet(a) => Action::Resource(ResourceAction::CompareAndSet {
            fence: fence(&a.fence)?,
            expected_value_revision: a.expected_value_revision,
            new_value: a.new_value.clone(),
        }),
        ActionV1::Release(a) => Action::Resource(ResourceAction::Release {
            fence: fence(&a.fence)?,
        }),
        ActionV1::ReleaseWithValue(a) => Action::Resource(ResourceAction::ReleaseWithValue {
            fence: fence(&a.fence)?,
            expected_value_revision: a.expected_value_revision,
            final_value: a.final_value.clone(),
        }),
        ActionV1::Revoke(a) => Action::Resource(ResourceAction::Revoke {
            resource: raw_key(&a.resource),
            expected_epoch: a.expected_epoch,
        }),
        ActionV1::SetGrant(a) => Action::Grant(GrantAction {
            resource: raw_key(&a.resource),
            subject: node_id(&a.target_node_id)?,
            rights: a.rights,
            expected_acl_revision: a.expected_acl_revision,
        }),
        ActionV1::SetAdmin(a) => Action::Admin(AdminAction {
            target: node_id(&a.target_node_id)?,
            enabled: a.enabled,
            expected_admin_revision: a.expected_admin_revision,
        }),
        ActionV1::ChangeMembership(a) => Action::ChangeMembership(ChangeMembershipAction {
            operation_id: client_id(&a.operation_id)?,
            expected_membership_log_id: match &a.expected_membership_log_id {
                Some(id) => Some(log_id(id)?),
                None => None,
            },
            plan: MembershipPlan {
                desired_voters: a
                    .desired_voters
                    .iter()
                    .map(|n| node_id(&n.node_id))
                    .collect::<Result<Vec<_>, _>>()?,
                retained_learners: a
                    .retained_learners
                    .iter()
                    .map(|n| node_id(&n.node_id))
                    .collect::<Result<Vec<_>, _>>()?,
                member_seeds: a
                    .member_seeds
                    .iter()
                    .map(|s| {
                        Ok(MemberSeed {
                            node_id: node_id(&s.node_id)?,
                            ticket: s.ticket.clone().into_bytes(),
                            join_permit: s.signed_join_permit.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            },
        }),
        ActionV1::CancelMembership(a) => Action::CancelMembership(CancelMembershipAction {
            operation_id: client_id(&a.operation_id)?,
            expected_phase_code: a.expected_phase_code,
        }),
        // §5.2.1: never hashable, never proposable, never appliable.
        ActionV1::Unknown(_) => return Err(NarrowError::UnknownVariant),
    })
}

/// Narrow a wire command to a model command.
///
/// The `Client` arm additionally **verifies the request hash**: `request_hash`
/// is what the session table compares a retry against, so a hash that is not the
/// canonical digest of its own action would let a client present one action
/// under another's identity -- replaying a benign hash to smuggle a different
/// mutation past the classifier. §5.2.1 defines the hash over the action, and
/// the only way that binding holds is if apply checks it.
pub fn command_from_wire(command: &CommandV1) -> Result<Command, NarrowError> {
    Ok(match command {
        CommandV1::Client(ClientCommandV1 {
            actor_node_id,
            client_id: cid,
            sequence,
            request_hash,
            action,
        }) => Command::Client {
            client: ClientRef {
                actor: node_id(actor_node_id)?,
                client_id: client_id(cid)?,
                sequence: *sequence,
                request_hash: hash(request_hash)?,
            },
            action: {
                let narrowed = action_from_wire(action)?;
                let expected = crate::coordination::hash::request_hash(action)
                    .map_err(|_| NarrowError::MalformedField(ShapeLimit::RequestHashMismatch))?;
                if hash(request_hash)? != expected {
                    return Err(NarrowError::MalformedField(ShapeLimit::RequestHashMismatch));
                }
                narrowed
            },
        },
        CommandV1::Expire(ExpireCommandV1 {
            resource,
            epoch,
            lease_revision,
        }) => Command::Expire(ExpireCommand {
            resource: raw_key(resource)
                .validate()
                .map_err(NarrowError::MalformedField)?,
            epoch: *epoch,
            lease_revision: *lease_revision,
        }),
        CommandV1::MembershipProgress(MembershipProgressCommandV1 {
            operation_id,
            expected_phase,
            new_phase,
            observed_membership_log_id,
            result_code,
        }) => Command::MembershipProgress(MembershipProgressCommand {
            operation_id: client_id(operation_id)?,
            expected_phase: *expected_phase,
            new_phase: *new_phase,
            observed_membership_log_id: match observed_membership_log_id {
                Some(id) => Some(log_id(id)?),
                None => None,
            },
            result_code: *result_code,
        }),
        CommandV1::RetireClient(RetireClientCommandV1 {
            actor_node_id,
            target_node_id,
            target_client_id,
        }) => Command::RetireClient(RetireCommand {
            actor: node_id(actor_node_id)?,
            target_actor: node_id(target_node_id)?,
            target_client_id: client_id(target_client_id)?,
        }),
        CommandV1::Unknown(_) => Command::Unknown,
    })
}

/// The §12.1 `max_encoded_command` bound, checked on the **encoded** form.
///
/// Not reconstructible from field limits, which is why it takes the bytes: an
/// action whose individual fields are each in range can still encode past the
/// cap once framing is counted.
pub fn check_encoded_command(encoded_len: usize) -> Option<ShapeLimit> {
    shape::check_encoded_command(encoded_len)
}

/// Apply one production `CommandV1` and produce its `CommandReplyV1`.
///
/// Total: an input this voter cannot narrow is a [`Fault::UnknownCommand`],
/// exactly as a `CommandV1::Unknown` is. §5.3.1 makes both fatal rather than
/// skippable, because a voter that skipped one would diverge from the voters
/// that could apply it.
pub fn apply_wire_command(
    state: &mut State,
    log_id: LogId,
    group_id: &[u8],
    command: &CommandV1,
) -> Result<CommandReplyV1, Fault> {
    // §5.3.1: a variant this version does not know is fatal. Everything else --
    // including a request the protocol cannot make sense of -- is a command that
    // goes through apply.
    let model = command_for_apply(command).map_err(|_| Fault::UnknownCommand)?;

    // A first attempt reports the value it wrote; the value lives in state, not
    // in the saved reply, so it is read here and only here.
    let reply = apply_command(state, log_id, &model)?;
    Ok(to_wire(state, group_id, &model, reply))
}

/// Narrow a committed command for apply, turning a request-shape violation into
/// the command §10.1 forces **through the log** rather than into an early reply.
///
/// The distinction that matters is where the violation is:
///
/// * a well-formed mutation prefix with a shapeless action becomes
///   [`Command::RejectedClient`], which consumes the sequence, saves its reply,
///   audits, charges reserve and advances `last_applied` in one transaction;
/// * an ill-formed prefix becomes [`Command::Shapeless`], which names no session
///   and therefore can do none of that;
/// * an unknown variant stays fatal.
///
/// An earlier revision short-circuited both shape cases to a reply built at this
/// boundary, so a committed entry left no trace in replicated state at all.
pub fn command_for_apply(command: &CommandV1) -> Result<Command, NarrowError> {
    // **Unknown detection comes first, before any shape or size classification.**
    // An earlier revision checked the encoded size first, so a large action from
    // a newer version became `RejectedClient { operation_code: 0 }` -- a
    // deterministic rejection this voter had no business making. §5.3.1 makes an
    // uninterpretable entry fatal precisely because the voters that *can* read
    // it will apply it: rejecting it here diverges the group while every local
    // test stays green.
    if unknown_variant(command) {
        return Err(NarrowError::UnknownVariant);
    }

    // §12.1's `max_encoded_command`, checked on the **encoded** form. The bound
    // is not reconstructible from field limits: an action whose fields are each
    // in range can still encode past the cap once framing is counted.
    // Re-encoding is deterministic, so every voter reaches the same verdict on
    // the same entry -- which is what makes this a committed rejection rather
    // than a host-local ingress check some voters applied and others did not.
    if oversized(command) {
        return Ok(shape_rejection(command));
    }

    match command_from_wire(command) {
        Ok(model) => Ok(model),
        Err(NarrowError::UnknownVariant) => Err(NarrowError::UnknownVariant),
        // §10.1: a **known** command with an ill-formed field is a deterministic
        // rejection, not a halt. Every voter knows what it was meant to be and
        // refuses it identically; halting would let a malformed field become an
        // availability kill switch.
        Err(NarrowError::MalformedField(_)) => Ok(shape_rejection(command)),
    }
}

/// Does this command carry a variant this build cannot interpret?
///
/// Checked at both levels: the command union and the action union inside a
/// client command. A `CommandV1` this voter knows carrying an `ActionV1` it does
/// not is still an entry it cannot apply.
fn unknown_variant(command: &CommandV1) -> bool {
    match command {
        CommandV1::Unknown(_) => true,
        CommandV1::Client(c) => matches!(c.action, ActionV1::Unknown(_)),
        _ => false,
    }
}

/// Which forced-apply command a shape violation becomes.
///
/// The prefix decides. A validated `(actor, client_id, sequence, request_hash)`
/// is a session identity even when the action attached to it is nonsense, so it
/// takes the consuming path; anything less takes the shapeless one.
fn shape_rejection(command: &CommandV1) -> Command {
    match command {
        CommandV1::Client(c) => {
            match (
                node_id(&c.actor_node_id),
                client_id(&c.client_id),
                hash(&c.request_hash),
            ) {
                (Ok(actor), Ok(cid), Ok(request_hash)) => Command::RejectedClient {
                    client: ClientRef {
                        actor,
                        client_id: cid,
                        sequence: c.sequence,
                        request_hash,
                    },
                    operation_code: wire_operation_code(&c.action),
                    // §10.1: the identity is stored when it is **valid**. An
                    // oversized command names a perfectly good resource -- the
                    // encoding is what is too big -- and dropping it would save
                    // a reply the client cannot match to anything.
                    resource: wire_resource(&c.action),
                },
                // The actor may still have narrowed even when the client id or
                // hash did not, and an attributable entry should be attributed.
                (actor, _, _) => Command::Shapeless(ShapelessCommand {
                    operation_code: wire_operation_code(&c.action),
                    reply_shape: ReplyShape::Client,
                    system_result_code: 0,
                    actor: actor.ok(),
                }),
            }
        }
        CommandV1::RetireClient(r) => Command::Shapeless(ShapelessCommand {
            operation_code: OPERATION_RETIRE_CLIENT,
            reply_shape: ReplyShape::Retirement,
            system_result_code: 0,
            actor: node_id(&r.actor_node_id).ok(),
        }),
        CommandV1::Expire(_) => Command::Shapeless(ShapelessCommand {
            operation_code: OPERATION_EXPIRE,
            reply_shape: ReplyShape::System,
            // Its own code: a malformed expiry answered with a membership
            // phase code tells an operator the wrong subsystem is broken.
            system_result_code: SYS_EXPIRE_MALFORMED,
            // A system command's actor is the proposing leader, which the
            // caller stamps; nothing in the entry itself names one.
            actor: None,
        }),
        CommandV1::MembershipProgress(_) => Command::Shapeless(ShapelessCommand {
            operation_code: OPERATION_MEMBERSHIP_PROGRESS,
            reply_shape: ReplyShape::System,
            system_result_code: SYS_MEMBERSHIP_MALFORMED,
            actor: None,
        }),
        // Unreachable: `command_for_apply` returns `UnknownVariant` first.
        CommandV1::Unknown(_) => Command::Unknown,
    }
}

/// The resource a wire action names, **if that identity validates**.
fn wire_resource(action: &ActionV1) -> Option<super::state::ResourceKey> {
    let raw = match action {
        ActionV1::Acquire(a) => raw_key(&a.resource),
        ActionV1::Renew(a) => raw_key(&a.fence.resource),
        ActionV1::CompareAndSet(a) => raw_key(&a.fence.resource),
        ActionV1::Release(a) => raw_key(&a.fence.resource),
        ActionV1::ReleaseWithValue(a) => raw_key(&a.fence.resource),
        ActionV1::Revoke(a) => raw_key(&a.resource),
        ActionV1::SetGrant(a) => raw_key(&a.resource),
        // These name no resource at all; §8.1 makes them group-level.
        ActionV1::SetAdmin(_)
        | ActionV1::ChangeMembership(_)
        | ActionV1::CancelMembership(_)
        | ActionV1::Unknown(_) => return None,
    };
    raw.validate().ok()
}

/// The §5.2 method a wire action names.
///
/// Read from the **variant**, which is known even when the fields inside it are
/// not, so a rejected command still audits what was attempted rather than an
/// anonymous failure.
fn wire_operation_code(action: &ActionV1) -> i32 {
    match action {
        ActionV1::Acquire(_) => 1,
        ActionV1::Renew(_) => 2,
        ActionV1::CompareAndSet(_) => 3,
        ActionV1::Release(_) => 4,
        ActionV1::ReleaseWithValue(_) => 5,
        ActionV1::Revoke(_) => 6,
        ActionV1::SetGrant(_) => 7,
        ActionV1::SetAdmin(_) => 8,
        ActionV1::ChangeMembership(_) => 9,
        // §5.2 skips 10; cancel is 11.
        ActionV1::CancelMembership(_) => 11,
        // Not reachable through `shape_rejection`, which runs only after
        // `command_for_apply` has ruled the variant known.
        ActionV1::Unknown(_) => 0,
    }
}

/// Does this command exceed §12.1's encoded-size cap?
///
/// A command that will not re-encode is treated as oversized rather than as
/// zero bytes: an unmeasurable request is not a small one, and the only safe
/// direction to guess in is refusal.
fn oversized(command: &CommandV1) -> bool {
    let encoded_len = crate::coordination::contract::payload_fory::<CommandV1>()
        .serialize(command)
        .map_or(usize::MAX, |bytes| bytes.len());
    check_encoded_command(encoded_len).is_some()
}

fn to_wire(
    state: &State,
    group_id: &[u8],
    command: &Command,
    reply: CommandReply,
) -> CommandReplyV1 {
    match reply {
        CommandReply::Client(result) => {
            CommandReplyV1::Client(client_reply_for(state, group_id, command, &result))
        }
        CommandReply::System(r) => CommandReplyV1::System(system_reply(
            r.system_result_code,
            r.lease_revision,
            state.membership_log_id.as_ref(),
        )),
        CommandReply::Retirement(r) => {
            CommandReplyV1::Retirement(retirement_reply(r.outcome, r.reason))
        }
    }
}

/// Project a client result, supplying the live value only when the actor may
/// read it.
///
/// **Two independent conditions, both required.**
///
/// A *replay* never carries a value: the bytes were never saved, and
/// substituting the current ones would hand back something the original command
/// did not produce.
///
/// A first attempt carries one only if the actor holds `READ` on that resource.
/// §8.1 grants bind one exact `(group, resource, subject)` and imply nothing
/// else, so `ACQUIRE` is permission to take a lease — not to read what the lease
/// guards. Returning the value on every successful command let an actor with
/// `ACQUIRE` alone acquire a resource and receive its existing secret, which is
/// a confidentiality break rather than an inconvenience: the actor never had to
/// call `read_linearizable`, so the ACL check that exists for reads was simply
/// bypassed.
///
/// Without `READ` the reply is `(None, false)` — absent, and **not** flagged
/// redacted. `value_redacted` means "this was a replay and the bytes were never
/// stored"; using it for an authorization outcome would tell the client to retry
/// for a value it is not entitled to.
fn client_reply_for(
    state: &State,
    group_id: &[u8],
    command: &Command,
    result: &ClientResult,
) -> crate::coordination::contract::ClientReplyV1 {
    let provenance = if result.replayed {
        Provenance::Replay
    } else {
        Provenance::FirstAttempt
    };

    let actor = match command {
        Command::Client { client, .. } => Some(client.actor),
        _ => None,
    };

    let value = match provenance {
        Provenance::Replay => None,
        Provenance::FirstAttempt => result.reply.resource().and_then(|key| {
            let actor = actor?;
            let may_read = resource::granted_rights(state, key, &actor) & RIGHT_READ != 0;
            may_read
                .then(|| state.resources.get(key).map(|row| row.value.clone()))
                .flatten()
        }),
    };

    client_reply(
        group_id,
        &result.reply,
        result.disposition,
        provenance,
        value.as_deref(),
    )
}

/// The disposition codes §5.2 fixes, exposed so a caller need not restate them.
pub fn disposition_code(disposition: Disposition) -> i32 {
    match disposition {
        Disposition::Consumed => 1,
        Disposition::NotConsumed => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::contract::{
        AcquireActionV1, MemberSeedV1, NodeIdV1, ResourceKeyV1, SetAdminActionV1,
    };
    use crate::coordination::hash::request_hash;
    use crate::coordination::model::limits::ALL_RIGHTS;
    use crate::coordination::model::state::ResourceKey;
    use crate::coordination::reason::{Outcome, Reason};

    const ACTOR: [u8; 32] = [2u8; 32];
    const ADMIN: [u8; 32] = [1u8; 32];
    const LEADER: [u8; 32] = [0x11u8; 32];
    const GROUP: [u8; 16] = [0xAB; 16];

    fn log(index: u64) -> LogId {
        LogId::new(1, LEADER, index)
    }

    fn wire_key() -> ResourceKeyV1 {
        ResourceKeyV1 {
            namespace: "ns".into(),
            key: b"k".to_vec(),
        }
    }

    fn key() -> ResourceKey {
        ResourceKey::new("ns", b"k".to_vec()).expect("valid")
    }

    fn genesis() -> State {
        let mut s = State::default();
        s.admins.insert(ADMIN);
        s.admin_revision = 1;
        s.acl.insert((key(), ACTOR), ALL_RIGHTS);
        s.growth_bytes = s.recompute_growth();
        s.reserve_bytes = s.recompute_reserve();
        s
    }

    fn client_command(actor: [u8; 32], sequence: u64, action: ActionV1) -> CommandV1 {
        let hash = request_hash(&action).expect("hashable");
        CommandV1::Client(ClientCommandV1 {
            actor_node_id: actor.to_vec(),
            client_id: vec![9u8; 16],
            sequence,
            request_hash: hash.to_vec(),
            action,
        })
    }

    fn acquire() -> ActionV1 {
        ActionV1::Acquire(AcquireActionV1 {
            resource: wire_key(),
            ttl_ms: 30_000,
        })
    }

    /// §12.1's encoded-command cap is enforced by the production apply path, and
    /// the rejection goes **through** apply rather than around it. An earlier
    /// revision answered at the wire boundary, so a committed entry consumed no
    /// sequence, saved no result, wrote no audit row, charged no reserve and did
    /// not advance `last_applied` -- every one of the things §10.1 requires it
    /// to do atomically.
    #[test]
    fn an_oversized_command_is_rejected_through_the_apply_path() {
        use crate::coordination::contract::CompareAndSetActionV1;
        use crate::coordination::model::limits::MAX_ENCODED_COMMAND;

        let mut s = genesis();
        s.last_applied = Some(log(0));
        let reserve_before = s.reserve_bytes;

        let oversized = client_command(
            ACTOR,
            1,
            ActionV1::CompareAndSet(CompareAndSetActionV1 {
                fence: fence_wire(),
                expected_value_revision: 0,
                new_value: vec![0xA5u8; MAX_ENCODED_COMMAND + 1],
            }),
        );
        assert!(
            super::oversized(&oversized),
            "the fixture must actually exceed the cap it is testing"
        );

        let reply = apply_wire_command(&mut s, log(1), &GROUP, &oversized).expect("no fault");
        let first = match &reply {
            CommandReplyV1::Client(r) => r.clone(),
            other => panic!("expected a client reply, got {other:?}"),
        };
        // §10.1: a committed request-shape violation, the one condition that
        // carries no reason code.
        assert_eq!(first.disposition_code, 1, "Consumed");
        assert_eq!(first.outcome_code, 100, "Rejected");
        assert_eq!(first.reason_code, 0, "shapeless to the protocol");
        assert!(
            s.resources.is_empty(),
            "a rejected command mutates no business state"
        );

        // The four things the early reply skipped.
        assert_eq!(
            s.last_applied,
            Some(log(1)),
            "the entry was applied, so last_applied moves"
        );
        let session = s
            .sessions
            .get(&(ACTOR, [9u8; 16]))
            .expect("the sequence was consumed, so a session row exists");
        assert_eq!(session.last_sequence, 1);
        let saved = session
            .last_result
            .as_ref()
            .expect("the reply was saved for replay");
        assert_eq!(saved.outcome, Outcome::Rejected);
        assert_eq!(saved.reason, Reason::None);
        // §10.1: a rejected request's identity is stored **when it is valid**.
        // The oversized command names a perfectly good resource -- the encoding
        // is what is too big -- so the saved reply carries it, and a client can
        // match the reply to the request it sent.
        let snapshot = saved
            .snapshot
            .as_ref()
            .expect("the resource is valid, so it is stored");
        assert_eq!(snapshot.resource, key());

        let audited = s.audit.get(&(1, 1)).expect("an audit row for the entry");
        assert_eq!(audited.actor, Some(ACTOR));
        assert_eq!(
            audited.operation_code, 3,
            "compare_and_set, from the variant"
        );
        assert_eq!(
            audited.resource,
            Some(key()),
            "a valid identity is recorded; only a malformed one is dropped"
        );
        assert!(
            matches!(
                audited.result,
                crate::coordination::model::state::AuditResult::Client(Outcome::Rejected)
            ),
            "a client rejection, not a system code"
        );
        assert!(
            s.reserve_bytes > reserve_before,
            "the audit row is charged against the reserve like any other"
        );

        // Exact replay: the client retries the same sequence and gets the same
        // typed reply back, not a reclassification.
        let replay = apply_wire_command(&mut s, log(2), &GROUP, &oversized).expect("no fault");
        match &replay {
            CommandReplyV1::Client(r) => {
                // Verbatim, field for field -- except the redaction flag, which
                // is what tells the client this is a replay whose value bytes
                // were never stored.
                let mut expected = first.clone();
                if let Some(s) = expected.snapshot.as_mut() {
                    s.value_redacted = true;
                }
                assert_eq!(r, &expected, "a replay returns the saved reply");
                assert!(
                    r.snapshot.as_ref().unwrap().value_redacted,
                    "and marks itself a replay"
                );
                assert!(!first.snapshot.as_ref().unwrap().value_redacted);
            }
            other => panic!("expected a client reply, got {other:?}"),
        }
        assert_eq!(
            s.last_applied,
            Some(log(2)),
            "the replay is an applied entry"
        );
        assert_eq!(
            s.sessions.get(&(ACTOR, [9u8; 16])).unwrap().last_sequence,
            1,
            "a replay consumes nothing new"
        );
    }

    /// **Unknown detection precedes every shape and size classification.**
    ///
    /// Reached through the real compatibility carrier: a newer peer's action is
    /// encoded under a v2 union and decoded as `ActionV1`, which lands in
    /// `Unknown`. Its payload is deliberately larger than `max_encoded_command`,
    /// because that is the ordering bug -- an earlier revision measured the size
    /// first and turned a fatal entry into `RejectedClient { operation_code: 0 }`,
    /// a rejection this voter had no business deciding. The voters that *can*
    /// read the action would apply it, so the group diverges while every local
    /// test stays green.
    #[test]
    fn an_oversized_unknown_action_is_fatal_not_rejected() {
        use crate::coordination::model::limits::MAX_ENCODED_COMMAND;
        use fory_derive::{ForyStruct, ForyUnion};

        // Both variants carry a type this build **does** know -- the carrier can
        // only hold a payload whose type is registered locally -- under a wire
        // name and field shape matching the real `CompareAndSetActionV1`. What
        // makes the decode land in `Unknown` is the variant **id**, not the
        // payload type: id 12 is a method §5.2 does not define.
        #[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
        #[aster(wire = "aster.coordination.v1/CompareAndSetAction")]
        struct Stand {
            #[fory(id = 1)]
            fence: crate::coordination::contract::FenceV1,
            #[fory(id = 2)]
            expected_value_revision: u64,
            #[fory(id = 3, bytes)]
            new_value: Vec<u8>,
        }

        #[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
        #[aster(wire = "aster.coordination.v1/Action")]
        enum ActionV2 {
            #[fory(default)]
            #[fory(id = 3)]
            CompareAndSet(Stand),
            /// A method this build does not have.
            #[fory(id = 12)]
            Future(Stand),
            #[fory(unknown)]
            Unknown(fory_core::UnknownCase),
        }

        let encoded = crate::coordination::contract::payload_fory::<ActionV2>()
            .serialize(&ActionV2::Future(Stand {
                fence: fence_wire(),
                expected_value_revision: 0,
                new_value: vec![0xA5u8; MAX_ENCODED_COMMAND + 1],
            }))
            .expect("encode");
        let action: ActionV1 = crate::coordination::contract::payload_fory::<ActionV1>()
            .deserialize(&encoded)
            .expect("decode into the carrier");
        assert!(
            matches!(action, ActionV1::Unknown(_)),
            "the fixture must reach the real carrier, not a known variant"
        );

        let command = CommandV1::Client(ClientCommandV1 {
            actor_node_id: ACTOR.to_vec(),
            client_id: vec![9u8; 16],
            sequence: 1,
            // Unhashable, which is the point: the action cannot be hashed either.
            request_hash: vec![0u8; 32],
            action,
        });
        assert!(
            super::oversized(&command),
            "the fixture must also exceed the size cap, or it does not test the ordering"
        );

        assert_eq!(
            super::command_for_apply(&command).err(),
            Some(NarrowError::UnknownVariant),
            "an entry this voter cannot interpret is fatal, whatever its size"
        );

        let mut s = genesis();
        s.last_applied = Some(log(0));
        let before = s.clone();
        assert_eq!(
            apply_wire_command(&mut s, log(1), &GROUP, &command),
            Err(Fault::UnknownCommand)
        );
        assert_eq!(
            before, s,
            "fatal before anything is written, including last_applied"
        );
    }

    /// The same ordering for an unknown **command** variant, which the carrier
    /// reaches through `CommandV1` rather than `ActionV1`.
    #[test]
    fn an_unknown_command_variant_is_fatal_before_any_size_check() {
        let command = CommandV1::Unknown(unknown_case());
        assert_eq!(
            super::command_for_apply(&command).err(),
            Some(NarrowError::UnknownVariant)
        );
    }

    /// The delivery guard runs on a shape rejection like any other command    /// The delivery guard runs on a shape rejection like any other command: it
    /// is applied, so redelivering it is a fatal storage invariant rather than a
    /// second rejection.
    #[test]
    fn a_shape_rejection_is_guarded_against_redelivery() {
        use crate::coordination::contract::CompareAndSetActionV1;
        use crate::coordination::model::limits::MAX_ENCODED_COMMAND;

        let mut s = genesis();
        s.last_applied = Some(log(0));
        let oversized = client_command(
            ACTOR,
            1,
            ActionV1::CompareAndSet(CompareAndSetActionV1 {
                fence: fence_wire(),
                expected_value_revision: 0,
                new_value: vec![0xA5u8; MAX_ENCODED_COMMAND + 1],
            }),
        );
        apply_wire_command(&mut s, log(5), &GROUP, &oversized).expect("no fault");
        let before = s.clone();

        assert!(
            apply_wire_command(&mut s, log(5), &GROUP, &oversized).is_err(),
            "delivery at or below last_applied faults"
        );
        assert_eq!(before, s, "and faults before anything is written");
    }

    /// A malformed **action** on a well-formed prefix takes the same path as an
    /// oversized one: the sequence is real, so it is consumed and answered.
    #[test]
    fn a_malformed_action_consumes_its_sequence_and_saves_a_reply() {
        use crate::coordination::contract::AcquireActionV1;

        let mut s = genesis();
        s.last_applied = Some(log(0));

        // An over-long namespace: a known variant whose field will not validate.
        let bad = client_command(
            ACTOR,
            1,
            ActionV1::Acquire(AcquireActionV1 {
                resource: ResourceKeyV1 {
                    namespace: "n".repeat(4096),
                    key: b"k".to_vec(),
                },
                ttl_ms: 30_000,
            }),
        );
        assert!(
            !super::oversized(&bad),
            "this is a field violation, not a size one"
        );

        let reply = apply_wire_command(&mut s, log(1), &GROUP, &bad).expect("no fault");
        match &reply {
            CommandReplyV1::Client(r) => {
                assert_eq!(
                    (r.disposition_code, r.outcome_code, r.reason_code),
                    (1, 100, 0)
                );
            }
            other => panic!("expected a client reply, got {other:?}"),
        }
        assert_eq!(s.last_applied, Some(log(1)));
        assert_eq!(
            s.sessions.get(&(ACTOR, [9u8; 16])).unwrap().last_sequence,
            1
        );
        assert_eq!(
            s.audit.get(&(1, 1)).expect("audited").operation_code,
            1,
            "acquire, read from the variant that would not validate"
        );
    }

    /// A malformed **prefix** cannot create a session identity, so it takes the
    /// separate rule: `last_applied` advances and nothing else is written. An
    /// audit row would have to name an actor, and there is no validated one --
    /// a zeroed NodeId would record an action by a node that does not exist.
    #[test]
    fn a_malformed_prefix_advances_last_applied_and_writes_nothing_else() {
        let mut s = genesis();
        s.last_applied = Some(log(0));
        let before = s.clone();

        let mut bad = match client_command(ACTOR, 1, acquire()) {
            CommandV1::Client(c) => c,
            other => panic!("expected a client command, got {other:?}"),
        };
        bad.actor_node_id = vec![0u8; 31];
        let bad = CommandV1::Client(bad);

        assert!(matches!(
            super::command_for_apply(&bad),
            Ok(Command::Shapeless(_))
        ));

        let reply = apply_wire_command(&mut s, log(1), &GROUP, &bad).expect("no fault");
        match &reply {
            CommandReplyV1::Client(r) => {
                // NotConsumed: there is no session for a sequence to be
                // consumed in.
                assert_eq!(
                    (r.disposition_code, r.outcome_code, r.reason_code),
                    (2, 100, 0)
                );
            }
            other => panic!("expected a client reply, got {other:?}"),
        }
        assert_eq!(s.last_applied, Some(log(1)), "the entry was applied");
        assert_eq!(s.sessions, before.sessions, "no session row: no identity");
        assert_eq!(s.resources, before.resources, "and no business state");

        // §11.3: exactly one audit row for every committed command. A hostile
        // malformed entry is precisely the one that must not vanish from the
        // record, so it is audited with attribution marked absent rather than
        // dropped or faked.
        assert_eq!(s.audit.len(), 1);
        let audited = s.audit.get(&(1, 1)).expect("the entry is audited");
        assert_eq!(
            audited.actor, None,
            "the actor id is what would not narrow, so attribution is absent"
        );
        assert_eq!(audited.operation_code, 1, "acquire, read from the variant");
        assert_eq!(audited.resource, None, "the identity was malformed");
        assert!(
            s.reserve_bytes > before.reserve_bytes,
            "and the row is charged like any other"
        );
    }

    /// A **valid actor** with a malformed client id is still attributable. The
    /// entry cannot name a session -- the client id is half the session key --
    /// but "no session" and "no actor" are different facts, and the audit row
    /// records the one that is true.
    #[test]
    fn a_malformed_client_id_still_audits_the_actor() {
        let mut s = genesis();
        s.last_applied = Some(log(0));

        let mut bad = match client_command(ACTOR, 1, acquire()) {
            CommandV1::Client(c) => c,
            other => panic!("expected a client command, got {other:?}"),
        };
        bad.client_id = vec![9u8; 8];
        let bad = CommandV1::Client(bad);

        apply_wire_command(&mut s, log(1), &GROUP, &bad).expect("no fault");
        let audited = s.audit.get(&(1, 1)).expect("the entry is audited");
        assert_eq!(
            audited.actor,
            Some(ACTOR),
            "the actor narrowed, so the row says who it was"
        );
        assert!(
            s.sessions.is_empty(),
            "but there is still no session to key"
        );
    }

    /// Every malformed system command reports **its own** code. Sharing one
    /// generic reply told an operator a malformed expiry was a membership phase
    /// problem.
    #[test]
    fn each_malformed_system_command_reports_its_own_code() {
        use crate::coordination::contract::{
            ExpireCommandV1, MembershipProgressCommandV1, SYS_EXPIRE_MALFORMED,
            SYS_MEMBERSHIP_MALFORMED,
        };

        let mut s = genesis();
        s.last_applied = Some(log(0));

        // An expiry naming an invalid resource.
        let expire = CommandV1::Expire(ExpireCommandV1 {
            resource: ResourceKeyV1 {
                namespace: "NOT VALID".into(),
                key: b"k".to_vec(),
            },
            epoch: 1,
            lease_revision: 1,
        });
        match apply_wire_command(&mut s, log(1), &GROUP, &expire).expect("no fault") {
            CommandReplyV1::System(r) => assert_eq!(
                r.system_result_code, SYS_EXPIRE_MALFORMED,
                "an expiry problem, not a membership one"
            ),
            other => panic!("expected a system reply, got {other:?}"),
        }
        assert_eq!(
            s.audit.get(&(1, 1)).expect("audited").operation_code,
            100,
            "the audit row files it under Expire"
        );

        // A progress report with a malformed operation id.
        let progress = CommandV1::MembershipProgress(MembershipProgressCommandV1 {
            operation_id: vec![6u8; 8],
            expected_phase: 1,
            new_phase: 2,
            result_code: 0,
            observed_membership_log_id: None,
        });
        match apply_wire_command(&mut s, log(2), &GROUP, &progress).expect("no fault") {
            CommandReplyV1::System(r) => {
                assert_eq!(r.system_result_code, SYS_MEMBERSHIP_MALFORMED)
            }
            other => panic!("expected a system reply, got {other:?}"),
        }
        assert_ne!(
            SYS_EXPIRE_MALFORMED, SYS_MEMBERSHIP_MALFORMED,
            "two subsystems, two codes"
        );
        assert_eq!(s.audit.len(), 2, "one row per committed entry");
    }

    /// A hash that does not match its action is a request-shape violation, so it
    /// commits and consumes like one -- it does not halt, and it does not slip
    /// through as a valid request.
    #[test]
    fn a_mismatched_request_hash_commits_as_a_shape_violation() {
        let mut s = genesis();
        s.last_applied = Some(log(0));

        let mut bad = match client_command(ACTOR, 1, acquire()) {
            CommandV1::Client(c) => c,
            other => panic!("expected a client command, got {other:?}"),
        };
        bad.request_hash = vec![0xEEu8; 32];
        let bad = CommandV1::Client(bad);

        let reply = apply_wire_command(&mut s, log(1), &GROUP, &bad).expect("no fault");
        match &reply {
            CommandReplyV1::Client(r) => {
                assert_eq!(
                    (r.disposition_code, r.outcome_code, r.reason_code),
                    (1, 100, 0)
                );
            }
            other => panic!("expected a client reply, got {other:?}"),
        }
        assert!(
            s.resources.is_empty(),
            "the acquire it claimed to be did not happen"
        );
        assert_eq!(
            s.sessions.get(&(ACTOR, [9u8; 16])).unwrap().last_sequence,
            1,
            "the sequence was really sent, so it is really consumed"
        );
    }

    /// The classifier still comes first. A sequence conflict on a shapeless
    /// action keeps its own §5.2 reason: `(100, 0)` is reserved for requests the
    /// protocol cannot make sense of, and a stale sequence is not one.
    #[test]
    fn a_classifier_refusal_outranks_the_shape_rejection() {
        use crate::coordination::contract::CompareAndSetActionV1;
        use crate::coordination::model::limits::MAX_ENCODED_COMMAND;

        let mut s = genesis();
        s.last_applied = Some(log(0));

        // Consume sequence 1 with an ordinary command.
        apply_wire_command(&mut s, log(1), &GROUP, &client_command(ACTOR, 1, acquire()))
            .expect("no fault");

        // Now an oversized command at the **same** sequence with a different
        // request: a sequence conflict, which §5.2 has a reason code for.
        let stale = client_command(
            ACTOR,
            1,
            ActionV1::CompareAndSet(CompareAndSetActionV1 {
                fence: fence_wire(),
                expected_value_revision: 0,
                new_value: vec![0xA5u8; MAX_ENCODED_COMMAND + 1],
            }),
        );
        let reply = apply_wire_command(&mut s, log(2), &GROUP, &stale).expect("no fault");
        match &reply {
            CommandReplyV1::Client(r) => {
                assert_eq!(r.disposition_code, 2, "NotConsumed");
                assert_eq!(r.outcome_code, 100);
                assert_ne!(
                    r.reason_code, 0,
                    "a sequence conflict has a reason code and must keep it"
                );
            }
            other => panic!("expected a client reply, got {other:?}"),
        }
        assert_eq!(
            s.sessions.get(&(ACTOR, [9u8; 16])).unwrap().last_sequence,
            1,
            "a refusal consumes nothing"
        );
    }

    /// A command at the cap still applies. Without this the previous test would
    /// pass on a check that rejected everything.
    #[test]
    fn a_command_within_the_cap_still_applies() {
        use crate::coordination::contract::CompareAndSetActionV1;

        let mut s = genesis();
        // Acquire first, since a compare-and-set needs a fence from a real hold.
        apply_wire_command(&mut s, log(1), &GROUP, &client_command(ACTOR, 1, acquire()))
            .expect("no fault");
        let epoch = s.resources.get(&key()).expect("a row").epoch;

        let mut fence = fence_wire();
        fence.epoch = epoch;
        let sized = client_command(
            ACTOR,
            2,
            ActionV1::CompareAndSet(CompareAndSetActionV1 {
                fence,
                expected_value_revision: 0,
                new_value: vec![0xA5u8; 1024],
            }),
        );
        assert!(!super::oversized(&sized));
        let reply = apply_wire_command(&mut s, log(2), &GROUP, &sized).expect("no fault");
        match reply {
            CommandReplyV1::Client(r) => assert_ne!(
                r.outcome_code, 100,
                "an in-range command is not a shape violation"
            ),
            other => panic!("expected a client reply, got {other:?}"),
        }
    }

    /// The boundary the whole system runs through: a production `CommandV1`
    /// applies and produces a production `CommandReplyV1`.
    #[test]
    fn a_production_client_command_applies_and_replies() {
        let mut s = genesis();
        let reply =
            apply_wire_command(&mut s, log(1), &GROUP, &client_command(ACTOR, 1, acquire()))
                .expect("no fault");

        match reply {
            CommandReplyV1::Client(r) => {
                assert_eq!(r.disposition_code, 1, "Consumed");
                assert_eq!(r.outcome_code, 1, "Granted");
                assert_eq!(r.reason_code, 0);
                let snap = r.snapshot.expect("a snapshot");
                assert_eq!(snap.epoch, 1);
                assert!(snap.held);
                assert_eq!(snap.holder, Some(ACTOR.to_vec()));
                assert_eq!(snap.group_id, GROUP.to_vec());
                assert!(!snap.value_redacted, "a first attempt is not redacted");
            }
            other => panic!("expected a client reply, got {other:?}"),
        }
        assert!(s.resources.contains_key(&key()));
        assert!(s.counters_match_recomputation());
    }

    /// The command's reply variant matches what `CommandV1::expected_reply_variant`
    /// declares, for every shape -- so OpenRaft's `R` is never the wrong kind.
    #[test]
    fn every_command_produces_its_declared_reply_variant() {
        let mut s = genesis();
        let mut index = 0u64;
        let cases: Vec<CommandV1> = vec![
            client_command(ACTOR, 1, acquire()),
            CommandV1::Expire(ExpireCommandV1 {
                resource: wire_key(),
                epoch: 1,
                lease_revision: 1,
            }),
            CommandV1::MembershipProgress(MembershipProgressCommandV1 {
                operation_id: vec![1u8; 16],
                expected_phase: 1,
                new_phase: 2,
                observed_membership_log_id: None,
                result_code: 3,
            }),
            CommandV1::RetireClient(RetireClientCommandV1 {
                actor_node_id: ACTOR.to_vec(),
                target_node_id: ACTOR.to_vec(),
                target_client_id: vec![9u8; 16],
            }),
        ];

        for command in cases {
            index += 1;
            let reply = apply_wire_command(&mut s, log(index), &GROUP, &command).expect("no fault");
            assert_eq!(
                reply.variant_id(),
                command.expected_reply_variant(),
                "{command:?} produced the wrong reply variant"
            );
        }
        assert!(s.counters_match_recomputation());
    }

    /// Every action variant narrows, so no production action is unreachable
    /// through the boundary.
    #[test]
    fn every_action_variant_narrows() {
        let node = NodeIdV1 {
            node_id: vec![3u8; 32],
        };
        let actions: Vec<ActionV1> = vec![
            acquire(),
            ActionV1::Renew(crate::coordination::contract::RenewActionV1 {
                fence: fence_wire(),
            }),
            ActionV1::CompareAndSet(crate::coordination::contract::CompareAndSetActionV1 {
                fence: fence_wire(),
                expected_value_revision: 0,
                new_value: b"v".to_vec(),
            }),
            ActionV1::Release(crate::coordination::contract::ReleaseActionV1 {
                fence: fence_wire(),
            }),
            ActionV1::ReleaseWithValue(crate::coordination::contract::ReleaseWithValueActionV1 {
                fence: fence_wire(),
                expected_value_revision: 0,
                final_value: b"v".to_vec(),
            }),
            ActionV1::Revoke(crate::coordination::contract::RevokeActionV1 {
                resource: wire_key(),
                expected_epoch: Some(1),
            }),
            ActionV1::SetGrant(crate::coordination::contract::SetGrantActionV1 {
                resource: wire_key(),
                target_node_id: vec![4u8; 32],
                rights: 1,
                expected_acl_revision: 0,
            }),
            ActionV1::SetAdmin(SetAdminActionV1 {
                target_node_id: vec![5u8; 32],
                enabled: true,
                expected_admin_revision: 1,
            }),
            ActionV1::ChangeMembership(crate::coordination::contract::ChangeMembershipActionV1 {
                operation_id: vec![6u8; 16],
                expected_membership_log_id: None,
                desired_voters: vec![node.clone()],
                retained_learners: vec![node.clone()],
                member_seeds: vec![MemberSeedV1 {
                    node_id: vec![7u8; 32],
                    ticket: "aster1abc".into(),
                    signed_join_permit: Some(vec![1, 2, 3]),
                }],
            }),
            ActionV1::CancelMembership(crate::coordination::contract::CancelMembershipActionV1 {
                operation_id: vec![6u8; 16],
                expected_phase_code: 1,
            }),
        ];

        assert_eq!(actions.len(), 10, "§5.2 declares ten actions");
        for action in &actions {
            assert!(
                action_from_wire(action).is_ok(),
                "{action:?} does not narrow"
            );
        }
    }

    fn fence_wire() -> crate::coordination::contract::FenceV1 {
        crate::coordination::contract::FenceV1 {
            group_id: GROUP.to_vec(),
            resource: wire_key(),
            epoch: 1,
            holder: ACTOR.to_vec(),
        }
    }

    /// A membership plan survives the boundary whole, so a failover controller
    /// can re-run it.
    #[test]
    fn a_membership_plan_survives_the_boundary_whole() {
        let action =
            ActionV1::ChangeMembership(crate::coordination::contract::ChangeMembershipActionV1 {
                operation_id: vec![6u8; 16],
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
            });

        let Ok(Action::ChangeMembership(narrowed)) = action_from_wire(&action) else {
            panic!("expected a change-membership action");
        };
        assert_eq!(narrowed.plan.desired_voters.len(), 3);
        assert_eq!(narrowed.plan.retained_learners, vec![[4u8; 32]]);
        assert_eq!(narrowed.plan.member_seeds.len(), 1);
        assert_eq!(narrowed.plan.member_seeds[0].node_id, [5u8; 32]);
        assert_eq!(narrowed.plan.member_seeds[0].ticket, b"aster1seed");
        assert_eq!(
            narrowed.plan.member_seeds[0].join_permit,
            Some(vec![9, 9, 9])
        );
    }

    /// A **variant** this version does not know is fatal -- §5.3.1. The voter
    /// cannot know what the command *is*, so no deterministic rejection exists.
    #[test]
    fn an_unknown_command_variant_is_fatal() {
        let mut s = genesis();
        assert_eq!(
            apply_wire_command(&mut s, log(1), &GROUP, &CommandV1::Unknown(unknown_case())),
            Err(Fault::UnknownCommand)
        );
        assert!(s.audit.is_empty(), "nothing was written");
        assert_eq!(s.last_applied, None);
    }

    /// A **known** command with an ill-formed field is a deterministic
    /// rejection, not a halt (§10.1). Every voter knows exactly what it was
    /// meant to be and refuses it identically; halting would let a malformed
    /// field become an availability kill switch.
    #[test]
    fn a_known_command_with_a_malformed_field_is_rejected_not_fatal() {
        let mut s = genesis();

        let malformed = CommandV1::RetireClient(RetireClientCommandV1 {
            actor_node_id: vec![1u8; 8],
            target_node_id: ACTOR.to_vec(),
            target_client_id: vec![9u8; 16],
        });
        let reply = apply_wire_command(&mut s, log(1), &GROUP, &malformed)
            .expect("a known command is never fatal");
        match reply {
            CommandReplyV1::Retirement(r) => assert_eq!(r.outcome_code, 100),
            other => panic!("expected a retirement reply, got {other:?}"),
        }
    }

    /// A client command whose `request_hash` is not the canonical hash of its
    /// own action is refused. Without this the hash binds nothing: a client
    /// could present one action under another's identity and slip a different
    /// mutation past the classifier.
    #[test]
    fn a_request_hash_that_does_not_match_its_action_is_refused() {
        let mut s = genesis();
        let mut command = client_command(ACTOR, 1, acquire());
        let CommandV1::Client(ref mut c) = command else {
            panic!("a client command")
        };
        c.request_hash = vec![0xFF; 32];

        let reply = apply_wire_command(&mut s, log(1), &GROUP, &command)
            .expect("a known command is never fatal");
        match reply {
            CommandReplyV1::Client(r) => {
                assert_eq!(r.outcome_code, 100);
                assert_eq!(r.disposition_code, 1, "it committed, so it is consumed");
            }
            other => panic!("expected a client reply, got {other:?}"),
        }
        assert!(
            s.resources.is_empty(),
            "a mismatched hash must not apply its action"
        );
    }

    /// And the matching hash is accepted, so the check is on the binding rather
    /// than on the field being present.
    #[test]
    fn a_matching_request_hash_is_accepted() {
        let mut s = genesis();
        let reply =
            apply_wire_command(&mut s, log(1), &GROUP, &client_command(ACTOR, 1, acquire()))
                .expect("no fault");
        match reply {
            CommandReplyV1::Client(r) => assert_eq!(r.outcome_code, 1),
            other => panic!("expected a client reply, got {other:?}"),
        }
    }

    /// The forward-compat carrier, reached by decoding a newer peer's command.
    fn unknown_case() -> fory_core::UnknownCase {
        use crate::coordination::contract::payload_fory;
        use fory_derive::{ForyStruct, ForyUnion};

        #[derive(ForyStruct, aster_macros::AsterType, Debug, Default, Clone, PartialEq)]
        #[aster(wire = "aster.coordination.v1/ExpireCommand")]
        struct Stand {
            #[fory(id = 1)]
            resource: ResourceKeyV1,
            #[fory(id = 2)]
            epoch: u64,
            #[fory(id = 3)]
            lease_revision: u64,
        }

        #[derive(ForyUnion, aster_macros::AsterType, Debug, Clone, PartialEq)]
        #[aster(wire = "aster.coordination.v1/Command")]
        enum CommandV2 {
            #[fory(default)]
            #[fory(id = 2)]
            Expire(Stand),
            #[fory(id = 9)]
            Future(Stand),
            #[fory(unknown)]
            Unknown(fory_core::UnknownCase),
        }

        let encoded = payload_fory::<CommandV2>()
            .serialize(&CommandV2::Future(Stand {
                resource: wire_key(),
                epoch: 1,
                lease_revision: 1,
            }))
            .expect("encode");
        let decoded: CommandV1 = payload_fory::<CommandV1>()
            .deserialize(&encoded)
            .expect("decode");
        match decoded {
            CommandV1::Unknown(u) => u,
            other => panic!("expected the carrier, got {other:?}"),
        }
    }

    /// A replay through the boundary redacts the value while reproducing every
    /// other field, including the ones the model would otherwise have had to
    /// re-read from state.
    #[test]
    fn a_replay_through_the_boundary_is_exact_and_redacted() {
        let mut s = genesis();
        let command = client_command(ACTOR, 1, acquire());

        let CommandReplyV1::Client(first) =
            apply_wire_command(&mut s, log(1), &GROUP, &command).expect("no fault")
        else {
            panic!("expected a client reply");
        };

        // Mutate the resource underneath: a different holder and a new epoch.
        let row = s.resources.get_mut(&key()).expect("the row");
        row.holder = Some([0xEE; 32]);
        row.epoch = 99;
        row.ttl_ms = 1;
        row.lease_revision = 77;

        let CommandReplyV1::Client(replayed) =
            apply_wire_command(&mut s, log(2), &GROUP, &command).expect("no fault")
        else {
            panic!("expected a client reply");
        };

        let (a, b) = (
            first.snapshot.expect("first"),
            replayed.snapshot.expect("replay"),
        );
        assert_eq!(b.holder, a.holder, "the saved holder, not the current one");
        assert_eq!(b.epoch, a.epoch);
        assert_eq!(b.ttl_ms, a.ttl_ms);
        assert_eq!(b.lease_revision, a.lease_revision);
        assert_eq!(b.value, None);
        assert!(b.value_redacted);
        assert!(!a.value_redacted);
    }

    // ── Value confidentiality ───────────────────────────────────────────────

    /// **An actor without `READ` never receives the value**, even on a command
    /// it is fully authorized to run.
    ///
    /// §8.1 grants bind one exact `(group, resource, subject)` and imply nothing
    /// else, so `ACQUIRE` is permission to take a lease -- not to read what the
    /// lease guards. Returning the value on every success let an actor with
    /// `ACQUIRE` alone acquire and receive the existing secret without ever
    /// calling `read_linearizable`, bypassing the ACL check reads exist for.
    #[test]
    fn an_actor_without_read_never_receives_the_value() {
        use crate::coordination::model::limits::{RIGHT_ACQUIRE, RIGHT_READ, RIGHT_RELEASE};

        let mut s = State::default();
        s.admins.insert(ADMIN);
        s.admin_revision = 1;
        // Everything except READ.
        s.acl.insert((key(), ACTOR), RIGHT_ACQUIRE | RIGHT_RELEASE);
        // A resource that already holds a secret.
        s.resources.insert(
            key(),
            crate::coordination::model::state::ResourceRow {
                value: b"the-existing-secret".to_vec(),
                ..Default::default()
            },
        );
        s.growth_bytes = s.recompute_growth();
        s.reserve_bytes = s.recompute_reserve();

        let CommandReplyV1::Client(reply) =
            apply_wire_command(&mut s, log(1), &GROUP, &client_command(ACTOR, 1, acquire()))
                .expect("no fault")
        else {
            panic!("expected a client reply");
        };

        assert_eq!(reply.outcome_code, 1, "the acquire itself is authorized");
        let snap = reply.snapshot.expect("a snapshot");
        assert_eq!(
            snap.value, None,
            "an actor without READ must not receive the value"
        );
        assert!(
            !snap.value_redacted,
            "absent for authorization is not the same as redacted for a replay: \
             `value_redacted` would tell the client to retry for something it is \
             not entitled to"
        );
        // The value is still there -- it was withheld, not erased.
        assert_eq!(
            s.resources.get(&key()).unwrap().value,
            b"the-existing-secret"
        );
        let _ = RIGHT_READ;
    }

    /// The same command, with `READ`, does return it -- so the check is on the
    /// right, not on the command.
    #[test]
    fn an_actor_with_read_receives_the_value() {
        use crate::coordination::model::limits::{RIGHT_ACQUIRE, RIGHT_READ};

        let mut s = State::default();
        s.acl.insert((key(), ACTOR), RIGHT_ACQUIRE | RIGHT_READ);
        s.resources.insert(
            key(),
            crate::coordination::model::state::ResourceRow {
                value: b"visible".to_vec(),
                ..Default::default()
            },
        );
        s.growth_bytes = s.recompute_growth();

        let CommandReplyV1::Client(reply) =
            apply_wire_command(&mut s, log(1), &GROUP, &client_command(ACTOR, 1, acquire()))
                .expect("no fault")
        else {
            panic!("expected a client reply");
        };
        let snap = reply.snapshot.expect("a snapshot");
        assert_eq!(snap.value, Some(b"visible".to_vec()));
        assert!(!snap.value_redacted);
    }

    /// Losing `READ` between the first attempt and a replay changes nothing:
    /// a replay carries no value either way, so the two paths cannot be played
    /// against each other.
    #[test]
    fn a_replay_carries_no_value_whatever_the_actors_rights() {
        use crate::coordination::model::limits::ALL_RIGHTS;

        let mut s = State::default();
        s.acl.insert((key(), ACTOR), ALL_RIGHTS);
        s.resources.insert(
            key(),
            crate::coordination::model::state::ResourceRow {
                value: b"secret".to_vec(),
                ..Default::default()
            },
        );
        s.growth_bytes = s.recompute_growth();

        let command = client_command(ACTOR, 1, acquire());
        apply_wire_command(&mut s, log(1), &GROUP, &command).expect("no fault");

        let CommandReplyV1::Client(replayed) =
            apply_wire_command(&mut s, log(2), &GROUP, &command).expect("no fault")
        else {
            panic!("expected a client reply");
        };
        let snap = replayed.snapshot.expect("a snapshot");
        assert_eq!(snap.value, None);
        assert!(
            snap.value_redacted,
            "a replay is redacted, not unauthorized"
        );
    }

    /// A cancellation reports `10 MembershipCancelled` through the boundary.
    #[test]
    fn a_cancellation_reports_membership_cancelled_on_the_wire() {
        let mut s = genesis();

        let accept = client_command(
            ADMIN,
            1,
            ActionV1::ChangeMembership(crate::coordination::contract::ChangeMembershipActionV1 {
                operation_id: vec![6u8; 16],
                expected_membership_log_id: None,
                // §9: exactly three or five voters.
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
                retained_learners: Vec::new(),
                // §9: every new node is seeded, and a new node's seed carries a
                // join permit. All three are new against an empty committed
                // membership.
                member_seeds: (1u8..=3)
                    .map(|b| MemberSeedV1 {
                        node_id: vec![b; 32],
                        ticket: "aster1ticket".into(),
                        signed_join_permit: Some(vec![0xEEu8; 64]),
                    })
                    .collect(),
            }),
        );
        apply_wire_command(&mut s, log(1), &GROUP, &accept).expect("no fault");

        let cancel = client_command(
            ADMIN,
            2,
            ActionV1::CancelMembership(crate::coordination::contract::CancelMembershipActionV1 {
                operation_id: vec![6u8; 16],
                expected_phase_code: 1,
            }),
        );
        let CommandReplyV1::Client(reply) =
            apply_wire_command(&mut s, log(2), &GROUP, &cancel).expect("no fault")
        else {
            panic!("expected a client reply");
        };

        assert_eq!(
            reply.outcome_code, 10,
            "a cancellation is `10 MembershipCancelled`, not `8 MembershipAccepted`"
        );
        assert_eq!(reply.operation_id, Some(vec![6u8; 16]));
    }
}
