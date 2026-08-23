//! The coordination services exist only when a host asked for them, refuse an
//! unauthenticatable transport, carry their §12.3 frame caps, and demand a
//! restart-stable identity.
//!
//! Slice 0 wrote these against two *reservations*. Slice 3 replaced the Raft
//! reservation with a live dispatcher, and the properties are unchanged: the
//! name is still taken, the caps are still installed before any handler runs,
//! and `iroh_only` is still on. What changed is the answer a configured,
//! authenticated caller gets — `FailedPrecondition` from a node whose runtime
//! has not attached, rather than `UNIMPLEMENTED` from a reservation — and the
//! tests assert the new one rather than being relaxed to accept either.
//!
//! Three of these are absence proofs, which are the easiest kind of test to
//! write vacuously. Each is therefore paired with the positive case that would
//! fail if the mechanism it claims to exercise were broken:
//!
//! * "absent unless configured" is paired with a configured server answering
//!   from the reserved dispatcher, so a test that passed because *nothing*
//!   answers would fail its partner;
//! * dispatch rejects an unregistered service and an `iroh_only` refusal with
//!   the byte-identical `"not found"` trailer -- deliberately, since over an
//!   unauthenticated transport the service's existence is not something to
//!   confirm -- so the reserved dispatcher's own trailer text is the only
//!   evidence that it ran at all.

#![cfg(feature = "coordination")]

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use aster::coordination::host::{CoordinationConfig, CoordinationStart};
use aster::coordination::model::authority::{
    canonical_document_bytes, genesis_signing_bytes, signed_document_from_parts,
};
use aster::coordination::raft::{GenesisV1, NodeIdV1, VoterSeedV1};
use aster::coordination::service::{RAFT_SERVICE, STORE_SERVICE};
use aster::rpc::{
    AsterServer, CallParts, Dispatcher, OutgoingFrame, PeerProvenance, RequestFrame, StreamHeader,
};
use aster::{AsterConfig, RelayMode, SecretKey};
use aster_transport_core::framing::FLAG_END_STREAM;
use aster_transport_core::reactor::{response_channel, FrameLimit};
use ed25519_dalek::{Signer, SigningKey};
use tokio::sync::mpsc;

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .root_pubkey(aster::PublicKey::from_bytes(
            authority().verifying_key().to_bytes(),
        ))
        .build()
}

/// A real one-voter configuration for `dir`, signed by a real authority.
///
/// Slice 0 used a config that never opened storage, because
/// `builtin_coordination` only registered reservations. It now starts a voter,
/// so these tests start one — which is strictly better evidence: the dispatcher
/// they inspect is the dispatcher a running group serves, not one assembled for
/// a test.
fn coordination_config(dir: &std::path::Path, secret: [u8; 32]) -> CoordinationConfig {
    let node_id = SigningKey::from_bytes(&secret)
        .verifying_key()
        .to_bytes()
        .to_vec();
    let ticket = aster_transport_core::ticket::AsterTicket {
        endpoint_id: node_id.clone().try_into().expect("32 bytes"),
        relay: None,
        direct_addrs: Vec::new(),
        credential: Some(aster_transport_core::ticket::TicketCredential::Open),
    }
    .to_base58_string()
    .expect("a ticket");

    let genesis = GenesisV1 {
        format_version: 1,
        group_id: [0xC0u8; 32].to_vec(),
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
    let signature = authority().sign(&genesis_signing_bytes(&document));
    let signed = signed_document_from_parts(
        document,
        &authority().verifying_key().to_bytes(),
        &signature.to_bytes(),
    )
    .expect("encode");

    CoordinationConfig::builder()
        .storage_dir(dir)
        .authority_public_key(authority().verifying_key().to_bytes())
        .authority_domain(DOMAIN)
        .start_mode(CoordinationStart::Bootstrap {
            signed_genesis: signed,
            recovery_export_path: None,
        })
        .verified_admission(Arc::new(NoAdmissions))
        .acknowledge_single_voter_development_group(true)
        .build()
        .expect("build coordination config")
}

/// The `Existing` counterpart of [`coordination_config`], for a directory a
/// previous start already bound.
///
/// The group id, genesis hash and profile are supplied **independently** of the
/// store, which is the point of the mode: they are what the store is checked
/// against, so reading them from it would make the comparison tautological.
fn existing_config(dir: &std::path::Path, secret: [u8; 32]) -> CoordinationConfig {
    let (signed, genesis_hash) = {
        use aster::coordination::raft::SignedCoordinationDocumentV1;
        let CoordinationStart::Bootstrap { signed_genesis, .. } =
            coordination_config(dir, secret).start_mode().clone()
        else {
            unreachable!("the fixture is a bootstrap")
        };
        let envelope: SignedCoordinationDocumentV1 =
            aster::coordination::contract::payload_fory::<SignedCoordinationDocumentV1>()
                .deserialize(&signed_genesis)
                .expect("decode");
        (
            signed_genesis,
            aster::coordination::model::authority::genesis_hash(&envelope.document_bytes),
        )
    };

    CoordinationConfig::builder()
        .storage_dir(dir)
        .authority_public_key(authority().verifying_key().to_bytes())
        .authority_domain(DOMAIN)
        .start_mode(CoordinationStart::Existing {
            signed_genesis: signed,
            expected_group_id: aster::coordination::ids::CoordinationGroupId::from_array(
                [0xC0u8; 32],
            ),
            expected_genesis_hash: genesis_hash,
            expected_deployment_profile_code: 2,
        })
        .verified_admission(Arc::new(NoAdmissions))
        .acknowledge_single_voter_development_group(true)
        .build()
        .expect("build coordination config")
}

const DOMAIN: &str = "coordination.test";

fn authority() -> SigningKey {
    SigningKey::from_bytes(&[0x42u8; 32])
}

/// An admission source that admits nobody.
///
/// These tests are about registration, provenance and frame caps — never about
/// step 3 — and an all-denying source makes that explicit. A permissive one
/// would leave a reader wondering whether admission was what let a call
/// through.
struct NoAdmissions;

impl aster::coordination::admission::VerifiedAdmissionSource for NoAdmissions {
    fn get(
        &self,
        _node: aster::coordination::ids::CoordinationNodeId,
    ) -> Result<
        Option<aster::coordination::admission::VerifiedAdmission>,
        aster::coordination::admission::AdmissionLookupError,
    > {
        Ok(None)
    }
}

/// A voter that will not be started -- only its dispatcher is inspected -- so
/// the identity rule still has to be satisfied. A fixed key keeps the test from
/// depending on file I/O.
fn pinned_cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .secret_key(SecretKey::from_bytes(PINNED_SECRET))
        .root_pubkey(aster::PublicKey::from_bytes(
            authority().verifying_key().to_bytes(),
        ))
        .build()
}

/// The endpoint secret every voter in this file runs under.
///
/// Fixed, and shared with `coordination_config`, because the signed genesis has
/// to name the node id this secret produces: bootstrap refuses a node that is
/// not one of its initial voters, and a fixture whose two halves disagreed
/// would fail at start with a message about membership rather than about
/// whatever the test is really checking.
const PINNED_SECRET: [u8; 32] = [7u8; 32];

fn header(service: &str, method: &str) -> Vec<u8> {
    aster::rpc::encode_stream_header(&StreamHeader {
        service: service.to_string(),
        version: 1,
        method: method.to_string(),
        ..Default::default()
    })
    .expect("encode header")
}

/// Drive one call through `dispatcher` and return every byte written back.
async fn dispatch(
    dispatcher: &Dispatcher,
    service: &str,
    method: &str,
    provenance: PeerProvenance,
) -> String {
    let (resp_tx, mut resp_rx) = response_channel();
    let (_req_tx, req_rx) = mpsc::channel::<RequestFrame>(1);
    let parts = CallParts::new(
        "b7e1c0de00000000000000000000000000000000000000000000000000000000".into(),
        provenance,
        header(service, method),
        Vec::new(),
        FLAG_END_STREAM,
        resp_tx,
        req_rx,
        Arc::new(AtomicBool::new(false)),
    );
    dispatcher.dispatch_parts(parts).await;

    let mut out = Vec::new();
    while let Ok(item) = resp_rx.try_recv() {
        let (frame, _lease) = item.into_parts();
        let bytes = match frame {
            OutgoingFrame::Frame(b)
            | OutgoingFrame::Trailer(b)
            | OutgoingFrame::CompleteUnary(b) => b,
        };
        out.extend_from_slice(&bytes);
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── Absence unless configured ────────────────────────────────────────────────

#[tokio::test]
async fn the_reserved_services_are_absent_unless_coordination_is_configured() {
    let server = AsterServer::builder()
        .config(cfg())
        .start()
        .await
        .expect("start");
    let dispatcher = server.dispatcher();

    // Authenticated provenance, so a refusal cannot be blamed on `iroh_only`:
    // the only reason left is that nothing is registered under the name.
    for (service, method) in [(STORE_SERVICE, "acquire"), (RAFT_SERVICE, "vote")] {
        let out = dispatch(
            &dispatcher,
            service,
            method,
            PeerProvenance::IrohAuthenticated,
        )
        .await;
        assert!(
            out.contains("not found"),
            "{service} answered on a node that never asked to host coordination: {out}"
        );
        assert!(
            !out.contains("reserves"),
            "{service} reached the reserved dispatcher without being configured: {out}"
        );
    }

    server.shutdown().await;
}

/// The partner to the absence proof: with `builtin_coordination` both services
/// answer, and each says something a missing service cannot.
///
/// The two answers differ now, and deliberately. `Store` is still a
/// reservation — Slice 4 owns it — so it says so. `Raft` is **live**, so it
/// refuses the empty payload these probes send as a malformed envelope, which
/// is stronger evidence than a reservation trailer: only the real §5.3 check
/// order produces it.
#[tokio::test]
async fn configuring_coordination_registers_the_store_reservation_and_the_live_raft() {
    let dir = tempfile::tempdir().expect("a storage directory");
    let server = AsterServer::builder()
        .config(pinned_cfg())
        .builtin_coordination(coordination_config(dir.path(), PINNED_SECRET))
        .start()
        .await
        .expect("start");
    let dispatcher = server.dispatcher();

    let store = dispatch(
        &dispatcher,
        STORE_SERVICE,
        "acquire",
        PeerProvenance::IrohAuthenticated,
    )
    .await;
    assert!(
        store.contains("aster.coordination.Store v1 reserves 'acquire'"),
        "expected the reserved-store trailer, got: {store}"
    );

    let raft = dispatch(
        &dispatcher,
        RAFT_SERVICE,
        "vote",
        PeerProvenance::IrohAuthenticated,
    )
    .await;
    assert!(
        raft.contains("not a coordination Raft envelope"),
        "expected the live Raft dispatcher's step-2 refusal, got: {raft}"
    );
    assert!(
        !raft.contains("reserves"),
        "the Raft reservation must be gone: a reservation cannot check an envelope"
    );

    server.shutdown().await;
}

/// A method outside the §5.2 table must be rejected by dispatch's method
/// routing, not answered by the reservation -- otherwise the reserved service
/// would confirm the existence of methods the contract does not declare.
#[tokio::test]
async fn a_method_outside_the_reserved_table_is_not_answered_by_the_reservation() {
    let dir = tempfile::tempdir().expect("a storage directory");
    let server = AsterServer::builder()
        .config(pinned_cfg())
        .builtin_coordination(coordination_config(dir.path(), PINNED_SECRET))
        .start()
        .await
        .expect("start");

    let out = dispatch(
        &server.dispatcher(),
        STORE_SERVICE,
        "definitely_not_a_method",
        PeerProvenance::IrohAuthenticated,
    )
    .await;
    assert!(
        out.contains("not implemented") && !out.contains("reserves"),
        "expected dispatch's method-table rejection, got: {out}"
    );

    server.shutdown().await;
}

// ── Provenance ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_reserved_services_refuse_an_unauthenticatable_transport() {
    let dir = tempfile::tempdir().expect("a storage directory");
    let server = AsterServer::builder()
        .config(pinned_cfg())
        .builtin_coordination(coordination_config(dir.path(), PINNED_SECRET))
        .start()
        .await
        .expect("start");
    let dispatcher = server.dispatcher();

    for (service, method) in [(STORE_SERVICE, "acquire"), (RAFT_SERVICE, "append_entries")] {
        let out = dispatch(&dispatcher, service, method, PeerProvenance::Other).await;
        assert!(
            out.contains("not found"),
            "{service} must not confirm its existence to an unauthenticated peer: {out}"
        );
        assert!(
            !out.contains("reserves"),
            "{service} reached the reserved dispatcher over a transport that cannot \
             authenticate the peer: {out}"
        );
    }

    server.shutdown().await;
}

// ── Frame caps through the production resolver ───────────────────────────────

/// The unit tests in `coordination::service` assert what the dispatchers
/// *declare*. This asserts what the resolver the server actually installs
/// *returns* -- the path a frame is measured against.
#[tokio::test]
async fn the_reserved_frame_caps_reach_the_production_resolver() {
    let dir = tempfile::tempdir().expect("a storage directory");
    let server = AsterServer::builder()
        .config(pinned_cfg())
        .builtin_coordination(coordination_config(dir.path(), PINNED_SECRET))
        .start()
        .await
        .expect("start");
    let resolve = server.dispatcher().frame_limit_resolver();

    assert!(
        matches!(
            resolve(&header(STORE_SERVICE, "acquire")),
            FrameLimit::Limit(2_097_152)
        ),
        "store methods are capped at 2 MiB"
    );
    assert!(
        matches!(
            resolve(&header(RAFT_SERVICE, "vote")),
            FrameLimit::Limit(8_388_608)
        ),
        "raft unary calls are capped at 8 MiB"
    );
    assert!(
        matches!(
            resolve(&header(RAFT_SERVICE, "append_entries")),
            FrameLimit::Limit(8_388_608)
        ),
        "raft unary calls are capped at 8 MiB"
    );
    assert!(
        matches!(
            resolve(&header(RAFT_SERVICE, "install_snapshot")),
            FrameLimit::Limit(2_097_152)
        ),
        "a snapshot chunk must not inherit the 8 MiB unary cap"
    );

    server.shutdown().await;
}

/// Without coordination the names carry no cap at all -- which is also why the
/// caps above are evidence of registration and not of a table baked into core.
#[tokio::test]
async fn an_unconfigured_node_installs_no_coordination_frame_caps() {
    let server = AsterServer::builder()
        .config(cfg())
        .start()
        .await
        .expect("start");
    let resolve = server.dispatcher().frame_limit_resolver();

    assert!(matches!(
        resolve(&header(STORE_SERVICE, "acquire")),
        FrameLimit::Default
    ));
    assert!(matches!(
        resolve(&header(RAFT_SERVICE, "vote")),
        FrameLimit::Default
    ));

    server.shutdown().await;
}

// ── §3.3 restart-stable identity ─────────────────────────────────────────────

#[tokio::test]
async fn a_voter_without_a_persistent_identity_is_refused_at_start() {
    let dir = tempfile::tempdir().expect("a storage directory");
    let msg = match AsterServer::builder()
        .config(cfg())
        .builtin_coordination(coordination_config(dir.path(), PINNED_SECRET))
        .start()
        .await
    {
        Ok(server) => {
            server.shutdown().await;
            panic!("an ephemeral identity must not host a voter");
        }
        Err(err) => err.to_string(),
    };
    assert!(
        msg.contains("persistent endpoint identity"),
        "the error must explain the identity rule, got: {msg}"
    );
    assert!(
        msg.contains("identity(path)") && msg.contains("secret_key"),
        "the error must name both remedies, got: {msg}"
    );
}

/// The control: the same builder without `builtin_coordination` starts fine on
/// an ephemeral identity, so the refusal above is the coordination rule and not
/// a generally broken config.
#[tokio::test]
async fn an_ordinary_server_still_starts_without_a_persistent_identity() {
    let server = AsterServer::builder()
        .config(cfg())
        .start()
        .await
        .expect("an ordinary server does not need a persistent identity");
    server.shutdown().await;
}

#[tokio::test]
async fn an_explicit_secret_key_satisfies_the_voter_identity_rule() {
    let dir = tempfile::tempdir().expect("a storage directory");
    let server = AsterServer::builder()
        .config(pinned_cfg())
        .builtin_coordination(coordination_config(dir.path(), PINNED_SECRET))
        .start()
        .await
        .expect("an explicitly configured secret key is persistent");
    server.shutdown().await;
}

/// An identity *path* satisfies the rule on first start, when the file does not
/// exist yet -- and the reason it counts is that the NodeId then survives a
/// restart. Asserting the surviving id rather than merely that `start()`
/// returned `Ok` is the difference between testing the rule and testing the
/// `is_none()` check that implements it.
#[tokio::test]
async fn an_identity_path_yields_the_same_voter_node_id_after_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let key_path = dir.path().join("node.secret");
    assert!(!key_path.exists(), "the file must not exist on first start");

    // The genesis has to name the node this identity file will produce, and
    // bootstrap refuses a node that is not one of its initial voters — so the
    // key is minted here and written first. That is also what an operator does:
    // provision the identity, then sign a genesis naming it.
    let secret = [0x21u8; 32];
    // The identity file is hex, which is what `AsterServerBuilder::identity`
    // writes and reads.
    std::fs::write(
        &key_path,
        secret
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    )
    .expect("persist the identity");
    let storage = dir.path().join("storage");
    std::fs::create_dir_all(&storage).expect("a storage directory");

    let first = AsterServer::builder()
        .config(cfg())
        .identity(&key_path)
        .builtin_coordination(coordination_config(&storage, secret))
        .start()
        .await
        .expect("first start");
    let first_id = first.id();
    first.shutdown().await;

    assert!(
        key_path.exists(),
        "the key must still be there after a start"
    );

    // The second start reopens rather than re-founding: the directory is
    // initialized now, so the mode is `Existing`.
    let second = AsterServer::builder()
        .config(cfg())
        .identity(&key_path)
        .builtin_coordination(existing_config(&storage, secret))
        .start()
        .await
        .expect("restart");
    let second_id = second.id();
    second.shutdown().await;

    assert_eq!(
        first_id.to_string(),
        second_id.to_string(),
        "a voter's NodeId is named by committed membership; it must survive restart"
    );
}
