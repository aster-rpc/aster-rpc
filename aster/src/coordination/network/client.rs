//! `RaftNetwork` and `RaftNetworkFactory` over Aster RPC (§5.3, §5.4).
//!
//! ## Routes are dialling hints, and identity is not
//!
//! A `CoordinationNodeId` **is** an Ed25519 public key, so it is the whole
//! identity: dialling the right node is guaranteed by the transport handshake
//! rather than by the address book. A [`RouteTable`] entry therefore only makes
//! a node *reachable sooner* — an entry that is wrong or stale costs a
//! discovery round trip, and cannot cause a call to land on the wrong peer.
//!
//! That is why routes are advisory here and are not part of the admission
//! chain. §7.2's allowlist is about *who may speak to us*; this is about how we
//! find them.
//!
//! ## Every failure goes through the mapping tables
//!
//! Nothing in this module constructs an `RPCError` directly from an
//! `Error` — every path classifies through [`errors`](super::errors) first, so
//! the retry behaviour and the operator-visible fault class are decided in one
//! place. §5.4 requires the tables to exist before the factory that uses them,
//! and this is the factory using them.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use openraft::error::{Fatal, RPCError, RaftError, ReplicationClosed, StreamingError};
use openraft::network::{RPCOption, RPCTypes, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, SnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::{Snapshot, Vote};

use super::admit::{PROTOCOL_VERSION, RAFT_CODEC_VERSION};
use super::errors::{
    classify, classify_stream, to_rpc_error, Classified, FaultClass, Retry, StreamFailure,
    Transport,
};
use crate::coordination::contract::payload_fory;
use crate::coordination::ids::{CoordinationGroupId, CoordinationNodeId};
use crate::coordination::raft::{
    RaftEnvelopeV1, RaftMessageV1, RaftResponseEnvelopeV1, SnapshotMetaV1, VoteV1,
};
use crate::coordination::service::{RAFT_METHODS, RAFT_SERVICE, RAFT_VERSION};
use crate::coordination::typeconfig::{CoordinationNode, CoordinationTypeConfig};
use crate::rpc::codec::StreamHeader;
use crate::rpc::StatusCode;
use crate::{Error, Node};

/// Advisory dialling hints, keyed by node id.
///
/// Shared and swappable: the membership controller replaces entries as routes
/// commit, and the Raft network reads whatever is current. A missing entry is
/// not an error — the transport may already know the address, or discovery may
/// find it.
#[derive(Clone, Default)]
pub(crate) struct RouteTable {
    inner: Arc<RwLock<BTreeMap<CoordinationNodeId, String>>>,
}

impl RouteTable {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record a validated ticket for `node`.
    ///
    /// The caller has already validated it — §9's `check_seed_ticket` for a
    /// replicated route, or genesis verification for a seed. This does not
    /// re-validate, and it must not: a second opinion about what a ticket means
    /// is a second place for the two to disagree.
    pub(crate) fn insert(&self, node: CoordinationNodeId, ticket: String) {
        self.inner
            .write()
            .expect("the route table mutex")
            .insert(node, ticket);
    }

    pub(crate) fn get(&self, node: &CoordinationNodeId) -> Option<String> {
        self.inner
            .read()
            .expect("the route table mutex")
            .get(node)
            .cloned()
    }

    pub(crate) fn len(&self) -> usize {
        self.inner.read().expect("the route table mutex").len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A latched permanent fault, for the health surface.
///
/// §5.4: a version mismatch, a wrong group or a decode failure is not
/// repairable by retry, and OpenRaft has **no** permanent-fault variant to
/// carry that — every one of them has to map to `Unreachable` so replication
/// backs off instead of hot-looping. Without somewhere to record the
/// difference, an operator sees a peer that is merely unreachable.
#[derive(Clone, Default)]
pub(crate) struct FaultLatch {
    inner: Arc<RwLock<BTreeMap<CoordinationNodeId, FaultClass>>>,
}

impl FaultLatch {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record a classified failure, keeping the most serious class seen.
    ///
    /// Most serious rather than most recent: a permanent protocol fault that is
    /// followed by an ordinary timeout has not been repaired, and letting the
    /// timeout overwrite it would hide exactly the condition this exists for.
    pub(crate) fn record(&self, peer: CoordinationNodeId, class: FaultClass) {
        if !class.is_latched() {
            return;
        }
        let mut latched = self.inner.write().expect("the fault latch mutex");
        latched
            .entry(peer)
            .and_modify(|held| {
                if class > *held {
                    *held = class;
                }
            })
            .or_insert(class);
    }

    pub(crate) fn get(&self, peer: &CoordinationNodeId) -> Option<FaultClass> {
        self.inner
            .read()
            .expect("the fault latch mutex")
            .get(peer)
            .copied()
    }

    /// Clear a peer's latch. Called when an operator repairs the condition, or
    /// when a successful exchange proves it gone.
    pub(crate) fn clear(&self, peer: &CoordinationNodeId) {
        self.inner
            .write()
            .expect("the fault latch mutex")
            .remove(peer);
    }
}

/// Everything a per-peer client needs, shared across all of them.
#[derive(Clone)]
pub(crate) struct NetworkContext {
    pub(crate) node: Node,
    pub(crate) group: CoordinationGroupId,
    pub(crate) local: CoordinationNodeId,
    pub(crate) routes: RouteTable,
    pub(crate) faults: FaultLatch,
    /// The §6.2 fault injector, or `None` for a real network.
    ///
    /// `None` in every production start — asserted by a source guard in
    /// `boot.rs`, because a chaos handle reaching a shipping voter would be a
    /// deliberately unreliable network that nothing would notice. It lives on
    /// the production path rather than behind `cfg(test)` on purpose: a harness
    /// that wrapped a *different* client would be testing the wrapper, and the
    /// §5.3 checks, the envelope and the error classification are exactly what
    /// chaos has to run through.
    pub(crate) chaos: Option<std::sync::Arc<super::chaos::ChaosNetwork>>,
}

/// The factory. One per voter; it makes a client per peer.
#[derive(Clone)]
pub(crate) struct CoordinationNetworkFactory {
    ctx: NetworkContext,
}

impl CoordinationNetworkFactory {
    pub(crate) fn new(ctx: NetworkContext) -> Self {
        Self { ctx }
    }
}

impl RaftNetworkFactory<CoordinationTypeConfig> for CoordinationNetworkFactory {
    type Network = CoordinationNetwork;

    /// Deliberately does not connect. OpenRaft's own contract: this builds a
    /// client that will connect when required, because a factory that dialled
    /// eagerly would fail for a peer that is merely not up yet, and OpenRaft
    /// can only ignore the error.
    async fn new_client(
        &mut self,
        target: CoordinationNodeId,
        _node: &CoordinationNode,
    ) -> Self::Network {
        CoordinationNetwork {
            ctx: self.ctx.clone(),
            target,
        }
    }
}

/// One peer's client.
pub(crate) struct CoordinationNetwork {
    ctx: NetworkContext,
    target: CoordinationNodeId,
}

impl CoordinationNetwork {
    /// Wrap a message in the §5.3 envelope.
    fn envelope(&self, message: &RaftMessageV1) -> Result<Vec<u8>, Transport> {
        let payload = payload_fory::<RaftMessageV1>()
            .serialize(message)
            .map_err(|_| Transport::Undecodable)?;
        payload_fory::<RaftEnvelopeV1>()
            .serialize(&RaftEnvelopeV1 {
                protocol_version: PROTOCOL_VERSION,
                raft_codec_version: RAFT_CODEC_VERSION,
                group_id: self.ctx.group.to_vec(),
                sender_node_id: self.ctx.local.to_vec(),
                payload,
            })
            .map_err(|_| Transport::Undecodable)
    }

    /// Dial, importing this peer's advisory route first if we hold one.
    ///
    /// The import is best-effort by design: a malformed stored ticket must not
    /// stop the dial, because the transport may already know the address.
    /// Identity is the handshake's, so a bad hint costs a round trip and
    /// nothing else.
    async fn connect(&self) -> Result<crate::rpc::RpcConnection, Transport> {
        if let Some(ticket) = self.ctx.routes.get(&self.target) {
            if let Ok(parsed) = crate::ticket::Ticket::from_base58(&ticket) {
                let _ = self.ctx.node.add_ticket_addr(&parsed);
            }
        }
        let peer = crate::NodeId::from_hex(self.target.to_hex());
        self.ctx
            .node
            .rpc_connect(&peer)
            .await
            .map_err(|_| Transport::NotDialable)
    }

    /// One unary exchange, through the §6.2 fault schedule when one is
    /// installed.
    ///
    /// The schedule is consulted **before** the frame is built, so a dropped
    /// message costs what a dropped message costs: nothing is serialised, no
    /// connection is opened, and the peer never hears from us. A `Swallow`
    /// answers `NotDialable`, which is what a request that vanished looks like
    /// from the sender — it classifies `Unreachable`, so OpenRaft retries,
    /// which is the whole point of dropping it.
    async fn unary(
        &self,
        method: &'static str,
        message: &RaftMessageV1,
        option: &RPCOption,
    ) -> Result<RaftMessageV1, Transport> {
        let Some(chaos) = self.ctx.chaos.clone() else {
            return self.unary_once(method, message, option).await;
        };
        match chaos.before_send(&self.ctx.local, &self.target).await {
            super::chaos::ChaosAction::Send => self.unary_once(method, message, option).await,
            super::chaos::ChaosAction::Swallow => Err(Transport::NotDialable),
            // Both copies are really sent, and the **first** answer is the one
            // returned: a duplicate that discarded its first reply would test
            // nothing, because the receiver's second handling is exactly the
            // idempotence under test.
            super::chaos::ChaosAction::SendTwice => {
                let first = self.unary_once(method, message, option).await;
                let _ = self.unary_once(method, message, option).await;
                first
            }
        }
    }

    /// The real exchange, classified through the mapping tables at every exit.
    async fn unary_once(
        &self,
        method: &'static str,
        message: &RaftMessageV1,
        option: &RPCOption,
    ) -> Result<RaftMessageV1, Transport> {
        debug_assert!(RAFT_METHODS.contains(&method), "an undeclared Raft method");

        let frame = self.envelope(message)?;
        // §5.4: an outbound frame we cannot send is **ours**, and is refused
        // here rather than discovered by the transport. Which variant that
        // becomes is method-aware and lives in `errors::oversized_request`.
        if frame.len() > crate::coordination::service::RAFT_UNARY_FRAME_MAX as usize {
            return Err(Transport::OutboundOversized);
        }

        let connection = self.connect().await?;
        let header = StreamHeader {
            service: RAFT_SERVICE.to_string(),
            method: method.to_string(),
            version: RAFT_VERSION,
            deadline: option.hard_ttl().as_secs().min(i16::MAX as u64) as i16,
            ..Default::default()
        };

        let reply = match connection.unary(&header, frame).await {
            Ok(bytes) => bytes,
            Err(err) => return Err(transport_of(&err)),
        };
        // A completed exchange proves the latched condition is gone: a peer
        // that answers is not running an incompatible protocol, is not in
        // another group, and is not denying us. `FaultLatch::clear`'s own
        // contract says a successful exchange clears it, and nothing was
        // calling it.
        self.ctx.faults.clear(&self.target);

        self.open_response(&reply)
    }

    /// Check a response envelope and return the message inside it.
    ///
    /// **One rule for both the unary and the streaming reply paths.** An
    /// earlier revision checked protocol, codec and group on the unary reply
    /// and only the responder on the snapshot reply — so a peer answering a
    /// snapshot stream could have used any protocol version and any group id.
    /// The reply half deserves the same checks as the request half, and having
    /// two copies is how one of them ends up shorter.
    fn open_response(&self, reply: &[u8]) -> Result<RaftMessageV1, Transport> {
        let envelope: RaftResponseEnvelopeV1 = payload_fory::<RaftResponseEnvelopeV1>()
            .deserialize(reply)
            .map_err(|_| Transport::Undecodable)?;
        if envelope.protocol_version != PROTOCOL_VERSION
            || envelope.raft_codec_version != RAFT_CODEC_VERSION
        {
            return Err(Transport::VersionMismatch);
        }
        if envelope.group_id != self.ctx.group.as_bytes() {
            return Err(Transport::WrongGroup);
        }
        // §5.3: "a response's responder must equal the connected peer". Without
        // this a redirected or proxied reply would be accepted as the target's.
        if envelope.responder_node_id != self.target.as_bytes() {
            return Err(Transport::WrongGroup);
        }
        let message: RaftMessageV1 = payload_fory::<RaftMessageV1>()
            .deserialize(&envelope.payload)
            .map_err(|_| Transport::Undecodable)?;
        // And the reply payload is canonicality-checked for the same reason the
        // request's is: Fory tolerates trailing bytes, so a decoding payload is
        // not thereby its own canonical encoding, and §5.1 refuses trailing
        // internal-codec bytes in either direction.
        let reencoded = payload_fory::<RaftMessageV1>()
            .serialize(&message)
            .map_err(|_| Transport::Undecodable)?;
        if reencoded != envelope.payload {
            return Err(Transport::Undecodable);
        }
        Ok(message)
    }

    /// Classify, latch, and build the `RPCError` the trait returns.
    fn fail<E>(
        &self,
        failure: Transport,
    ) -> RPCError<CoordinationNodeId, CoordinationNode, RaftError<CoordinationNodeId, E>>
    where
        E: std::error::Error,
    {
        let classified = classify(&failure);
        self.ctx.faults.record(self.target, classified.fault);
        to_rpc_error(classified, format!("{failure:?}"))
    }

    /// The method-aware oversized rule (§5.4), applied at one place.
    ///
    /// `append_entries` is the only method that may be asked to split, and it
    /// supplies its own hint — so this returns the classification for the other
    /// two and lets the caller construct the hint itself.
    fn oversized(&self, method: RPCTypes) -> Option<Classified> {
        let classified = super::errors::oversized_request(method);
        if let Some(c) = classified {
            self.ctx.faults.record(self.target, c.fault);
        }
        classified
    }
}

/// Map an Aster RPC error into the mapping table's domain.
///
/// The one place a transport error becomes a `Transport`, so the table is
/// consulted for every outcome rather than for the ones somebody remembered.
fn transport_of(err: &Error) -> Transport {
    match err {
        // A non-OK trailer. The status table decides.
        Error::Rpc { code, .. } => Transport::Status(StatusCode::from_i32(*code)),
        Error::Connection(_) => Transport::NotDialable,
        // Everything else that reaches here came off an open stream. `Network`
        // — an immediate retry — is correct only for a *local* I/O error, and
        // this is the arm that produces it.
        _ => Transport::LocalIo,
    }
}

impl RaftNetwork<CoordinationTypeConfig> for CoordinationNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<CoordinationTypeConfig>,
        option: RPCOption,
    ) -> Result<
        AppendEntriesResponse<CoordinationNodeId>,
        RPCError<CoordinationNodeId, CoordinationNode, RaftError<CoordinationNodeId>>,
    > {
        let entries = rpc.entries.len();
        let message =
            RaftMessageV1::AppendRequest(crate::coordination::raft::AppendRequestV1::from(&rpc));

        match self.unary("append_entries", &message, &option).await {
            Ok(reply) => AppendEntriesResponse::try_from(&reply)
                .map_err(|_| self.fail::<openraft::error::Infallible>(Transport::Undecodable)),
            // The one method that may be asked to split. The hint must
            // **strictly decrease** the next request, so it is half the batch
            // rather than `entries` — a hint equal to what we just sent retries
            // the identical frame forever, and `new_entries_hint` asserts it is
            // above zero, so a single oversized entry has no representable
            // outcome at all. §12.3's startup invariant is what makes that
            // unreachable.
            Err(Transport::OutboundOversized) if entries > 1 => {
                self.ctx
                    .faults
                    .record(self.target, FaultClass::LocalInvariant);
                Err(RPCError::PayloadTooLarge(
                    openraft::error::PayloadTooLarge::new_entries_hint((entries / 2).max(1) as u64),
                ))
            }
            Err(failure) => Err(self.fail(failure)),
        }
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<CoordinationNodeId>,
        option: RPCOption,
    ) -> Result<
        VoteResponse<CoordinationNodeId>,
        RPCError<CoordinationNodeId, CoordinationNode, RaftError<CoordinationNodeId>>,
    > {
        let message =
            RaftMessageV1::VoteRequest(crate::coordination::raft::VoteRequestV1::from(&rpc));

        match self.unary("vote", &message, &option).await {
            Ok(RaftMessageV1::VoteResponse(reply)) => VoteResponse::try_from(&reply)
                .map_err(|_| self.fail::<openraft::error::Infallible>(Transport::Undecodable)),
            // Any other variant on the `vote` reply is the same protocol fault
            // the request-side pairing check catches inbound: a reply that is
            // not this method's message is not a reply we can act on.
            Ok(_) => Err(self.fail(Transport::Undecodable)),
            // **Never `PayloadTooLarge`.** A vote request is fixed-shape and
            // cannot be split, and a `Vote`-actioned one reaches `unreachable!`
            // in OpenRaft's own `Display` and `ReplicationCore::update_hint` —
            // a panic in the replication task. `Unreachable` plus a latched
            // local invariant is the honest answer: the diagnosis lives in the
            // latch, and the OpenRaft variant only chooses the retry behaviour.
            Err(Transport::OutboundOversized) => {
                let classified = self
                    .oversized(RPCTypes::Vote)
                    .expect("vote is not a splittable method");
                debug_assert_eq!(classified.retry, Retry::Unreachable);
                Err(to_rpc_error(
                    classified,
                    "this store built a vote frame above the negotiated cap".into(),
                ))
            }
            Err(failure) => Err(self.fail(failure)),
        }
    }

    async fn full_snapshot(
        &mut self,
        vote: Vote<CoordinationNodeId>,
        snapshot: Snapshot<CoordinationTypeConfig>,
        cancel: impl std::future::Future<Output = ReplicationClosed> + openraft::OptionalSend + 'static,
        option: RPCOption,
    ) -> Result<
        SnapshotResponse<CoordinationNodeId>,
        StreamingError<CoordinationTypeConfig, Fatal<CoordinationNodeId>>,
    > {
        // A snapshot stream is subject to the same schedule as a unary call,
        // and for the same reason: catch-up through a lossy network is exactly
        // the case §6.2 asks about. A duplicate is meaningless for a stream —
        // the receiver rejects a second concurrent one by design — so a
        // `SendTwice` verdict sends once here.
        if let Some(chaos) = self.ctx.chaos.clone() {
            if chaos.before_send(&self.ctx.local, &self.target).await
                == super::chaos::ChaosAction::Swallow
            {
                self.ctx.faults.record(self.target, FaultClass::Transient);
                return Err(stream_error(
                    super::errors::StreamRetry::Unreachable,
                    "the chaos schedule dropped this snapshot stream".into(),
                ));
            }
        }
        match self.send_snapshot(vote, snapshot, cancel, option).await {
            Ok(response) => Ok(response),
            Err(failure) => {
                let (retry, fault) = classify_stream(&failure);
                self.ctx.faults.record(self.target, fault);
                Err(stream_error(retry, format!("{failure:?}")))
            }
        }
    }
}

/// Build the `StreamingError` a classified stream failure returns.
///
/// Written as an exhaustive match with no wildcard, so a variant added to
/// [`StreamRetry`](super::errors::StreamRetry) is a compile error here rather
/// than one that silently lands in the nearest arm.
fn stream_error(
    retry: super::errors::StreamRetry,
    detail: String,
) -> StreamingError<CoordinationTypeConfig, Fatal<CoordinationNodeId>> {
    use super::errors::StreamRetry;
    match retry {
        StreamRetry::Closed => StreamingError::Closed(ReplicationClosed::new(detail)),
        StreamRetry::StorageError => StreamingError::StorageError(
            openraft::StorageIOError::read_snapshot(None, openraft::AnyError::error(detail)).into(),
        ),
        StreamRetry::Timeout => StreamingError::Timeout(openraft::error::Timeout {
            action: RPCTypes::InstallSnapshot,
            id: Default::default(),
            target: Default::default(),
            timeout: std::time::Duration::ZERO,
        }),
        StreamRetry::Unreachable => StreamingError::Unreachable(openraft::error::Unreachable::new(
            &std::io::Error::other(detail),
        )),
        StreamRetry::Network => StreamingError::Network(openraft::error::NetworkError::new(
            &std::io::Error::other(detail),
        )),
    }
}

/// Fill `buffer` from `reader`, returning how many bytes were read.
///
/// Returns less than `buffer.len()` **only** at end of file. A short read is
/// not EOF — `Read` may return fewer bytes than asked for at any time — and
/// treating one as the end would truncate the body and fail the receiver's
/// checksum with no explanation of why.
///
/// The one reading rule, shared by the hashing pass and the sending pass, so
/// the two cannot disagree about where the body ends.
pub(crate) fn read_chunk(
    reader: &mut impl std::io::Read,
    buffer: &mut [u8],
) -> Result<usize, StreamFailure> {
    let mut filled = 0usize;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(StreamFailure::LocalStorage),
        }
    }
    Ok(filled)
}

/// Hash a snapshot body in `chunk`-sized pieces, returning its length and
/// digest.
///
/// Constant memory: one chunk, reused. An earlier revision called
/// `read_to_end` here and then built every encoded frame into a second `Vec`,
/// so a 512 MiB snapshot needed over a gigabyte — against a §11.4 body ceiling
/// of exactly 512 MiB and a §11.3 rule that "no snapshot is held whole in
/// memory". The bound was declared and the sender ignored it.
pub(crate) fn measure_body(
    reader: &mut impl std::io::Read,
    chunk: usize,
) -> Result<(u64, [u8; 32]), StreamFailure> {
    debug_assert!(chunk > 0, "a zero-sized chunk never terminates");
    let mut buffer = vec![0u8; chunk];
    let mut hasher = blake3::Hasher::new();
    let mut total = 0u64;
    loop {
        let filled = read_chunk(reader, &mut buffer)?;
        if filled == 0 {
            break;
        }
        hasher.update(&buffer[..filled]);
        total += filled as u64;
        if filled < chunk {
            break;
        }
    }
    Ok((total, *hasher.finalize().as_bytes()))
}

impl CoordinationNetwork {
    /// The snapshot stream itself: one header frame, then contiguous bounded
    /// chunks, each encoded and written before the next is read.
    ///
    /// Two streaming passes over the body, and **neither holds it whole**. The
    /// header must carry `total_len` and the checksum before the first chunk —
    /// that is what lets a receiver bound the transfer instead of discovering
    /// its size as it arrives — so the digest needs a pass of its own.
    ///
    /// Reading the file twice is the cost of putting the checksum in the
    /// header, and it is the right trade: the alternative is a trailer, which
    /// would mean the receiver has already written 512 MiB before it can find
    /// out the bytes were wrong.
    ///
    /// Peak memory is one chunk plus its encoded frame, whatever the body's
    /// size.
    async fn send_snapshot(
        &mut self,
        vote: Vote<CoordinationNodeId>,
        snapshot: Snapshot<CoordinationTypeConfig>,
        cancel: impl std::future::Future<Output = ReplicationClosed> + openraft::OptionalSend + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<CoordinationNodeId>, StreamFailure> {
        use crate::coordination::raft::{
            RaftSnapshotChunkWireV1, RaftSnapshotFrameV1, RaftSnapshotHeaderWireV1,
        };

        tokio::pin!(cancel);

        let chunk_size = super::super::runtime::config::SNAPSHOT_MAX_CHUNK_SIZE as usize;

        // Pass one: length and digest, in constant memory.
        let (total_len, checksum) = {
            let mut hashing = snapshot
                .snapshot
                .reader()
                .map_err(|_| StreamFailure::LocalStorage)?;
            measure_body(&mut hashing, chunk_size)?
        };

        let meta = SnapshotMetaV1::from(&snapshot.meta);
        let header = RaftSnapshotHeaderWireV1 {
            protocol_version: PROTOCOL_VERSION,
            raft_codec_version: RAFT_CODEC_VERSION,
            group_id: self.ctx.group.to_vec(),
            sender_node_id: self.ctx.local.to_vec(),
            vote_payload: payload_fory::<VoteV1>()
                .serialize(&VoteV1::from(&vote))
                .map_err(|_| StreamFailure::LocalStorage)?,
            snapshot_meta_payload: payload_fory::<SnapshotMetaV1>()
                .serialize(&meta)
                .map_err(|_| StreamFailure::LocalStorage)?,
            total_len,
            blake3_checksum: checksum.to_vec(),
        };

        let connection = self.connect().await.map_err(StreamFailure::Transport)?;
        let stream_header = StreamHeader {
            service: RAFT_SERVICE.to_string(),
            method: "install_snapshot".to_string(),
            version: RAFT_VERSION,
            // §5.3: the caller's deadline rides on the header, as it does for
            // the unary path. An earlier revision ignored `option` entirely, so
            // a snapshot transfer had no deadline at all while
            // `install_snapshot_timeout` was configured at 30 s and enforced
            // nowhere.
            deadline: option.hard_ttl().as_secs().min(i16::MAX as u64) as i16,
            ..Default::default()
        };

        // `bidi_open` rather than `client_stream`: the latter takes
        // `Vec<Vec<u8>>` and would put every encoded frame in memory at once,
        // which is the allocation this method exists to avoid.
        // `BidiSink::send` writes one frame immediately and awaits, so the
        // transport's own backpressure paces the read.
        let (sink, mut responses) = connection
            .bidi_open(&stream_header)
            .await
            .map_err(|err| StreamFailure::Transport(transport_of(&err)))?;

        let fory = payload_fory::<RaftSnapshotFrameV1>();
        let send_frame = |frame: RaftSnapshotFrameV1| {
            let sink = &sink;
            let encoded = fory.serialize(&frame);
            async move {
                let encoded = encoded.map_err(|_| StreamFailure::LocalStorage)?;
                sink.send(encoded)
                    .await
                    .map_err(|err| StreamFailure::Transport(transport_of(&err)))
            }
        };

        send_frame(RaftSnapshotFrameV1::Header(header)).await?;

        // Pass two: read one chunk, encode it, write it, and only then read the
        // next. Nothing accumulates.
        let mut sending = snapshot
            .snapshot
            .reader()
            .map_err(|_| StreamFailure::LocalStorage)?;
        let mut buffer = vec![0u8; chunk_size];
        let mut offset = 0u64;
        loop {
            let filled = read_chunk(&mut sending, &mut buffer)?;
            if filled == 0 {
                break;
            }
            let chunk = RaftSnapshotFrameV1::Chunk(RaftSnapshotChunkWireV1 {
                offset,
                data: buffer[..filled].to_vec(),
                final_chunk: false,
            });
            tokio::select! {
                biased;
                _ = &mut cancel => return Err(StreamFailure::Closed),
                sent = send_frame(chunk) => sent?,
            }
            offset += filled as u64;
            if filled < chunk_size {
                break;
            }
        }

        // The final marker is its own zero-length chunk rather than a flag on
        // the last data chunk. That makes an empty body and a truncated stream
        // different things the receiver can tell apart, under one rule.
        send_frame(RaftSnapshotFrameV1::Chunk(RaftSnapshotChunkWireV1 {
            offset,
            data: Vec::new(),
            final_chunk: true,
        }))
        .await?;

        sink.finish()
            .await
            .map_err(|err| StreamFailure::Transport(transport_of(&err)))?;

        let reply = match responses.next().await {
            Some(Ok(payload)) => payload,
            Some(Err(err)) => return Err(StreamFailure::Transport(transport_of(&err))),
            None => return Err(StreamFailure::Transport(Transport::ConnectionLost)),
        };
        // Drain the trailer so a non-OK status still surfaces.
        if let Some(Err(err)) = responses.next().await {
            return Err(StreamFailure::Transport(transport_of(&err)));
        }
        self.ctx.faults.clear(&self.target);

        let message = self
            .open_response(&reply)
            .map_err(StreamFailure::Transport)?;
        match message {
            RaftMessageV1::SnapshotResponse(response) => Ok(SnapshotResponse {
                vote: Vote::try_from(&response.vote)
                    .map_err(|_| StreamFailure::Transport(Transport::Undecodable))?,
            }),
            _ => Err(StreamFailure::Transport(Transport::Undecodable)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::errors::classify_status;
    use super::*;

    fn node(n: u8) -> CoordinationNodeId {
        CoordinationNodeId::from_array([n; 32])
    }

    /// Routes are advisory: a table entry makes a peer reachable sooner and
    /// cannot make a call land on the wrong node, because identity is the
    /// handshake's.
    #[test]
    fn the_route_table_stores_and_replaces_hints() {
        let routes = RouteTable::new();
        assert!(routes.is_empty());
        assert_eq!(routes.get(&node(1)), None);

        routes.insert(node(1), "aster1one".into());
        assert_eq!(routes.get(&node(1)), Some("aster1one".into()));
        assert_eq!(routes.len(), 1);

        routes.insert(node(1), "aster1two".into());
        assert_eq!(
            routes.get(&node(1)),
            Some("aster1two".into()),
            "a newer route replaces an older one"
        );
        assert_eq!(routes.len(), 1);
    }

    /// The latch keeps the **most serious** class, not the most recent.
    ///
    /// A permanent protocol fault followed by an ordinary timeout has not been
    /// repaired, and letting the timeout overwrite it would hide exactly the
    /// condition the latch exists to surface.
    #[test]
    fn the_fault_latch_keeps_the_most_serious_class() {
        let faults = FaultLatch::new();
        assert_eq!(faults.get(&node(1)), None);

        // Transient classes are not latched at all: backoff already handles
        // them, and latching them would fill the surface with churn.
        faults.record(node(1), FaultClass::Transient);
        assert_eq!(faults.get(&node(1)), None);
        faults.record(node(1), FaultClass::PeerLoad);
        assert_eq!(faults.get(&node(1)), None);

        faults.record(node(1), FaultClass::PeerFault);
        assert_eq!(faults.get(&node(1)), Some(FaultClass::PeerFault));

        // A more serious one replaces it.
        faults.record(node(1), FaultClass::PermanentProtocol);
        assert_eq!(faults.get(&node(1)), Some(FaultClass::PermanentProtocol));

        // A less serious one does not.
        faults.record(node(1), FaultClass::PeerFault);
        assert_eq!(
            faults.get(&node(1)),
            Some(FaultClass::PermanentProtocol),
            "a later, milder failure does not mean the permanent one was repaired"
        );

        // And a transient one does not clear it either.
        faults.record(node(1), FaultClass::Transient);
        assert_eq!(faults.get(&node(1)), Some(FaultClass::PermanentProtocol));

        faults.clear(&node(1));
        assert_eq!(faults.get(&node(1)), None);
    }

    /// Peers are latched independently: one peer's permanent fault is not the
    /// group's.
    #[test]
    fn the_fault_latch_is_per_peer() {
        let faults = FaultLatch::new();
        faults.record(node(1), FaultClass::PermanentProtocol);
        faults.record(node(2), FaultClass::PeerRefusal);
        assert_eq!(faults.get(&node(1)), Some(FaultClass::PermanentProtocol));
        assert_eq!(faults.get(&node(2)), Some(FaultClass::PeerRefusal));
        assert_eq!(faults.get(&node(3)), None);
    }

    /// Every Aster transport error lands in the mapping table's domain, and the
    /// status arm routes through the eighteen-row table rather than guessing.
    #[test]
    fn every_transport_error_is_classified_through_the_tables() {
        assert_eq!(
            transport_of(&Error::Connection("no route".into())),
            Transport::NotDialable
        );
        assert_eq!(
            transport_of(&Error::InvalidArgument("write failed".into())),
            Transport::LocalIo
        );

        for code in [
            StatusCode::PermissionDenied,
            StatusCode::Unimplemented,
            StatusCode::DataLoss,
            StatusCode::Internal,
        ] {
            let err = Error::Rpc {
                code: code.as_i32(),
                message: String::new(),
                details: Vec::new(),
            };
            assert_eq!(transport_of(&err), Transport::Status(code));
            // And the status table decides what happens, not this function.
            assert_eq!(classify(&transport_of(&err)), classify_status(code));
        }

        // An unrecognised wire code funnels to `Unknown`, so there is no
        // status this function can produce that the table does not cover.
        let unknown = Error::Rpc {
            code: 4242,
            message: String::new(),
            details: Vec::new(),
        };
        assert_eq!(
            transport_of(&unknown),
            Transport::Status(StatusCode::Unknown)
        );
    }

    /// Each `StreamRetry` builds its own `StreamingError` variant, with no
    /// wildcard collapsing two of them.
    #[test]
    fn each_stream_classification_builds_its_own_variant() {
        use super::super::errors::StreamRetry;
        assert!(matches!(
            stream_error(StreamRetry::Closed, "x".into()),
            StreamingError::Closed(_)
        ));
        assert!(matches!(
            stream_error(StreamRetry::StorageError, "x".into()),
            StreamingError::StorageError(_)
        ));
        assert!(matches!(
            stream_error(StreamRetry::Timeout, "x".into()),
            StreamingError::Timeout(_)
        ));
        assert!(matches!(
            stream_error(StreamRetry::Unreachable, "x".into()),
            StreamingError::Unreachable(_)
        ));
        assert!(matches!(
            stream_error(StreamRetry::Network, "x".into()),
            StreamingError::Network(_)
        ));
    }

    /// A reader that records the largest single buffer it is ever asked to
    /// fill, and how many bytes it has served.
    ///
    /// The instrument for the flat-peak-memory claim: `Read::read` is handed
    /// the *whole* destination buffer, so the maximum slice length this sees is
    /// the largest allocation the caller made to hold body bytes. A
    /// `read_to_end` grows its buffer as it goes and would show a maximum on
    /// the order of the body.
    struct Watched {
        remaining: u64,
        max_ask: usize,
        served: u64,
    }

    impl std::io::Read for Watched {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.max_ask = self.max_ask.max(buf.len());
            if self.remaining == 0 {
                return Ok(0);
            }
            let n = (buf.len() as u64).min(self.remaining) as usize;
            buf[..n].fill(0xA5);
            self.remaining -= n as u64;
            self.served += n as u64;
            Ok(n)
        }
    }

    /// **Peak memory is flat in the body size.**
    ///
    /// A 64 MiB body through a 1 MiB chunk never asks for more than one chunk
    /// at a time, and the total served equals the body — so the pass is
    /// complete as well as bounded. Sixty-four times the chunk size is enough
    /// to make a whole-body buffer unmistakable while keeping the test fast.
    #[test]
    fn hashing_a_body_never_holds_more_than_one_chunk() {
        const CHUNK: usize = 1024 * 1024;
        const BODY: u64 = 64 * 1024 * 1024;

        let mut reader = Watched {
            remaining: BODY,
            max_ask: 0,
            served: 0,
        };
        let (len, digest) = measure_body(&mut reader, CHUNK).expect("hash");

        assert_eq!(len, BODY, "the whole body must be measured");
        assert_eq!(reader.served, BODY);
        assert_eq!(
            reader.max_ask, CHUNK,
            "the largest buffer offered to the reader must be one chunk, not the body"
        );

        // And the digest is the body's, computed here independently of the
        // chunking — so a pass that dropped or double-counted a chunk fails.
        let expected = blake3::hash(&vec![0xA5u8; BODY as usize]);
        assert_eq!(&digest, expected.as_bytes());
    }

    /// The chunk size does not change the answer, which is what makes the
    /// chunking incidental rather than part of the protocol's meaning.
    #[test]
    fn the_digest_is_independent_of_the_chunk_size() {
        const BODY: u64 = 300_000;
        let digests: Vec<[u8; 32]> = [1usize, 7, 4096, 65_536, 1 << 20]
            .into_iter()
            .map(|chunk| {
                let mut reader = Watched {
                    remaining: BODY,
                    max_ask: 0,
                    served: 0,
                };
                let (len, digest) = measure_body(&mut reader, chunk).expect("hash");
                assert_eq!(len, BODY, "chunk {chunk}");
                assert!(reader.max_ask <= chunk, "chunk {chunk}");
                digest
            })
            .collect();
        assert!(
            digests.windows(2).all(|w| w[0] == w[1]),
            "the digest must not depend on how the body was chunked"
        );
    }

    /// A short read is not end-of-file.
    ///
    /// `Read` may return fewer bytes than asked for at any time. Treating one
    /// as EOF truncates the body and fails the receiver's checksum with no
    /// explanation, which is the worst possible way for this to break: the
    /// sender looks healthy and the receiver looks corrupt.
    #[test]
    fn a_short_read_does_not_end_the_body() {
        /// Serves one byte at a time however much is asked for.
        struct Dribble(u64);
        impl std::io::Read for Dribble {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 == 0 || buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = 0x5A;
                self.0 -= 1;
                Ok(1)
            }
        }

        let (len, digest) = measure_body(&mut Dribble(5_000), 1024).expect("hash");
        assert_eq!(
            len, 5_000,
            "a one-byte-at-a-time reader must still be read whole"
        );
        assert_eq!(&digest, blake3::hash(&vec![0x5Au8; 5_000]).as_bytes());
    }

    /// An empty body measures as empty rather than failing.
    #[test]
    fn an_empty_body_measures_as_empty() {
        let (len, digest) = measure_body(&mut std::io::empty(), 1024).expect("hash");
        assert_eq!(len, 0);
        assert_eq!(&digest, blake3::hash(&[]).as_bytes());
    }

    /// **No whole-body buffering anywhere in the network tree.**
    ///
    /// The source-level half of the claim above, because the memory property is
    /// about code that does not exist: a single `read_to_end` reintroduces the
    /// gigabyte, and no runtime test of the *streaming* path would notice
    /// another path that buffers.
    #[test]
    fn nothing_in_the_network_tree_reads_a_body_whole() {
        use crate::coordination::guards::{production_code, SOURCES};
        let mut sites = Vec::new();
        for (name, source) in SOURCES {
            if !name.starts_with("network/") {
                continue;
            }
            for (n, line) in production_code(source) {
                for forbidden in ["read_to_end(", "client_stream("] {
                    if line.contains(forbidden) {
                        sites.push(format!("{name}:{n} {forbidden}"));
                    }
                }
            }
        }
        assert!(
            sites.is_empty(),
            "a snapshot body must never be held whole: read_to_end grows to the body's \
             size, and client_stream takes Vec<Vec<u8>> so every encoded frame is live at \
             once; found {sites:?}"
        );
    }

    /// The source-level half of §5.4's method-awareness rule.
    ///
    /// `PayloadTooLarge` may be constructed on exactly one path — the
    /// `append_entries` oversized arm. The mutation `S3-NETERR` names is
    /// returning `new_entries_hint(1)` from `vote`, which **compiles**: the
    /// constructor does not know which method called it. Nothing at runtime
    /// distinguishes the two call sites, so the guard is a statement about the
    /// source.
    #[test]
    fn payload_too_large_is_constructed_only_on_the_append_path() {
        let source = include_str!("client.rs");
        let end = source
            .rfind("\n#[cfg(test)]\nmod ")
            .map(|at| at + 1)
            .unwrap_or(source.len());
        let production = &source[..end];

        let sites: Vec<usize> = production
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                let code = line.split("//").next().unwrap_or("");
                code.contains("new_entries_hint(")
            })
            .map(|(n, _)| n + 1)
            .collect();
        assert_eq!(
            sites.len(),
            1,
            "the split hint must be constructed once, on the append path; found {sites:?}"
        );

        // And it is inside `append_entries`, not merely somewhere in the file.
        let append_at = production
            .find("async fn append_entries(")
            .expect("append_entries exists");
        let vote_at = production.find("async fn vote(").expect("vote exists");
        let hint_at = production
            .find("new_entries_hint(")
            .expect("the hint exists");
        assert!(
            append_at < hint_at && hint_at < vote_at,
            "the hint must sit inside append_entries, before vote begins"
        );
    }
}
