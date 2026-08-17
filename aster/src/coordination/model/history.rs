//! Deterministic generated-history tests over the whole model.
//!
//! ## Why generated rather than hand-picked
//!
//! Hand-picked cases are exactly what let the spike's preflight and apply paths
//! diverge for sixteen review rounds: every case someone thought to write
//! passed, and the two states nobody thought of were the ones that disagreed.
//! These histories walk a large, mechanically enumerated space instead.
//!
//! ## Why not `proptest`
//!
//! A replicated state machine's tests must be reproducible on a reviewer's
//! machine and in CI without a seed to chase. These generators are deterministic
//! by construction: a fixed command alphabet driven by a counter, so the same
//! source produces the same history every run. There is no RNG to seed and no
//! shrinking to interpret -- a failure names the exact step.
//!
//! ## The invariants asserted after **every** applied step
//!
//! * both byte counters equal a full recomputation;
//! * `last_applied` advances and never regresses;
//! * `audit_tip` stays consistent with `last_applied`;
//! * no saved rejection carries `Reason::None` unless it is the §10.1 forced
//!   shape case;
//! * no reserved reason is ever emitted.
//!
//! And over a whole history: replaying the same initial state and the same
//! ordered commands produces byte-identical state, replies and audit rows.

#![cfg(test)]

use crate::coordination::reason::{Disposition, Outcome, Reason};

use super::admin::AdminAction;
use super::apply::{
    apply_command, Action, ClientRef, Command, CommandReply, OPERATION_EXPIRE,
    OPERATION_MEMBERSHIP_PROGRESS, OPERATION_RETIRE_CLIENT,
};
use super::expire::ExpireCommand;
use super::grant::GrantAction;
use super::limits::{ALL_RIGHTS, RIGHT_ACQUIRE, RIGHT_MUTATE, RIGHT_READ, RIGHT_RELEASE};
use super::membership::{
    CancelMembershipAction, ChangeMembershipAction, MembershipProgressCommand,
    PHASE_ADDING_LEARNERS, PHASE_CHANGING_VOTERS, PHASE_COMPLETED, PHASE_PENDING,
};
use super::resource::{Fence, RawResourceKey, ResourceAction};
use super::retire::RetireCommand;
use super::state::{AuditResult, LogId, MemberSeed, MembershipPlan, NodeId, ResourceKey, State};

const ADMIN: NodeId = [1u8; 32];
const ALICE: NodeId = [2u8; 32];
const BOB: NodeId = [3u8; 32];
const LEADER: NodeId = [0x11u8; 32];

fn raw(n: u8) -> RawResourceKey {
    RawResourceKey::new("ns", vec![n])
}

fn key(n: u8) -> ResourceKey {
    raw(n).validate().expect("valid")
}

/// A group with one administrator and rights granted to two actors on three
/// resources -- enough for holder, non-holder and revocation paths to be
/// reachable without every command being refused for authorization.
fn genesis() -> State {
    let mut s = State::default();
    s.admins.insert(ADMIN);
    s.admin_revision = 1;
    // A group that is already running: §12.1 allows at most two new voters per
    // change, so a history starting from an empty membership could not accept
    // the three-voter plans it generates.
    s.authority = crate::coordination::model::authority::testing::authority();
    s.deployment_profile_code = crate::coordination::model::shape::PROFILE_PROD;
    s.members = (1u8..=3).map(|b| [b; 32]).collect();
    for n in 0..3u8 {
        for actor in [ALICE, BOB] {
            s.acl.insert((key(n), actor), ALL_RIGHTS);
        }
    }
    s.growth_bytes = s.recompute_growth();
    s.reserve_bytes = s.recompute_reserve();
    s
}

/// One step of a generated history: a command and the client that issued it.
struct Step {
    command: Command,
    /// The actor whose sequence advances, if any.
    sequencer: Option<(NodeId, [u8; 16])>,
}

/// The command alphabet, indexed by a step counter.
///
/// Deliberately covers every command shape the model implements: all six
/// resource actions, `set_grant`, `set_admin`, both membership actions, system
/// `Expire`, `MembershipProgress`, and `retire_client`.
#[allow(clippy::too_many_lines)]
fn step(state: &State, n: u64) -> Step {
    let actor = if n.is_multiple_of(2) { ALICE } else { BOB };
    let client_id = [(n % 3) as u8; 16];
    let resource = (n % 3) as u8;
    let row = state.resources.get(&key(resource));
    let epoch = row.map_or(0, |r| r.epoch);
    let value_revision = row.map_or(0, |r| r.value_revision);
    let lease_revision = row.map_or(0, |r| r.lease_revision);
    let holder = row.and_then(|r| r.holder).unwrap_or(actor);

    let sequence = state
        .sessions
        .get(&(actor, client_id))
        .map_or(1, |s| s.last_sequence + 1);
    let client = ClientRef {
        actor,
        client_id,
        sequence,
        request_hash: [(n % 251) as u8; 32],
    };
    let sequencer = Some((actor, client_id));

    let fence = Fence {
        resource: raw(resource),
        epoch,
        holder,
    };

    // 13 shapes, so the cycle length is coprime with the 2-actor and 3-resource
    // cycles above and every combination is eventually reached.
    let action = match n % 13 {
        0 => Action::Resource(ResourceAction::Acquire {
            resource: raw(resource),
            ttl_ms: 30_000,
        }),
        1 => Action::Resource(ResourceAction::Renew { fence }),
        2 => Action::Resource(ResourceAction::CompareAndSet {
            fence,
            expected_value_revision: value_revision,
            new_value: vec![n as u8; (n % 11) as usize],
        }),
        3 => Action::Resource(ResourceAction::Release { fence }),
        4 => Action::Resource(ResourceAction::ReleaseWithValue {
            fence,
            expected_value_revision: value_revision,
            final_value: vec![n as u8; (n % 5) as usize],
        }),
        5 => Action::Resource(ResourceAction::Revoke {
            resource: raw(resource),
            expected_epoch: if n.is_multiple_of(4) {
                Some(epoch)
            } else {
                None
            },
        }),
        6 => Action::Grant(GrantAction {
            resource: raw(resource),
            subject: if n.is_multiple_of(3) { ALICE } else { BOB },
            rights: match n % 5 {
                0 => 0,
                1 => RIGHT_READ,
                2 => RIGHT_READ | RIGHT_ACQUIRE,
                3 => ALL_RIGHTS & !RIGHT_MUTATE,
                _ => ALL_RIGHTS & !RIGHT_RELEASE,
            },
            expected_acl_revision: row.map_or(0, |r| r.acl_revision),
        }),
        7 => Action::Admin(AdminAction {
            target: if n.is_multiple_of(4) { ALICE } else { BOB },
            enabled: n % 8 < 4,
            expected_admin_revision: state.admin_revision,
        }),
        8 => Action::ChangeMembership(ChangeMembershipAction {
            operation_id: [(n % 7) as u8; 16],
            expected_membership_log_id: state.membership_log_id,
            plan: MembershipPlan {
                desired_voters: vec![[1u8; 32], [2u8; 32], [3u8; 32]],
                retained_learners: Vec::new(),
                // §9: every new node is seeded, sorted by node id, and a new
                // node's seed carries a join permit. The generator's plan never
                // matches the committed membership, so all three are new.
                // The generator's plan is always the committed membership, so
                // no seed is a join and none carries a permit.
                member_seeds: (1u8..=3)
                    .map(|b| MemberSeed {
                        node_id: [b; 32],
                        ticket: crate::coordination::model::membership::test_ticket(&[b; 32]),
                        join_permit: None,
                    })
                    .collect(),
            },
        }),
        9 => Action::CancelMembership(CancelMembershipAction {
            operation_id: [(n % 7) as u8; 16],
            // Cycles through the phases an intent can be in, so both the
            // immediate-cancel and the refuse-mid-flight paths are reached.
            expected_phase_code: match n % 3 {
                0 => PHASE_PENDING,
                1 => PHASE_ADDING_LEARNERS,
                _ => PHASE_CHANGING_VOTERS,
            },
        }),
        10 => {
            return Step {
                command: Command::Expire(ExpireCommand {
                    resource: key(resource),
                    epoch,
                    lease_revision,
                }),
                sequencer: None,
            }
        }
        11 => {
            return Step {
                command: Command::MembershipProgress(MembershipProgressCommand {
                    operation_id: [(n % 7) as u8; 16],
                    expected_phase: PHASE_PENDING,
                    new_phase: PHASE_ADDING_LEARNERS,
                    observed_membership_log_id: Some(LogId::new(1, LEADER, n + 1)),
                    result_code: crate::coordination::contract::SYS_MEMBERSHIP_ADVANCED,
                }),
                sequencer: None,
            }
        }
        _ => {
            return Step {
                command: Command::RetireClient(RetireCommand {
                    actor,
                    target_actor: actor,
                    // A different client id from the sequenced ones most of the
                    // time, so retirement does not simply close every session.
                    target_client_id: [(n % 5 + 3) as u8; 16],
                }),
                sequencer: None,
            };
        }
    };

    Step {
        command: Command::Client { client, action },
        sequencer,
    }
}

/// Assert every per-step invariant.
fn assert_invariants(state: &State, previous: Option<LogId>, at: u64) {
    assert!(
        state.counters_match_recomputation(),
        "counters drifted at step {at}: growth {} vs {}, reserve {} vs {}",
        state.growth_bytes,
        state.recompute_growth(),
        state.reserve_bytes,
        state.recompute_reserve()
    );

    let last = state.last_applied.expect("an applied entry");
    if let Some(previous) = previous {
        assert!(
            last.index > previous.index,
            "last_applied regressed at step {at}"
        );
    }

    if let Some(tip) = state.audit_tip {
        assert!(
            tip.index < last.index || tip == last,
            "the audit tip ran ahead of last_applied at step {at}"
        );
    }

    for session in state.sessions.values() {
        if let Some(saved) = &session.last_result {
            assert!(
                !saved.reason.is_reserved(),
                "a reserved reason was saved at step {at}"
            );
            if saved.outcome != Outcome::Rejected {
                assert_eq!(
                    saved.reason,
                    Reason::None,
                    "a successful outcome carried a reason at step {at}"
                );
            }
        }
    }
}

/// Run a history and return the final state plus every reply and audit row.
fn run(steps: u64) -> (State, Vec<String>) {
    let mut state = genesis();
    let mut trace = Vec::new();
    let mut previous = None;

    for n in 0..steps {
        let step = step(&state, n);
        let log_id = LogId::new(1, LEADER, n + 1);
        let reply = apply_command(&mut state, log_id, &step.command).expect("no fault");
        trace.push(render(&reply));
        assert_invariants(&state, previous, n);
        previous = state.last_applied;
        let _ = step.sequencer;
    }

    for ((term, index), row) in &state.audit {
        trace.push(format!(
            "audit {term}/{index} op={} result={:?} epoch={} vrev={} leader={:02x}",
            row.operation_code, row.result, row.epoch, row.value_revision, row.leader_node_id[0]
        ));
    }
    (state, trace)
}

fn render(reply: &CommandReply) -> String {
    match reply {
        CommandReply::Client(r) => format!(
            "client d={:?} o={:?} r={:?} epoch={} vrev={} acl={} admin={}",
            r.disposition,
            r.reply.outcome,
            r.reply.reason,
            r.reply.snapshot.as_ref().map_or(0, |s| s.epoch),
            r.reply.snapshot.as_ref().map_or(0, |s| s.value_revision),
            r.reply.acl_revision,
            r.reply.admin_revision
        ),
        CommandReply::System(r) => format!(
            "system code={} lease={}",
            r.system_result_code, r.lease_revision
        ),
        CommandReply::Retirement(r) => format!("retire o={:?} r={:?}", r.outcome, r.reason),
    }
}

// ── The properties ──────────────────────────────────────────────────────────

/// The headline property: the same initial state and the same ordered history
/// produce byte-identical state, replies and audit rows.
///
/// This is what every voter's agreement reduces to. If it held only *usually*,
/// two replicas applying the same log would diverge on the first difference.
#[test]
fn replaying_one_history_reproduces_state_replies_and_audit_exactly() {
    let (first_state, first_trace) = run(400);
    let (second_state, second_trace) = run(400);

    assert_eq!(first_trace, second_trace, "replies or audit rows differed");
    assert_eq!(first_state, second_state, "final state differed");
}

/// Counters equal a full recomputation after every applied step -- checked
/// inside `run`, and restated here so a failure names the property rather than
/// a helper.
#[test]
fn counters_equal_recomputation_after_every_step_of_a_long_history() {
    let (state, _) = run(600);
    assert!(state.counters_match_recomputation());
    assert!(
        state.growth_bytes > 0 && state.reserve_bytes > 0,
        "a history that wrote nothing would satisfy this vacuously"
    );
}

/// The history actually exercises what it claims to. A generator that refused
/// every command would pass every invariant above while proving nothing, so the
/// coverage is asserted rather than assumed.
#[test]
fn the_generated_history_reaches_every_command_shape_and_both_dispositions() {
    let (state, _) = run(400);

    let mut operations: Vec<i32> = state.audit.values().map(|r| r.operation_code).collect();
    operations.sort_unstable();
    operations.dedup();

    for expected in [
        1,
        2,
        3,
        4,
        5,
        6,
        7,
        8,
        9,
        11, // the ten mutation methods
        OPERATION_RETIRE_CLIENT,
        OPERATION_EXPIRE,
        OPERATION_MEMBERSHIP_PROGRESS,
    ] {
        assert!(
            operations.contains(&expected),
            "operation {expected} never appeared; the generator does not cover it"
        );
    }

    // Both dispositions, and both successes and refusals.
    let outcomes: Vec<AuditResult> = state.audit.values().map(|r| r.result).collect();
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, AuditResult::Client(c) if *c != Outcome::Rejected)),
        "no client command ever succeeded"
    );
    assert!(
        outcomes.contains(&AuditResult::Client(Outcome::Rejected)),
        "no client command was ever rejected"
    );
    assert!(
        outcomes.iter().any(|o| matches!(o, AuditResult::System(_))),
        "no system command was ever audited"
    );
}

/// Every saved rejection carries a defined coordination reason, except the one
/// §10.1 case -- checked over a whole history rather than at hand-picked sites.
#[test]
fn no_saved_rejection_loses_its_reason_over_a_long_history() {
    let (state, _) = run(600);

    let mut saw_rejection = false;
    for session in state.sessions.values() {
        let Some(saved) = &session.last_result else {
            continue;
        };
        if saved.outcome != Outcome::Rejected {
            continue;
        }
        saw_rejection = true;
        // The generator issues no shape-violating command, so every rejection
        // here must name a cause.
        assert_ne!(
            saved.reason,
            Reason::None,
            "a rejection lost its reason: {saved:?}"
        );
    }
    assert!(
        saw_rejection,
        "no rejection was saved; the check is vacuous"
    );
}

/// Replaying a consumed sequence returns its saved result without re-applying,
/// at every point in a history rather than once.
#[test]
fn every_consumed_sequence_replays_identically() {
    let mut state = genesis();
    let mut replays = 0;
    // Strictly monotonic across both the original and its duplicate: a
    // committed duplicate occupies its own log position, so rewinding the index
    // would be modelling something the log cannot do.
    let mut index = 0u64;

    for n in 0..200u64 {
        let step = step(&state, n);
        index += 1;
        let reply = apply_command(&mut state, LogId::new(1, LEADER, index), &step.command)
            .expect("no fault");

        // Re-issue the same client command at the next index and require the
        // identical answer.
        if let (Command::Client { client, action }, CommandReply::Client(first)) =
            (&step.command, &reply)
        {
            if first.disposition == Disposition::Consumed {
                let business = (
                    state.resources.clone(),
                    state.acl.clone(),
                    state.admins.clone(),
                );
                index += 1;
                let again = apply_command(
                    &mut state,
                    LogId::new(1, LEADER, index),
                    &Command::Client {
                        client: client.clone(),
                        action: action.clone(),
                    },
                )
                .expect("no fault");
                match again {
                    CommandReply::Client(second) => {
                        // The saved reply is what a replay must reproduce; the
                        // `replayed` flag is expected to differ, since it is
                        // what tells projection to redact the value.
                        assert_eq!(second.reply, first.reply, "a replay differed at step {n}");
                        assert_eq!(second.disposition, first.disposition);
                        assert!(second.replayed && !first.replayed);
                    }
                    other => panic!("expected a client reply, got {other:?}"),
                }
                assert_eq!(
                    (
                        state.resources.clone(),
                        state.acl.clone(),
                        state.admins.clone()
                    ),
                    business,
                    "a replay mutated business state at step {n}"
                );
                replays += 1;
            }
        }
        assert!(state.counters_match_recomputation());
    }

    assert!(replays > 20, "only {replays} replays were exercised");
}

/// Retirement ordering: whichever way the log orders a mutation and the
/// retirement of its client, the outcome is determined and the two orderings
/// differ in exactly the way §10 describes.
#[test]
fn retirement_ordering_is_determined_in_both_directions() {
    let client = ClientRef {
        actor: ALICE,
        client_id: [1u8; 16],
        sequence: 1,
        request_hash: [7u8; 32],
    };
    let action = Action::Resource(ResourceAction::Acquire {
        resource: raw(0),
        ttl_ms: 30_000,
    });
    let retire = Command::RetireClient(RetireCommand {
        actor: ALICE,
        target_actor: ALICE,
        target_client_id: [1u8; 16],
    });

    // Ordering A: the mutation commits, then retirement.
    let mut a = genesis();
    let first = apply_command(
        &mut a,
        LogId::new(1, LEADER, 1),
        &Command::Client {
            client: client.clone(),
            action: action.clone(),
        },
    )
    .expect("no fault");
    apply_command(&mut a, LogId::new(1, LEADER, 2), &retire).expect("no fault");

    // Ordering B: retirement commits first.
    let mut b = genesis();
    apply_command(&mut b, LogId::new(1, LEADER, 1), &retire).expect("no fault");
    let refused = apply_command(
        &mut b,
        LogId::new(1, LEADER, 2),
        &Command::Client {
            client: client.clone(),
            action: action.clone(),
        },
    )
    .expect("no fault");

    // A: the action applied and its result survives retirement.
    match (&first, &refused) {
        (CommandReply::Client(applied), CommandReply::Client(rejected)) => {
            assert_eq!(applied.reply.outcome, Outcome::Granted);
            assert_eq!(rejected.reply.reason, Reason::ClientRetired);
        }
        other => panic!("unexpected replies: {other:?}"),
    }

    // And the retired client can still read its committed result in ordering A,
    // which is the whole reason retirement retains the saved reply.
    let replay = apply_command(
        &mut a,
        LogId::new(1, LEADER, 3),
        &Command::Client { client, action },
    )
    .expect("no fault");
    match replay {
        CommandReply::Client(r) => assert_eq!(r.reply.outcome, Outcome::Granted),
        other => panic!("expected a client reply, got {other:?}"),
    }

    // The business state differs between orderings, which is what makes the
    // client's replay the only way to tell them apart.
    assert!(a.resources.get(&key(0)).is_some_and(|r| r.held));
    assert!(!b.resources.contains_key(&key(0)));
}

/// A renew and an expiry for the same lease resolve by log order, and the loser
/// changes nothing.
#[test]
fn renew_and_expire_resolve_by_log_order() {
    let mut state = genesis();
    let acquire = Command::Client {
        client: ClientRef {
            actor: ALICE,
            client_id: [1u8; 16],
            sequence: 1,
            request_hash: [1u8; 32],
        },
        action: Action::Resource(ResourceAction::Acquire {
            resource: raw(0),
            ttl_ms: 30_000,
        }),
    };
    apply_command(&mut state, LogId::new(1, LEADER, 1), &acquire).expect("no fault");
    let held = state.resources.get(&key(0)).unwrap().clone();

    // Ordering A: renew first, then the expiry armed against the old lease
    // revision -- which is now stale.
    let mut a = state.clone();
    apply_command(
        &mut a,
        LogId::new(1, LEADER, 2),
        &Command::Client {
            client: ClientRef {
                actor: ALICE,
                client_id: [1u8; 16],
                sequence: 2,
                request_hash: [2u8; 32],
            },
            action: Action::Resource(ResourceAction::Renew {
                fence: Fence {
                    resource: raw(0),
                    epoch: held.epoch,
                    holder: ALICE,
                },
            }),
        },
    )
    .expect("no fault");
    let stale = apply_command(
        &mut a,
        LogId::new(1, LEADER, 3),
        &Command::Expire(ExpireCommand {
            resource: key(0),
            epoch: held.epoch,
            lease_revision: held.lease_revision,
        }),
    )
    .expect("no fault");
    match stale {
        CommandReply::System(r) => assert_eq!(
            r.system_result_code,
            crate::coordination::contract::SYS_EXPIRY_STALE
        ),
        other => panic!("expected a system reply, got {other:?}"),
    }
    assert!(
        a.resources.get(&key(0)).unwrap().held,
        "the renewed lease survives a stale expiry"
    );

    // Ordering B: the expiry commits first and wins; the renew then finds no
    // lease it can renew.
    let mut b = state.clone();
    apply_command(
        &mut b,
        LogId::new(1, LEADER, 2),
        &Command::Expire(ExpireCommand {
            resource: key(0),
            epoch: held.epoch,
            lease_revision: held.lease_revision,
        }),
    )
    .expect("no fault");
    assert!(!b.resources.get(&key(0)).unwrap().held);

    let renew = apply_command(
        &mut b,
        LogId::new(1, LEADER, 3),
        &Command::Client {
            client: ClientRef {
                actor: ALICE,
                client_id: [1u8; 16],
                sequence: 2,
                request_hash: [2u8; 32],
            },
            action: Action::Resource(ResourceAction::Renew {
                fence: Fence {
                    resource: raw(0),
                    epoch: held.epoch,
                    holder: ALICE,
                },
            }),
        },
    )
    .expect("no fault");
    match renew {
        CommandReply::Client(r) => assert_eq!(r.reply.reason, Reason::NotHeld),
        other => panic!("expected a client reply, got {other:?}"),
    }
}

/// Two grants racing on the same resource: the log orders them, the second sees
/// the first's revision, and exactly one wins per revision.
#[test]
fn an_acl_race_is_resolved_by_revision_and_only_one_wins() {
    let mut state = genesis();
    let base = state.resources.get(&key(0)).map_or(0, |r| r.acl_revision);

    let grant = |seq: u64, subject: NodeId, hash: u8| Command::Client {
        client: ClientRef {
            actor: ADMIN,
            client_id: [1u8; 16],
            sequence: seq,
            request_hash: [hash; 32],
        },
        action: Action::Grant(GrantAction {
            resource: raw(0),
            subject,
            rights: RIGHT_READ,
            expected_acl_revision: base,
        }),
    };

    let first =
        apply_command(&mut state, LogId::new(1, LEADER, 1), &grant(1, ALICE, 1)).expect("no fault");
    let second =
        apply_command(&mut state, LogId::new(1, LEADER, 2), &grant(2, BOB, 2)).expect("no fault");

    match (first, second) {
        (CommandReply::Client(a), CommandReply::Client(b)) => {
            assert_eq!(a.reply.outcome, Outcome::GrantUpdated);
            // The loser committed against a revision the winner already moved.
            assert_eq!(b.reply.outcome, Outcome::Rejected);
            assert_eq!(b.reply.reason, Reason::AclRevisionMismatch);
            assert_eq!(
                b.disposition,
                Disposition::Consumed,
                "a stale revision is a consumed conflict, not a pre-proposal denial"
            );
        }
        other => panic!("unexpected replies: {other:?}"),
    }
    assert!(state.counters_match_recomputation());
}

/// The same race on the administrator table.
#[test]
fn an_admin_race_is_resolved_by_revision() {
    let mut state = genesis();
    let set = |seq: u64, target: NodeId, hash: u8| Command::Client {
        client: ClientRef {
            actor: ADMIN,
            client_id: [2u8; 16],
            sequence: seq,
            request_hash: [hash; 32],
        },
        action: Action::Admin(AdminAction {
            target,
            enabled: true,
            expected_admin_revision: 1,
        }),
    };

    apply_command(&mut state, LogId::new(1, LEADER, 1), &set(1, ALICE, 1)).expect("no fault");
    let loser =
        apply_command(&mut state, LogId::new(1, LEADER, 2), &set(2, BOB, 2)).expect("no fault");

    match loser {
        CommandReply::Client(r) => {
            assert_eq!(r.reply.reason, Reason::AdminRevisionMismatch);
            assert_eq!(r.disposition, Disposition::Consumed);
        }
        other => panic!("expected a client reply, got {other:?}"),
    }
    assert_eq!(state.admin_revision, 2);
    assert!(state.admins.contains(&ALICE));
    assert!(!state.admins.contains(&BOB));
}

/// Every membership-intent transition the model defines is reachable, the
/// terminal-history cap never grows past 64, and eviction is by completion.
#[test]
fn membership_intents_transition_and_stay_within_their_cap() {
    let mut state = genesis();
    let mut index = 0u64;

    for round in 0..80u8 {
        // The client's next sequence: one per accepted change, since the
        // interleaved progress reports are system commands and consume none.
        let sequence = u64::from(round) + 1;
        let op = [round; 16];
        let expected = state.membership_log_id;
        let members = state.members.clone();

        // Accept.
        index += 1;
        apply_command(
            &mut state,
            LogId::new(1, LEADER, index),
            &Command::Client {
                client: ClientRef {
                    actor: ADMIN,
                    client_id: [1u8; 16],
                    sequence,
                    request_hash: [round; 32],
                },
                action: Action::ChangeMembership(ChangeMembershipAction {
                    operation_id: op,
                    expected_membership_log_id: expected,
                    plan: MembershipPlan {
                        desired_voters: vec![[1u8; 32], [2u8; 32], [3u8; 32]],
                        retained_learners: Vec::new(),
                        // §9: a join permit is mandatory for a **new** node and
                        // absent for an existing member. After the first round
                        // completes, these three are the committed membership,
                        // so their seeds stop carrying permits -- derived from
                        // state rather than hard-coded, because the rule is
                        // exactly that it depends on state.
                        member_seeds: (1u8..=3)
                            .map(|b| MemberSeed {
                                node_id: [b; 32],
                                ticket: crate::coordination::model::membership::test_ticket(
                                    &[b; 32],
                                ),
                                join_permit: (!members.contains(&[b; 32])).then(|| {
                                    crate::coordination::model::authority::testing::valid(
                                        &[b; 32],
                                        &crate::coordination::model::membership::test_ticket(
                                            &[b; 32],
                                        ),
                                    )
                                }),
                            })
                            .collect(),
                    },
                }),
            },
        )
        .expect("no fault");

        // Walk the legal phase chain to completion, one report per step.
        for (from, to) in [
            (PHASE_PENDING, PHASE_ADDING_LEARNERS),
            (PHASE_ADDING_LEARNERS, PHASE_CHANGING_VOTERS),
            (PHASE_CHANGING_VOTERS, PHASE_COMPLETED),
        ] {
            index += 1;
            apply_command(
                &mut state,
                LogId::new(1, LEADER, index),
                &Command::MembershipProgress(MembershipProgressCommand {
                    operation_id: op,
                    expected_phase: from,
                    new_phase: to,
                    observed_membership_log_id: Some(LogId::new(1, LEADER, index)),
                    result_code: crate::coordination::contract::SYS_MEMBERSHIP_ADVANCED,
                }),
            )
            .expect("no fault");
        }

        assert!(
            state.intents.len() as u64 <= super::limits::MAX_RETAINED_INTENTS,
            "the intent table grew past its cap at round {round}"
        );
        assert!(state.counters_match_recomputation());
    }

    assert_eq!(
        state.intents.len() as u64,
        super::limits::MAX_RETAINED_INTENTS,
        "80 completed intents must leave exactly the retained 64"
    );
    // Every retained intent completed, and the oldest completions were the ones
    // evicted -- so the survivors are the most recent 64.
    let mut completions: Vec<u64> = state
        .intents
        .values()
        .map(|i| i.completed_log_id.expect("terminal").index)
        .collect();
    completions.sort_unstable();
    assert_eq!(
        completions.len() as u64,
        super::limits::MAX_RETAINED_INTENTS
    );
    assert!(
        completions[0] > 4,
        "the earliest completions must have been evicted"
    );
}

/// Tombstones are permanent: no command route removes a client-session row.
#[test]
fn no_command_route_ever_removes_a_session_row() {
    let mut state = genesis();
    let mut seen: usize = 0;

    for n in 0..500u64 {
        let step = step(&state, n);
        let before = state.sessions.len();
        apply_command(&mut state, LogId::new(1, LEADER, n + 1), &step.command).expect("no fault");
        assert!(
            state.sessions.len() >= before,
            "the session table shrank at step {n}"
        );
        seen = seen.max(state.sessions.len());
    }

    assert!(seen > 0, "no session row was ever created");
    // And every retired row is still there.
    assert!(
        state.sessions.values().any(|s| s.retired),
        "no tombstone was created, so permanence is untested"
    );
}
