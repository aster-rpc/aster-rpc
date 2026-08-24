//! The live `aster.coordination.Raft` v1 service (§5.3).
//!
//! Replaces the Slice 0 reservation once a runtime exists. Everything the
//! reservation established — the name, the per-method frame caps, `iroh_only` —
//! is preserved here rather than restated, by delegating to the same constants.
//!
//! ## Registered before the Raft exists, on purpose
//!
//! §3.3 fixes the startup order: register the Iroh-only services, start the RPC
//! acceptor, *then* spawn the controllers. Bootstrap cannot run before peers can
//! dial the services, so the service has to exist before the `Raft` it serves.
//!
//! [`RaftEndpoint`] is therefore late-bound. Before the runtime attaches, every
//! call is answered `FailedPrecondition` — §5.4's "the peer is up but not in a
//! state to serve Raft", classified `transient` by the sender, which is exactly
//! right during a rolling start. Answering `Unimplemented` would be a lie (the
//! method exists), and answering `Internal` would latch a peer fault on a node
//! that is merely still starting.
//!
//! ## The check order lives in `admit`, not here
//!
//! This module gathers the transport facts and calls
//! [`admit`](super::admit::admit) once. It does not re-check anything, and it
//! must not: a second copy of the order is a second thing to get wrong, and the
//! ordering test only covers the copy it can see.
//!
//! ## Admission is rechecked inside a snapshot stream
//!
//! A stream outlives the decision that admitted it. §8.4 requires a recheck at
//! bounded chunk intervals and once more before install; one chunk is at most
//! 1 MiB, so that is a bounded revocation interval with no wall-clock
//! dependency. Mid-stream revocation is one of §6.2's required oracles.

use std::collections::BTreeSet;
use std::sync::Arc;

use openraft::Raft;
use tokio::sync::RwLock;

use super::admit::{
    admit, admit_identity, AdmissionContext, ClaimedIdentity, Refusal, SenderAllowlist,
    PROTOCOL_VERSION, RAFT_CODEC_VERSION,
};
use super::handoff::{AdmittedHeader, InstallRefusal, SnapshotStreamReceiver, StreamRefusal};
use crate::coordination::admission::VerifiedAdmissionSource;
use crate::coordination::contract::payload_fory;
use crate::coordination::ids::{CoordinationGroupId, CoordinationNodeId};
use crate::coordination::raft::{
    RaftMessageV1, RaftResponseEnvelopeV1, RaftSnapshotFrameV1, SnapshotResponseV1, VoteV1,
};
use crate::coordination::service::{
    RAFT_METHODS, RAFT_SERVICE, RAFT_UNARY_FRAME_MAX, RAFT_VERSION, SNAPSHOT_CHUNK_FRAME_MAX,
};
use crate::coordination::storage::machine::StateHandle;
use crate::coordination::storage::snapshot::PrepareInstallError;
use crate::coordination::typeconfig::CoordinationTypeConfig;
use crate::rpc::{Call, RpcStatus, ServiceDispatch, StatusCode};

/// The runtime this service serves, attached after registration.
///
/// `RwLock<Option<..>>` rather than a `OnceCell`, because shutdown detaches it:
/// §7.6 deregisters RPC before `Raft::shutdown()`, and a call that arrives in
/// that window must be refused rather than reaching a Raft that is stopping.
#[derive(Clone, Default)]
pub(crate) struct RaftEndpoint {
    inner: Arc<RwLock<Option<AttachedRuntime>>>,
}

/// Everything a live handler needs.
#[derive(Clone)]
pub(crate) struct AttachedRuntime {
    pub(crate) raft: Raft<CoordinationTypeConfig>,
    pub(crate) state: StateHandle,
    pub(crate) group: CoordinationGroupId,
    pub(crate) local: CoordinationNodeId,
    pub(crate) admission: Arc<dyn VerifiedAdmissionSource>,
    pub(crate) allowlist: Arc<SenderAllowlist>,
    /// Where this store's `incoming/` lives. Carried here rather than reached
    /// through the state actor: creating the receiver-owned temporary is a
    /// filesystem step the actor has no reason to serialise, and queuing it
    /// behind an apply would stall a snapshot transfer on unrelated work.
    pub(crate) layout: crate::coordination::storage::schema::StorageLayout,
    /// The lifecycle facts an accepted append updates.
    ///
    /// A follower's `millis_since_quorum_ack` is always `None`, so §7.6's
    /// advisory freshness comes from here — and it is only advisory because
    /// one leader reaching us is not evidence of quorum.
    pub(crate) facts: Arc<crate::coordination::runtime::owner::RuntimeFacts>,
}

impl RaftEndpoint {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Attach the runtime. Called once the `Raft` exists.
    pub(crate) async fn attach(&self, runtime: AttachedRuntime) {
        *self.inner.write().await = Some(runtime);
    }

    /// Detach, so calls arriving during shutdown are refused rather than
    /// reaching a stopping `Raft`.
    pub(crate) async fn detach(&self) {
        *self.inner.write().await = None;
    }

    async fn attached(&self) -> Option<AttachedRuntime> {
        self.inner.read().await.clone()
    }
}

/// Who may speak Raft to this node: the **union** of the committed and
/// effective memberships.
///
/// The two halves answer different failures, and using either alone breaks a
/// real case.
///
/// **Committed alone starves the node that most needs to be spoken to.** A
/// voter returning onto its own store starts already graduated
/// ([`SenderAllowlist::from_membership`]), so this set *is* its allowlist — and
/// a node whose state machine is behind its log has applied no membership at
/// all. Its committed membership is empty, it admits nobody, and the leader
/// cannot reach it to send the snapshot that would let it catch up. That is a
/// deadlock, and it is exactly the stranded-follower case §S3-F exists to
/// prove: `a_stranded_follower_catches_up_by_snapshot` sat at `applied None`
/// for the full patience window. OpenRaft loads effective membership from the
/// **log** at startup, which is why it is available when the applied one is
/// not.
///
/// **Effective alone was the defect this replaced.** It is the newest
/// membership entry *seen*, committed or not, so a node named only by an entry
/// no quorum accepted would be admitted.
///
/// Admitting on the union is the weaker of the two risks by a wide margin, and
/// deliberately so: this is re-evaluated on **every request**, so an entry that
/// is rolled back stops admitting its nodes immediately. §7.2's *graduation* —
/// the permanent step, where a node stops being checked against its signed
/// seeds for good — reads the committed membership only, in
/// [`runtime::owner`](crate::coordination::runtime::owner). Those are different
/// decisions and they get different answers.
///
/// The voter *union* across a joint change, so a node that votes in either
/// configuration may speak.
fn admissible_members(runtime: &AttachedRuntime) -> BTreeSet<CoordinationNodeId> {
    let mut members = BTreeSet::new();
    let mut add = |membership: &openraft::Membership<
        CoordinationNodeId,
        crate::coordination::typeconfig::CoordinationNode,
    >| {
        members.extend(membership.voter_ids());
        // Learners too: a learner catching up must be able to answer the
        // leader's appends, and §7.3 adds one before it is ever a voter.
        members.extend(membership.nodes().map(|(id, _)| *id));
    };
    add(runtime.state.committed_membership().membership());
    add(runtime
        .raft
        .metrics()
        .borrow()
        .membership_config
        .membership());
    members
}

/// The live service.
pub(crate) struct CoordinationRaftService {
    endpoint: RaftEndpoint,
}

impl CoordinationRaftService {
    pub(crate) fn new(endpoint: RaftEndpoint) -> Self {
        Self { endpoint }
    }

    /// Wrap a reply in the §5.3 response envelope.
    fn envelope(runtime: &AttachedRuntime, message: &RaftMessageV1) -> Option<Vec<u8>> {
        let payload = payload_fory::<RaftMessageV1>().serialize(message).ok()?;
        payload_fory::<RaftResponseEnvelopeV1>()
            .serialize(&RaftResponseEnvelopeV1 {
                protocol_version: PROTOCOL_VERSION,
                raft_codec_version: RAFT_CODEC_VERSION,
                group_id: runtime.group.to_vec(),
                responder_node_id: runtime.local.to_vec(),
                payload,
            })
            .ok()
    }
}

/// The status this node answers with before its runtime is attached.
///
/// Named rather than inlined so the two call sites cannot drift, and so the
/// choice is stated once: `FailedPrecondition` is §5.4's "up but not in a state
/// to serve Raft", which the sender's table classifies transient.
fn not_yet_serving() -> RpcStatus {
    RpcStatus::error(
        StatusCode::FailedPrecondition,
        "this node is not yet serving coordination Raft",
    )
}

#[async_trait::async_trait]
impl ServiceDispatch for CoordinationRaftService {
    fn name(&self) -> &str {
        RAFT_SERVICE
    }

    fn version(&self) -> i32 {
        RAFT_VERSION
    }

    fn methods(&self) -> &[&str] {
        RAFT_METHODS
    }

    /// A voter's identity **is** its authority, so the service is unreachable
    /// over a transport that cannot authenticate the peer. Declared here as
    /// well as in the reservation, because this type replaces that one and a
    /// property that moved with the registration would move silently.
    fn iroh_only(&self) -> bool {
        true
    }

    /// The §12.3 caps, identical to the reservation's. Delegating to the same
    /// constants rather than restating the numbers is what keeps the live
    /// service from quietly widening a cap the reservation installed.
    fn max_frame_size(&self, method: &str) -> Option<u32> {
        match method {
            "vote" | "append_entries" => Some(RAFT_UNARY_FRAME_MAX),
            _ => Some(SNAPSHOT_CHUNK_FRAME_MAX),
        }
    }

    async fn dispatch(&self, method: &str, call: Call) {
        let Some(runtime) = self.endpoint.attached().await else {
            let _ = call.finish(&not_yet_serving()).await;
            return;
        };
        match method {
            "vote" | "append_entries" => unary(&runtime, method, call).await,
            "install_snapshot" => install_snapshot(&runtime, call).await,
            // Unreachable: `methods()` is non-empty, so dispatch rejects an
            // unknown method before this runs. Answered rather than panicked,
            // because a `todo!` here would turn a routing-table edit into a
            // crash in the accept loop.
            other => {
                let _ = call
                    .finish(&RpcStatus::error(
                        StatusCode::Unimplemented,
                        format!("{RAFT_SERVICE} v1 has no method '{other}'"),
                    ))
                    .await;
            }
        }
    }
}

/// `vote` and `append_entries`.
async fn unary(runtime: &AttachedRuntime, method: &str, mut call: Call) {
    let provenance = call.provenance();
    let peer = call.peer().to_string();
    let Some(frame) = call.recv_request().await else {
        let _ = call
            .finish(&RpcStatus::error(
                StatusCode::InvalidArgument,
                "a unary Raft call carries exactly one request frame",
            ))
            .await;
        return;
    };

    let members = admissible_members(runtime);
    let ctx = AdmissionContext {
        group: &runtime.group,
        admission: runtime.admission.as_ref(),
        allowlist: &runtime.allowlist,
        members: &members,
    };
    let admitted = match admit(&ctx, method, provenance, &peer, &frame) {
        Ok(admitted) => admitted,
        Err(refusal) => {
            let _ = call.finish(&refusal.status()).await;
            return;
        }
    };

    // Step 6: the exact handler. `admit` has already proved the pairing, so
    // each arm can only see its own message — the other arms are refusals of a
    // pairing that cannot reach here, not fallbacks.
    let reply = match admitted.message {
        RaftMessageV1::VoteRequest(request) => {
            match openraft::raft::VoteRequest::try_from(&request) {
                Ok(request) => match runtime.raft.vote(request).await {
                    Ok(response) => Ok(RaftMessageV1::VoteResponse(
                        crate::coordination::raft::VoteResponseV1::from(&response),
                    )),
                    Err(err) => Err(fatal_status(&err)),
                },
                Err(_) => Err(RpcStatus::error(
                    StatusCode::InvalidArgument,
                    "the vote request is not a vote",
                )),
            }
        }
        RaftMessageV1::AppendRequest(request) => {
            match openraft::raft::AppendEntriesRequest::try_from(&request) {
                Ok(request) => match runtime.raft.append_entries(request).await {
                    Ok(response) => {
                        // §7.6's advisory follower observation. Recorded only
                        // on a *success*: a rejected append proves the sender
                        // is reachable and **not** that it is our leader, and
                        // counting it would keep a stale contact fresh through
                        // exactly the disagreement that should age it out.
                        if matches!(
                            response,
                            openraft::raft::AppendEntriesResponse::Success
                                | openraft::raft::AppendEntriesResponse::PartialSuccess(_)
                        ) {
                            runtime.facts.note_leader_contact();
                        }
                        Ok(RaftMessageV1::from(&response))
                    }
                    Err(err) => Err(fatal_status(&err)),
                },
                Err(_) => Err(RpcStatus::error(
                    StatusCode::InvalidArgument,
                    "the append request is not an append",
                )),
            }
        }
        // `pairs_with` refused every other variant at step 5.
        _ => Err(RpcStatus::error(
            StatusCode::InvalidArgument,
            "the message type does not match the method it arrived on",
        )),
    };

    match reply {
        Ok(message) => match CoordinationRaftService::envelope(runtime, &message) {
            Some(bytes) => {
                let _ = call.respond(bytes, &RpcStatus::ok()).await;
            }
            None => {
                let _ = call
                    .finish(&RpcStatus::error(
                        StatusCode::Internal,
                        "the response could not be encoded",
                    ))
                    .await;
            }
        },
        Err(status) => {
            let _ = call.finish(&status).await;
        }
    }
}

/// A `Fatal` from our own Raft is **ours**, not the peer's.
///
/// §5.4 assigns `INTERNAL` to a durable failure followed by local not-ready
/// state, and the sender's table classifies `Internal` as `peer fault` — which
/// is exactly what it is from over there. Answering `Unavailable` would invite
/// an immediate retry into a node that has stopped.
fn fatal_status<E: std::fmt::Display>(err: &E) -> RpcStatus {
    RpcStatus::error(
        StatusCode::Internal,
        format!("this node cannot serve Raft: {err}"),
    )
}

/// `install_snapshot`: one header frame, then contiguous bounded chunks.
///
/// The whole §11.4 corridor on the receiving side — receive, prepare, hand off
/// — with the preparation refusal answered **here**, outside any OpenRaft call.
async fn install_snapshot(runtime: &AttachedRuntime, mut call: Call) {
    let provenance = call.provenance();
    let peer = call.peer().to_string();

    // The header frame carries the vote and metadata as one value, and steps
    // 0–4 run against it before a single chunk is accepted.
    let Some(first) = call.recv_request().await else {
        let _ = call
            .finish(&RpcStatus::error(
                StatusCode::InvalidArgument,
                "a snapshot stream begins with a header frame",
            ))
            .await;
        return;
    };

    // The header is a `RaftSnapshotFrameV1`, not a `RaftEnvelopeV1`, so the
    // envelope decode of step 2a does not apply to it. Everything else does,
    // and it is the *same* code: the header's claims are named as a
    // `ClaimedIdentity` and go through `admit_identity`.
    let frame = match decode_snapshot_frame(&first) {
        Ok(frame) => frame,
        Err(status) => {
            let _ = call.finish(&status).await;
            return;
        }
    };
    let RaftSnapshotFrameV1::Header(header) = frame else {
        let _ = call
            .finish(&RpcStatus::error(
                StatusCode::InvalidArgument,
                "a snapshot stream begins with a header frame, not a chunk",
            ))
            .await;
        return;
    };

    // One closure, used for the initial admission **and** every recheck. Not
    // two code paths that ought to agree: the previous revision admitted the
    // header one way and rechecked it another, and the recheck was the copy
    // that could not work.
    let recheck = |members: &BTreeSet<CoordinationNodeId>| -> Result<(), Refusal> {
        let ctx = AdmissionContext {
            group: &runtime.group,
            admission: runtime.admission.as_ref(),
            allowlist: &runtime.allowlist,
            members,
        };
        // Re-admitted rather than a cached verdict replayed: §8.4's recheck is
        // about the *current* policy, and a saved answer is the previous one by
        // definition.
        admit_identity(&ctx, provenance, &peer, &header_claim(&header)).map(|_| ())
    };

    if let Err(refusal) = recheck(&admissible_members(runtime)) {
        let _ = call.finish(&refusal.status()).await;
        return;
    }

    // The two header payloads are decoded into **one** sealed value, and the
    // receiver creates its own temporary — so there is no argument list here
    // into which another stream's vote, metadata or body could be passed.
    let admitted = match AdmittedHeader::decode(&header) {
        Ok(admitted) => admitted,
        Err(refusal) => return refuse_stream(call, &refusal).await,
    };
    let mut receiver = match SnapshotStreamReceiver::begin(&runtime.layout, admitted) {
        Ok(receiver) => receiver,
        Err(refusal) => return refuse_stream(call, &refusal).await,
    };

    // Chunks, with the §8.4 recheck on **every** one. A stream outlives the
    // decision that admitted it, and one chunk is at most 1 MiB, so this is a
    // bounded revocation interval without a wall-clock dependency.
    while !receiver.is_complete() {
        let Some(bytes) = call.recv_request().await else {
            let _ = call
                .finish(&RpcStatus::error(
                    StatusCode::InvalidArgument,
                    "the snapshot stream ended without a final chunk",
                ))
                .await;
            return;
        };
        if let Err(refusal) = recheck(&admissible_members(runtime)) {
            let _ = call.finish(&refusal.status()).await;
            return;
        }
        let frame = match decode_snapshot_frame(&bytes) {
            Ok(frame) => frame,
            Err(status) => {
                let _ = call.finish(&status).await;
                return;
            }
        };
        let RaftSnapshotFrameV1::Chunk(chunk) = frame else {
            let _ = call
                .finish(&RpcStatus::error(
                    StatusCode::InvalidArgument,
                    "a snapshot stream carries exactly one header",
                ))
                .await;
            return;
        };
        if let Err(refusal) = receiver.accept_chunk(&chunk) {
            return refuse_stream(call, &refusal).await;
        }
    }

    // Once more before install, per §8.4.
    if let Err(refusal) = recheck(&admissible_members(runtime)) {
        let _ = call.finish(&refusal.status()).await;
        return;
    }

    let receipt = match receiver.finish() {
        Ok(receipt) => receipt,
        Err(refusal) => return refuse_stream(call, &refusal).await,
    };

    // Preparation: outside any OpenRaft call, so a refusal of the sender's
    // bytes is answered here and this voter keeps serving.
    let handoff = match receipt.prepare(&runtime.state).await {
        Ok(handoff) => handoff,
        Err(err) => {
            let _ = call.finish(&prepare_status(&err)).await;
            return;
        }
    };

    // And the handoff, which is the sole production call site of
    // `install_full_snapshot`.
    match handoff.install(&runtime.raft).await {
        Ok(response) => {
            let message = RaftMessageV1::SnapshotResponse(SnapshotResponseV1 {
                vote: VoteV1::from(&response.vote),
            });
            match CoordinationRaftService::envelope(runtime, &message) {
                Some(bytes) => {
                    let _ = call.respond(bytes, &RpcStatus::ok()).await;
                }
                None => {
                    let _ = call
                        .finish(&RpcStatus::error(
                            StatusCode::Internal,
                            "the snapshot response could not be encoded",
                        ))
                        .await;
                }
            }
        }
        // A leader declining to install is not a fault on either side, and
        // answering `Internal` would latch a peer fault on a node that is
        // healthy and leading.
        Err(err @ InstallRefusal::NotAFollower) => {
            let _ = call
                .finish(&RpcStatus::error(
                    StatusCode::FailedPrecondition,
                    err.to_string(),
                ))
                .await;
        }
        Err(err) => {
            let _ = call.finish(&fatal_status(&err)).await;
        }
    }
}

/// A snapshot header's four identity claims, named for the shared check order.
///
/// This is the *whole* snapshot-specific part of steps 1–4: which fields carry
/// the claims. What is then done with them is [`admit_identity`], the same
/// function the unary path uses — so there is no second transcription of the
/// order to drift, and no way for the snapshot path to be checked by rules the
/// unary path does not have or vice versa.
fn header_claim(
    header: &crate::coordination::raft::RaftSnapshotHeaderWireV1,
) -> ClaimedIdentity<'_> {
    ClaimedIdentity {
        protocol_version: header.protocol_version,
        raft_codec_version: header.raft_codec_version,
        group_id: &header.group_id,
        sender_node_id: &header.sender_node_id,
    }
}

/// Decode one snapshot frame and prove it is its own canonical encoding.
///
/// §5.1 refuses trailing internal-codec bytes **before dispatch**, and Fory
/// tolerates them. The header's two nested payloads are checked separately
/// inside `AdmittedHeader::decode`, but that check cannot see the frame that
/// carried them: bytes appended after a valid `RaftSnapshotFrameV1` leave every
/// nested payload canonical while the frame itself has a second spelling.
fn decode_snapshot_frame(bytes: &[u8]) -> Result<RaftSnapshotFrameV1, RpcStatus> {
    let frame: RaftSnapshotFrameV1 = payload_fory::<RaftSnapshotFrameV1>()
        .deserialize(bytes)
        .map_err(|_| {
            RpcStatus::error(
                StatusCode::InvalidArgument,
                "the snapshot frame will not decode",
            )
        })?;
    let reencoded = payload_fory::<RaftSnapshotFrameV1>()
        .serialize(&frame)
        .map_err(|_| {
            RpcStatus::error(
                StatusCode::InvalidArgument,
                "the snapshot frame could not be re-encoded",
            )
        })?;
    if reencoded != bytes {
        return Err(RpcStatus::error(
            StatusCode::InvalidArgument,
            "the snapshot frame is not its own canonical encoding",
        ));
    }
    Ok(frame)
}

/// A stream refusal is the **sender's** problem except when it is not.
///
/// `Storage` is this store's own failure and answers `Internal`; everything
/// else is a malformed or inconsistent stream and answers `InvalidArgument`.
/// The peer must never be told its snapshot was bad because our disk filled.
async fn refuse_stream(call: Call, refusal: &StreamRefusal) {
    let code = match refusal {
        StreamRefusal::Storage(_) => StatusCode::Internal,
        StreamRefusal::ChecksumMismatch => StatusCode::DataLoss,
        StreamRefusal::Malformed(_)
        | StreamRefusal::NonContiguous { .. }
        | StreamRefusal::LengthMismatch { .. }
        | StreamRefusal::Oversized { .. }
        | StreamRefusal::Incomplete => StatusCode::InvalidArgument,
    };
    let _ = call
        .finish(&RpcStatus::error(code, refusal.to_string()))
        .await;
}

/// §5.4's outbound table: what a peer sees when our preparation fails.
///
/// The distinction `PrepareInstallError` carries has to survive the wire, or a
/// receiver's storage failure and a receiver's refusal of the sender's bytes
/// arrive as the same thing — and the sender's fault class, which is the
/// column that actually discriminates, would collapse with them.
fn prepare_status(err: &PrepareInstallError) -> RpcStatus {
    use crate::coordination::storage::error::StorageError;
    match err {
        // A bounds refusal. Not `ResourceExhausted`: that reads as "try again
        // later", and the same body is over the ceiling every time.
        PrepareInstallError::Rejected(StorageError::SnapshotTooLarge { .. }) => RpcStatus::error(
            StatusCode::InvalidArgument,
            "the snapshot exceeds this store's ceiling",
        ),
        // The sender's bytes did not survive their own structure.
        PrepareInstallError::Rejected(_) => {
            RpcStatus::error(StatusCode::DataLoss, "the snapshot was refused as unusable")
        }
        // Ours. The peer must not be told its snapshot was bad.
        PrepareInstallError::Storage(_) => RpcStatus::error(
            StatusCode::Internal,
            "this node failed while staging the snapshot",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::storage::error::StorageError;

    /// The three outbound statuses of §5.4, and what makes them different.
    ///
    /// The sender maps all three back through the eighteen-row table, where
    /// they land on `peer refusal`, `permanent protocol` and `peer fault`
    /// respectively — the distinction `PrepareInstallError` carries, preserved
    /// across the wire rather than lost at it.
    #[test]
    fn each_preparation_outcome_answers_its_own_status() {
        use super::super::errors::{classify_status, FaultClass};

        let corrupt = prepare_status(&PrepareInstallError::Rejected(StorageError::corrupt(
            crate::coordination::storage::error::Db::State,
            "snapshot",
            "truncated",
        )));
        assert_eq!(corrupt.code, StatusCode::DataLoss.as_i32());

        let oversized = prepare_status(&PrepareInstallError::Rejected(
            StorageError::SnapshotTooLarge { limit: 1, found: 2 },
        ));
        assert_eq!(oversized.code, StatusCode::InvalidArgument.as_i32());

        let ours = prepare_status(&PrepareInstallError::Storage(StorageError::Fatal));
        assert_eq!(ours.code, StatusCode::Internal.as_i32());

        // And the peer is never told its snapshot was bad because our disk was.
        assert_ne!(ours.code, corrupt.code);
        assert!(
            !ours.message.contains("snapshot was refused"),
            "a local failure must not read as a refusal of the sender's bytes: {}",
            ours.message
        );

        // The sender's fault classes stay distinct on the far side.
        assert_eq!(
            classify_status(StatusCode::DataLoss).fault,
            FaultClass::PeerRefusal
        );
        assert_eq!(
            classify_status(StatusCode::Internal).fault,
            FaultClass::PeerFault
        );
        assert_eq!(
            classify_status(StatusCode::InvalidArgument).fault,
            FaultClass::PermanentProtocol
        );
    }

    /// A node whose runtime has not attached answers `FailedPrecondition`.
    ///
    /// The status matters: the sender's table classifies it **transient**, so a
    /// peer that is merely still starting is retried rather than latched as a
    /// permanent fault. `Unimplemented` would be a lie — the method exists —
    /// and `Internal` would record a peer fault on a healthy node.
    #[test]
    fn a_node_that_is_not_yet_serving_answers_transiently() {
        use super::super::errors::{classify_status, FaultClass};

        let status = not_yet_serving();
        assert_eq!(status.code, StatusCode::FailedPrecondition.as_i32());
        assert_eq!(
            classify_status(StatusCode::FailedPrecondition).fault,
            FaultClass::Transient,
            "a starting node must not be latched as a permanent fault"
        );
    }

    /// The endpoint is attachable and detachable, and reads empty in both the
    /// pre-attach and post-detach windows.
    ///
    /// The detach half is the one that matters: §7.6 deregisters RPC before
    /// `Raft::shutdown()`, and a call arriving in that window must be refused
    /// rather than reach a Raft that is stopping.
    #[tokio::test]
    async fn the_endpoint_is_empty_before_attach_and_after_detach() {
        let endpoint = RaftEndpoint::new();
        assert!(endpoint.attached().await.is_none());
        endpoint.detach().await;
        assert!(
            endpoint.attached().await.is_none(),
            "detaching an unattached endpoint is a no-op, not a panic"
        );
    }

    /// The live service preserves every property the reservation installed.
    ///
    /// Asserted against the reservation itself rather than against literals: it
    /// is the thing this type replaces, so any cap or flag that moved is
    /// visible here. A live service that quietly widened `install_snapshot` to
    /// the 8 MiB unary cap would otherwise be a four-times-larger allocation
    /// nothing failed on.
    #[test]
    fn the_live_service_keeps_the_reservations_caps_and_flags() {
        use crate::coordination::service::ReservedRaft;

        let live = CoordinationRaftService::new(RaftEndpoint::new());
        assert_eq!(live.name(), ReservedRaft.name());
        assert_eq!(live.version(), ReservedRaft.version());
        assert_eq!(live.methods(), ReservedRaft.methods());
        assert_eq!(live.iroh_only(), ReservedRaft.iroh_only());
        assert!(live.iroh_only(), "and it is on");

        for method in RAFT_METHODS.iter().chain(["no_such_method"].iter()) {
            assert_eq!(
                live.max_frame_size(method),
                ReservedRaft.max_frame_size(method),
                "{method}"
            );
        }
        // The one that would be silently four times too large.
        assert_eq!(
            live.max_frame_size("install_snapshot"),
            Some(2_097_152),
            "a snapshot chunk must not inherit the 8 MiB unary cap"
        );
    }

    /// Neither handler restates the check order; both delegate to `admit`.
    ///
    /// This once exempted the snapshot path, on the grounds that a header is
    /// not a `RaftEnvelopeV1` and so "cannot go through `admit`". The exemption
    /// was the defect: the transcription that grew under it drifted into
    /// calling the envelope-only `admit` with a header frame, which cannot
    /// decode, so every per-chunk recheck refused and no snapshot was ever
    /// received. Only the *decode* differs between the two frames, and
    /// `ClaimedIdentity` is what separates it from the checks that do not.
    ///
    /// The subsystem-wide form — steps 1–4 exist in exactly one place across
    /// every coordination source file — is in
    /// [`coordination::guards`](crate::coordination::guards), because a rule
    /// scanning one file is exempt from itself the moment a third handler
    /// appears somewhere else.
    #[test]
    fn neither_handler_restates_the_check_order() {
        let source = include_str!("service.rs");
        let production = &source[..crate::coordination::guards::production_end(source)];

        let unary_at = production
            .find("async fn unary(")
            .expect("the unary handler exists");
        let install_at = production
            .find("async fn install_snapshot(")
            .expect("the snapshot handler exists");

        assert!(
            production[unary_at..install_at].contains("admit(&ctx,"),
            "the unary path must call admit"
        );
        assert!(
            production[install_at..].contains("admit_identity(&ctx,"),
            "the snapshot path must call admit_identity"
        );

        // Neither may ask any of the questions itself. `header_claim` names the
        // fields; it does not compare them.
        for own_check in [
            "is_authenticated()",
            "parse_hex(",
            "protocol_version !=",
            "allowlist.permits(",
            "admission::decide(",
        ] {
            assert!(
                !production[unary_at..].contains(own_check),
                "a handler repeats {own_check:?} instead of delegating the order"
            );
        }
    }
}
