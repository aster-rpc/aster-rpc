//! The public coordination client (§3.4, S4-D, S4-E).
//!
//! `coordination-client` compiles all of this; nothing here reaches OpenRaft,
//! redb, or any voter runtime — `check-coordination-deps.sh` holds that graph
//! exact.
//!
//! ## Two clients
//!
//! [`CoordinationClient`] binds one local Aster identity to one group and owns
//! the durable request journal: it is the only way to mutate.
//! [`CoordinationReadClient`] omits the journal and exposes only the
//! linearizable read/status surface — it has no mutation or lease
//! constructor, structurally.
//!
//! ## The delivery contract (S4-JOURNAL-DELIVERY, frozen)
//!
//! Every mutation returns a [`ResolvedMutation`] whose reply was fsynced
//! `ResolvedAwaitingAck` **before** the method returned. The application
//! acknowledges it — [`ResolvedMutation::acknowledge`] — after its own durable
//! handling; until then no new sequence is permitted, across calls and across
//! process restarts. After a restart, [`CoordinationClient::unacknowledged`]
//! redelivers the exact saved reply and [`CoordinationClient::resume`] drives
//! any half-finished request to resolution first. v1 guarantees exactly-once
//! **server effect** and durable result **redelivery**; exactly-once
//! application-side processing additionally requires the application to make
//! its durable handling and its acknowledgement atomic, which only it can do.

mod driver;
pub(crate) mod engine;
pub(crate) mod inbound;
/// Crate-private on purpose (the third review's capability note): the only
/// paths that may persist into a journal are the driver — which checks the
/// reply echoes the pending entry before `resolve` — and the client's own
/// transitions. A public mutation surface would let arbitrary bytes become
/// "this entry's saved answer" without that check.
pub(crate) mod journal;
pub mod lease;
pub mod manager;
pub mod placement;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::coordination::contract::{
    payload_fory, AcquireActionV1, ActionV1, CancelMembershipActionV1, ChangeMembershipActionV1,
    CompareAndSetActionV1, FenceV1, MemberSeedV1, NodeIdV1, ReleaseActionV1,
    ReleaseWithValueActionV1, ResourceKeyV1, RevokeActionV1, SetAdminActionV1, SetGrantActionV1,
};
use crate::coordination::hash;
use crate::coordination::ids::CoordinationGroupId;
use crate::coordination::store::wire::{
    AclStatusReplyWireV1, AclStatusRequestV1, AcquireRequestV1, AdminStatusReplyWireV1,
    AdminStatusRequestV1, AuditRecordWireV1, AuditStreamRequestV1, CancelMembershipRequestV1,
    ChangeMembershipRequestV1, CompareAndSetRequestV1, GroupStatusReplyWireV1,
    GroupStatusRequestV1, MembershipStatusReplyWireV1, MembershipStatusRequestV1,
    MutationReplyWireV1, ProbeClientReplyWireV1, ProbeClientRequestV1, ReadReplyWireV1,
    ReadRequestV1, RecoveryExportFrameV1, RecoveryExportManifestWireV1, RecoveryExportRequestV1,
    ReleaseRequestV1, RenewRequestV1, RetireClientReplyWireV1, RetireClientRequestV1,
    RevokeRequestV1, SetAdminRequestV1, SetGrantRequestV1, STORE_PROTOCOL_VERSION,
};
use crate::error::{Error, Result};
use crate::node::Node;
use crate::ticket::{Credential, Ticket};

use driver::{drive, Delivery};
use engine::{CallFailure, CallOutcome, Engine, SharedEngine};
use journal::{AckToken, Journal, JournalState};

fn client_error(what: impl Into<String>) -> Error {
    Error::InvalidArgument(format!("coordination client: {}", what.into()))
}

fn random_id16() -> Result<[u8; 16]> {
    let mut id = [0u8; 16];
    getrandom::getrandom(&mut id)
        .map_err(|err| client_error(format!("no entropy for a client id: {err}")))?;
    Ok(id)
}

/// Builder for both clients (§3.4).
pub struct CoordinationClientBuilder {
    node: Node,
    group: CoordinationGroupId,
    seeds: BTreeSet<[u8; 32]>,
    journal_dir: Option<PathBuf>,
    seed_error: Option<Error>,
}

impl CoordinationClientBuilder {
    /// Seed routing with validated member tickets.
    ///
    /// Tickets are **routing hints only** (§3.4): the Iroh handshake
    /// authenticates the connected identity, and possessing a ticket grants no
    /// coordination membership or resource right. A credential-bearing ticket
    /// is refused — coordination access is decided by the replicated ACL, and
    /// a ticket that *looks* like a credential would misstate where authority
    /// lives.
    pub fn seed_tickets<I, S>(mut self, tickets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for text in tickets {
            let ticket = match Ticket::from_base58(text.as_ref()) {
                Ok(ticket) => ticket,
                Err(err) => {
                    self.seed_error = Some(client_error(format!("seed ticket: {err}")));
                    continue;
                }
            };
            if !matches!(ticket.credential(), Credential::Open) {
                self.seed_error = Some(client_error(
                    "a seed ticket must carry the Open credential: routing hints grant nothing",
                ));
                continue;
            }
            let node_id = ticket.node_id();
            let Ok(decoded) = hex::decode(node_id.as_str()) else {
                self.seed_error = Some(client_error("a seed ticket names a malformed node id"));
                continue;
            };
            let Ok(id) = <[u8; 32]>::try_from(decoded.as_slice()) else {
                self.seed_error = Some(client_error("a seed ticket names a malformed node id"));
                continue;
            };
            self.seeds.insert(id);
            // Address material is an advisory dialling hint; discovery and the
            // handshake own identity.
            if let Ok(addr) = ticket.to_node_addr() {
                let _ = self.node.add_node_addr(addr);
            }
        }
        self
    }

    /// Where the durable journal lives. Required for the mutating client.
    pub fn journal_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.journal_dir = Some(dir.into());
        self
    }

    /// Build the full durable client. Requires writable journal storage and
    /// takes the per-actor/group lifetime lock.
    pub fn build(self) -> Result<CoordinationClient> {
        if let Some(err) = self.seed_error {
            return Err(err);
        }
        let dir = self.journal_dir.ok_or_else(|| {
            client_error(
                "a mutating client requires journal_dir(...); use build_read_only() otherwise",
            )
        })?;
        let engine = Arc::new(Engine::new(self.node, self.group, self.seeds));
        let actor = engine.local_id()?;
        let journal = Journal::open(dir, actor, self.group, random_id16()?)?;
        Ok(CoordinationClient {
            engine,
            journal: Arc::new(Mutex::new(journal)),
        })
    }

    /// Build the read-only client: linearizable reads and status only, no
    /// journal, no mutation or lease constructor.
    pub fn build_read_only(self) -> Result<CoordinationReadClient> {
        if let Some(err) = self.seed_error {
            return Err(err);
        }
        Ok(CoordinationReadClient {
            engine: Arc::new(Engine::new(self.node, self.group, self.seeds)),
        })
    }
}

/// The full durable, redirecting client (§3.4).
#[derive(Clone)]
pub struct CoordinationClient {
    engine: SharedEngine,
    journal: Arc<Mutex<Journal>>,
}

/// The read-only client (§3.4).
#[derive(Clone)]
pub struct CoordinationReadClient {
    engine: SharedEngine,
}

/// A resolved mutation, fsynced and awaiting the application's explicit
/// acknowledgement (S4-JOURNAL-DELIVERY).
///
/// Dropping it without acknowledging is safe and deliberate: the journal
/// remains `ResolvedAwaitingAck`, no new sequence is permitted, and
/// [`CoordinationClient::unacknowledged`] redelivers the same exact reply —
/// including after a process restart.
#[must_use = "acknowledge() after durably handling the result, or the journal stays blocked"]
pub struct ResolvedMutation {
    client: CoordinationClient,
    reply: MutationReplyWireV1,
    via_retirement: bool,
    /// Binds this handle to the exact persisted entry it was delivered for:
    /// a stale handle cannot acknowledge a later result (the journal refuses
    /// any token but the current entry's).
    token: AckToken,
}

impl std::fmt::Debug for ResolvedMutation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedMutation")
            .field("sequence", &self.reply.sequence)
            .field("outcome_code", &self.reply.outcome_code)
            .field("via_retirement", &self.via_retirement)
            .finish()
    }
}

impl ResolvedMutation {
    /// The exact reply the journal holds.
    pub fn reply(&self) -> &MutationReplyWireV1 {
        &self.reply
    }

    /// Acknowledge: the application has durably handled this result. Frees
    /// the journal for the next sequence — by an ordinary acknowledgement, or
    /// by the §10 rotation when the closing corridor resolved it.
    pub async fn acknowledge(self) -> Result<()> {
        let mut journal = self.client.journal.lock().await;
        if self.via_retirement {
            journal.rotate(&self.token, random_id16()?)
        } else {
            journal.acknowledge(&self.token)
        }
    }
}

impl CoordinationClient {
    /// Start building a client bound to `node`'s identity and `group`.
    pub fn builder(node: Node, group: CoordinationGroupId) -> CoordinationClientBuilder {
        CoordinationClientBuilder {
            node,
            group,
            seeds: BTreeSet::new(),
            journal_dir: None,
            seed_error: None,
        }
    }

    /// The saved result a previous run resolved and nobody acknowledged, if
    /// any. Call on startup, before any mutation: the §S4-D contract refuses
    /// new sequences until it is taken and acknowledged.
    pub async fn unacknowledged(&self) -> Result<Option<ResolvedMutation>> {
        let journal = self.journal.lock().await;
        let Some((bytes, token)) = journal.unacknowledged() else {
            return Ok(None);
        };
        let reply: MutationReplyWireV1 =
            payload_fory::<MutationReplyWireV1>()
                .deserialize(bytes)
                .map_err(|_| client_error("the saved reply does not decode"))?;
        let via_retirement = journal.record().retirement_confirmed;
        drop(journal);
        Ok(Some(ResolvedMutation {
            client: self.clone(),
            reply,
            via_retirement,
            token,
        }))
    }

    /// The exact durable request this journal is holding:
    /// `(state code, method, request bytes, action hash, sequence)`.
    ///
    /// §S5-F's corridor has to prove that a manager killed **after** the
    /// journal fsync retries *the same operation id and the same request
    /// bytes* rather than planning afresh, and "the same bytes" is only
    /// assertable against the bytes themselves: two plans that select the same
    /// members produce different operation ids, and a report naming the right
    /// members proves nothing about which request was sent.
    ///
    /// Reported for every state, not only `Pending`. `resolve` carries the
    /// request frame and its hash forward unchanged, so the `ResolvedAwaitingAck`
    /// record still names exactly which request the redelivered reply answers —
    /// which is the half of the retry contract a resolved-before-death manager
    /// exercises.
    ///
    /// Read-only, and test-gated. The journal's own mutation surface stays
    /// crate-private for the reason its module header gives.
    #[cfg(feature = "coordination-testing")]
    pub async fn journal_entry_for_testing(&self) -> (i32, String, Vec<u8>, Vec<u8>, u64) {
        let journal = self.journal.lock().await;
        let state = journal.state().code();
        let record = journal.record();
        (
            state,
            record.pending_method.clone(),
            record.pending_request.clone(),
            record.request_hash.clone(),
            record.sequence,
        )
    }

    /// Drive any half-finished request from a previous run to resolution.
    ///
    /// `Ok(None)` means the journal is `Ready` and mutations may proceed.
    /// `Ok(Some(..))` is the redelivered result, which must be acknowledged
    /// like any other.
    pub async fn resume(&self) -> Result<Option<ResolvedMutation>> {
        let mut journal = self.journal.lock().await;
        match journal.state() {
            JournalState::Ready => Ok(None),
            JournalState::ResolvedAwaitingAck => {
                drop(journal);
                self.unacknowledged().await
            }
            JournalState::FatalJournal => Err(client_error(
                "the journal is fatal (§10 reason 18); no further mutation is possible",
            )),
            JournalState::Pending | JournalState::Closing | JournalState::ReplayAfterRetire => {
                let delivery = drive(&self.engine, &mut journal).await?;
                drop(journal);
                Ok(Some(self.deliver(delivery)))
            }
        }
    }

    fn deliver(&self, delivery: Delivery) -> ResolvedMutation {
        ResolvedMutation {
            client: self.clone(),
            reply: delivery.reply,
            via_retirement: delivery.via_retirement,
            token: delivery.token,
        }
    }

    /// The shared mutation path: journal, then drive (§10).
    async fn mutate(
        &self,
        method: &'static str,
        action: &ActionV1,
        request: Vec<u8>,
    ) -> Result<ResolvedMutation> {
        let request_hash = hash::request_hash(action)
            .map_err(|_| client_error("the action has no canonical hash"))?;
        // The outbound half of the per-method specification, before anything
        // is journaled: §12.1 leaf bounds, and a fence that names this
        // client's group and actor — a fence minted elsewhere is §4.1-invalid
        // and sending it could only burn a sequence on a guaranteed refusal.
        let facts = inbound::decode_facts(method, &request).map_err(client_error)?;
        inbound::validate_request(
            self.engine.group.as_bytes(),
            &self.engine.local_id()?,
            &facts,
        )
        .map_err(client_error)?;
        let mut journal = self.journal.lock().await;
        match journal.state() {
            JournalState::Ready => {
                let sequence = journal.sequence() + 1;
                journal.begin(sequence, method, request, request_hash)?;
            }
            JournalState::Pending if journal.record().request_hash == request_hash.to_vec() => {
                // The same mutation, called again after an interruption:
                // continue driving the exact fsynced bytes.
            }
            JournalState::Pending | JournalState::Closing | JournalState::ReplayAfterRetire => {
                return Err(client_error(
                    "another request is unresolved on this journal; resume() first",
                ));
            }
            JournalState::ResolvedAwaitingAck => {
                return Err(client_error(
                    "a resolved result awaits acknowledgement; take unacknowledged() first",
                ));
            }
            JournalState::FatalJournal => {
                return Err(client_error(
                    "the journal is fatal (§10 reason 18); no further mutation is possible",
                ));
            }
        }
        let delivery = drive(&self.engine, &mut journal).await?;
        drop(journal);
        Ok(self.deliver(delivery))
    }

    /// `acquire` (§5.2 code 1).
    pub async fn acquire(&self, resource: ResourceKeyV1, ttl_ms: u64) -> Result<ResolvedMutation> {
        let action = ActionV1::Acquire(AcquireActionV1 {
            resource: resource.clone(),
            ttl_ms,
        });
        self.sequenced("acquire", action, move |group, client_id, sequence| {
            encode(&AcquireRequestV1 {
                protocol_version: STORE_PROTOCOL_VERSION,
                group_id: group,
                client_id,
                sequence,
                resource,
                ttl_ms,
            })
        })
        .await
    }

    /// `renew` (§5.2 code 2).
    pub async fn renew(&self, fence: FenceV1) -> Result<ResolvedMutation> {
        let action = ActionV1::Renew(crate::coordination::contract::RenewActionV1 {
            fence: fence.clone(),
        });
        self.sequenced("renew", action, move |group, client_id, sequence| {
            encode(&RenewRequestV1 {
                protocol_version: STORE_PROTOCOL_VERSION,
                group_id: group,
                client_id,
                sequence,
                fence,
            })
        })
        .await
    }

    /// `compare_and_set` (§5.2 code 3).
    pub async fn compare_and_set(
        &self,
        fence: FenceV1,
        expected_value_revision: u64,
        new_value: Vec<u8>,
    ) -> Result<ResolvedMutation> {
        let action = ActionV1::CompareAndSet(CompareAndSetActionV1 {
            fence: fence.clone(),
            expected_value_revision,
            new_value: new_value.clone(),
        });
        self.sequenced(
            "compare_and_set",
            action,
            move |group, client_id, sequence| {
                encode(&CompareAndSetRequestV1 {
                    protocol_version: STORE_PROTOCOL_VERSION,
                    group_id: group,
                    client_id,
                    sequence,
                    fence,
                    expected_value_revision,
                    new_value,
                })
            },
        )
        .await
    }

    /// `release` (§5.2 code 4).
    pub async fn release(&self, fence: FenceV1) -> Result<ResolvedMutation> {
        let action = ActionV1::Release(ReleaseActionV1 {
            fence: fence.clone(),
        });
        self.sequenced("release", action, move |group, client_id, sequence| {
            encode(&ReleaseRequestV1 {
                protocol_version: STORE_PROTOCOL_VERSION,
                group_id: group,
                client_id,
                sequence,
                fence,
            })
        })
        .await
    }

    /// `release_with_value` (§5.2 code 5): the atomic final-value handoff.
    pub async fn release_with_value(
        &self,
        fence: FenceV1,
        expected_value_revision: u64,
        final_value: Vec<u8>,
    ) -> Result<ResolvedMutation> {
        let action = ActionV1::ReleaseWithValue(ReleaseWithValueActionV1 {
            fence: fence.clone(),
            expected_value_revision,
            final_value: final_value.clone(),
        });
        self.sequenced(
            "release_with_value",
            action,
            move |group, client_id, sequence| {
                encode(
                    &crate::coordination::store::wire::ReleaseWithValueRequestV1 {
                        protocol_version: STORE_PROTOCOL_VERSION,
                        group_id: group,
                        client_id,
                        sequence,
                        fence,
                        expected_value_revision,
                        final_value,
                    },
                )
            },
        )
        .await
    }

    /// `revoke` (§5.2 code 6). `expected_epoch: None` is unconditional.
    pub async fn revoke(
        &self,
        resource: ResourceKeyV1,
        expected_epoch: Option<u64>,
    ) -> Result<ResolvedMutation> {
        let action = ActionV1::Revoke(RevokeActionV1 {
            resource: resource.clone(),
            expected_epoch,
        });
        self.sequenced("revoke", action, move |group, client_id, sequence| {
            encode(&RevokeRequestV1 {
                protocol_version: STORE_PROTOCOL_VERSION,
                group_id: group,
                client_id,
                sequence,
                resource,
                expected_epoch,
            })
        })
        .await
    }

    /// `set_grant` (§5.2 code 7).
    pub async fn set_grant(
        &self,
        resource: ResourceKeyV1,
        target_node_id: [u8; 32],
        rights: u64,
        expected_acl_revision: u64,
    ) -> Result<ResolvedMutation> {
        let action = ActionV1::SetGrant(SetGrantActionV1 {
            resource: resource.clone(),
            target_node_id: target_node_id.to_vec(),
            rights,
            expected_acl_revision,
        });
        self.sequenced("set_grant", action, move |group, client_id, sequence| {
            encode(&SetGrantRequestV1 {
                protocol_version: STORE_PROTOCOL_VERSION,
                group_id: group,
                client_id,
                sequence,
                resource,
                target_node_id: target_node_id.to_vec(),
                rights,
                expected_acl_revision,
            })
        })
        .await
    }

    /// `set_admin` (§5.2 code 8).
    pub async fn set_admin(
        &self,
        target_node_id: [u8; 32],
        enabled: bool,
        expected_admin_revision: u64,
    ) -> Result<ResolvedMutation> {
        let action = ActionV1::SetAdmin(SetAdminActionV1 {
            target_node_id: target_node_id.to_vec(),
            enabled,
            expected_admin_revision,
        });
        self.sequenced("set_admin", action, move |group, client_id, sequence| {
            encode(&SetAdminRequestV1 {
                protocol_version: STORE_PROTOCOL_VERSION,
                group_id: group,
                client_id,
                sequence,
                target_node_id: target_node_id.to_vec(),
                enabled,
                expected_admin_revision,
            })
        })
        .await
    }

    /// `change_membership` (§5.2 code 9).
    pub async fn change_membership(
        &self,
        operation_id: [u8; 16],
        expected_membership_log_id: Option<crate::coordination::contract::LogIdV1>,
        desired_voters: Vec<NodeIdV1>,
        retained_learners: Vec<NodeIdV1>,
        member_seeds: Vec<MemberSeedV1>,
    ) -> Result<ResolvedMutation> {
        // §10 liveness: a plan the group would refuse at stage 4 must never
        // be journaled, and the plan-shape rules are group-relative — the
        // committed membership decides which nodes are new, the genesis
        // fixes the profile and the ROOT. So the client runs **the same**
        // §9 predicate apply runs, against one validated linearizable
        // status cut — which carries the group's **replicated** authority
        // facts (ROOT key and domain), so the proof rests on sealed group
        // evidence rather than trusted local configuration. The voter
        // checks the membership revision fence before the plan shape, so at
        // apply the plan is judged against exactly the state it was proven
        // against — or the fence answers the typed, journal-resolvable
        // `13`.
        let status = self.reads().group_status().await?;
        // The proof is bound to the request's own fence: a plan validated
        // against this cut but fenced elsewhere could be judged against a
        // membership it was never proven for. Requiring equality closes it —
        // and a fence that goes stale after this check answers typed.
        if expected_membership_log_id != status.membership_log_id {
            return Err(client_error(
                "the plan's membership fence does not match the validated status cut; \
                 re-plan against the current membership",
            ));
        }
        let plan = narrow_plan(&desired_voters, &retained_learners, &member_seeds)?;
        let committed = crate::coordination::model::state::CommittedMembership {
            voters: status
                .voters
                .iter()
                .filter_map(|n| <[u8; 32]>::try_from(n.node_id.as_slice()).ok())
                .collect(),
            learners: status
                .learners
                .iter()
                .filter_map(|n| <[u8; 32]>::try_from(n.node_id.as_slice()).ok())
                .collect(),
            joint: status.membership_joint,
        };
        let authority = crate::coordination::model::state::GroupAuthority {
            public_key: <[u8; 32]>::try_from(status.authority_public_key.as_slice())
                .map_err(|_| client_error("the status cut carries a malformed authority key"))?,
            domain: status.authority_domain.clone(),
            group_id: *self.engine.group.as_bytes(),
            genesis_hash: <[u8; 32]>::try_from(status.genesis_hash.as_slice())
                .map_err(|_| client_error("the status cut carries a malformed genesis hash"))?,
        };
        if let Some(limit) = crate::coordination::model::shape::plan_shape(
            &plan,
            &committed,
            &authority,
            status.deployment_profile_code,
        ) {
            return Err(client_error(format!(
                "the membership plan cannot apply against this group: {}",
                limit.describe()
            )));
        }

        let action = ActionV1::ChangeMembership(ChangeMembershipActionV1 {
            operation_id: operation_id.to_vec(),
            expected_membership_log_id: expected_membership_log_id.clone(),
            desired_voters: desired_voters.clone(),
            retained_learners: retained_learners.clone(),
            member_seeds: member_seeds.clone(),
        });
        self.sequenced(
            "change_membership",
            action,
            move |group, client_id, sequence| {
                encode(&ChangeMembershipRequestV1 {
                    protocol_version: STORE_PROTOCOL_VERSION,
                    group_id: group,
                    client_id,
                    sequence,
                    operation_id: operation_id.to_vec(),
                    expected_membership_log_id,
                    desired_voters,
                    retained_learners,
                    member_seeds,
                })
            },
        )
        .await
    }

    /// `cancel_membership` (§5.2 code 11).
    pub async fn cancel_membership(
        &self,
        operation_id: [u8; 16],
        expected_phase_code: i32,
    ) -> Result<ResolvedMutation> {
        let action = ActionV1::CancelMembership(CancelMembershipActionV1 {
            operation_id: operation_id.to_vec(),
            expected_phase_code,
        });
        self.sequenced(
            "cancel_membership",
            action,
            move |group, client_id, sequence| {
                encode(&CancelMembershipRequestV1 {
                    protocol_version: STORE_PROTOCOL_VERSION,
                    group_id: group,
                    client_id,
                    sequence,
                    operation_id: operation_id.to_vec(),
                    expected_phase_code,
                })
            },
        )
        .await
    }

    /// Build the request under the journal's own client id and next sequence,
    /// then run the shared mutation path.
    async fn sequenced<F>(
        &self,
        method: &'static str,
        action: ActionV1,
        build: F,
    ) -> Result<ResolvedMutation>
    where
        F: FnOnce(Vec<u8>, Vec<u8>, u64) -> Result<Vec<u8>>,
    {
        let (group, client_id, sequence) = {
            let journal = self.journal.lock().await;
            (
                self.engine.group.as_bytes().to_vec(),
                journal.record().client_id.clone(),
                journal.sequence() + 1,
            )
        };
        let request = build(group, client_id, sequence)?;
        self.mutate(method, &action, request).await
    }

    /// `retire_client` for one of this actor's **other** client ids —
    /// operational cleanup (freeing an active slot) without touching this
    /// journal. Retiring this journal's own id happens only through the §10
    /// closing corridor.
    pub async fn retire_other_client(
        &self,
        target_client_id: [u8; 16],
    ) -> Result<RetireClientReplyWireV1> {
        let actor = self.engine.local_id()?;
        if target_client_id.to_vec() == self.journal.lock().await.record().client_id {
            return Err(client_error(
                "this journal's own client id is retired only by its closing corridor",
            ));
        }
        let request = RetireClientRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: self.engine.group.as_bytes().to_vec(),
            target_node_id: actor.to_vec(),
            target_client_id: target_client_id.to_vec(),
        };
        // The reply is held to the frozen §10 pair set for a self-targeted
        // retirement — `(9, 0)` or `(100, 18)` — by the inbound boundary.
        unary_typed(&self.engine, "retire_client", &request).await
    }

    /// The group this client is bound to.
    pub fn group(&self) -> CoordinationGroupId {
        self.engine.group
    }

    /// The local actor identity, as the group authenticates it.
    pub fn local_id(&self) -> Result<[u8; 32]> {
        self.engine.local_id()
    }

    /// The journal's current client id. Operational: pair it with
    /// [`CoordinationReadClient::probe_client`] diagnostics; it never appears
    /// in a request as an actor.
    pub async fn client_id(&self) -> [u8; 16] {
        self.journal.lock().await.client_id()
    }

    /// The read/status surface, shared with the read-only client.
    pub fn reads(&self) -> CoordinationReadClient {
        CoordinationReadClient {
            engine: Arc::clone(&self.engine),
        }
    }
}

/// Narrow a wire membership plan into the model plan `shape::plan_shape`
/// judges — widths refused here are the same widths the voter refuses.
fn narrow_plan(
    desired_voters: &[NodeIdV1],
    retained_learners: &[NodeIdV1],
    member_seeds: &[MemberSeedV1],
) -> Result<crate::coordination::model::state::MembershipPlan> {
    let narrow = |nodes: &[NodeIdV1]| -> Result<Vec<[u8; 32]>> {
        nodes
            .iter()
            .map(|n| {
                <[u8; 32]>::try_from(n.node_id.as_slice())
                    .map_err(|_| client_error("a plan node id is not 32 bytes"))
            })
            .collect()
    };
    Ok(crate::coordination::model::state::MembershipPlan {
        desired_voters: narrow(desired_voters)?,
        retained_learners: narrow(retained_learners)?,
        member_seeds: member_seeds
            .iter()
            .map(|seed| {
                Ok(crate::coordination::model::state::MemberSeed {
                    node_id: <[u8; 32]>::try_from(seed.node_id.as_slice())
                        .map_err(|_| client_error("a seed node id is not 32 bytes"))?,
                    ticket: seed.ticket.clone().into_bytes(),
                    join_permit: seed.signed_join_permit.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?,
    })
}

fn encode<T>(value: &T) -> Result<Vec<u8>>
where
    T: crate::rpc::contract::WireField + fory_core::Serializer<Target = T> + 'static,
{
    payload_fory::<T>()
        .serialize(value)
        .map_err(|_| client_error("the request does not encode"))
}

fn decode<T>(bytes: &[u8]) -> Result<T>
where
    T: crate::rpc::contract::WireField + fory_core::Serializer<Target = T> + 'static,
{
    payload_fory::<T>()
        .deserialize(bytes)
        .map_err(|_| client_error("the reply does not decode"))
}

/// The envelope every §5.2 reply carries. The client's one inbound
/// validation boundary reads it: a reply that decodes but names another
/// protocol version or another group is refused, never handed to the
/// caller — live replies are held to the same standard the journal holds
/// its saved ones to.
trait ReplyEnvelope {
    fn envelope(&self) -> (i32, &[u8]);
}

macro_rules! reply_envelope {
    ($($ty:ty),+ $(,)?) => {$(
        impl ReplyEnvelope for $ty {
            fn envelope(&self) -> (i32, &[u8]) {
                (self.protocol_version, &self.group_id)
            }
        }
    )+};
}

reply_envelope!(
    RetireClientReplyWireV1,
    ReadReplyWireV1,
    AclStatusReplyWireV1,
    AdminStatusReplyWireV1,
    MembershipStatusReplyWireV1,
    ProbeClientReplyWireV1,
    GroupStatusReplyWireV1,
    crate::coordination::store::wire::AuditRecordWireV1,
    crate::coordination::store::wire::RecoveryExportManifestWireV1,
);

/// The inbound boundary itself: protocol version and group on every typed
/// reply the client accepts, then the reply type's own **request-aware**
/// validity — identity echoes, code domains, ordering, and facts that must
/// not contradict themselves (`client::inbound::ValidReply`).
fn check_envelope<T: ReplyEnvelope + inbound::ValidReply>(
    engine: &Engine,
    method: &str,
    request: &T::Request,
    reply: T,
) -> Result<T> {
    let (protocol_version, group_id) = reply.envelope();
    if protocol_version != STORE_PROTOCOL_VERSION {
        return Err(client_error(format!(
            "{method}: the reply carries protocol version {protocol_version}, not \
             {STORE_PROTOCOL_VERSION}"
        )));
    }
    if group_id != engine.group.as_bytes() {
        return Err(client_error(format!(
            "{method}: the reply names another coordination group"
        )));
    }
    reply
        .validate_reply(engine.group.as_bytes(), request)
        .map_err(|why| client_error(format!("{method}: {why}")))?;
    Ok(reply)
}

/// One redirect-routed unary call with a typed reply; a denial surfaces as
/// `Error::Rpc` and ambiguity as a transport error — reads are safely
/// re-issued by the caller. Every reply passes the envelope boundary,
/// validated against the typed request it answers.
async fn unary_typed<T>(engine: &Engine, method: &str, request: &T::Request) -> Result<T>
where
    T: crate::rpc::contract::WireField
        + fory_core::Serializer<Target = T>
        + ReplyEnvelope
        + inbound::ValidReply
        + 'static,
    T::Request:
        crate::rpc::contract::WireField + fory_core::Serializer<Target = T::Request> + 'static,
{
    let bytes = encode(request)?;
    match engine.unary(method, bytes).await {
        Ok(CallOutcome::Reply(payload)) => {
            decode(&payload).and_then(|reply| check_envelope(engine, method, request, reply))
        }
        Ok(CallOutcome::Denied { code, message }) => Err(Error::Rpc {
            code,
            message,
            details: Vec::new(),
        }),
        Err(CallFailure::Ambiguous(detail)) | Err(CallFailure::Unavailable(detail)) => {
            Err(Error::Transport(anyhow::anyhow!("{method}: {detail}")))
        }
    }
}

impl CoordinationReadClient {
    /// `read_linearizable` (§5.2): the leader barrier and the one pinned
    /// transaction are the server's; the reply is the snapshot at that cut.
    pub async fn read_linearizable(&self, resource: ResourceKeyV1) -> Result<ReadReplyWireV1> {
        // Request correlation — the snapshot is about the resource this call
        // asked for, carries its value, and is never redacted — is the
        // validator's (`inbound::ValidReply`), run against this request.
        let request = ReadRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: self.engine.group.as_bytes().to_vec(),
            resource,
        };
        unary_typed(&self.engine, "read_linearizable", &request).await
    }

    /// `inspect_acl` (§5.2, administrator).
    pub async fn inspect_acl(&self, resource: ResourceKeyV1) -> Result<AclStatusReplyWireV1> {
        let request = AclStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: self.engine.group.as_bytes().to_vec(),
            resource,
        };
        unary_typed(&self.engine, "inspect_acl", &request).await
    }

    /// `inspect_admins` (§5.2, administrator).
    pub async fn inspect_admins(&self) -> Result<AdminStatusReplyWireV1> {
        let request = AdminStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: self.engine.group.as_bytes().to_vec(),
        };
        unary_typed(&self.engine, "inspect_admins", &request).await
    }

    /// `membership_status` (§5.2, administrator).
    pub async fn membership_status(
        &self,
        operation_id: Option<[u8; 16]>,
    ) -> Result<MembershipStatusReplyWireV1> {
        let request = MembershipStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: self.engine.group.as_bytes().to_vec(),
            operation_id: operation_id.map(|id| id.to_vec()),
        };
        unary_typed(&self.engine, "membership_status", &request).await
    }

    /// `group_status` (§5.2, administrator): the S4-STATUS-CUT reply.
    pub async fn group_status(&self) -> Result<GroupStatusReplyWireV1> {
        let request = GroupStatusRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: self.engine.group.as_bytes().to_vec(),
        };
        unary_typed(&self.engine, "group_status", &request).await
    }

    /// `probe_client` (§5.2): diagnostic only, never authority to reassign a
    /// sequence.
    pub async fn probe_client(
        &self,
        target_node_id: [u8; 32],
        target_client_id: [u8; 16],
    ) -> Result<ProbeClientReplyWireV1> {
        let request = ProbeClientRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: self.engine.group.as_bytes().to_vec(),
            target_node_id: target_node_id.to_vec(),
            target_client_id: target_client_id.to_vec(),
        };
        unary_typed(&self.engine, "probe_client", &request).await
    }

    /// `audit_stream` (§5.2, administrator): at most `limit` retained rows
    /// strictly after `after_log_id`, ascending.
    ///
    /// Every contract word is enforced **as the frames arrive**, before the
    /// next one is read: each record passes the envelope and its own §5.2
    /// validity, the count never exceeds the requested limit, the log
    /// indices ascend strictly, and the first is strictly after the cursor.
    /// A stream that violates any of these stops there — nothing past the
    /// violation is decoded, and nothing is returned.
    pub async fn audit_stream(
        &self,
        after_log_id: Option<crate::coordination::contract::LogIdV1>,
        limit: u32,
    ) -> Result<Vec<AuditRecordWireV1>> {
        // The §5.2 hard cap is enforced here, not merely trusted to the
        // server: a limit of zero asks for nothing, and one above
        // `MAX_AUDIT_STREAM_LIMIT` is a request no conforming server
        // honors — and an unbounded license for a counterfeit one.
        crate::coordination::store::wire::audit_limit_bounds(limit)
            .map_err(|invalid| client_error(format!("audit_stream: {}", invalid.describe())))?;
        let request = encode(&AuditStreamRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: self.engine.group.as_bytes().to_vec(),
            after_log_id: after_log_id.clone(),
            limit,
        })?;
        let mut stream = match self.engine.open_stream("audit_stream", request).await {
            Ok(Ok(stream)) => stream,
            Ok(Err((code, message))) => {
                return Err(Error::Rpc {
                    code,
                    message,
                    details: Vec::new(),
                })
            }
            Err(CallFailure::Ambiguous(detail)) | Err(CallFailure::Unavailable(detail)) => {
                return Err(Error::Transport(anyhow::anyhow!("audit_stream: {detail}")))
            }
        };
        // The absolute budget earns the declared work: the validated limit's
        // rows at the per-item allowance.
        stream.allow_items(u64::from(limit));
        let mut records: Vec<AuditRecordWireV1> = Vec::new();
        let mut floor: Option<u64> = after_log_id.as_ref().map(|id| id.index);
        while let Some(frame) = stream.next().await {
            let frame = frame?;
            if records.len() as u64 >= u64::from(limit) {
                return Err(client_error(
                    "audit_stream: the stream overran the requested limit",
                ));
            }
            let record: AuditRecordWireV1 = decode(&frame)?;
            let record = check_envelope(&self.engine, "audit_stream", &(), record)?;
            if let Some(floor) = floor {
                if record.log_id.index <= floor {
                    return Err(client_error(
                        "audit_stream: the rows are not strictly ascending past the cursor",
                    ));
                }
            }
            floor = Some(record.log_id.index);
            records.push(record);
        }
        Ok(records)
    }

    /// `recovery_export` (§5.2, administrator; S4-EXPORT-OWNERSHIP), streamed:
    /// the manifest, and the canonical body written into `into` as its chunks
    /// arrive — decoded, length-checked and hashed incrementally, never
    /// assembled in memory.
    ///
    /// The writer is returned so a caller can hand a file, take it back, and
    /// only then rename it into place once the checksum has held.
    pub async fn recovery_export_to<W: std::io::Write>(
        &self,
        into: W,
    ) -> Result<(RecoveryExportManifestWireV1, W)> {
        let typed_request = RecoveryExportRequestV1 {
            protocol_version: STORE_PROTOCOL_VERSION,
            group_id: self.engine.group.as_bytes().to_vec(),
        };
        let request = encode(&typed_request)?;
        let mut stream = match self.engine.open_stream("recovery_export", request).await {
            Ok(Ok(stream)) => stream,
            Ok(Err((code, message))) => {
                return Err(Error::Rpc {
                    code,
                    message,
                    details: Vec::new(),
                })
            }
            Err(CallFailure::Ambiguous(detail)) | Err(CallFailure::Unavailable(detail)) => {
                return Err(Error::Transport(anyhow::anyhow!(
                    "recovery_export: {detail}"
                )))
            }
        };

        let mut writer = into;
        let mut manifest: Option<RecoveryExportManifestWireV1> = None;
        let mut hasher = blake3::Hasher::new();
        let mut received = 0u64;
        let mut finished = false;
        while let Some(frame) = stream.next().await {
            let frame = frame?;
            if finished {
                return Err(client_error("the export continued past its final chunk"));
            }
            match decode::<RecoveryExportFrameV1>(&frame)? {
                RecoveryExportFrameV1::Manifest(m) => {
                    // The manifest passes the same envelope boundary as
                    // every typed reply: a manifest for another group or
                    // protocol binds nothing here, and one whose claimed
                    // length exceeds the §11.4 body ceiling is refused
                    // before a byte is written toward it.
                    let m = check_envelope(&self.engine, "recovery_export", &typed_request, m)?;
                    // The validated manifest's declared body earns transfer
                    // budget at the declared minimum throughput — bounded,
                    // because the ceiling check above already held.
                    stream.allow_bytes(m.total_len);
                    if manifest.replace(m).is_some() {
                        return Err(client_error("the export carried two manifests"));
                    }
                }
                RecoveryExportFrameV1::Chunk(chunk) => {
                    let Some(seen) = manifest.as_ref() else {
                        return Err(client_error("the export sent a chunk before its manifest"));
                    };
                    // §5.2 slices the body into bounded chunks; an oversized
                    // one is not this protocol's frame, refused before the
                    // writer sees it.
                    if chunk.data.len() > crate::coordination::store::wire::MAX_EXPORT_CHUNK_BYTES {
                        return Err(client_error("the export chunk exceeds the §5.2 bound"));
                    }
                    // Every non-final chunk advances the body. An empty one
                    // makes no progress — a frame that exists only to reset
                    // idle timers — and no conforming slicer emits it.
                    if chunk.data.is_empty() && !chunk.final_chunk {
                        return Err(client_error("the export sent a chunk with no progress"));
                    }
                    if chunk.offset != received {
                        return Err(client_error("the export chunks are not contiguous"));
                    }
                    received += chunk.data.len() as u64;
                    if received > seen.total_len {
                        return Err(client_error("the export body overruns its manifest"));
                    }
                    hasher.update(&chunk.data);
                    writer
                        .write_all(&chunk.data)
                        .map_err(|err| client_error(format!("the export writer failed: {err}")))?;
                    if chunk.final_chunk {
                        if received != seen.total_len {
                            return Err(client_error("the export body ends short of its manifest"));
                        }
                        finished = true;
                    }
                }
                RecoveryExportFrameV1::Unknown(_) => {
                    return Err(client_error("the export carried an unknown frame"));
                }
            }
        }
        let manifest = manifest.ok_or_else(|| client_error("the export carried no manifest"))?;
        if !finished {
            return Err(client_error("the export ended without a final chunk"));
        }
        if hasher.finalize().as_bytes().to_vec() != manifest.blake3_checksum {
            return Err(client_error("the export body fails its manifest checksum"));
        }
        writer
            .flush()
            .map_err(|err| client_error(format!("the export writer failed: {err}")))?;
        Ok((manifest, writer))
    }

    /// [`recovery_export_to`](Self::recovery_export_to) into a `Vec` — the
    /// convenience for small groups and tests; the streaming variant is the
    /// primitive.
    pub async fn recovery_export(&self) -> Result<(RecoveryExportManifestWireV1, Vec<u8>)> {
        self.recovery_export_to(Vec::new()).await
    }
}
