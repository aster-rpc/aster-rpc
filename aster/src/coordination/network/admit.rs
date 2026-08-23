//! The §5.3 request-check order, as one function.
//!
//! ## The order is the contract
//!
//! | Step | Check | May decode the OpenRaft payload? |
//! | ---: | --- | --- |
//! | 0 | transport framing/flag/per-method cap | no |
//! | 1 | `iroh_only` and `PeerProvenance::IrohAuthenticated` | no |
//! | 2 | envelope canonicality, protocol/codec, group, sender width, sender == peer | no |
//! | 3 | exact-subject Producer admission | no |
//! | 4 | bootstrap/join or committed-member allowlist | no |
//! | 5 | payload ceiling, canonical Fory decode, method/message pairing | yes |
//! | 6 | invoke the exact OpenRaft handler | yes |
//!
//! Step 0 belongs to the transport — the §12.4 frame resolver applies the
//! per-method cap before a body frame is read, because the harm it prevents is
//! the allocation itself. Everything from step 1 is here, in [`admit`], as a
//! single straight-line function.
//!
//! **The order is load-bearing, not cosmetic.** Steps 1–4 must not decode the
//! inner payload: a node that decoded first would be doing work — allocation,
//! Fory type resolution, union dispatch — on behalf of a sender it has not yet
//! agreed to talk to. That is why `RaftEnvelopeV1::payload` is bounded opaque
//! bytes rather than a nested `RaftMessageV1`.
//!
//! It is written as one function rather than as a chain of guards each call
//! site assembles, because an order that lives in call sites is an order that
//! is right until the second call site.
//!
//! ## Identity comes from the transport, never from the payload
//!
//! `Call::peer()` is parsed into a `CoordinationNodeId` **only after**
//! `PeerProvenance::IrohAuthenticated` has been established. Provenance itself
//! carries no NodeId — it says how the id was learned, not what it is — so the
//! two are checked together and neither alone is sufficient. The envelope's
//! `sender_node_id` is then compared to that authenticated id: a remote can put
//! anything in the field, and this is what stops it selecting a local role.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::coordination::admission::{decide, AdmissionDenial, VerifiedAdmissionSource};
use crate::coordination::contract::payload_fory;
use crate::coordination::ids::{CoordinationGroupId, CoordinationNodeId};
use crate::coordination::model::limits::MAX_ENCODED_COMMAND;
use crate::coordination::raft::{RaftEnvelopeV1, RaftMessageV1};
use crate::rpc::{PeerProvenance, RpcStatus, StatusCode};

/// §5.1: the one protocol version v1 speaks.
pub(crate) const PROTOCOL_VERSION: i32 = 1;
/// §5.1: the internal Raft canonical codec version.
pub(crate) const RAFT_CODEC_VERSION: i32 = 1;

/// The §12.1 ceiling on one envelope payload.
///
/// The same bound as an encoded command, because the largest Raft payload is an
/// `AppendEntriesRequest` whose entries are commands — and §12.3's batch
/// invariant already sizes the *frame* for `max_payload_entries` of them. This
/// is the per-payload check that runs before the decode.
pub(crate) const MAX_RAFT_PAYLOAD: usize = MAX_ENCODED_COMMAND * 32;

/// Why a request was refused, and at which step.
///
/// Carrying the step is not decoration: the §6.2 oracle list requires proving
/// that a request failing two checks is refused by the **earlier** one, and a
/// refusal that only carried a status could not tell "denied because
/// unauthenticated" from "denied because not a member" when both are true.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// The check that refused, 1..=5.
    pub(crate) step: u8,
    /// The status the peer is answered with.
    pub(crate) status: StatusCode,
    /// Operator-facing detail. Sent to the peer only as a short reason; it
    /// never names another node's state.
    pub(crate) detail: String,
}

impl Refusal {
    fn at(step: u8, status: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            step,
            status,
            detail: detail.into(),
        }
    }

    /// The status to answer with.
    pub(crate) fn status(&self) -> RpcStatus {
        RpcStatus::error(self.status, self.detail.clone())
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "step {} refused: {}", self.step, self.detail)
    }
}

/// A request that passed every check, with the facts the handler needs.
#[derive(Debug)]
pub(crate) struct Admitted {
    /// The authenticated sender, from the transport.
    pub(crate) sender: CoordinationNodeId,
    /// The decoded message, already paired with its method.
    pub(crate) message: RaftMessageV1,
}

/// Who this voter accepts Raft traffic from (§7.2).
///
/// Two states, and the transition is one-way. Before a committed membership
/// contains the local node, the only senders are the ones a **signed** document
/// named: a genesis's initial voters for `Bootstrap`, a permit's
/// `bootstrap_senders` for `Join`. Afterwards the committed membership replaces
/// that list entirely.
///
/// `Existing` starts already graduated: its storage is initialized, so a
/// committed membership exists and the seed list would be a second, staler
/// answer to a question the membership already answers.
#[derive(Debug)]
pub(crate) struct SenderAllowlist {
    /// The signed seeds. Empty and unused once graduated.
    seeds: BTreeSet<CoordinationNodeId>,
    /// Latched. §7.2: "the committed membership replaces the permit
    /// allowlist", and a group that could fall back to the seed list would
    /// accept a node the group has since removed.
    graduated: AtomicBool,
}

impl SenderAllowlist {
    /// Before membership lands, accept only these signed senders.
    pub(crate) fn seeded(seeds: impl IntoIterator<Item = CoordinationNodeId>) -> Self {
        Self {
            seeds: seeds.into_iter().collect(),
            graduated: AtomicBool::new(false),
        }
    }

    /// Already a member: the committed membership is the allowlist.
    pub(crate) fn from_membership() -> Self {
        Self {
            seeds: BTreeSet::new(),
            graduated: AtomicBool::new(true),
        }
    }

    /// Whether the seed list has been replaced by committed membership.
    pub(crate) fn is_graduated(&self) -> bool {
        self.graduated.load(Ordering::SeqCst)
    }

    /// Record a committed membership.
    ///
    /// Graduation happens only when the membership contains the **local** node:
    /// §7.2 makes that the event, and a node that graduated on any membership
    /// entry would drop its seed list while still unable to be spoken to by the
    /// group it is joining.
    pub(crate) fn observe(
        &self,
        local: &CoordinationNodeId,
        members: &BTreeSet<CoordinationNodeId>,
    ) {
        if members.contains(local) {
            self.graduated.store(true, Ordering::SeqCst);
        }
    }

    /// Whether `sender` may speak Raft to this node right now.
    pub(crate) fn permits(
        &self,
        sender: &CoordinationNodeId,
        members: &BTreeSet<CoordinationNodeId>,
    ) -> bool {
        if self.is_graduated() {
            members.contains(sender)
        } else {
            self.seeds.contains(sender)
        }
    }
}

/// Everything the check order reads that is not part of the request.
pub(crate) struct AdmissionContext<'a> {
    pub(crate) group: &'a CoordinationGroupId,
    pub(crate) admission: &'a dyn VerifiedAdmissionSource,
    pub(crate) allowlist: &'a SenderAllowlist,
    /// The current committed membership, as the allowlist reads it.
    pub(crate) members: &'a BTreeSet<CoordinationNodeId>,
}

/// Steps 1 through 5, in order, for one unary request.
///
/// Returns at the **first** failure. `frame` is the envelope bytes; the inner
/// payload is not touched until step 5.
pub(crate) fn admit(
    ctx: &AdmissionContext<'_>,
    method: &str,
    provenance: PeerProvenance,
    peer: &str,
    frame: &[u8],
) -> Result<Admitted, Refusal> {
    // ── 1. Transport identity ────────────────────────────────────────────────
    //
    // Both halves. Provenance says the id was learned from an authenticated
    // handshake; parsing says it is a node id at all. An HTTP caller whose
    // principal string is identical to a voter's NodeId fails the first, and a
    // malformed authenticated peer fails the second.
    if !provenance.is_authenticated() {
        return Err(Refusal::at(
            1,
            StatusCode::Unauthenticated,
            "the coordination Raft service requires an authenticated Iroh peer",
        ));
    }
    let sender = CoordinationNodeId::parse_hex(peer).map_err(|e| {
        Refusal::at(
            1,
            StatusCode::Unauthenticated,
            format!("the authenticated peer is not a coordination node id: {e}"),
        )
    })?;

    // ── 2. The envelope, without its payload ─────────────────────────────────
    let envelope: RaftEnvelopeV1 = payload_fory::<RaftEnvelopeV1>()
        .deserialize(frame)
        .map_err(|_| {
            Refusal::at(
                2,
                StatusCode::InvalidArgument,
                "the request is not a coordination Raft envelope",
            )
        })?;

    // Canonicality before anything is read from it: a value with a second valid
    // spelling is a value two peers can disagree about while both verifying.
    let reencoded = payload_fory::<RaftEnvelopeV1>()
        .serialize(&envelope)
        .map_err(|_| {
            Refusal::at(
                2,
                StatusCode::InvalidArgument,
                "the envelope could not be re-encoded",
            )
        })?;
    if reencoded != frame {
        return Err(Refusal::at(
            2,
            StatusCode::InvalidArgument,
            "the envelope is not its own canonical encoding",
        ));
    }

    if envelope.protocol_version != PROTOCOL_VERSION
        || envelope.raft_codec_version != RAFT_CODEC_VERSION
    {
        // A permanent protocol fault. §5.4 refuses it rather than negotiating:
        // v1 supports same-protocol rolling restarts only.
        return Err(Refusal::at(
            2,
            StatusCode::InvalidArgument,
            format!(
                "protocol {}/codec {} is not v1",
                envelope.protocol_version, envelope.raft_codec_version
            ),
        ));
    }

    if envelope.group_id != ctx.group.as_bytes() {
        // `PermissionDenied` rather than `InvalidArgument`: the envelope is
        // well formed, and what is wrong is that it is addressed to a group
        // this node is not. The reply names no local state.
        return Err(Refusal::at(
            2,
            StatusCode::PermissionDenied,
            "this node does not serve that group",
        ));
    }

    // The width check is separate from the equality check below, so a
    // truncated id is diagnosed as malformed rather than as impersonation.
    let claimed = CoordinationNodeId::from_slice(&envelope.sender_node_id).map_err(|e| {
        Refusal::at(
            2,
            StatusCode::InvalidArgument,
            format!("the envelope's sender_node_id is not a node id: {e}"),
        )
    })?;
    if claimed != sender {
        return Err(Refusal::at(
            2,
            StatusCode::PermissionDenied,
            "the envelope's sender does not match the authenticated peer",
        ));
    }

    // ── 3. Verified producer admission ───────────────────────────────────────
    //
    // Rechecked on **every** call, per §8.4: a decision made once outlives the
    // policy that made it, and revocation has to take effect somewhere.
    decide(ctx.admission, sender).map_err(|denial| {
        Refusal::at(
            3,
            admission_status(&denial),
            // The peer learns that it was denied, not why: which of the five
            // denials it was is this node's operational state.
            "producer admission denied",
        )
    })?;

    // ── 4. The allowlist ─────────────────────────────────────────────────────
    if !ctx.allowlist.permits(&sender, ctx.members) {
        return Err(Refusal::at(
            4,
            StatusCode::PermissionDenied,
            "the sender is not a member of this group",
        ));
    }

    // ── 5. The payload, at last ──────────────────────────────────────────────
    if envelope.payload.len() > MAX_RAFT_PAYLOAD {
        // An inbound oversized payload is a peer's protocol fault, never a
        // `PayloadTooLarge` back at it — that would ask the peer to split
        // something it may not be able to split.
        return Err(Refusal::at(
            5,
            StatusCode::InvalidArgument,
            "the Raft payload exceeds its ceiling",
        ));
    }
    let message: RaftMessageV1 = payload_fory::<RaftMessageV1>()
        .deserialize(&envelope.payload)
        .map_err(|_| {
            Refusal::at(
                5,
                StatusCode::InvalidArgument,
                "the Raft payload will not decode",
            )
        })?;

    // The **inner** payload is canonicality-checked too, and the outer check
    // cannot stand in for it. Fory tolerates trailing bytes, so appending one
    // to a valid `RaftMessageV1` and wrapping the result in a freshly encoded
    // envelope passes the envelope comparison above while the message has a
    // second valid spelling. §5.1 refuses "trailing internal-codec bytes"
    // before dispatch, and this is where a Raft message meets that rule.
    let reencoded_payload = payload_fory::<RaftMessageV1>()
        .serialize(&message)
        .map_err(|_| {
            Refusal::at(
                5,
                StatusCode::InvalidArgument,
                "the Raft payload could not be re-encoded",
            )
        })?;
    if reencoded_payload != envelope.payload {
        return Err(Refusal::at(
            5,
            StatusCode::InvalidArgument,
            "the Raft payload is not its own canonical encoding",
        ));
    }

    if !pairs_with(method, &message) {
        return Err(Refusal::at(
            5,
            StatusCode::InvalidArgument,
            "the message type does not match the method it arrived on",
        ));
    }

    Ok(Admitted { sender, message })
}

/// Which status each admission denial answers with.
///
/// All five deny, and the peer is told the same thing; the *status* still
/// differs, because a source outage is not the peer's fault and a retry is
/// reasonable, while a missing or consumer record is not going to change on
/// its own.
fn admission_status(denial: &AdmissionDenial) -> StatusCode {
    match denial {
        // The source is down. §5.4 assigns `UNAVAILABLE` to conditions a later
        // message repairs, and this is one — mapping it to `PermissionDenied`
        // would tell the peer it is not enrolled when it may well be.
        AdmissionDenial::Unavailable { .. } => StatusCode::Unavailable,
        AdmissionDenial::Missing { .. }
        | AdmissionDenial::Malformed { .. }
        | AdmissionDenial::WrongSubject { .. }
        | AdmissionDenial::NotProducer { .. } => StatusCode::PermissionDenied,
    }
}

/// §5.3: a `vote` call cannot carry an append message.
///
/// Written as an exhaustive match over the union rather than as a lookup on the
/// method string, so a message variant added later is a compile error here
/// rather than a variant that pairs with nothing and is silently refused —
/// or, worse, one that pairs with everything.
pub(crate) fn pairs_with(method: &str, message: &RaftMessageV1) -> bool {
    match message {
        RaftMessageV1::VoteRequest(_) => method == "vote",
        RaftMessageV1::AppendRequest(_) => method == "append_entries",
        // Responses never arrive as requests. Listing them explicitly rather
        // than under a wildcard is what makes the compile-time check real.
        RaftMessageV1::VoteResponse(_)
        | RaftMessageV1::AppendSuccess(_)
        | RaftMessageV1::AppendPartialSuccess(_)
        | RaftMessageV1::AppendConflict(_)
        | RaftMessageV1::AppendHigherVote(_)
        | RaftMessageV1::SnapshotResponse(_) => false,
        // The forward-compatibility carrier is not a message this build can
        // act on. Accepting it would let a newer peer's payload reach a handler
        // that cannot mean the same thing by it.
        RaftMessageV1::Unknown(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::admission::testing::source_admitting;
    use crate::coordination::raft::{
        AppendRequestV1, AppendSuccessV1, VoteRequestV1, VoteResponseV1, VoteV1,
    };

    const LOCAL: [u8; 32] = [1u8; 32];
    const SENDER: [u8; 32] = [2u8; 32];
    const GROUP: [u8; 32] = [0xAB; 32];

    fn group() -> CoordinationGroupId {
        CoordinationGroupId::from_array(GROUP)
    }

    fn members() -> BTreeSet<CoordinationNodeId> {
        [LOCAL, SENDER]
            .into_iter()
            .map(CoordinationNodeId::from_array)
            .collect()
    }

    fn vote_message() -> RaftMessageV1 {
        RaftMessageV1::VoteRequest(VoteRequestV1 {
            vote: VoteV1 {
                term: 3,
                leader_node_id: SENDER.to_vec(),
                committed: false,
            },
            last_log_id: None,
        })
    }

    fn envelope(message: &RaftMessageV1) -> RaftEnvelopeV1 {
        RaftEnvelopeV1 {
            protocol_version: PROTOCOL_VERSION,
            raft_codec_version: RAFT_CODEC_VERSION,
            group_id: GROUP.to_vec(),
            sender_node_id: SENDER.to_vec(),
            payload: payload_fory::<RaftMessageV1>()
                .serialize(message)
                .expect("encode"),
        }
    }

    fn encode(envelope: &RaftEnvelopeV1) -> Vec<u8> {
        payload_fory::<RaftEnvelopeV1>()
            .serialize(envelope)
            .expect("encode")
    }

    struct Ctx {
        source: crate::coordination::admission::SignedAdmissionSet,
        allowlist: SenderAllowlist,
        members: BTreeSet<CoordinationNodeId>,
        group: CoordinationGroupId,
    }

    impl Ctx {
        fn new() -> Self {
            Self {
                source: source_admitting(&[LOCAL, SENDER]),
                allowlist: SenderAllowlist::from_membership(),
                members: members(),
                group: group(),
            }
        }

        fn ctx(&self) -> AdmissionContext<'_> {
            AdmissionContext {
                group: &self.group,
                admission: &self.source,
                allowlist: &self.allowlist,
                members: &self.members,
            }
        }
    }

    fn admit_with(ctx: &Ctx, frame: &[u8]) -> Result<Admitted, Refusal> {
        admit(
            &ctx.ctx(),
            "vote",
            PeerProvenance::IrohAuthenticated,
            &CoordinationNodeId::from_array(SENDER).to_hex(),
            frame,
        )
    }

    #[test]
    fn a_well_formed_request_from_an_admitted_member_is_accepted() {
        let ctx = Ctx::new();
        let admitted = admit_with(&ctx, &encode(&envelope(&vote_message()))).expect("admitted");
        assert_eq!(admitted.sender.as_bytes(), &SENDER);
        assert_eq!(admitted.message, vote_message());
    }

    /// **The order itself**, proved by making two checks fail at once and
    /// requiring the earlier one to win.
    ///
    /// This is the property a chain of independent guards cannot give you: each
    /// guard would be individually correct and the observable refusal would
    /// depend on which happened to run first. Every adjacent pair is exercised,
    /// so a reordering anywhere in the chain fails rather than only a
    /// reordering at the ends.
    #[test]
    fn a_request_failing_two_checks_is_refused_by_the_earlier_one() {
        let ctx = Ctx::new();

        // An envelope that is wrong at *every* step from 2 onward: wrong
        // version, wrong group, wrong sender, oversized undecodable payload,
        // and a message that pairs with no method.
        let broken = RaftEnvelopeV1 {
            protocol_version: 99,
            raft_codec_version: 99,
            group_id: vec![0xCD; 32],
            sender_node_id: vec![9u8; 32],
            payload: vec![0xFF; 16],
        };
        let frame = encode(&broken);

        // 1 beats 2: unauthenticated, though the envelope is also broken.
        let unauthenticated = admit(
            &ctx.ctx(),
            "vote",
            PeerProvenance::Other,
            &CoordinationNodeId::from_array(SENDER).to_hex(),
            &frame,
        )
        .expect_err("refused");
        assert_eq!(unauthenticated.step, 1, "{unauthenticated}");

        // 2 beats 3 and 4: the version is checked before admission, so a
        // sender that is *also* not admitted still fails at step 2.
        let mut denied = Ctx::new();
        denied.source = source_admitting(&[LOCAL]);
        denied.allowlist = SenderAllowlist::seeded([]);
        let version = admit_with(&denied, &frame).expect_err("refused");
        assert_eq!(version.step, 2, "{version}");

        // 3 beats 4: a sender outside the allowlist *and* unadmitted fails at
        // admission.
        let good = encode(&envelope(&vote_message()));
        let admission_first = admit_with(&denied, &good).expect_err("refused");
        assert_eq!(admission_first.step, 3, "{admission_first}");

        // 4 beats 5: an admitted non-member with an undecodable payload fails
        // at the allowlist.
        let mut outside = Ctx::new();
        outside.allowlist = SenderAllowlist::seeded([]);
        let mut bad_payload = envelope(&vote_message());
        bad_payload.payload = vec![0xFF; 16];
        let allowlist_first = admit_with(&outside, &encode(&bad_payload)).expect_err("refused");
        assert_eq!(allowlist_first.step, 4, "{allowlist_first}");

        // And 5 is reachable at all, so the chain above is not passing because
        // everything fails early.
        let mut mismatched = envelope(&vote_message());
        mismatched.payload = payload_fory::<RaftMessageV1>()
            .serialize(&RaftMessageV1::AppendSuccess(AppendSuccessV1 {}))
            .expect("encode");
        let pairing = admit_with(&ctx, &encode(&mismatched)).expect_err("refused");
        assert_eq!(pairing.step, 5, "{pairing}");
    }

    /// An HTTP caller whose principal string *is* a voter's NodeId is still
    /// denied. Identity is how it was learned, not what it says.
    #[test]
    fn an_unauthenticated_transport_cannot_impersonate_a_voter() {
        let ctx = Ctx::new();
        let refusal = admit(
            &ctx.ctx(),
            "vote",
            PeerProvenance::Other,
            &CoordinationNodeId::from_array(SENDER).to_hex(),
            &encode(&envelope(&vote_message())),
        )
        .expect_err("refused");
        assert_eq!(refusal.step, 1);
        assert_eq!(refusal.status, StatusCode::Unauthenticated);
    }

    /// The envelope's `sender_node_id` is compared to the **authenticated**
    /// peer, so a remote cannot select an identity from payload bytes.
    #[test]
    fn a_valid_envelope_under_the_wrong_peer_is_refused() {
        let ctx = Ctx::new();
        let refusal = admit(
            &ctx.ctx(),
            "vote",
            PeerProvenance::IrohAuthenticated,
            // Authenticated as LOCAL, claiming to be SENDER.
            &CoordinationNodeId::from_array(LOCAL).to_hex(),
            &encode(&envelope(&vote_message())),
        )
        .expect_err("refused");
        assert_eq!(refusal.step, 2);
        assert_eq!(refusal.status, StatusCode::PermissionDenied);
        assert!(refusal.detail.contains("authenticated peer"), "{refusal}");
    }

    /// A member of another group is refused without learning anything about
    /// this one.
    #[test]
    fn a_member_of_another_group_is_refused() {
        let ctx = Ctx::new();
        let mut elsewhere = envelope(&vote_message());
        elsewhere.group_id = vec![0xCD; 32];
        let refusal = admit_with(&ctx, &encode(&elsewhere)).expect_err("refused");
        assert_eq!(refusal.step, 2);
        assert!(
            !refusal.detail.contains(&group().to_hex()),
            "the reply must not name this node's group: {refusal}"
        );
    }

    /// A non-canonical envelope is refused, though it decodes.
    ///
    /// The oracle is a byte the encoder never emits: appending to a valid frame
    /// still decodes under Fory's framing, and the re-encode comparison is what
    /// catches it.
    #[test]
    fn an_envelope_with_a_second_spelling_is_refused() {
        let ctx = Ctx::new();
        let mut frame = encode(&envelope(&vote_message()));
        frame.push(0);
        match admit_with(&ctx, &frame) {
            Err(refusal) => {
                assert_eq!(refusal.step, 2, "{refusal}");
            }
            Ok(_) => panic!("a frame with trailing bytes must not be admitted"),
        }
    }

    /// Every producer denial refuses, and the outage case is answered
    /// differently from the others — a peer told `PermissionDenied` during our
    /// directory outage would reasonably conclude it had been removed.
    #[test]
    fn a_producer_denial_refuses_and_an_outage_is_answered_as_transient() {
        let mut ctx = Ctx::new();
        ctx.source = source_admitting(&[LOCAL]);
        let refusal = admit_with(&ctx, &encode(&envelope(&vote_message()))).expect_err("refused");
        assert_eq!(refusal.step, 3);
        assert_eq!(refusal.status, StatusCode::PermissionDenied);

        assert_eq!(
            admission_status(&AdmissionDenial::Unavailable {
                detail: "reloading".into()
            }),
            StatusCode::Unavailable
        );
        for denial in [
            AdmissionDenial::Missing {
                node: CoordinationNodeId::from_array(SENDER),
            },
            AdmissionDenial::NotProducer {
                node: CoordinationNodeId::from_array(SENDER),
            },
        ] {
            assert_eq!(admission_status(&denial), StatusCode::PermissionDenied);
        }
    }

    /// The denial reason is this node's operational state and is not sent to
    /// the peer.
    #[test]
    fn a_denied_peer_is_not_told_which_denial_it_was() {
        let mut ctx = Ctx::new();
        ctx.source = source_admitting(&[LOCAL]);
        let refusal = admit_with(&ctx, &encode(&envelope(&vote_message()))).expect_err("refused");
        assert_eq!(refusal.detail, "producer admission denied");
    }

    /// A `vote` call cannot carry an append message, and the pairing is
    /// exhaustive over the union rather than a lookup that could miss a
    /// variant.
    #[test]
    fn each_method_accepts_only_its_own_request_message() {
        let append = RaftMessageV1::AppendRequest(AppendRequestV1 {
            vote: VoteV1 {
                term: 3,
                leader_node_id: SENDER.to_vec(),
                committed: true,
            },
            prev_log_id: None,
            entries: Vec::new(),
            leader_commit: None,
        });
        assert!(pairs_with("vote", &vote_message()));
        assert!(pairs_with("append_entries", &append));
        assert!(!pairs_with("append_entries", &vote_message()));
        assert!(!pairs_with("vote", &append));

        // Responses are never requests, on any method.
        for method in ["vote", "append_entries", "install_snapshot"] {
            for response in [
                RaftMessageV1::VoteResponse(VoteResponseV1::default()),
                RaftMessageV1::AppendSuccess(AppendSuccessV1 {}),
                RaftMessageV1::Unknown(fory_core::UnknownCase::new(99u32, ())),
            ] {
                assert!(
                    !pairs_with(method, &response),
                    "{method} must not accept {response:?}"
                );
            }
        }
    }

    /// The allowlist is seeded before membership and replaced by it after, and
    /// the transition is one-way.
    #[test]
    fn the_allowlist_graduates_from_signed_seeds_to_committed_membership() {
        let local = CoordinationNodeId::from_array(LOCAL);
        let seed = CoordinationNodeId::from_array(SENDER);
        let stranger = CoordinationNodeId::from_array([9u8; 32]);

        let list = SenderAllowlist::seeded([seed]);
        assert!(!list.is_graduated());
        // Before membership, the seed list decides and the (empty) membership
        // does not.
        assert!(list.permits(&seed, &BTreeSet::new()));
        assert!(!list.permits(&stranger, &BTreeSet::new()));

        // A membership that does not contain us does not graduate us: we are
        // still joining, and dropping the seed list now would leave us
        // unreachable.
        let without_us: BTreeSet<_> = [seed].into_iter().collect();
        list.observe(&local, &without_us);
        assert!(!list.is_graduated());

        // A membership containing us does.
        let with_us: BTreeSet<_> = [seed, local].into_iter().collect();
        list.observe(&local, &with_us);
        assert!(list.is_graduated());

        // And now the membership decides, not the seeds: a seed that has since
        // been removed is refused.
        let after_removal: BTreeSet<_> = [local].into_iter().collect();
        assert!(!list.permits(&seed, &after_removal));
        assert!(list.permits(&local, &after_removal));

        // Latched: a later membership without us does not restore the seeds.
        list.observe(&local, &without_us);
        assert!(list.is_graduated());
        assert!(!list.permits(&seed, &after_removal));
    }

    /// An `Existing` start is already graduated: its storage holds a committed
    /// membership, and a seed list would be a staler second answer.
    #[test]
    fn an_existing_start_reads_the_membership_from_the_first_call() {
        let list = SenderAllowlist::from_membership();
        assert!(list.is_graduated());
        let seed = CoordinationNodeId::from_array(SENDER);
        assert!(!list.permits(&seed, &BTreeSet::new()));
        assert!(list.permits(&seed, &[seed].into_iter().collect()));
    }

    /// The **inner** payload is canonicality-checked, and the outer envelope
    /// check does not stand in for it.
    ///
    /// The fixture is the exact escape: a byte appended to a valid
    /// `RaftMessageV1`, wrapped in a **freshly encoded** envelope. The envelope
    /// is canonical — it was just produced by the encoder — so the outer
    /// comparison passes, and Fory accepts the trailing byte on the way in. Two
    /// spellings of one vote request is a vote two peers can disagree about
    /// while both verify.
    #[test]
    fn a_payload_with_a_second_spelling_is_refused_though_its_envelope_is_canonical() {
        let ctx = Ctx::new();

        let mut payload = payload_fory::<RaftMessageV1>()
            .serialize(&vote_message())
            .expect("encode");
        payload.push(0);
        let smuggled = RaftEnvelopeV1 {
            payload,
            ..envelope(&vote_message())
        };
        let frame = encode(&smuggled);

        // The envelope really is canonical, so this test is not passing for the
        // outer reason.
        let decoded: RaftEnvelopeV1 = payload_fory::<RaftEnvelopeV1>()
            .deserialize(&frame)
            .expect("decode");
        assert_eq!(
            payload_fory::<RaftEnvelopeV1>()
                .serialize(&decoded)
                .expect("encode"),
            frame,
            "the fixture's envelope must be canonical, or the inner check is untested"
        );
        // And the payload really does still decode, so the refusal is about
        // canonicality rather than about a decode failure.
        assert!(payload_fory::<RaftMessageV1>()
            .deserialize::<RaftMessageV1>(&decoded.payload)
            .is_ok());

        let refusal = admit_with(&ctx, &frame).expect_err("refused");
        assert_eq!(refusal.step, 5, "{refusal}");
        assert!(refusal.detail.contains("canonical"), "{refusal}");
    }

    /// An oversized payload is refused before it is decoded, and as the peer's
    /// protocol fault rather than as a request to split.
    #[test]
    fn an_oversized_inbound_payload_is_refused_before_the_decode() {
        let ctx = Ctx::new();
        let mut oversized = envelope(&vote_message());
        oversized.payload = vec![0u8; MAX_RAFT_PAYLOAD + 1];
        let refusal = admit_with(&ctx, &encode(&oversized)).expect_err("refused");
        assert_eq!(refusal.step, 5);
        assert_eq!(refusal.status, StatusCode::InvalidArgument);
        assert!(refusal.detail.contains("ceiling"), "{refusal}");
    }
}
