//! The live `install_snapshot` stream, driven frame by frame (§5.3, §8.4, §11.4).
//!
//! ## Why these are behavioural and the previous ones were not
//!
//! The properties here were once asserted by counting substrings in
//! `service.rs`: two occurrences of a recheck call meant admission was
//! rechecked per chunk and once before install. That is evidence the call
//! *sites* exist. It is not evidence they work, and they did not — the recheck
//! passed a `RaftSnapshotFrameV1` header to a function that decodes a
//! `RaftEnvelopeV1`, so every recheck refused at step 2 and no snapshot chunk
//! was ever accepted by any node. The counting test passed throughout.
//!
//! So every property below is observed from outside: frames go into the real
//! `Dispatcher`, through the real handler, into the real receiver, and the
//! status that comes back is the one a peer would see.
//!
//! ## How "how often" is made observable
//!
//! "Admission is rechecked on every chunk and once more before install" is a
//! statement about frequency, and a stream driven under an unchanging policy
//! cannot see it — which is why it was counted in the source in the first
//! place.
//!
//! [`BudgetedAdmission`] makes it observable instead: it admits the first `n`
//! lookups and denies afterwards. A one-chunk stream performs exactly three —
//! the header, the chunk, the pre-install recheck — so the budget at which the
//! stream first fails *is* the number of checks performed, and each of the four
//! budgets pins a different one. A handler that checked once would pass at
//! budget 1 and be caught immediately.

#![cfg(feature = "coordination-testing")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aster::coordination::admission::{
    AdmissionLookupError, VerifiedAdmission, VerifiedAdmissionClass, VerifiedAdmissionSource,
};
use aster::coordination::contract::payload_fory;
use aster::coordination::host::{CoordinationConfig, CoordinationStart};
use aster::coordination::ids::CoordinationNodeId;
use aster::coordination::model::authority::{
    canonical_document_bytes, genesis_signing_bytes, signed_document_from_parts,
};
use aster::coordination::raft::{
    GenesisV1, NodeIdV1, RaftSnapshotChunkWireV1, RaftSnapshotFrameV1, RaftSnapshotHeaderWireV1,
    SnapshotMetaV1, VoteV1, VoterSeedV1,
};
use aster::coordination::service::RAFT_SERVICE;
use aster::rpc::{
    AsterServer, CallParts, Dispatcher, OutgoingFrame, PeerProvenance, RequestFrame, StatusCode,
    StreamHeader,
};
use aster::{AsterConfig, RelayMode, SecretKey};
use aster_transport_core::reactor::response_channel;
use ed25519_dalek::{Signer, SigningKey};
use tokio::sync::mpsc;

const DOMAIN: &str = "snapshot.test";
const GROUP: [u8; 32] = [0xD1u8; 32];
const SECRET: [u8; 32] = [0x9Au8; 32];
/// The two-voter fixture. A snapshot is installed on a **follower**, so the
/// gates that reach preparation need a group with one.
const PAIR: [[u8; 32]; 2] = [[0x9Bu8; 32], [0x9Cu8; 32]];

fn authority() -> SigningKey {
    SigningKey::from_bytes(&[0x31u8; 32])
}

fn id_of(secret: [u8; 32]) -> Vec<u8> {
    SigningKey::from_bytes(&secret)
        .verifying_key()
        .to_bytes()
        .to_vec()
}

fn node_id() -> Vec<u8> {
    id_of(SECRET)
}

/// An admission source with a lookup budget.
///
/// Admits while the budget lasts, then denies every later lookup. The point is
/// not to model a real revocation policy — it is to make the *number* of
/// admission checks a stream performs visible from outside, which no
/// unchanging policy can do.
struct BudgetedAdmission {
    remaining: AtomicUsize,
}

impl BudgetedAdmission {
    fn new(budget: usize) -> Arc<Self> {
        Arc::new(Self {
            remaining: AtomicUsize::new(budget),
        })
    }
}

impl VerifiedAdmissionSource for BudgetedAdmission {
    fn get(
        &self,
        node: CoordinationNodeId,
    ) -> Result<Option<VerifiedAdmission>, AdmissionLookupError> {
        // `fetch_update` rather than a load-then-store: the handler and the
        // runtime's own controllers share this source, and a decrement that
        // could be lost would make the budget mean nothing.
        let allowed = self
            .remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if allowed {
            Ok(Some(producer(node)))
        } else {
            Ok(None)
        }
    }
}

/// A source that admits everything, for the streams whose refusal must come
/// from somewhere other than admission.
struct AlwaysAdmits;

impl VerifiedAdmissionSource for AlwaysAdmits {
    fn get(
        &self,
        node: CoordinationNodeId,
    ) -> Result<Option<VerifiedAdmission>, AdmissionLookupError> {
        Ok(Some(producer(node)))
    }
}

/// A producer record for `node`.
///
/// Built directly rather than verified from a signed set: these tests are about
/// the *stream*, and the signed-set path has its own gates. The subject is
/// always the node that was asked about, so the exact-subject rule is satisfied
/// honestly rather than bypassed.
fn producer(node: CoordinationNodeId) -> VerifiedAdmission {
    VerifiedAdmission {
        subject: node,
        class: VerifiedAdmissionClass::Producer,
        authority_fingerprint: [0x01u8; 32],
        evidence_digest: [0x02u8; 32],
        revision: 1,
    }
}

fn ticket_for(id: &[u8]) -> String {
    aster_transport_core::ticket::AsterTicket {
        endpoint_id: id.to_vec().try_into().expect("32 bytes"),
        relay: None,
        direct_addrs: Vec::new(),
        credential: Some(aster_transport_core::ticket::TicketCredential::Open),
    }
    .to_base58_string()
    .expect("a ticket")
}

fn coordination_config(
    dir: &std::path::Path,
    secrets: &[[u8; 32]],
    admission: Arc<dyn VerifiedAdmissionSource>,
) -> CoordinationConfig {
    let mut initial_voters: Vec<VoterSeedV1> = secrets
        .iter()
        .map(|secret| {
            let node_id = id_of(*secret);
            VoterSeedV1 {
                ticket: ticket_for(&node_id),
                node_id,
            }
        })
        .collect();
    initial_voters.sort_by(|l, r| l.node_id.cmp(&r.node_id));
    let mut initial_admins: Vec<NodeIdV1> = secrets
        .iter()
        .map(|secret| NodeIdV1 {
            node_id: id_of(*secret),
        })
        .collect();
    initial_admins.sort_by(|l, r| l.node_id.cmp(&r.node_id));

    let genesis = GenesisV1 {
        format_version: 1,
        group_id: GROUP.to_vec(),
        authority_domain: DOMAIN.into(),
        genesis_nonce: vec![0x11u8; 32],
        // 2 admits one voter, 3 admits two. §7.1's profile codes, and the
        // fixture picks the one its own size requires rather than carrying an
        // acknowledgement for a shape it is not.
        deployment_profile_code: if secrets.len() == 1 { 2 } else { 3 },
        initial_voters,
        initial_admins,
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
        .verified_admission(admission)
        .acknowledge_single_voter_development_group(true)
        .acknowledge_even_quorum_development_group(true)
        .build()
        .expect("a complete configuration")
}

/// A started voter and the directory it owns.
struct Voter {
    server: AsterServer,
    secret: [u8; 32],
    _dir: tempfile::TempDir,
}

impl Voter {
    fn id_hex(&self) -> String {
        hex::encode(id_of(self.secret))
    }

    fn coordination(&self) -> aster::coordination::Coordination {
        self.server.coordination().expect("hosted")
    }
}

async fn start_one(
    secrets: &[[u8; 32]],
    secret: [u8; 32],
    admission: Arc<dyn VerifiedAdmissionSource>,
) -> Voter {
    let dir = tempfile::tempdir().expect("a storage directory");
    let server = AsterServer::builder()
        .config(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(SecretKey::from_bytes(secret))
                .root_pubkey(aster::PublicKey::from_bytes(
                    authority().verifying_key().to_bytes(),
                ))
                .build(),
        )
        .builtin_coordination(coordination_config(dir.path(), secrets, admission))
        .start()
        .await
        .expect("a voter starts");
    Voter {
        server,
        secret,
        _dir: dir,
    }
}

/// A lone voter, ready.
///
/// Used by every gate whose stream is refused before preparation: those never
/// reach `install_full_snapshot`, so the node's leadership is irrelevant to
/// them, and a one-voter group is the cheapest fixture that has a live `Raft`
/// behind the service.
async fn start_voter(admission: Arc<dyn VerifiedAdmissionSource>) -> Voter {
    let voter = start_one(&[SECRET], SECRET, admission).await;
    await_ready(&voter).await;
    voter
}

async fn await_ready(voter: &Voter) {
    tokio::time::timeout(Duration::from_secs(30), voter.coordination().wait_ready())
        .await
        .expect("the voter becomes ready in time")
        .expect("the voter becomes ready");
}

/// Two voters, with the leader's index.
///
/// A snapshot is installed on a **follower** — OpenRaft's own
/// `handle_install_full_snapshot` runs the following handler, which asserts the
/// node is not leading — so any gate that means to observe a successful install
/// needs a group that has a follower in it. A one-voter group never does.
async fn start_pair(admission: Arc<dyn VerifiedAdmissionSource>) -> (Vec<Voter>, usize) {
    let mut voters = Vec::new();
    for secret in PAIR {
        voters.push(start_one(&PAIR, secret, Arc::clone(&admission)).await);
    }

    // The genesis seeds carry no addresses — a node has none before it binds —
    // so the real ones are exchanged now, exactly as the cluster gates do.
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

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        for (index, voter) in voters.iter().enumerate() {
            let coordination = voter.coordination();
            if coordination.lifecycle_code() == 3 && coordination.quorum_available() == Some(true) {
                return (voters, index);
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pair elected no leader in time"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// What the peer saw: the decoded trailer status.
///
/// Decoded, not matched as a substring. A test that asserted
/// `text.contains("denied")` would pass on a `DataLoss` whose message happened
/// to mention denial, and these cases turn on telling `PermissionDenied`,
/// `InvalidArgument`, `DataLoss` and `Internal` apart.
struct Answer {
    code: i32,
    text: String,
}

impl Answer {
    fn is(&self, code: StatusCode) -> bool {
        self.code == code.as_i32()
    }
}

/// Drive one `install_snapshot` stream and return the peer's answer.
///
/// The header frame rides in as the call's first request frame and the rest are
/// pushed through the request channel, which is exactly the shape the reactor
/// delivers a client stream in. `FLAG_END_STREAM` is deliberately **not** set
/// on the first frame: a snapshot stream is never one frame, and setting it
/// would make `recv_request` return `None` at the first chunk.
async fn drive_stream(
    dispatcher: &Dispatcher,
    peer: &str,
    frames: Vec<RaftSnapshotFrameV1>,
) -> Answer {
    drive_raw(dispatcher, peer, encode_all(frames)).await
}

fn encode_all(frames: Vec<RaftSnapshotFrameV1>) -> Vec<Vec<u8>> {
    let fory = payload_fory::<RaftSnapshotFrameV1>();
    frames
        .into_iter()
        .map(|frame| fory.serialize(&frame).expect("encode a frame"))
        .collect()
}

async fn drive_raw(dispatcher: &Dispatcher, peer: &str, frames: Vec<Vec<u8>>) -> Answer {
    let (resp_tx, mut resp_rx) = response_channel();
    let (req_tx, req_rx) = mpsc::channel::<RequestFrame>(4);

    let mut frames = frames.into_iter();
    let first = frames.next().expect("a stream has at least one frame");

    let header = aster::rpc::encode_stream_header(&StreamHeader {
        service: RAFT_SERVICE.to_string(),
        version: 1,
        method: "install_snapshot".to_string(),
        ..Default::default()
    })
    .expect("encode the stream header");

    let parts = CallParts::new(
        peer.into(),
        PeerProvenance::IrohAuthenticated,
        header,
        first,
        // Not END_STREAM: more frames follow.
        0,
        resp_tx,
        req_rx,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );

    // The handler has to be running while the remaining frames are fed to it,
    // so dispatch and the feeder run concurrently — as they do on the wire.
    let dispatcher = dispatcher.clone();
    let dispatching = tokio::spawn(async move { dispatcher.dispatch_parts(parts).await });

    for (index, frame) in frames.enumerate() {
        let last = index + 1;
        let sent = req_tx
            .send(RequestFrame {
                payload: frame,
                flags: 0,
            })
            .await;
        if sent.is_err() {
            // The handler refused and dropped its receiver. That is an answer,
            // not a failure of the harness.
            break;
        }
        let _ = last;
    }
    drop(req_tx);

    dispatching.await.expect("the handler does not panic");

    // The trailer is the last frame, whichever variant carried it, and it is
    // the only one that holds the status.
    let mut raw = Vec::new();
    while let Ok(item) = resp_rx.try_recv() {
        let (frame, _lease) = item.into_parts();
        let bytes = match frame {
            OutgoingFrame::Frame(b)
            | OutgoingFrame::Trailer(b)
            | OutgoingFrame::CompleteUnary(b) => b,
        };
        raw.extend_from_slice(&bytes);
    }

    let mut status = None;
    let mut at = 0usize;
    while at < raw.len() {
        let Ok((payload, flags, consumed)) =
            aster_transport_core::framing::decode_frame(&raw[at..])
        else {
            break;
        };
        if flags & aster_transport_core::framing::FLAG_TRAILER != 0 {
            status = aster::rpc::decode_rpc_status(&payload).ok();
        }
        at += consumed;
    }

    let status = status.expect("every call ends with a trailer carrying its status");
    Answer {
        code: status.code,
        text: status.message,
    }
}

/// The header a well-formed stream from `sender` begins with.
fn header_for(
    sender: &[u8],
    vote: &openraft::Vote<CoordinationNodeId>,
    meta: &openraft::SnapshotMeta<
        CoordinationNodeId,
        aster::coordination::typeconfig::CoordinationNode,
    >,
    body: &[u8],
) -> RaftSnapshotHeaderWireV1 {
    RaftSnapshotHeaderWireV1 {
        protocol_version: 1,
        raft_codec_version: 1,
        group_id: GROUP.to_vec(),
        sender_node_id: sender.to_vec(),
        vote_payload: payload_fory::<VoteV1>()
            .serialize(&VoteV1::from(vote))
            .expect("encode the vote"),
        snapshot_meta_payload: payload_fory::<SnapshotMetaV1>()
            .serialize(&SnapshotMetaV1::from(meta))
            .expect("encode the metadata"),
        total_len: body.len() as u64,
        blake3_checksum: blake3::hash(body).as_bytes().to_vec(),
    }
}

fn chunk(offset: u64, data: Vec<u8>, final_chunk: bool) -> RaftSnapshotFrameV1 {
    RaftSnapshotFrameV1::Chunk(RaftSnapshotChunkWireV1 {
        offset,
        data,
        final_chunk,
    })
}

/// The voter's own snapshot, as bytes, plus the vote and metadata that bind it.
///
/// Its own, deliberately: a body this store built is a body this store can
/// install, so a refusal in the tests below is never the fixture's fault.
async fn own_snapshot(
    voter: &Voter,
) -> (
    openraft::Vote<CoordinationNodeId>,
    openraft::SnapshotMeta<CoordinationNodeId, aster::coordination::typeconfig::CoordinationNode>,
    Vec<u8>,
) {
    let coordination = voter.coordination();
    coordination
        .trigger_snapshot_for_testing()
        .await
        .expect("the snapshot trigger is accepted");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some((meta, body)) = coordination.snapshot_for_testing().await {
            assert!(!body.is_empty(), "a built snapshot has a body");
            return (coordination.vote_for_testing(), meta, body);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no snapshot was built within the timeout"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ── The recheck, at each of the three points it must happen ──────────────────

/// **Admission is checked at the header, at every chunk, and once before
/// install — and each check works.**
///
/// One stream shape, four admission budgets, four different answers. The budget
/// at which the stream stops succeeding is the number of checks it performed,
/// so this pins the frequency §8.4 requires without counting anything in the
/// source.
///
/// Budget 3 is the one that matters most: it is the only case that reaches
/// preparation, and before the recheck was repaired *no* budget did — the first
/// chunk was refused at step 2 whatever the policy said, because the recheck
/// decoded the header as an envelope. Every other assertion here passed under
/// that defect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_admission_check_in_the_stream_is_real() {
    for (budget, expected) in [
        (0usize, "the header"),
        (1, "the chunk"),
        (2, "the pre-install recheck"),
        (3, "nothing — admission is satisfied throughout"),
    ] {
        // A permissive voter builds the snapshot; the budget is installed only
        // for the stream itself, so the runtime's own startup lookups do not
        // spend it.
        let admission = BudgetedAdmission::new(usize::MAX);
        let voter = start_voter(Arc::clone(&admission) as Arc<dyn VerifiedAdmissionSource>).await;
        let (vote, meta, body) = own_snapshot(&voter).await;

        admission.remaining.store(budget, Ordering::SeqCst);

        let header = header_for(&node_id(), &vote, &meta, &body);
        let answer = drive_stream(
            &voter.server.dispatcher(),
            &hex::encode(node_id()),
            vec![
                RaftSnapshotFrameV1::Header(header),
                chunk(0, body.clone(), true),
            ],
        )
        .await;

        if budget < 3 {
            assert!(
                answer.is(StatusCode::PermissionDenied),
                "budget {budget} must be refused at {expected}; got {} / {}",
                answer.code,
                answer.text
            );
        } else {
            assert!(
                !answer.is(StatusCode::PermissionDenied),
                "budget {budget}: {expected}, so no answer may be an admission \
                 refusal; got {}",
                answer.text
            );
        }

        voter.server.shutdown().await;
    }
}

/// **A leader's snapshot installs on a follower.**
///
/// The positive partner to every refusal here, and the case that was broken:
/// under the previous recheck no stream could reach preparation at all, so the
/// refusals all passed for the wrong reason. A refusal suite without this
/// cannot tell "refused correctly" from "refused always".
///
/// Everything on the wire is the real thing — the leader's own built snapshot,
/// the leader's own vote, the leader's own metadata, delivered to the follower
/// that would receive them in a catch-up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leaders_snapshot_installs_on_a_follower() {
    let (voters, leader) = start_pair(Arc::new(AlwaysAdmits)).await;
    let follower = 1 - leader;

    let (vote, meta, body) = own_snapshot(&voters[leader]).await;
    let header = header_for(&id_of(voters[leader].secret), &vote, &meta, &body);

    let answer = drive_stream(
        &voters[follower].server.dispatcher(),
        &voters[leader].id_hex(),
        vec![
            RaftSnapshotFrameV1::Header(header),
            chunk(0, body.clone(), true),
        ],
    )
    .await;

    assert!(
        answer.is(StatusCode::Ok),
        "a stream carrying the leader's own snapshot, vote and metadata must \
         install on the follower; got {} / {}",
        answer.code,
        answer.text
    );

    // Still serving afterwards — an install is not a stop.
    assert_eq!(
        voters[follower].coordination().lifecycle_code(),
        3,
        "the follower must still be Ready after installing a snapshot"
    );

    for voter in voters {
        voter.server.shutdown().await;
    }
}

/// **A snapshot streamed at a leader under its own vote is refused, not
/// fatal.**
///
/// Found by the gate above rather than by review. OpenRaft's
/// `handle_install_full_snapshot` calls `accept_vote` and then the *following*
/// handler, which asserts `self.leader.is_none()`. A strictly greater vote
/// steps the node down first and the assertion holds; a vote equal to the
/// leader's own does not step it down, and the engine walks into its own
/// assertion — a panicked state-machine worker in a debug build, and a leader
/// installing a snapshot in a release one.
///
/// Nothing in §5.3 prevents it: every check passes, because none of them
/// compares the arriving vote with local leadership. Any admitted member could
/// send it. So the receiver asks the question itself, and answers
/// `FailedPrecondition` — the peer is not wrong, this node is simply not a
/// follower, and leadership is exactly the kind of condition a later message
/// repairs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_at_a_leader_under_its_own_vote_is_refused_and_it_keeps_leading() {
    let (voters, leader) = start_pair(Arc::new(AlwaysAdmits)).await;
    let follower = 1 - leader;

    // The leader's own snapshot and its own vote, aimed back at the leader.
    let (vote, meta, body) = own_snapshot(&voters[leader]).await;
    let header = header_for(&id_of(voters[follower].secret), &vote, &meta, &body);

    let answer = drive_stream(
        &voters[leader].server.dispatcher(),
        &voters[follower].id_hex(),
        vec![
            RaftSnapshotFrameV1::Header(header),
            chunk(0, body.clone(), true),
        ],
    )
    .await;

    assert!(
        answer.is(StatusCode::FailedPrecondition),
        "a leader must decline an install rather than assert its way through \
         one; got {} / {}",
        answer.code,
        answer.text
    );
    assert!(
        !answer.is(StatusCode::Internal),
        "declining is not a fault: {}",
        answer.text
    );

    // And it is still the leader — the whole point is that this costs the
    // group nothing.
    assert_eq!(
        voters[leader].coordination().quorum_available(),
        Some(true),
        "the leader must still hold direct quorum evidence after declining"
    );

    for voter in voters {
        voter.server.shutdown().await;
    }
}

// ── Canonicality, at the frame level ─────────────────────────────────────────

/// **A frame with trailing bytes is refused (§5.1).**
///
/// Fory tolerates them, so the frame still decodes and every nested payload
/// inside it is still canonical — which is precisely why the nested checks
/// cannot stand in for this one. A frame with a second valid spelling is a
/// frame two nodes can disagree about while both verifying.
///
/// Asserted at both sites, because they are separate decodes and only one of
/// them was ever going to be remembered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_frame_with_trailing_bytes_is_refused_at_both_sites() {
    let voter = start_voter(Arc::new(AlwaysAdmits)).await;
    let (vote, meta, body) = own_snapshot(&voter).await;
    let peer = hex::encode(node_id());

    let clean = encode_all(vec![
        RaftSnapshotFrameV1::Header(header_for(&node_id(), &vote, &meta, &body)),
        chunk(0, body.clone(), true),
    ]);

    for (site, index) in [("the header", 0usize), ("a chunk", 1)] {
        let mut frames = clean.clone();
        frames[index].push(0x00);
        let answer = drive_raw(&voter.server.dispatcher(), &peer, frames).await;
        assert!(
            answer.is(StatusCode::InvalidArgument),
            "{site} with a trailing byte must be refused as non-canonical; got {} / {}",
            answer.code,
            answer.text
        );
        assert!(
            answer.text.contains("canonical"),
            "{site}: the refusal must name canonicality, not merely fail to \
             decode; got {}",
            answer.text
        );
    }

    voter.server.shutdown().await;
}

// ── The declared and per-chunk ceilings ──────────────────────────────────────

/// **A body declared above the 512 MiB ceiling is refused before staging.**
///
/// Refused on the *claim*, which is the only point at which refusing it costs
/// nothing. Without this the receiver would open a temporary and stream into it
/// until `IncomingWriter` hit its own ceiling — surfacing as a local storage
/// failure and answering `Internal`, which tells a hostile peer its oversized
/// stream found a fault in our disk and invites a retry.
///
/// The status is the evidence: `InvalidArgument` says "your stream", `Internal`
/// says "our disk", and the whole point is which one a 4 GiB claim gets.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_body_declared_above_the_ceiling_is_the_senders_fault() {
    let voter = start_voter(Arc::new(AlwaysAdmits)).await;
    let (vote, meta, body) = own_snapshot(&voter).await;

    let mut header = header_for(&node_id(), &vote, &meta, &body);
    header.total_len = 4 * 1024 * 1024 * 1024;

    let answer = drive_stream(
        &voter.server.dispatcher(),
        &hex::encode(node_id()),
        vec![
            RaftSnapshotFrameV1::Header(header),
            chunk(0, body.clone(), false),
        ],
    )
    .await;

    assert!(
        answer.is(StatusCode::InvalidArgument),
        "a 4 GiB declared body is the sender's fault, not this store's; got {} / {}",
        answer.code,
        answer.text
    );
    assert!(
        !answer.is(StatusCode::Internal),
        "an oversized claim must never read as a local failure: {}",
        answer.text
    );

    voter.server.shutdown().await;
}

/// **A chunk carrying more than 1 MiB of data is refused.**
///
/// The transport's per-method frame cap bounds the *frame*, which is larger
/// than the data it carries by the chunk header and frame overhead. This is the
/// bound on the data, and it is what makes "one chunk" a known revocation
/// interval rather than whatever the sender chose to send — §8.4's recheck
/// interval is stated in chunks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chunk_above_the_data_ceiling_is_refused() {
    let voter = start_voter(Arc::new(AlwaysAdmits)).await;
    let (vote, meta, body) = own_snapshot(&voter).await;

    let oversized = vec![0x7Eu8; 1024 * 1024 + 1];
    let mut header = header_for(&node_id(), &vote, &meta, &body);
    header.total_len = oversized.len() as u64;
    header.blake3_checksum = blake3::hash(&oversized).as_bytes().to_vec();

    let answer = drive_stream(
        &voter.server.dispatcher(),
        &hex::encode(node_id()),
        vec![
            RaftSnapshotFrameV1::Header(header),
            chunk(0, oversized, true),
        ],
    )
    .await;

    assert!(
        answer.is(StatusCode::InvalidArgument),
        "a chunk one byte over the ceiling must be refused; got {} / {}",
        answer.code,
        answer.text
    );
    assert!(
        answer.text.contains("ceiling"),
        "the refusal must name the ceiling it crossed; got {}",
        answer.text
    );

    voter.server.shutdown().await;
}

// ── Contiguity, checksum, and staying alive afterwards ───────────────────────

/// **A corrupt or non-contiguous stream is refused, and the voter keeps
/// serving** (§6.2's required oracle).
///
/// Three malformations, each with its own answer, and readiness checked after
/// every one. A voter that refused a bad snapshot by dying would satisfy a test
/// that only looked at the status — which is why the second assertion is here
/// rather than assumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_stream_leaves_the_voter_serving() {
    // The pair, so the closing assertion can be a real install rather than
    // another refusal. "Still serving" has to mean "still able to do the work",
    // and a suite that only ever showed refusals could not distinguish a voter
    // that survived from one that had stopped accepting anything.
    let (voters, leader) = start_pair(Arc::new(AlwaysAdmits)).await;
    let follower = 1 - leader;
    let voter = &voters[follower];
    let (vote, meta, body) = own_snapshot(&voters[leader]).await;
    let sender = id_of(voters[leader].secret);
    let peer = voters[leader].id_hex();
    let coordination = voter.coordination();

    // 1. A chunk that skips forward, leaving a hole the checksum would have to
    //    catch after the bytes were already written.
    let answer = drive_stream(
        &voter.server.dispatcher(),
        &peer,
        vec![
            RaftSnapshotFrameV1::Header(header_for(&sender, &vote, &meta, &body)),
            chunk(7, body.clone(), true),
        ],
    )
    .await;
    assert!(
        answer.is(StatusCode::InvalidArgument),
        "a non-contiguous chunk must be refused; got {}",
        answer.text
    );
    assert_eq!(coordination.lifecycle_code(), 3, "still serving");

    // 2. Contiguous, complete, and wrong: the declared checksum does not match
    //    the bytes. `DataLoss` rather than `InvalidArgument` — the stream was
    //    well formed and the *content* did not survive it.
    let mut corrupt = body.clone();
    corrupt[0] ^= 0xFF;
    let answer = drive_stream(
        &voter.server.dispatcher(),
        &peer,
        vec![
            RaftSnapshotFrameV1::Header(header_for(&sender, &vote, &meta, &body)),
            chunk(0, corrupt, true),
        ],
    )
    .await;
    assert!(
        answer.is(StatusCode::DataLoss),
        "a body that does not match its checksum must answer DataLoss; got {} / {}",
        answer.code,
        answer.text
    );
    assert_eq!(coordination.lifecycle_code(), 3, "still serving");

    // 3. A stream that ends short of what it declared.
    let answer = drive_stream(
        &voter.server.dispatcher(),
        &peer,
        vec![
            RaftSnapshotFrameV1::Header(header_for(&sender, &vote, &meta, &body)),
            chunk(0, body[..body.len() / 2].to_vec(), true),
        ],
    )
    .await;
    assert!(
        answer.is(StatusCode::InvalidArgument),
        "a short stream must be refused; got {}",
        answer.text
    );
    assert_eq!(coordination.lifecycle_code(), 3, "still serving");

    // And a well-formed stream still succeeds afterwards, so "still serving" is
    // not merely "still answering with errors".
    let answer = drive_stream(
        &voter.server.dispatcher(),
        &peer,
        vec![
            RaftSnapshotFrameV1::Header(header_for(&sender, &vote, &meta, &body)),
            chunk(0, body.clone(), true),
        ],
    )
    .await;
    assert!(
        answer.is(StatusCode::Ok),
        "after three refusals a good stream must still install; got {} / {}",
        answer.code,
        answer.text
    );

    for voter in voters {
        voter.server.shutdown().await;
    }
}

/// **A stream from a peer that is not the sender the header names is refused.**
///
/// The envelope's `sender_node_id` is a claim by the remote; the transport's
/// peer is not. This is the check that stops a member of the group selecting
/// another member's identity inside a stream it is otherwise entitled to open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_header_naming_another_sender_is_refused() {
    let voter = start_voter(Arc::new(AlwaysAdmits)).await;
    let (vote, meta, body) = own_snapshot(&voter).await;

    let mut header = header_for(&node_id(), &vote, &meta, &body);
    header.sender_node_id = vec![0x5Cu8; 32];

    let answer = drive_stream(
        &voter.server.dispatcher(),
        &hex::encode(node_id()),
        vec![
            RaftSnapshotFrameV1::Header(header),
            chunk(0, body.clone(), true),
        ],
    )
    .await;

    assert!(
        answer.is(StatusCode::PermissionDenied),
        "a header naming a sender other than the authenticated peer must be \
         refused; got {} / {}",
        answer.code,
        answer.text
    );

    voter.server.shutdown().await;
}

/// **A stream addressed to another group is refused.**
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_header_for_another_group_is_refused() {
    let voter = start_voter(Arc::new(AlwaysAdmits)).await;
    let (vote, meta, body) = own_snapshot(&voter).await;

    let mut header = header_for(&node_id(), &vote, &meta, &body);
    header.group_id = vec![0x00u8; 32];

    let answer = drive_stream(
        &voter.server.dispatcher(),
        &hex::encode(node_id()),
        vec![
            RaftSnapshotFrameV1::Header(header),
            chunk(0, body.clone(), true),
        ],
    )
    .await;

    assert!(
        answer.is(StatusCode::PermissionDenied),
        "a header for another group must be refused; got {} / {}",
        answer.code,
        answer.text
    );

    voter.server.shutdown().await;
}
