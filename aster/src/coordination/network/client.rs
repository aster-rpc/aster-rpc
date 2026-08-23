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

    /// One unary exchange, classified through the mapping tables at every exit.
    async fn unary(
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

        let envelope: RaftResponseEnvelopeV1 = payload_fory::<RaftResponseEnvelopeV1>()
            .deserialize(&reply)
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
        payload_fory::<RaftMessageV1>()
            .deserialize(&envelope.payload)
            .map_err(|_| Transport::Undecodable)
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

impl CoordinationNetwork {
    /// The snapshot stream itself: one header frame, then contiguous bounded
    /// chunks.
    ///
    /// Split out so [`full_snapshot`](RaftNetwork::full_snapshot) is only the
    /// classification, and this is only the protocol.
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
        use std::io::Read;

        let _ = option;
        tokio::pin!(cancel);

        // Read the whole body through the role-safe reader, hashing as we go.
        // The length and checksum go in the **header**, so they are known
        // before the first chunk — which is what lets a receiver bound the
        // transfer rather than discovering its size as it arrives.
        let mut reader = snapshot
            .snapshot
            .reader()
            .map_err(|_| StreamFailure::LocalStorage)?;
        let mut body = Vec::new();
        reader
            .read_to_end(&mut body)
            .map_err(|_| StreamFailure::LocalStorage)?;
        let checksum = blake3::hash(&body);

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
            total_len: body.len() as u64,
            blake3_checksum: checksum.as_bytes().to_vec(),
        };

        let fory = payload_fory::<RaftSnapshotFrameV1>();
        let mut frames = vec![fory
            .serialize(&RaftSnapshotFrameV1::Header(header))
            .map_err(|_| StreamFailure::LocalStorage)?];

        // §7.5's `snapshot_max_chunk_size`, which §12.3 has already proved fits
        // its frame with the chunk header.
        let chunk_size = super::super::runtime::config::SNAPSHOT_MAX_CHUNK_SIZE as usize;
        let mut offset = 0u64;
        for piece in body.chunks(chunk_size) {
            let final_chunk = offset + piece.len() as u64 == body.len() as u64;
            frames.push(
                fory.serialize(&RaftSnapshotFrameV1::Chunk(RaftSnapshotChunkWireV1 {
                    offset,
                    data: piece.to_vec(),
                    final_chunk,
                }))
                .map_err(|_| StreamFailure::LocalStorage)?,
            );
            offset += piece.len() as u64;
        }
        // An empty body still needs a final chunk, or the receiver cannot tell
        // a finished stream from a truncated one.
        if body.is_empty() {
            frames.push(
                fory.serialize(&RaftSnapshotFrameV1::Chunk(RaftSnapshotChunkWireV1 {
                    offset: 0,
                    data: Vec::new(),
                    final_chunk: true,
                }))
                .map_err(|_| StreamFailure::LocalStorage)?,
            );
        }

        let connection = self.connect().await.map_err(StreamFailure::Transport)?;
        let stream_header = StreamHeader {
            service: RAFT_SERVICE.to_string(),
            method: "install_snapshot".to_string(),
            version: RAFT_VERSION,
            ..Default::default()
        };

        let reply = tokio::select! {
            biased;
            _ = &mut cancel => return Err(StreamFailure::Closed),
            reply = connection.client_stream(&stream_header, frames) => reply,
        };
        let reply = reply.map_err(|err| StreamFailure::Transport(transport_of(&err)))?;

        let envelope: RaftResponseEnvelopeV1 = payload_fory::<RaftResponseEnvelopeV1>()
            .deserialize(&reply)
            .map_err(|_| StreamFailure::Transport(Transport::Undecodable))?;
        if envelope.responder_node_id != self.target.as_bytes() {
            return Err(StreamFailure::Transport(Transport::WrongGroup));
        }
        let message: RaftMessageV1 = payload_fory::<RaftMessageV1>()
            .deserialize(&envelope.payload)
            .map_err(|_| StreamFailure::Transport(Transport::Undecodable))?;
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
