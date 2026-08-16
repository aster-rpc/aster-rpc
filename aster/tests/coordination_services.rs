//! Slice 0: the reserved coordination services exist only when a host asked
//! for them, refuse an unauthenticatable transport, carry their §12.3 frame
//! caps, and demand a restart-stable identity.
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

use aster::coordination::host::CoordinationConfig;
use aster::coordination::service::{RAFT_SERVICE, STORE_SERVICE};
use aster::rpc::{
    AsterServer, CallParts, Dispatcher, OutgoingFrame, PeerProvenance, RequestFrame, StreamHeader,
};
use aster::{AsterConfig, RelayMode, SecretKey};
use aster_transport_core::framing::FLAG_END_STREAM;
use aster_transport_core::reactor::{response_channel, FrameLimit};
use tokio::sync::mpsc;

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

fn coordination_config() -> CoordinationConfig {
    CoordinationConfig::builder()
        .storage_dir("/nonexistent/slice0-never-opened")
        .build()
        .expect("build coordination config")
}

/// A voter that will not be started -- only its dispatcher is inspected -- so
/// the identity rule still has to be satisfied. A fixed key keeps the test from
/// depending on file I/O.
fn pinned_cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .secret_key(SecretKey::from_bytes([7u8; 32]))
        .build()
}

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

/// The partner to the absence proof: with `builtin_coordination` the reserved
/// dispatcher answers, and it says something a missing service cannot.
#[tokio::test]
async fn configuring_coordination_registers_both_reserved_services() {
    let server = AsterServer::builder()
        .config(pinned_cfg())
        .builtin_coordination(coordination_config())
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
        raft.contains("aster.coordination.Raft v1 reserves 'vote'"),
        "expected the reserved-raft trailer, got: {raft}"
    );

    server.shutdown().await;
}

/// A method outside the §5.2 table must be rejected by dispatch's method
/// routing, not answered by the reservation -- otherwise the reserved service
/// would confirm the existence of methods the contract does not declare.
#[tokio::test]
async fn a_method_outside_the_reserved_table_is_not_answered_by_the_reservation() {
    let server = AsterServer::builder()
        .config(pinned_cfg())
        .builtin_coordination(coordination_config())
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
    let server = AsterServer::builder()
        .config(pinned_cfg())
        .builtin_coordination(coordination_config())
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
    let server = AsterServer::builder()
        .config(pinned_cfg())
        .builtin_coordination(coordination_config())
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
    let msg = match AsterServer::builder()
        .config(cfg())
        .builtin_coordination(coordination_config())
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
    let server = AsterServer::builder()
        .config(pinned_cfg())
        .builtin_coordination(coordination_config())
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

    let first = AsterServer::builder()
        .config(cfg())
        .identity(&key_path)
        .builtin_coordination(coordination_config())
        .start()
        .await
        .expect("first start");
    let first_id = first.id();
    first.shutdown().await;

    assert!(key_path.exists(), "first start must persist the key");

    let second = AsterServer::builder()
        .config(cfg())
        .identity(&key_path)
        .builtin_coordination(coordination_config())
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
