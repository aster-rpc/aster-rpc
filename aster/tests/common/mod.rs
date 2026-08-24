//! A signed group whose voters run behind the §6.2 fault injector.
//!
//! ## Why this is shared rather than copied
//!
//! Two gates need the same thing: real Aster voters, one signed genesis, one
//! admission set, and a [`ChaosNetwork`] on each voter's Raft client. Written
//! twice they would be two transcriptions of one document, and the first time
//! they drifted the failure would arrive as "this voter is not one of its
//! initial voters" — a message about membership, from a mismatch that is really
//! about the fixture.
//!
//! [`aster::coordination::testing::GroupFixture`] does the same job for gates
//! that do **not** install an injector. It cannot serve here because its
//! configuration builder is finished internally, and `chaos_for_testing` has to
//! be set before that.

#![cfg(feature = "coordination-testing")]
// Each integration binary compiles this module separately and uses a different
// part of it. Without this, the half a binary does not use is a warning, and
// `-D warnings` turns that into a failure that says nothing about the code.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use aster::coordination::admission::{
    admission_signing_bytes, AdmissionEntryV1, AdmissionSetV1, SignedAdmissionSet,
    VerifiedAdmissionSource, ADMISSION_CLASS_PRODUCER, ADMISSION_FORMAT_VERSION,
};
use aster::coordination::host::{CoordinationConfig, CoordinationStart};
use aster::coordination::ids::CoordinationNodeId;
use aster::coordination::model::authority::{
    canonical_document_bytes, genesis_signing_bytes, signed_document_from_parts,
};
use aster::coordination::raft::{GenesisV1, NodeIdV1, VoterSeedV1};
use aster::coordination::testing::{ChaosNetwork, FaultTally, Schedule};
use aster::rpc::AsterServer;
use aster::{AsterConfig, RelayMode, SecretKey};
use ed25519_dalek::{Signer, SigningKey};

/// One running voter, its directory, and the injector its client consults.
pub struct ChaoticVoter {
    pub server: AsterServer,
    pub chaos: Arc<ChaosNetwork>,
    pub id: CoordinationNodeId,
    _dir: tempfile::TempDir,
}

impl ChaoticVoter {
    pub fn coordination(&self) -> aster::coordination::Coordination {
        self.server.coordination().expect("hosted")
    }

    /// The term this voter is currently voting in.
    ///
    /// The independent half of "leadership moved": a successor at the same term
    /// would be the same leadership under another name.
    pub fn term(&self) -> u64 {
        self.coordination()
            .vote_for_testing()
            .leader_id()
            .get_term()
    }
}

/// A whole signed group, derived from one seed, whose voters carry injectors.
pub struct ChaosFixture {
    domain: String,
    group: [u8; 32],
    authority: SigningKey,
    secrets: Vec<[u8; 32]>,
    /// Candidates outside the genesis that the signed admission set still
    /// names. §5.3 checks the admission class **before** the membership
    /// allowlist, so a joiner missing from the set is refused for a reason that
    /// has nothing to do with the rule under test.
    admitted: Vec<[u8; 32]>,
}

impl ChaosFixture {
    /// A group of `size` voters. `seed` separates one fixture from another so
    /// two gates running concurrently do not mint the same node ids.
    pub fn new(domain: &str, seed: u8, size: usize) -> Self {
        assert!((1..=8).contains(&size), "a fixture group is 1..=8 voters");
        Self {
            domain: domain.to_string(),
            group: [seed; 32],
            authority: SigningKey::from_bytes(&[seed ^ 0xFF; 32]),
            secrets: (0..size).map(|i| [seed ^ (0x10 + i as u8); 32]).collect(),
            admitted: Vec::new(),
        }
    }

    /// Also name `candidate` in the signed admission set.
    pub fn admitting(mut self, candidate: [u8; 32]) -> Self {
        self.admitted.push(candidate);
        self
    }

    pub fn size(&self) -> usize {
        self.secrets.len()
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    pub fn authority_key(&self) -> [u8; 32] {
        self.authority.verifying_key().to_bytes()
    }

    pub fn node_id(&self, index: usize) -> Vec<u8> {
        SigningKey::from_bytes(&self.secrets[index])
            .verifying_key()
            .to_bytes()
            .to_vec()
    }

    pub fn id(&self, index: usize) -> CoordinationNodeId {
        CoordinationNodeId::from_slice(&self.node_id(index)).expect("a 32-byte node id")
    }

    pub fn secret(&self, index: usize) -> [u8; 32] {
        self.secrets[index]
    }

    /// The §7.1 profile code this group's size requires.
    pub fn profile(&self) -> i32 {
        match self.size() {
            1 => 2,
            2 => 3,
            _ => 1,
        }
    }

    /// An address-only Open ticket naming `node_id`.
    ///
    /// A genesis names its voters before any of them binds, so a seed ticket
    /// cannot carry addresses: §5.4 makes routes advisory dialling hints and
    /// identity the handshake's.
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

    pub fn signed_genesis(&self) -> Vec<u8> {
        let mut initial_voters: Vec<VoterSeedV1> = (0..self.size())
            .map(|i| {
                let node_id = self.node_id(i);
                VoterSeedV1 {
                    ticket: Self::seed_ticket(&node_id),
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

    pub fn admission(&self) -> Arc<dyn VerifiedAdmissionSource> {
        let mut entries: Vec<AdmissionEntryV1> = (0..self.size())
            .map(|i| self.node_id(i))
            .chain(self.admitted.iter().map(|secret| {
                SigningKey::from_bytes(secret)
                    .verifying_key()
                    .to_bytes()
                    .to_vec()
            }))
            .map(|node_id| AdmissionEntryV1 {
                node_id,
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
        Arc::new(
            SignedAdmissionSet::verify(&self.authority_key(), &self.domain, &signed)
                .expect("the fixture's own set verifies"),
        )
    }

    /// The hash of this fixture's genesis **document**.
    pub fn genesis_hash(&self) -> [u8; 32] {
        use aster::coordination::raft::SignedCoordinationDocumentV1;
        let signed = self.signed_genesis();
        let envelope: SignedCoordinationDocumentV1 =
            aster::coordination::contract::payload_fory::<SignedCoordinationDocumentV1>()
                .deserialize(&signed)
                .expect("the fixture's own genesis decodes");
        aster::coordination::model::authority::genesis_hash(&envelope.document_bytes)
    }

    /// A signed join permit for `candidate`, naming `senders` as the **only**
    /// bootstrap senders it will accept Raft from before graduation.
    ///
    /// The narrowness is the point for
    /// `a_joiner_survives_leadership_moving_outside_its_bootstrap_senders`:
    /// §7.2's seed allowlist is exactly this list, and the group's leader can
    /// move to a voter that is not on it.
    pub fn join_permit(&self, candidate: [u8; 32], senders: &[usize]) -> Vec<u8> {
        use aster::coordination::model::authority::{
            canonical_document_bytes, join_signing_bytes, signed_document_from_parts,
        };
        use aster::coordination::raft::JoinPermitV1;

        let candidate_key = SigningKey::from_bytes(&candidate)
            .verifying_key()
            .to_bytes();
        let candidate_id = candidate_key.to_vec();
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
            candidate_ticket: aster::coordination::model::authority::canonical_candidate_ticket(
                &candidate_key,
            )
            .expect("a canonical candidate ticket"),
            candidate_node_id: candidate_id,
            bootstrap_senders,
            join_nonce: vec![self.group[0] ^ 0x5A; 32],
        };
        let document = canonical_document_bytes(&permit).expect("encode a permit");
        let signature = self.authority.sign(&join_signing_bytes(&document));
        signed_document_from_parts(document, &self.authority_key(), &signature.to_bytes())
            .expect("encode a signed permit")
    }

    /// Start a **candidate**: a node outside the genesis, joining on a permit.
    ///
    /// It carries an injector like every other node here, so a gate can cut it
    /// off or cut off the voter it was told to talk to.
    pub async fn start_joiner(
        &self,
        candidate: [u8; 32],
        senders: &[usize],
        schedule: Schedule,
    ) -> ChaoticVoter {
        let dir = tempfile::tempdir().expect("a storage directory");
        let chaos = ChaosNetwork::new(schedule);
        let config = CoordinationConfig::builder()
            .storage_dir(dir.path())
            .authority_domain(&self.domain)
            .start_mode(CoordinationStart::Join {
                signed_join_permit: self.join_permit(candidate, senders),
                expected_deployment_profile_code: self.profile(),
            })
            .verified_admission(self.admission())
            .chaos_for_testing(Arc::clone(&chaos))
            .build()
            .expect("a complete configuration");
        let server = AsterServer::builder()
            .config(
                AsterConfig::builder()
                    .relay(RelayMode::Disabled)
                    .bind_addr("127.0.0.1:0")
                    .secret_key(SecretKey::from_bytes(candidate))
                    .root_pubkey(aster::PublicKey::from_bytes(self.authority_key()))
                    .build(),
            )
            .builtin_coordination(config)
            .start()
            .await
            .expect("a candidate joins with a valid permit");
        ChaoticVoter {
            server,
            chaos,
            id: CoordinationNodeId::from_slice(
                SigningKey::from_bytes(&candidate)
                    .verifying_key()
                    .as_bytes(),
            )
            .expect("a 32-byte node id"),
            _dir: dir,
        }
    }

    /// Start every voter, each behind its own injector running `schedule`, and
    /// register the peers' current addresses with each other.
    ///
    /// The address registration is the same `add_node_addr` substitution for
    /// discovery the other same-host cluster gates use: a voter binds an
    /// ephemeral port, so nothing signed can name where it is, and the group
    /// finds each other by NodeId.
    pub async fn start(&self, schedule: Schedule) -> Vec<ChaoticVoter> {
        let genesis = self.signed_genesis();
        let admission = self.admission();

        let mut voters = Vec::new();
        for index in 0..self.size() {
            let dir = tempfile::tempdir().expect("a storage directory");
            let chaos = ChaosNetwork::new(schedule);
            let config = CoordinationConfig::builder()
                .storage_dir(dir.path())
                .authority_domain(&self.domain)
                .start_mode(CoordinationStart::Bootstrap {
                    signed_genesis: genesis.clone(),
                    recovery_export_path: None,
                })
                .verified_admission(Arc::clone(&admission))
                .chaos_for_testing(Arc::clone(&chaos))
                .build()
                .expect("a complete configuration");
            let server = AsterServer::builder()
                .config(
                    AsterConfig::builder()
                        .relay(RelayMode::Disabled)
                        .bind_addr("127.0.0.1:0")
                        .secret_key(SecretKey::from_bytes(self.secret(index)))
                        .root_pubkey(aster::PublicKey::from_bytes(self.authority_key()))
                        .build(),
                )
                .builtin_coordination(config)
                .start()
                .await
                .expect("a voter starts");
            voters.push(ChaoticVoter {
                server,
                chaos,
                id: self.id(index),
                _dir: dir,
            });
        }

        let addrs: Vec<_> = voters.iter().map(|v| v.server.node().addr()).collect();
        for voter in &voters {
            for addr in &addrs {
                if addr.node_id != voter.server.node().addr().node_id {
                    voter
                        .server
                        .node()
                        .add_node_addr(addr.clone())
                        .expect("register a peer address");
                }
            }
        }
        voters
    }
}

/// Tell every node in `group` about every other's current address.
///
/// The same `add_node_addr` substitution for discovery the other same-host
/// cluster gates use: a voter binds an ephemeral port, so nothing signed can
/// name where it is, and the group finds each other by NodeId.
pub fn cross_register(group: &[&ChaoticVoter]) {
    let addrs: Vec<_> = group.iter().map(|v| v.server.node().addr()).collect();
    for voter in group {
        for addr in &addrs {
            if addr.node_id != voter.server.node().addr().node_id {
                voter
                    .server
                    .node()
                    .add_node_addr(addr.clone())
                    .expect("register a peer address");
            }
        }
    }
}

/// Wall-clock phase timing for a chaos gate, printed on every mark.
///
/// `S3-CHAOS-WALLTIME` observed identical code finish in 27 s and 629 s. The
/// suite's assertions cannot say where such a run spent its time, and a gate
/// whose duration swings by an order of magnitude will eventually be blamed on
/// CI rather than investigated. Every gate therefore narrates its phases; the
/// output costs nothing when nobody is asking and is the whole investigation
/// when somebody is.
pub struct PhaseClock {
    gate: &'static str,
    start: std::time::Instant,
    last: std::time::Instant,
}

impl PhaseClock {
    pub fn start(gate: &'static str) -> Self {
        let now = std::time::Instant::now();
        Self {
            gate,
            start: now,
            last: now,
        }
    }

    /// Record that `phase` just finished.
    pub fn mark(&mut self, phase: &str) {
        let now = std::time::Instant::now();
        println!(
            "[chaos-timing] {}: {} took {:.2}s (total {:.2}s)",
            self.gate,
            phase,
            now.duration_since(self.last).as_secs_f64(),
            now.duration_since(self.start).as_secs_f64(),
        );
        self.last = now;
    }
}

/// The applied-fault totals across every voter.
pub fn combined_tally(voters: &[ChaoticVoter]) -> FaultTally {
    voters
        .iter()
        .map(|v| v.chaos.tally())
        .fold(FaultTally::default(), |mut acc, t| {
            acc.delivered += t.delivered;
            acc.dropped += t.dropped;
            acc.delayed += t.delayed;
            acc.duplicated += t.duplicated;
            acc.reordered += t.reordered;
            acc.reorder_expired += t.reorder_expired;
            acc.partitioned += t.partitioned;
            acc
        })
}

/// Cut `node` off from every voter, in both directions.
///
/// Told to **every** injector, because a cut only one side observes is a
/// one-way link — a different fault, and one no consensus protocol promises
/// anything about.
pub fn isolate_everywhere(voters: &[ChaoticVoter], node: CoordinationNodeId) {
    for voter in voters {
        voter.chaos.isolate(node);
    }
}

pub fn rejoin_everywhere(voters: &[ChaoticVoter], node: &CoordinationNodeId) {
    for voter in voters {
        voter.chaos.rejoin(node);
    }
}

/// Wait for a quorum-acknowledged leader that is **not** one of `excluding`.
///
/// The exclusion is what makes "leadership moved" observable: a gate that
/// simply waited for a leader would accept the one it had just tried to depose,
/// and would then be asserting nothing about failover at all.
///
/// `quorum_available` is `Some` only on a leader — a follower holds no direct
/// evidence of a quorum — so this finds the leader rather than any ready node.
pub async fn await_leader_other_than(
    voters: &[ChaoticVoter],
    excluding: &[CoordinationNodeId],
    patience: Duration,
    what: &str,
) -> usize {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        for (index, voter) in voters.iter().enumerate() {
            if excluding.contains(&voter.id) {
                continue;
            }
            let coordination = voter.coordination();
            if coordination.lifecycle_code() == 3 && coordination.quorum_available() == Some(true) {
                return index;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let observed: Vec<(i32, Option<bool>)> = voters
                .iter()
                .map(|v| {
                    let c = v.coordination();
                    (c.lifecycle_code(), c.quorum_available())
                })
                .collect();
            panic!(
                "{what}: no leader within {patience:?}; observed {observed:?}, faults {:?}",
                combined_tally(voters)
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub async fn await_leader(voters: &[ChaoticVoter], patience: Duration, what: &str) -> usize {
    await_leader_other_than(voters, &[], patience, what).await
}

/// **Every voter's Raft core is still running.**
///
/// A Raft actor that dies does not fail a test on its own: the task is gone,
/// the survivors carry the group, and a gate that only asked whether the group
/// made progress passes with a voter's consensus machinery destroyed. That is
/// not hypothetical. This assertion found the pinned OpenRaft's
/// `ReplicationHandler::append_membership` `debug_assert` firing on 6 runs out
/// of 6, killing a voter's `RaftCore` while the membership gate went on
/// passing — and the cause was **this crate**, not OpenRaft: the controller
/// read `RaftMetrics::membership_config`, which is the *effective* membership,
/// so a leader that an unaccepted entry had already demoted kept driving the
/// corridor. Reading the committed membership and ending a generation once its
/// own removal commits took it to zero.
///
/// Asked of the core rather than of its metrics, which simply stop changing:
/// `is_initialized` is a round trip through `RaftCore`, so a dead one answers
/// `Fatal` instead of a stale `Ok`.
pub async fn every_core_is_alive(voters: &[ChaoticVoter], what: &str) {
    for (index, voter) in voters.iter().enumerate() {
        let alive = voter
            .coordination()
            .raft_for_testing()
            .is_initialized()
            .await;
        assert!(
            alive.is_ok(),
            "{what}: voter {index}'s Raft core is gone ({alive:?}) — the group may still \
             have made progress, but it did so with a voter destroyed"
        );
    }
}

pub async fn shutdown_all(voters: Vec<ChaoticVoter>) {
    for voter in voters {
        voter.server.shutdown().await;
    }
}
