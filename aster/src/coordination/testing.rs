//! The Slice 3 internal test harness (contract matrix §S3-E, §S3-F).
//!
//! ## Why this exists rather than a partial Store RPC
//!
//! Several Slice 3 properties are only observable once *committed traffic*
//! exists: a minority that cannot commit, a controller reconciling a durable
//! intent, a follower far enough behind to need a snapshot rather than a log
//! replay. All three need a client write, and the client write is
//! `aster.coordination.Store`, which Slice 4 owns.
//!
//! §6.2 forbids exposing a partial Store RPC to drive them early, and the
//! contract matrix says what to do instead: *"Slice 3 controller tests seed
//! accepted intents through the internal state-machine/test harness. They do
//! not expose a partial Store RPC merely to drive the controller before Slice
//! 4."* This module is that harness.
//!
//! ## What it is, and what it deliberately is not
//!
//! It is **one method**: propose an already-formed [`CommandV1`] through
//! `Raft::client_write`, the same call the Store service will make. Everything
//! else a gate needs — membership intents, log volume, a snapshot trigger —
//! composes from that, because a membership intent *is* a command.
//!
//! It is **not** a way to reach past Raft. There is no method here that writes
//! to the state machine directly, skips replication, or fabricates a commit:
//! a seam that could do any of those would let a test observe an outcome the
//! production path cannot produce, which is the failure mode these gates exist
//! to catch. Every byte still goes through election, replication and quorum.
//!
//! ## It is not compiled into anything that ships
//!
//! Gated on `coordination-testing`, which no shipping profile enables and
//! which `coordination` does **not** imply. `check-coordination-deps.sh`
//! asserts that, so turning it on for convenience would fail the gate rather
//! than quietly widening the public API.

use openraft::error::{ClientWriteError, RaftError};
use openraft::Raft;

use crate::coordination::contract::{CommandReplyV1, CommandV1};
use crate::coordination::typeconfig::CoordinationTypeConfig;
use crate::coordination::Coordination;

/// The `RaftError` a client write can answer with, spelled once.
pub type WriteRejection = RaftError<
    crate::coordination::ids::CoordinationNodeId,
    ClientWriteError<
        crate::coordination::ids::CoordinationNodeId,
        crate::coordination::typeconfig::CoordinationNode,
    >,
>;

/// Why a seeded proposal did not commit.
///
/// The two outcomes are kept apart because a gate asserting "a minority cannot
/// commit" has to distinguish *not committed* from *not attempted*. A single
/// opaque error would let a test that never reached Raft pass as evidence that
/// Raft refused.
#[derive(Debug)]
pub enum ProposeError {
    /// Raft answered, and the answer was a refusal — no leader, a forward, a
    /// rejected membership change. The proposal reached consensus machinery
    /// and consensus machinery declined it. Boxed because it dwarfs the other
    /// variant, which is the one a stalled minority actually produces.
    Rejected(Box<WriteRejection>),
    /// The proposal did not resolve within the caller's window. **Not** proof
    /// of refusal: an entry that times out here may still commit later, which
    /// is exactly why a minority-safety gate must also show that it did not.
    TimedOut,
}

impl std::fmt::Display for ProposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposeError::Rejected(err) => write!(f, "the proposal was refused: {err}"),
            ProposeError::TimedOut => f.write_str("the proposal did not resolve in time"),
        }
    }
}

impl std::error::Error for ProposeError {}

/// A proposal that committed and applied.
///
/// Carries the log index from `client_write`'s own response rather than leaving
/// a caller to read it from `RaftMetrics`. The metrics are published through a
/// watch channel and lag the apply, so a gate that compared metrics before and
/// after a write could see them unchanged on a write that had certainly
/// committed — which reads as "the group did not commit" and is wrong.
///
/// Metrics remain the right oracle for the *negative* case: when there is no
/// response, the log is the only thing that can speak.
#[derive(Debug)]
pub struct Committed {
    /// The index this entry was assigned.
    pub index: u64,
    /// The state machine's own reply.
    pub reply: CommandReplyV1,
}

impl Coordination {
    /// Propose one command through the production Raft path.
    ///
    /// Exactly what the Slice 4 Store service will do with a decoded request:
    /// `Raft::client_write`, awaited to commit-and-apply. A leader commits it
    /// once a quorum acknowledges; a follower answers `ForwardToLeader`; a
    /// node without a quorum never commits it at all, which is the point of
    /// the gate that calls this.
    ///
    /// The reply is the state machine's own — not a synthesised acknowledgement
    /// — so a caller can tell "applied" from "accepted for replication".
    pub async fn propose_for_testing(&self, command: CommandV1) -> Result<Committed, ProposeError> {
        self.raft
            .client_write(command)
            .await
            .map(|response| Committed {
                index: response.log_id.index,
                reply: response.data,
            })
            .map_err(|err| ProposeError::Rejected(Box::new(err)))
    }

    /// Propose, but give up after `within`.
    ///
    /// A proposal into a group that cannot reach a quorum does not fail — it
    /// *waits*, because the entry is legitimately still pending and OpenRaft
    /// has no basis to abandon it. A gate proving a minority cannot commit must
    /// therefore bound its own patience and then check the log, rather than
    /// waiting for an error that never comes.
    pub async fn propose_for_testing_within(
        &self,
        command: CommandV1,
        within: std::time::Duration,
    ) -> Result<Committed, ProposeError> {
        match tokio::time::timeout(within, self.propose_for_testing(command)).await {
            Ok(result) => result,
            Err(_) => Err(ProposeError::TimedOut),
        }
    }

    /// The last log index this node has **applied** to its state machine.
    ///
    /// The independent oracle for a commit: a proposal that returned a reply
    /// and a log that never advanced would be a fabricated acknowledgement, and
    /// nothing else in the public surface can tell those apart.
    pub fn last_applied_for_testing(&self) -> Option<u64> {
        self.raft
            .metrics()
            .borrow()
            .last_applied
            .map(|log_id| log_id.index)
    }

    /// The last log index included in this node's snapshot, if it has one.
    ///
    /// The evidence that distinguishes a catch-up by **snapshot** from a
    /// catch-up by log replay. Both end with the same applied index, so a gate
    /// that only checked the applied index would pass on a follower the leader
    /// had simply re-sent every entry to — and would have proven nothing about
    /// the install path.
    pub fn snapshot_index_for_testing(&self) -> Option<u64> {
        self.raft.metrics().borrow().snapshot.map(|id| id.index)
    }

    /// The last log index this node has purged, inclusive.
    ///
    /// A leader that has not purged past a follower's position can still replay
    /// to it, so this is how a gate knows the group has actually reached the
    /// state where a snapshot is the *only* way to catch that follower up.
    pub fn purged_index_for_testing(&self) -> Option<u64> {
        self.raft.metrics().borrow().purged.map(|id| id.index)
    }

    /// The log id of the currently committed membership.
    ///
    /// A `change_membership` intent names the membership it was planned
    /// against, and the apply path refuses one that names a different revision
    /// — which is what stops two administrators planning from the same starting
    /// point and both being admitted. A gate seeding an intent therefore has to
    /// read the same value the request will be compared against.
    pub fn membership_log_id_for_testing(&self) -> Option<crate::coordination::contract::LogIdV1> {
        self.raft
            .metrics()
            .borrow()
            .membership_config
            .log_id()
            .map(crate::coordination::contract::LogIdV1::from)
    }

    /// Whether a membership intent is still in flight.
    ///
    /// The **durable** answer, from the state machine rather than from
    /// `RaftMetrics`. The two settle at different moments and both are correct:
    /// the committed membership reaches its final shape one entry before the
    /// controller commits the intent's terminal phase, so a group can look
    /// finished and still refuse the next plan with `MembershipIntentActive`.
    ///
    /// A gate that submits a follow-up plan therefore has to wait on this, not
    /// on the shape — and a gate that retried until it was accepted would be
    /// hiding exactly the interval this exposes.
    pub async fn membership_intent_active_for_testing(&self) -> bool {
        self.state.active_intent().await.ok().flatten().is_some()
    }

    /// Whether this node has applied the group's signed genesis.
    pub async fn genesis_installed_for_testing(&self) -> bool {
        self.state.genesis_installed().await.unwrap_or(false)
    }

    /// The voter ids in the currently committed membership.
    ///
    /// Read from `RaftMetrics`, which is the group's agreed answer rather than
    /// a controller's local view of what it was trying to achieve — the
    /// distinction the §S3-E completion rule turns on.
    pub fn committed_voters_for_testing(
        &self,
    ) -> Vec<crate::coordination::ids::CoordinationNodeId> {
        self.raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .collect()
    }

    /// Every node in the committed membership, voter or learner.
    pub fn committed_nodes_for_testing(&self) -> Vec<crate::coordination::ids::CoordinationNodeId> {
        self.raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .nodes()
            .map(|(id, _)| *id)
            .collect()
    }

    /// Whether the committed membership is a **joint** configuration.
    ///
    /// One of the four `S3-JOINT-RESUME` boundaries is "leadership lost while a
    /// joint entry is committed", and a test cannot arrange to be at that
    /// boundary without being able to see it.
    pub fn membership_is_joint_for_testing(&self) -> bool {
        self.raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .get_joint_config()
            .len()
            > 1
    }

    /// Build a snapshot now, rather than waiting for the log-size policy.
    ///
    /// `trigger().snapshot()` is OpenRaft's own request; the snapshot it
    /// produces is the production builder's, written through the production
    /// storage path. A test that hand-assembled a body would be proving things
    /// about its own fixture.
    pub async fn trigger_snapshot_for_testing(&self) -> Result<(), ProposeError> {
        self.raft
            .trigger()
            .snapshot()
            .await
            .map_err(|err| ProposeError::Rejected(Box::new(RaftError::Fatal(err))))
    }

    /// This node's current snapshot, if it has built one: metadata and body.
    ///
    /// A gate that drives an `install_snapshot` stream needs the *same* values
    /// the production sender puts on the wire. Hand-assembling a body would
    /// make the gate a test of its own fixture — the receiver would be
    /// admitting bytes no builder ever produced, and a change to the body
    /// format would leave the gate passing against a shape nothing writes.
    ///
    /// The body is read whole, which the production sender deliberately never
    /// does (§11.3). That is correct here and only here: a test that means to
    /// corrupt a byte or truncate a stream has to hold the bytes. The gates
    /// using it drive snapshots of a handful of kilobytes.
    pub async fn snapshot_for_testing(
        &self,
    ) -> Option<(
        openraft::SnapshotMeta<
            crate::coordination::ids::CoordinationNodeId,
            crate::coordination::typeconfig::CoordinationNode,
        >,
        Vec<u8>,
    )> {
        use std::io::Read;

        let snapshot = self.raft.get_snapshot().await.ok().flatten()?;
        let meta = snapshot.meta.clone();
        let mut reader = snapshot.snapshot.reader().ok()?;
        let mut body = Vec::new();
        reader.read_to_end(&mut body).ok()?;
        Some((meta, body))
    }

    /// This node's current vote.
    ///
    /// The third value in the handoff triple. A stream carrying anything else
    /// is refused by OpenRaft's `accept_vote`, so a gate has to know it to
    /// build a stream that *should* succeed — and to build one that should not.
    pub fn vote_for_testing(&self) -> openraft::Vote<crate::coordination::ids::CoordinationNodeId> {
        self.raft.metrics().borrow().vote
    }

    /// Who this node believes leads, if anyone.
    ///
    /// OpenRaft's committed-vote projection. It carries no liveness — which is
    /// why §7.6 forbids it in the readiness predicate — but it is exactly the
    /// right thing for a harness that needs to *find* the leader in order to
    /// kill it.
    pub fn current_leader_for_testing(
        &self,
    ) -> Option<crate::coordination::ids::CoordinationNodeId> {
        self.raft.metrics().borrow().current_leader
    }

    /// The `Raft` handle itself, for gates that must reach a boundary no
    /// narrower method exposes.
    ///
    /// Deliberately last and deliberately blunt. Prefer the methods above: each
    /// of them names the property it serves, and a gate written against this
    /// one is a gate whose intent is not in its source.
    pub fn raft_for_testing(&self) -> &Raft<CoordinationTypeConfig> {
        &self.raft
    }
}

/// A whole signed group, derived from one seed.
///
/// ## Why this is not left in the test files
///
/// The process-loss gate runs voters in **separate processes**. A parent that
/// built the genesis one way and a child that built it another would be two
/// transcriptions of one document, and the failure would arrive as "the voter
/// is not one of its initial voters" — a message about membership, from a
/// mismatch that is really about the fixture. Every party derives the group
/// from this one type, so there is nothing to disagree about: the child is
/// given an index, and everything else follows.
///
/// It also removes the argument-passing problem. A subprocess needs the signed
/// genesis, the authority key, the domain, the group id and its own secret;
/// passing five values on a command line is five chances to pass them slightly
/// differently. It takes an index.
pub struct GroupFixture {
    domain: String,
    group: [u8; 32],
    authority: ed25519_dalek::SigningKey,
    secrets: Vec<[u8; 32]>,
}

impl GroupFixture {
    /// A group of `size` voters, deterministically derived.
    ///
    /// `seed` separates one fixture from another so two gates running
    /// concurrently do not mint the same node ids and try to bind the same
    /// identities.
    pub fn new(domain: &str, seed: u8, size: usize) -> Self {
        assert!(size > 0 && size <= 8, "a fixture group is 1..=8 voters");
        Self {
            domain: domain.to_string(),
            group: [seed; 32],
            authority: ed25519_dalek::SigningKey::from_bytes(&[seed ^ 0xFF; 32]),
            secrets: (0..size).map(|i| [seed ^ (0x10 + i as u8); 32]).collect(),
        }
    }

    pub fn size(&self) -> usize {
        self.secrets.len()
    }

    pub fn secret(&self, index: usize) -> [u8; 32] {
        self.secrets[index]
    }

    pub fn authority_key(&self) -> [u8; 32] {
        self.authority.verifying_key().to_bytes()
    }

    /// An `AsterConfig` carrying this fixture's ROOT key and voter `index`'s
    /// identity.
    ///
    /// **The ROOT key goes on `AsterConfig`**, which is where a deployment puts
    /// it, and the coordination config does not carry one at all. Every fixture
    /// took the coordination-local route until `resolved_root_key` was made to
    /// require the deployment's — which meant the whole suite exercised a path
    /// no deployment uses, and nothing proved a voter could start from
    /// `ASTER_ROOT_PUBKEY`.
    pub fn aster_config(&self, index: usize) -> crate::AsterConfig {
        crate::AsterConfig::builder()
            .relay(crate::RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(crate::SecretKey::from_bytes(self.secret(index)))
            .root_pubkey(crate::PublicKey::from_bytes(self.authority_key()))
            .build()
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// A signed join permit for `candidate`, naming `senders` as its bootstrap
    /// senders.
    ///
    /// `candidate_ticket` must be the candidate's **own** canonical Open
    /// ticket, because the permit binds it: `verify_join_permit_for_start`
    /// compares it with the ticket the starting node renders for itself, which
    /// is what stops a permit minted for one endpoint being replayed by
    /// another.
    pub fn join_permit_with_ticket(
        &self,
        candidate_secret: [u8; 32],
        candidate_ticket: &str,
        senders: &[usize],
    ) -> Vec<u8> {
        use crate::coordination::model::authority::{
            canonical_document_bytes, join_signing_bytes, signed_document_from_parts,
        };
        use crate::coordination::raft::{JoinPermitV1, NodeIdV1};

        let candidate = ed25519_dalek::SigningKey::from_bytes(&candidate_secret)
            .verifying_key()
            .to_bytes()
            .to_vec();
        let mut bootstrap_senders: Vec<NodeIdV1> = senders
            .iter()
            .map(|i| NodeIdV1 {
                node_id: self.node_id(*i),
            })
            .collect();
        bootstrap_senders.sort_by(|l, r| l.node_id.cmp(&r.node_id));

        let permit = JoinPermitV1 {
            format_version: 1,
            group_id: self.group.to_vec(),
            authority_domain: self.domain.clone(),
            genesis_hash: self.genesis_hash().to_vec(),
            candidate_ticket: candidate_ticket.to_string(),
            candidate_node_id: candidate,
            bootstrap_senders,
            join_nonce: vec![self.group[0] ^ 0x5A; 32],
        };
        let document = canonical_document_bytes(&permit).expect("encode a permit");
        let signature = self.sign_for_testing(&join_signing_bytes(&document));
        signed_document_from_parts(document, &self.authority_key(), &signature)
            .expect("encode a signed permit")
    }

    /// A `Join` configuration for `dir` under this fixture's group.
    ///
    /// `candidate_ticket` is the candidate's **own rendered** ticket, which a
    /// caller learns by starting the endpoint once and reading it. That is not
    /// a test artefact: a permit binds the ticket, so an operator mints one for
    /// a candidate whose address they already have.
    pub fn join_config(
        &self,
        dir: &std::path::Path,
        candidate_secret: [u8; 32],
        candidate_ticket: &str,
        senders: &[usize],
    ) -> crate::coordination::host::CoordinationConfig {
        use crate::coordination::host::{CoordinationConfig, CoordinationStart};

        self.finish(CoordinationConfig::builder().storage_dir(dir).start_mode(
            CoordinationStart::Join {
                signed_join_permit: self.join_permit_with_ticket(
                    candidate_secret,
                    candidate_ticket,
                    senders,
                ),
                expected_deployment_profile_code: self.profile(),
            },
        ))
    }

    /// Bind a storage directory and stop, **without** initializing Raft.
    ///
    /// Reproduces the interrupted bootstrap exactly: a fresh open commits both
    /// identity bindings before `Raft::initialize` runs, so a crash in between
    /// leaves a directory that is bound and has no membership. Nothing else can
    /// construct that state — a test that started a voter and shut it down
    /// after `wait_ready()` has a fully initialized group, which is the case
    /// that was already working.
    ///
    /// It opens through the production `VoterStorage::open` with the production
    /// identity, so the directory it leaves is the one a real crash leaves.
    pub fn bind_without_initializing_for_testing(&self, dir: &std::path::Path, local: &[u8]) {
        use crate::coordination::storage::identity::{OpenPolicy, StorageIdentity};
        use crate::coordination::storage::schema::StorageLayout;
        use crate::coordination::storage::VoterStorage;

        let identity = StorageIdentity::new(
            self.group_id(),
            self.genesis_hash(),
            self.authority_key(),
            self.domain(),
            crate::coordination::ids::CoordinationNodeId::from_slice(local).expect("a node id"),
            self.profile(),
        );
        let storage = VoterStorage::open(
            StorageLayout::new(dir),
            identity,
            OpenPolicy::production().allowing_broad_permissions(),
        )
        .expect("the directory binds");
        drop(storage);
    }

    /// Sign `bytes` with this fixture's ROOT secret.
    ///
    /// For gates that mint a document the fixture's own builders will not — a
    /// genesis naming ROOT as a voter, say. Such a document has to be
    /// *correctly signed* to test the rule that refuses it; an unsigned one
    /// would be refused for the signature and prove nothing.
    pub fn sign_for_testing(&self, bytes: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer;
        self.authority.sign(bytes).to_bytes()
    }

    pub fn node_id(&self, index: usize) -> Vec<u8> {
        ed25519_dalek::SigningKey::from_bytes(&self.secrets[index])
            .verifying_key()
            .to_bytes()
            .to_vec()
    }

    /// Which voter this node id is, if any.
    pub fn index_of(&self, node_id: &[u8]) -> Option<usize> {
        (0..self.size()).find(|i| self.node_id(*i) == node_id)
    }

    /// The §7.1 profile code this group's size requires.
    ///
    /// Derived rather than passed: a fixture that could be handed a profile its
    /// size does not admit would fail at start with a shape complaint.
    pub fn profile(&self) -> i32 {
        match self.size() {
            1 => 2,
            2 => 3,
            _ => 1,
        }
    }

    /// The authority-signed genesis naming every voter.
    pub fn signed_genesis(&self) -> Vec<u8> {
        use crate::coordination::model::authority::{
            canonical_document_bytes, genesis_signing_bytes, signed_document_from_parts,
        };
        use crate::coordination::raft::{GenesisV1, NodeIdV1, VoterSeedV1};
        use ed25519_dalek::Signer;

        let mut initial_voters: Vec<VoterSeedV1> = (0..self.size())
            .map(|i| {
                let node_id = self.node_id(i);
                VoterSeedV1 {
                    ticket: seed_ticket(&node_id),
                    node_id,
                }
            })
            .collect();
        initial_voters.sort_by(|l, r| l.node_id.cmp(&r.node_id));

        let mut initial_admins: Vec<NodeIdV1> = (0..self.size())
            .map(|i| NodeIdV1 {
                node_id: self.node_id(i),
            })
            .collect();
        initial_admins.sort_by(|l, r| l.node_id.cmp(&r.node_id));

        let genesis = GenesisV1 {
            format_version: 1,
            group_id: self.group.to_vec(),
            authority_domain: self.domain.clone(),
            genesis_nonce: vec![self.group[0]; 32],
            deployment_profile_code: self.profile(),
            initial_voters,
            initial_admins,
            recovery_export_hash: None,
        };
        let document = canonical_document_bytes(&genesis).expect("encode a genesis");
        let signature = self.authority.sign(&genesis_signing_bytes(&document));
        signed_document_from_parts(document, &self.authority_key(), &signature.to_bytes())
            .expect("encode a signed genesis")
    }

    /// An authority-signed admission set naming every voter a producer.
    pub fn admission(
        &self,
    ) -> std::sync::Arc<dyn crate::coordination::admission::VerifiedAdmissionSource> {
        use crate::coordination::admission::{
            admission_signing_bytes, AdmissionEntryV1, AdmissionSetV1, SignedAdmissionSet,
            ADMISSION_CLASS_PRODUCER, ADMISSION_FORMAT_VERSION,
        };
        use crate::coordination::model::authority::{
            canonical_document_bytes, signed_document_from_parts,
        };
        use ed25519_dalek::Signer;

        let mut entries: Vec<AdmissionEntryV1> = (0..self.size())
            .map(|i| AdmissionEntryV1 {
                node_id: self.node_id(i),
                class_code: ADMISSION_CLASS_PRODUCER,
            })
            .collect();
        entries.sort_by(|l, r| l.node_id.cmp(&r.node_id));

        let set = AdmissionSetV1 {
            format_version: ADMISSION_FORMAT_VERSION,
            authority_domain: self.domain.clone(),
            revision: 1,
            entries,
        };
        let document = canonical_document_bytes(&set).expect("encode an admission set");
        let signature = self.authority.sign(&admission_signing_bytes(&document));
        let signed =
            signed_document_from_parts(document, &self.authority_key(), &signature.to_bytes())
                .expect("encode a signed admission set");
        std::sync::Arc::new(
            SignedAdmissionSet::verify(&self.authority_key(), &self.domain, &signed)
                .expect("the fixture's own set verifies"),
        )
    }

    pub fn group_id(&self) -> crate::coordination::ids::CoordinationGroupId {
        crate::coordination::ids::CoordinationGroupId::from_array(self.group)
    }

    /// The hash of this fixture's genesis **document**.
    ///
    /// Computed from the fixture's own definition, not read out of a store —
    /// which is what makes it usable as the independent expectation an
    /// `Existing` start is checked against. Reading it from the database being
    /// validated would make that check tautological.
    pub fn genesis_hash(&self) -> [u8; 32] {
        use crate::coordination::raft::SignedCoordinationDocumentV1;

        let signed = self.signed_genesis();
        let envelope: SignedCoordinationDocumentV1 =
            crate::coordination::contract::payload_fory::<SignedCoordinationDocumentV1>()
                .deserialize(&signed)
                .expect("the fixture's own genesis decodes");
        crate::coordination::model::authority::genesis_hash(&envelope.document_bytes)
    }

    /// A **bootstrap** configuration for voter `index` in `dir`.
    ///
    /// For an empty directory only. A voter coming back onto a store it already
    /// owns wants [`existing_config`](Self::existing_config); the two modes are
    /// distinct in §7.2 precisely so that "found a store" and "founded a group"
    /// cannot be confused.
    pub fn config(
        &self,
        dir: &std::path::Path,
        _index: usize,
    ) -> crate::coordination::host::CoordinationConfig {
        use crate::coordination::host::{CoordinationConfig, CoordinationStart};

        self.finish(CoordinationConfig::builder().storage_dir(dir).start_mode(
            CoordinationStart::Bootstrap {
                signed_genesis: self.signed_genesis(),
                recovery_export_path: None,
            },
        ))
    }

    /// An **existing**-store configuration for a directory a previous start
    /// already bound.
    ///
    /// The group id, genesis hash and profile are supplied from the fixture,
    /// independently of the store: they are what the store is checked against.
    pub fn existing_config(
        &self,
        dir: &std::path::Path,
    ) -> crate::coordination::host::CoordinationConfig {
        use crate::coordination::host::{CoordinationConfig, CoordinationStart};

        self.finish(CoordinationConfig::builder().storage_dir(dir).start_mode(
            CoordinationStart::Existing {
                // The same document. A restart may still have to *propose* it:
                // a voter that crashed between `Raft::initialize` and the
                // genesis entry comes back with no administrators and nothing
                // able to appoint one.
                signed_genesis: self.signed_genesis(),
                expected_group_id: self.group_id(),
                expected_genesis_hash: self.genesis_hash(),
                expected_deployment_profile_code: self.profile(),
            },
        ))
    }

    fn finish(
        &self,
        builder: crate::coordination::host::CoordinationConfigBuilder,
    ) -> crate::coordination::host::CoordinationConfig {
        builder
            .authority_domain(&self.domain)
            .verified_admission(self.admission())
            // §7.1 requires each acknowledgement only for the shape it names,
            // and the profile is derived from the group's size above — so
            // offering both is not a way to start a shape that was not asked
            // for.
            .acknowledge_single_voter_development_group(true)
            .acknowledge_even_quorum_development_group(true)
            .build()
            .expect("a complete fixture configuration")
    }
}

/// A 16-byte id from a short label.
///
/// `ClientId` and `OperationId` are `[u8; 16]`, and a command carrying anything
/// else is refused before it reaches any decision — as a *shapeless request*,
/// which commits and replies `(100, 0)`. That is a rejection with no reason
/// code, because §5.2 has none for a request the protocol cannot parse, so a
/// gate that only checked "the proposal returned `Ok`" would see a committed
/// entry, an advancing log, and no change at all. Every fixture in this
/// subsystem mints its ids here so none of them can be the wrong width.
///
/// BLAKE3 rather than zero-padding: labels of different lengths that share a
/// prefix would otherwise collide, and two operations with one id is the case
/// the idempotency path is built to notice.
pub fn id16(label: &[u8]) -> Vec<u8> {
    blake3::hash(label).as_bytes()[..16].to_vec()
}

/// An address-only Open ticket naming `node_id`.
///
/// A genesis names its voters before any of them binds, so a seed ticket cannot
/// carry addresses. §5.4 makes routes advisory dialling hints and identity the
/// handshake's, so this costs a discovery round trip and cannot send a call to
/// the wrong node.
fn seed_ticket(node_id: &[u8]) -> String {
    aster_transport_core::ticket::AsterTicket {
        endpoint_id: node_id.to_vec().try_into().expect("32 bytes"),
        relay: None,
        direct_addrs: Vec::new(),
        credential: Some(aster_transport_core::ticket::TicketCredential::Open),
    }
    .to_base58_string()
    .expect("a canonical ticket")
}
