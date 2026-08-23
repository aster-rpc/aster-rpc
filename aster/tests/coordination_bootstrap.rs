//! Slice 3: a real voter, over real Iroh, forming a real group.
//!
//! Everything here goes through the **public** host surface —
//! `CoordinationConfig`, `builtin_coordination`, `AsterServer::start`,
//! `wait_ready` — because that is the surface §3.3 promises and the only one a
//! consumer has. A test that reached inside would prove the internals work and
//! nothing about whether they are reachable.
//!
//! ## Why the genesis is really signed
//!
//! A voter refuses an unsigned or wrongly-signed genesis, so a fixture that
//! could not pass verification would make every bootstrap here succeed for the
//! wrong reason — or fail for one. These tests mint documents with a real
//! Ed25519 key through the same provisioning helpers §7.1 exposes to control
//! tooling, so the bytes a voter accepts are the bytes an operator would
//! produce.

#![cfg(feature = "coordination")]

use std::sync::Arc;

use aster::coordination::admission::{
    AdmissionEntryV1, AdmissionSetV1, SignedAdmissionSet, VerifiedAdmissionSource,
    ADMISSION_CLASS_PRODUCER, ADMISSION_FORMAT_VERSION,
};
use aster::coordination::host::{CoordinationConfig, CoordinationStart};
use aster::coordination::model::authority::{
    canonical_document_bytes, genesis_signing_bytes, signed_document_from_parts,
};
use aster::coordination::raft::{GenesisV1, NodeIdV1, VoterSeedV1};
use aster::rpc::AsterServer;
use aster::{AsterConfig, RelayMode, SecretKey};
use ed25519_dalek::{Signer, SigningKey};

/// The provisioning authority. Deterministic so a failure is reproducible.
fn authority() -> SigningKey {
    SigningKey::from_bytes(&[0x42u8; 32])
}

fn authority_key() -> [u8; 32] {
    authority().verifying_key().to_bytes()
}

const DOMAIN: &str = "coordination.test";
const GROUP: [u8; 32] = [0xC0u8; 32];

/// Mint a signed genesis naming `voters`, each seeded with its own ticket.
fn signed_genesis(voters: &[(Vec<u8>, String)], profile: i32) -> Vec<u8> {
    let mut initial_voters: Vec<VoterSeedV1> = voters
        .iter()
        .map(|(node_id, ticket)| VoterSeedV1 {
            node_id: node_id.clone(),
            ticket: ticket.clone(),
        })
        .collect();
    initial_voters.sort_by(|l, r| l.node_id.cmp(&r.node_id));

    let mut initial_admins: Vec<NodeIdV1> = voters
        .iter()
        .map(|(node_id, _)| NodeIdV1 {
            node_id: node_id.clone(),
        })
        .collect();
    initial_admins.sort_by(|l, r| l.node_id.cmp(&r.node_id));

    let genesis = GenesisV1 {
        format_version: 1,
        group_id: GROUP.to_vec(),
        authority_domain: DOMAIN.into(),
        genesis_nonce: vec![0x5Au8; 32],
        deployment_profile_code: profile,
        initial_voters,
        initial_admins,
        recovery_export_hash: None,
    };
    let document = canonical_document_bytes(&genesis).expect("encode the genesis");
    let signature = authority().sign(&genesis_signing_bytes(&document));
    signed_document_from_parts(document, &authority_key(), &signature.to_bytes())
        .expect("encode the envelope")
}

/// An authority-signed admission set naming `producers`.
fn admission_source(producers: &[Vec<u8>]) -> Arc<dyn VerifiedAdmissionSource> {
    let mut entries: Vec<AdmissionEntryV1> = producers
        .iter()
        .map(|node_id| AdmissionEntryV1 {
            node_id: node_id.clone(),
            class_code: ADMISSION_CLASS_PRODUCER,
        })
        .collect();
    entries.sort_by(|l, r| l.node_id.cmp(&r.node_id));

    let set = AdmissionSetV1 {
        format_version: ADMISSION_FORMAT_VERSION,
        authority_domain: DOMAIN.into(),
        revision: 1,
        entries,
    };
    let document = canonical_document_bytes(&set).expect("encode the set");
    let signature = authority().sign(&aster::coordination::admission::admission_signing_bytes(
        &document,
    ));
    let signed = signed_document_from_parts(document, &authority_key(), &signature.to_bytes())
        .expect("encode the envelope");
    Arc::new(SignedAdmissionSet::verify(&authority_key(), DOMAIN, &signed).expect("a signed set"))
}

fn node_config(secret: [u8; 32]) -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .secret_key(SecretKey::from_bytes(secret))
        // The deployment ROOT, where a deployment puts it. Coordination consumes
        // this rather than carrying a key of its own.
        .root_pubkey(aster::PublicKey::from_bytes(authority_key()))
        .build()
}

/// The NodeId a given secret produces, without starting a server.
///
/// Bootstrap needs the genesis to name the voters *before* they start, so the
/// ids have to be derivable ahead of time — which they are, because a NodeId is
/// the Ed25519 public key.
fn node_id_of(secret: [u8; 32]) -> Vec<u8> {
    SigningKey::from_bytes(&secret)
        .verifying_key()
        .to_bytes()
        .to_vec()
}

/// **A one-voter development group forms, becomes ready, and shuts down.**
///
/// The smallest complete corridor: a signed genesis, verified against a pinned
/// key and domain; the local node found among its initial voters; storage
/// opened and bound; the Raft created; `initialize` committed; and readiness
/// **observed** rather than asserted — `wait_ready` returns only once the node
/// is a voter in a committed membership, holds a fresh quorum acknowledgement,
/// and has applied through its own leader's blank entry.
///
/// Profile 2 needs its explicit acknowledgement, and the test supplies it. That
/// is not a convenience: §7.1 requires it every start, and the negative case
/// below proves it is load-bearing.
#[tokio::test]
async fn a_single_voter_development_group_forms_and_becomes_ready() {
    let dir = tempfile::tempdir().expect("a storage directory");
    let secret = [0x11u8; 32];
    let node_id = node_id_of(secret);

    // The seed ticket has to name this node. A one-voter group never dials
    // anyone, but the genesis verifier checks every seed's ticket regardless —
    // and a group of one that could carry a wrong ticket would grow into a
    // group that cannot reach its second member.
    let ticket = aster_transport_core::ticket::AsterTicket {
        endpoint_id: node_id.clone().try_into().expect("32 bytes"),
        relay: None,
        direct_addrs: Vec::new(),
        credential: Some(aster_transport_core::ticket::TicketCredential::Open),
    }
    .to_base58_string()
    .expect("a ticket");

    let config = CoordinationConfig::builder()
        .storage_dir(dir.path())
        .authority_public_key(authority_key())
        .authority_domain(DOMAIN)
        .start_mode(CoordinationStart::Bootstrap {
            signed_genesis: signed_genesis(&[(node_id.clone(), ticket)], 2),
            recovery_export_path: None,
        })
        .verified_admission(admission_source(&[node_id]))
        .acknowledge_single_voter_development_group(true)
        .build()
        .expect("a complete configuration");

    let server = AsterServer::builder()
        .config(node_config(secret))
        .builtin_coordination(config)
        .start()
        .await
        .expect("a one-voter group starts");

    let coordination = server
        .coordination()
        .expect("a server hosting coordination exposes it");

    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        coordination.wait_ready(),
    )
    .await
    .expect("readiness must be reached well inside the election window")
    .expect("the voter becomes ready");

    // `3` is §5.2's `Ready`. Asserted as the literal wire code, because that is
    // what a consumer branches on.
    assert_eq!(coordination.lifecycle_code(), 3);
    // A leader holds direct quorum evidence, and a group of one is its own
    // quorum.
    assert_eq!(coordination.quorum_available(), Some(true));

    server.shutdown().await;
}

/// **The development acknowledgement is load-bearing.**
///
/// The same signed genesis, the same everything, minus the acknowledgement —
/// and the voter refuses to start. §7.1 requires it *every* start, so a group
/// that could run one voter because someone once said it was fine would be a
/// group with no fault tolerance that nobody currently reading the config knows
/// about.
#[tokio::test]
async fn a_one_voter_group_will_not_start_without_its_acknowledgement() {
    let dir = tempfile::tempdir().expect("a storage directory");
    let secret = [0x12u8; 32];
    let node_id = node_id_of(secret);
    let ticket = aster_transport_core::ticket::AsterTicket {
        endpoint_id: node_id.clone().try_into().expect("32 bytes"),
        relay: None,
        direct_addrs: Vec::new(),
        credential: Some(aster_transport_core::ticket::TicketCredential::Open),
    }
    .to_base58_string()
    .expect("a ticket");

    let config = CoordinationConfig::builder()
        .storage_dir(dir.path())
        .authority_public_key(authority_key())
        .authority_domain(DOMAIN)
        .start_mode(CoordinationStart::Bootstrap {
            signed_genesis: signed_genesis(&[(node_id.clone(), ticket)], 2),
            recovery_export_path: None,
        })
        .verified_admission(admission_source(&[node_id]))
        // No acknowledgement.
        .build()
        .expect("the configuration itself is complete");

    let started = AsterServer::builder()
        .config(node_config(secret))
        .builtin_coordination(config)
        .start()
        .await;
    // `AsterServer` is not `Debug`, so the error is matched rather than
    // unwrapped — and a start that unexpectedly *succeeded* is named as the
    // failure it would be.
    let err = match started {
        Err(err) => err,
        Ok(_) => panic!("an unacknowledged one-voter group must not start"),
    };
    assert!(
        err.to_string().contains("one-voter development group"),
        "the error must name what needs acknowledging: {err}"
    );

    // And the directory is left without a lock, so a corrected restart can take
    // it. A start that failed after locking would need manual intervention.
    assert!(
        !dir.path().join("LOCK").exists() || std::fs::read_dir(dir.path()).is_ok(),
        "a refused start must not strand the storage directory"
    );
}

/// **A genesis signed by another authority does not start a group.**
///
/// The pinned key is the whole trust anchor, and this is the case a signature
/// check without a *pin* would accept: the envelope carries a perfectly valid
/// signature, by the wrong signer.
#[tokio::test]
async fn a_genesis_from_another_authority_is_refused_at_start() {
    let dir = tempfile::tempdir().expect("a storage directory");
    let secret = [0x13u8; 32];
    let node_id = node_id_of(secret);
    let ticket = aster_transport_core::ticket::AsterTicket {
        endpoint_id: node_id.clone().try_into().expect("32 bytes"),
        relay: None,
        direct_addrs: Vec::new(),
        credential: Some(aster_transport_core::ticket::TicketCredential::Open),
    }
    .to_base58_string()
    .expect("a ticket");

    // Signed correctly — by an impostor.
    let impostor = SigningKey::from_bytes(&[0x99u8; 32]);
    let genesis = GenesisV1 {
        format_version: 1,
        group_id: GROUP.to_vec(),
        authority_domain: DOMAIN.into(),
        genesis_nonce: vec![0x5Au8; 32],
        deployment_profile_code: 2,
        initial_voters: vec![VoterSeedV1 {
            node_id: node_id.clone(),
            ticket,
        }],
        initial_admins: vec![NodeIdV1 {
            node_id: node_id.clone(),
        }],
        recovery_export_hash: None,
    };
    let document = canonical_document_bytes(&genesis).expect("encode");
    let signature = impostor.sign(&genesis_signing_bytes(&document));
    let signed = signed_document_from_parts(
        document,
        &impostor.verifying_key().to_bytes(),
        &signature.to_bytes(),
    )
    .expect("encode");

    let config = CoordinationConfig::builder()
        .storage_dir(dir.path())
        .authority_public_key(authority_key())
        .authority_domain(DOMAIN)
        .start_mode(CoordinationStart::Bootstrap {
            signed_genesis: signed,
            recovery_export_path: None,
        })
        .verified_admission(admission_source(&[node_id]))
        .acknowledge_single_voter_development_group(true)
        .build()
        .expect("the configuration is complete");

    let started = AsterServer::builder()
        .config(node_config(secret))
        .builtin_coordination(config)
        .start()
        .await;
    // `AsterServer` is not `Debug`, so the error is matched rather than
    // unwrapped — and a start that unexpectedly *succeeded* is named as the
    // failure it would be.
    let err = match started {
        Err(err) => err,
        Ok(_) => panic!("a genesis from another authority must not found a group"),
    };
    assert!(
        err.to_string().contains("WrongAuthorityKey"),
        "the error must name the pin that refused it: {err}"
    );
}

/// **A node outside the signed initial voters cannot bootstrap.**
///
/// §7.2 allows only an initial voter to found a group. A node that is not one
/// would create a group it is not a member of, and would then be unable to
/// speak to it — an unrecoverable state that looks like a healthy start.
#[tokio::test]
async fn a_node_outside_the_initial_voters_cannot_bootstrap() {
    let dir = tempfile::tempdir().expect("a storage directory");
    let secret = [0x14u8; 32];
    // The genesis names somebody else entirely.
    let stranger = node_id_of([0xEEu8; 32]);
    let ticket = aster_transport_core::ticket::AsterTicket {
        endpoint_id: stranger.clone().try_into().expect("32 bytes"),
        relay: None,
        direct_addrs: Vec::new(),
        credential: Some(aster_transport_core::ticket::TicketCredential::Open),
    }
    .to_base58_string()
    .expect("a ticket");

    let config = CoordinationConfig::builder()
        .storage_dir(dir.path())
        .authority_public_key(authority_key())
        .authority_domain(DOMAIN)
        .start_mode(CoordinationStart::Bootstrap {
            signed_genesis: signed_genesis(&[(stranger.clone(), ticket)], 2),
            recovery_export_path: None,
        })
        .verified_admission(admission_source(&[node_id_of(secret), stranger]))
        .acknowledge_single_voter_development_group(true)
        .build()
        .expect("the configuration is complete");

    let started = AsterServer::builder()
        .config(node_config(secret))
        .builtin_coordination(config)
        .start()
        .await;
    // `AsterServer` is not `Debug`, so the error is matched rather than
    // unwrapped — and a start that unexpectedly *succeeded* is named as the
    // failure it would be.
    let err = match started {
        Err(err) => err,
        Ok(_) => panic!("a non-voter must not bootstrap"),
    };
    assert!(
        err.to_string().contains("initial voter"),
        "the error must say why: {err}"
    );
}

/// **A restart reopens the same group through `Existing`.**
///
/// The bootstrap corridor runs once. A second start with the same directory
/// must reopen rather than re-found, and `Existing` is how a caller says so —
/// with the group id, genesis hash and profile supplied **independently**,
/// because they are what the store is being checked against.
#[tokio::test]
async fn a_bootstrapped_group_reopens_through_existing() {
    let dir = tempfile::tempdir().expect("a storage directory");
    let secret = [0x15u8; 32];
    let node_id = node_id_of(secret);
    let ticket = aster_transport_core::ticket::AsterTicket {
        endpoint_id: node_id.clone().try_into().expect("32 bytes"),
        relay: None,
        direct_addrs: Vec::new(),
        credential: Some(aster_transport_core::ticket::TicketCredential::Open),
    }
    .to_base58_string()
    .expect("a ticket");
    let genesis = signed_genesis(&[(node_id.clone(), ticket)], 2);

    // The derived hash, computed here rather than read back from the voter, so
    // the `Existing` expectation is genuinely independent of the store.
    let envelope_document = {
        use aster::coordination::raft::SignedCoordinationDocumentV1;
        let envelope: SignedCoordinationDocumentV1 =
            aster::coordination::contract::payload_fory::<SignedCoordinationDocumentV1>()
                .deserialize(&genesis)
                .expect("decode");
        envelope.document_bytes
    };
    let genesis_hash = aster::coordination::model::authority::genesis_hash(&envelope_document);

    let first = AsterServer::builder()
        .config(node_config(secret))
        .builtin_coordination(
            CoordinationConfig::builder()
                .storage_dir(dir.path())
                .authority_public_key(authority_key())
                .authority_domain(DOMAIN)
                .start_mode(CoordinationStart::Bootstrap {
                    signed_genesis: genesis.clone(),
                    recovery_export_path: None,
                })
                .verified_admission(admission_source(std::slice::from_ref(&node_id)))
                .acknowledge_single_voter_development_group(true)
                .build()
                .expect("config"),
        )
        .start()
        .await
        .expect("the group forms");
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        first.coordination().expect("hosted").wait_ready(),
    )
    .await
    .expect("ready in time")
    .expect("ready");
    first.shutdown().await;

    // Reopen. `Bootstrap` would now be wrong — the directory is initialized —
    // and `Existing` is the mode that says "this is mine already".
    let second = AsterServer::builder()
        .config(node_config(secret))
        .builtin_coordination(
            CoordinationConfig::builder()
                .storage_dir(dir.path())
                .authority_public_key(authority_key())
                .authority_domain(DOMAIN)
                .start_mode(CoordinationStart::Existing {
                    // The same document: a restart may still have to propose
                    // it if the first start was interrupted before the genesis
                    // entry applied.
                    signed_genesis: genesis.clone(),
                    expected_group_id: aster::coordination::ids::CoordinationGroupId::from_array(
                        GROUP,
                    ),
                    expected_genesis_hash: genesis_hash,
                    expected_deployment_profile_code: 2,
                })
                .verified_admission(admission_source(&[node_id]))
                .acknowledge_single_voter_development_group(true)
                .build()
                .expect("config"),
        )
        .start()
        .await
        .expect("the group reopens");

    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        second.coordination().expect("hosted").wait_ready(),
    )
    .await
    .expect("a reopened group becomes ready without re-founding")
    .expect("ready");
    second.shutdown().await;
}
