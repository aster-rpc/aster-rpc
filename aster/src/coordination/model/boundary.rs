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
use super::apply::{apply_command, Action, ClientRef, ClientResult, Command, CommandReply, Fault};
use super::expire::ExpireCommand;
use super::grant::GrantAction;
use super::membership::{
    CancelMembershipAction, ChangeMembershipAction, MembershipProgressCommand,
};
use super::project::{client_reply, retirement_reply, system_reply, Provenance};
use super::resource::{Fence, RawResourceKey, ResourceAction};
use super::retire::RetireCommand;
use super::state::{LogId, MemberSeed, MembershipPlan, NodeId, State};

/// Narrow a wire `Vec<u8>` to a 32-byte node id.
fn node_id(bytes: &[u8]) -> Option<NodeId> {
    bytes.try_into().ok()
}

fn client_id(bytes: &[u8]) -> Option<[u8; 16]> {
    bytes.try_into().ok()
}

fn hash(bytes: &[u8]) -> Option<[u8; 32]> {
    bytes.try_into().ok()
}

fn raw_key(k: &crate::coordination::contract::ResourceKeyV1) -> RawResourceKey {
    RawResourceKey::new(k.namespace.clone(), k.key.clone())
}

fn fence(f: &crate::coordination::contract::FenceV1) -> Option<Fence> {
    Some(Fence {
        resource: raw_key(&f.resource),
        epoch: f.epoch,
        holder: node_id(&f.holder)?,
    })
}

fn log_id(id: &crate::coordination::contract::LogIdV1) -> Option<LogId> {
    LogId::try_from(id).ok()
}

/// Narrow a wire action to a model action.
///
/// `None` means this voter cannot interpret it -- an ill-formed identity, or the
/// `Unknown` carrier, which §5.2.1 says is never proposable or appliable.
pub fn action_from_wire(action: &ActionV1) -> Option<Action> {
    Some(match action {
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
                    .collect::<Option<Vec<_>>>()?,
                retained_learners: a
                    .retained_learners
                    .iter()
                    .map(|n| node_id(&n.node_id))
                    .collect::<Option<Vec<_>>>()?,
                member_seeds: a
                    .member_seeds
                    .iter()
                    .map(|s| {
                        Some(MemberSeed {
                            node_id: node_id(&s.node_id)?,
                            ticket: s.ticket.clone().into_bytes(),
                            join_permit: s.signed_join_permit.clone(),
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
            },
        }),
        ActionV1::CancelMembership(a) => Action::CancelMembership(CancelMembershipAction {
            operation_id: client_id(&a.operation_id)?,
            expected_phase_code: a.expected_phase_code,
        }),
        // §5.2.1: never hashable, never proposable, never appliable.
        ActionV1::Unknown(_) => return None,
    })
}

/// Narrow a wire command to a model command.
pub fn command_from_wire(command: &CommandV1) -> Option<Command> {
    Some(match command {
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
            action: action_from_wire(action)?,
        },
        CommandV1::Expire(ExpireCommandV1 {
            resource,
            epoch,
            lease_revision,
        }) => Command::Expire(ExpireCommand {
            resource: raw_key(resource).validate().ok()?,
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
    let Some(model) = command_from_wire(command) else {
        return Err(Fault::UnknownCommand);
    };

    // A first attempt reports the value it wrote; the value lives in state, not
    // in the saved reply, so it is read here and only here.
    let reply = apply_command(state, log_id, &model)?;
    Ok(to_wire(state, group_id, &model, reply))
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

/// Project a client result, supplying the live value only for a first attempt.
fn client_reply_for(
    state: &State,
    group_id: &[u8],
    command: &Command,
    result: &ClientResult,
) -> crate::coordination::contract::ClientReplyV1 {
    // A replay is answered from the session table; its value was never saved.
    let provenance = if result.replayed {
        Provenance::Replay
    } else {
        Provenance::FirstAttempt
    };

    let value = match provenance {
        Provenance::Replay => None,
        Provenance::FirstAttempt => result
            .reply
            .resource()
            .and_then(|key| state.resources.get(key))
            .map(|row| row.value.clone()),
    };
    let _ = command;

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
                action_from_wire(action).is_some(),
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

        let Some(Action::ChangeMembership(narrowed)) = action_from_wire(&action) else {
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

    /// A command this voter cannot narrow is fatal, not skipped -- §5.3.1.
    #[test]
    fn an_uninterpretable_command_is_fatal() {
        let mut s = genesis();

        // An ill-formed node id: a real `CommandV1`, but not one this voter can
        // apply.
        let malformed = CommandV1::RetireClient(RetireClientCommandV1 {
            actor_node_id: vec![1u8; 8],
            target_node_id: ACTOR.to_vec(),
            target_client_id: vec![9u8; 16],
        });
        assert_eq!(
            apply_wire_command(&mut s, log(1), &GROUP, &malformed),
            Err(Fault::UnknownCommand)
        );
        assert!(s.audit.is_empty(), "nothing was written");
        assert_eq!(s.last_applied, None);
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
                desired_voters: vec![NodeIdV1 {
                    node_id: vec![1u8; 32],
                }],
                retained_learners: Vec::new(),
                member_seeds: Vec::new(),
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
