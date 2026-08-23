//! The snapshot handoff: from one `install_snapshot` stream to
//! `Raft::install_full_snapshot`, as a single value that cannot be taken apart.
//!
//! ## What the wire already binds, and what a naive receiver breaks
//!
//! `RaftSnapshotHeaderWireV1` carries `vote_payload` and
//! `snapshot_meta_payload` in **one** frozen header frame, and
//! `RaftSnapshotFrameV1` has exactly two variants — one header, then that
//! header's chunks. Vote, metadata and body therefore arrive from the network
//! already indivisible.
//!
//! `Raft::install_full_snapshot(vote, Snapshot { meta, snapshot })` takes them
//! as three separable arguments. A receiver that decodes one stream into three
//! locals and passes them on as three arguments dissolves a binding the format
//! had. That is the whole defect this module exists to prevent; nothing about
//! the wire format needs to change.
//!
//! ## The vote cannot be checked later, and the metadata can
//!
//! The two halves are **not** symmetric.
//!
//! `SnapshotMeta` is derived from the body, so a crossed one *is* catchable
//! after the fact: the state machine compares OpenRaft's separately supplied
//! `meta` against the metadata sealed inside the prepared value, and a crossed
//! pair dies there.
//!
//! **A vote is derived from nothing the receiver retains, and the state machine
//! never receives it** — `RaftStateMachine::install_snapshot(&mut self, meta,
//! data)` has no vote parameter. There is no field, at any layer, against which
//! a vote can be validated. A vote-only substitution — body and metadata from
//! stream A, vote from an equally valid same-group stream B — therefore passes
//! every runtime check that can be written. It is prevented **by construction
//! or not at all**.
//!
//! Nor is it the harmless part of the triple. OpenRaft's
//! `handle_install_full_snapshot` calls `accept_vote(&vote, …)` *before*
//! installing the snapshot, so a substituted vote is accepted — and can move
//! this node's vote and leadership view — whether or not the install that
//! nominally carried it then succeeds.
//!
//! ## So: one receiver, one receipt, one token
//!
//! [`SnapshotStreamReceiver`] owns the header and the body for exactly one
//! stream and is the **only** producer of a [`SnapshotReceipt`]. There is no
//! reusable `(vote, meta, body)` constructor anywhere — public, crate-private
//! or test-only. [`SnapshotReceipt::prepare`] consumes the receipt **by value**
//! and is the only constructor of a [`SnapshotHandoff`], and
//! [`SnapshotHandoff::install`] is the **sole production call site** of
//! `install_full_snapshot`.
//!
//! A token assembled from three separately obtained parts would move the
//! crossing one call earlier rather than removing it: a sealing type is only as
//! strong as its constructor's argument list.

use openraft::{Raft, Snapshot, SnapshotMeta, Vote};

use crate::coordination::ids::CoordinationNodeId;
use crate::coordination::raft::{
    RaftSnapshotChunkWireV1, RaftSnapshotHeaderWireV1, SnapshotMetaV1, VoteV1,
};
use crate::coordination::storage::machine::StateHandle;
use crate::coordination::storage::snapshot::{
    CoordinationSnapshotFile, IncomingWriter, PrepareInstallError, PreparedSnapshot,
};
use crate::coordination::typeconfig::{CoordinationNode, CoordinationTypeConfig};

/// The declared metadata for this type config.
type Meta = SnapshotMeta<CoordinationNodeId, CoordinationNode>;

/// Why a snapshot stream was refused before it ever became a receipt.
///
/// Every variant is the **sender's** problem, which is what makes them safe to
/// answer: none of them says anything about this store's durability. A local
/// failure while staging arrives as [`Storage`](Self::Storage) and is not
/// reported to the peer as a bad snapshot.
#[derive(Debug)]
pub(crate) enum StreamRefusal {
    /// A header field was the wrong width, or a version/group binding failed.
    Malformed(String),
    /// A chunk arrived out of order, or overlapped one already accepted.
    ///
    /// §5.3 requires **exactly contiguous** chunks: a gap would leave
    /// unwritten bytes that the checksum would then have to catch, and an
    /// overlap makes "how many bytes have we accepted" ambiguous.
    NonContiguous { expected: u64, found: u64 },
    /// More bytes arrived than the header declared, or the stream ended short.
    LengthMismatch { declared: u64, received: u64 },
    /// The assembled body does not hash to the checksum the header declared.
    ChecksumMismatch,
    /// The stream ended without a final chunk.
    Incomplete,
    /// This store failed while writing the body. **Not** the sender's problem.
    Storage(String),
}

impl std::fmt::Display for StreamRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamRefusal::Malformed(d) => {
                write!(f, "the snapshot stream header is malformed: {d}")
            }
            StreamRefusal::NonContiguous { expected, found } => write!(
                f,
                "a snapshot chunk arrived at offset {found}; the stream is at {expected} and \
                 chunks must be exactly contiguous"
            ),
            StreamRefusal::LengthMismatch { declared, received } => write!(
                f,
                "the snapshot stream declared {declared} bytes and carried {received}"
            ),
            StreamRefusal::ChecksumMismatch => {
                f.write_str("the assembled snapshot body does not match its declared checksum")
            }
            StreamRefusal::Incomplete => {
                f.write_str("the snapshot stream ended without a final chunk")
            }
            StreamRefusal::Storage(d) => {
                write!(f, "this store failed while receiving a snapshot: {d}")
            }
        }
    }
}

impl std::error::Error for StreamRefusal {}

/// One snapshot stream header, decoded — vote, metadata, length and checksum
/// as a single value.
///
/// **The only constructor decodes them all from one `RaftSnapshotHeaderWireV1`.**
/// That is the whole point: an earlier revision of this module handed
/// [`SnapshotStreamReceiver::begin`] a header, a decoded vote, a decoded
/// metadata and a body as four independent arguments, so
/// `begin(header_a, vote_b, meta_a, body_a)` compiled and nothing compared the
/// vote with `header_a.vote_payload`. That is exactly the vote-substitution
/// hole the whole handoff exists to close, reintroduced one level below it —
/// a sealing type is only as strong as its constructor's argument list, and
/// the argument list had grown back.
#[derive(Debug)]
pub(crate) struct AdmittedHeader {
    vote: Vote<CoordinationNodeId>,
    meta: Meta,
    total_len: u64,
    checksum: [u8; 32],
}

impl AdmittedHeader {
    /// Decode one header. The only way to obtain this value.
    ///
    /// Both payloads are canonicality-checked, not merely decoded: Fory accepts
    /// trailing bytes, so a payload that decodes is not thereby its own
    /// canonical encoding, and §5.1 rejects "trailing internal-codec bytes"
    /// before dispatch. A vote with a second valid spelling is a vote two
    /// peers can disagree about while both verifying.
    pub(crate) fn decode(header: &RaftSnapshotHeaderWireV1) -> Result<Self, StreamRefusal> {
        let checksum: [u8; 32] = header
            .blake3_checksum
            .as_slice()
            .try_into()
            .map_err(|_| StreamRefusal::Malformed("the checksum is not 32 bytes".into()))?;

        let wire_vote: VoteV1 = canonical_decode(&header.vote_payload, "the header vote")?;
        let vote = Vote::try_from(&wire_vote).map_err(|e| {
            StreamRefusal::Malformed(format!("the header vote is not a vote: {e:?}"))
        })?;

        let wire_meta: SnapshotMetaV1 =
            canonical_decode(&header.snapshot_meta_payload, "the header metadata")?;
        let meta = Meta::try_from(&wire_meta).map_err(|e| {
            StreamRefusal::Malformed(format!("the header metadata is not metadata: {e:?}"))
        })?;

        Ok(Self {
            vote,
            meta,
            total_len: header.total_len,
            checksum,
        })
    }

    /// The vote this stream arrived under, for the §8.4 recheck before install.
    ///
    /// A read, not an extraction: the value stays inside this header and is
    /// moved out only by [`SnapshotStreamReceiver::finish`], which consumes the
    /// receiver that owns it.
    pub(crate) fn vote(&self) -> &Vote<CoordinationNodeId> {
        &self.vote
    }

    /// How many bytes the sender declared. A claim, bounded before use.
    pub(crate) fn total_len(&self) -> u64 {
        self.total_len
    }
}

/// Decode a bounded payload and prove it is its own canonical encoding.
///
/// Fory tolerates trailing bytes, so "it decoded" is not "these are the bytes
/// the encoder would produce". §5.1 refuses trailing internal-codec bytes
/// before dispatch, and this is where that applies to a snapshot header's two
/// nested payloads — the outer envelope check cannot see inside them.
fn canonical_decode<T>(bytes: &[u8], what: &str) -> Result<T, StreamRefusal>
where
    T: crate::rpc::contract::WireField + fory_core::serializer::Serializer + fory_core::ForyDefault,
{
    use crate::coordination::contract::payload_fory;
    let value: T = payload_fory::<T>()
        .deserialize(bytes)
        .map_err(|e| StreamRefusal::Malformed(format!("{what} will not decode: {e}")))?;
    let reencoded = payload_fory::<T>()
        .serialize(&value)
        .map_err(|e| StreamRefusal::Malformed(format!("{what} will not re-encode: {e}")))?;
    if reencoded != bytes {
        return Err(StreamRefusal::Malformed(format!(
            "{what} is not its own canonical encoding"
        )));
    }
    Ok(value)
}

/// The receiving half of exactly one `install_snapshot` stream.
///
/// Owns one decoded header and the body its chunks are being written into, and
/// hands both on as one value. **This is the only place a [`SnapshotReceipt`]
/// is built**, and it is built by consuming this receiver.
///
/// There is nothing to cross: the header is one already-sealed value, and the
/// body is created *here* rather than passed in, so the constructor has no two
/// arguments that could come from two streams.
#[derive(Debug)]
pub(crate) struct SnapshotStreamReceiver {
    header: AdmittedHeader,
    body: Box<CoordinationSnapshotFile>,
    writer: IncomingWriter,
    /// The next offset a chunk must start at. §5.3 requires exact contiguity.
    expected_offset: u64,
    hasher: blake3::Hasher,
    complete: bool,
}

impl SnapshotStreamReceiver {
    /// Begin receiving into a fresh receiver-owned temporary.
    ///
    /// Takes the sealed header and the storage layout — **not** a body. The
    /// temporary is created here, so no caller can hand this receiver a body
    /// that belongs to another stream, and no caller can hand it a vote or
    /// metadata that came from another header.
    pub(crate) fn begin(
        layout: &crate::coordination::storage::schema::StorageLayout,
        header: AdmittedHeader,
    ) -> Result<Self, StreamRefusal> {
        let body = Box::new(
            crate::coordination::storage::snapshot::begin_receiving(layout)
                .map_err(|e| StreamRefusal::Storage(e.to_string()))?,
        );
        let writer = body
            .writer()
            .map_err(|e| StreamRefusal::Storage(e.to_string()))?;
        Ok(Self {
            header,
            body,
            writer,
            expected_offset: 0,
            hasher: blake3::Hasher::new(),
            complete: false,
        })
    }

    /// Accept one body frame.
    ///
    /// Contiguity is checked against this receiver's own running offset rather
    /// than trusted from the frame: the offset field is a claim by the sender,
    /// and a stream that could skip forward would leave uninitialised bytes in
    /// a file this store is about to treat as a verified body.
    pub(crate) fn accept_chunk(
        &mut self,
        chunk: &RaftSnapshotChunkWireV1,
    ) -> Result<(), StreamRefusal> {
        if self.complete {
            return Err(StreamRefusal::Malformed(
                "a chunk arrived after the final one".into(),
            ));
        }
        if chunk.offset != self.expected_offset {
            return Err(StreamRefusal::NonContiguous {
                expected: self.expected_offset,
                found: chunk.offset,
            });
        }
        let after = self.expected_offset.saturating_add(chunk.data.len() as u64);
        if after > self.header.total_len {
            return Err(StreamRefusal::LengthMismatch {
                declared: self.header.total_len,
                received: after,
            });
        }
        self.writer
            .append(&chunk.data)
            .map_err(|e| StreamRefusal::Storage(e.to_string()))?;
        self.hasher.update(&chunk.data);
        self.expected_offset = after;
        self.complete = chunk.final_chunk;
        Ok(())
    }

    /// Whether the sender has marked the stream finished.
    pub(crate) fn is_complete(&self) -> bool {
        self.complete
    }

    /// The header this stream arrived under, for the §8.4 recheck before
    /// install.
    pub(crate) fn header(&self) -> &AdmittedHeader {
        &self.header
    }

    /// Close the stream and produce the receipt.
    ///
    /// Consumes the receiver, so a stream cannot yield two receipts and a
    /// receipt cannot outlive the stream that produced it. The length and
    /// checksum are verified here, against the bytes actually written rather
    /// than against the sender's claims about them.
    pub(crate) fn finish(self) -> Result<SnapshotReceipt, StreamRefusal> {
        if !self.complete {
            return Err(StreamRefusal::Incomplete);
        }
        let SnapshotStreamReceiver {
            header,
            body,
            writer,
            expected_offset,
            hasher,
            complete: _,
        } = self;

        let written = writer
            .finish()
            .map_err(|e| StreamRefusal::Storage(e.to_string()))?;
        if written != header.total_len || expected_offset != header.total_len {
            return Err(StreamRefusal::LengthMismatch {
                declared: header.total_len,
                received: written,
            });
        }
        if hasher.finalize().as_bytes() != &header.checksum {
            return Err(StreamRefusal::ChecksumMismatch);
        }

        let AdmittedHeader { vote, meta, .. } = header;
        Ok(SnapshotReceipt { vote, meta, body })
    }
}

/// One stream's vote, metadata and body, as a single opaque value.
///
/// **No public constructor and no accessor that yields the parts separately.**
/// The only producer is [`SnapshotStreamReceiver::finish`]; the only consumer
/// is [`SnapshotReceipt::prepare`], which takes it by value.
#[derive(Debug)]
pub(crate) struct SnapshotReceipt {
    vote: Vote<CoordinationNodeId>,
    meta: Meta,
    body: Box<CoordinationSnapshotFile>,
}

impl SnapshotReceipt {
    /// Prove the body against its metadata, and move this receipt's own vote
    /// into the result.
    ///
    /// The **only** constructor of a [`SnapshotHandoff`]. It takes `self` by
    /// value and reads the vote from its own field, so there is no argument
    /// list into which a different stream's vote could be passed.
    ///
    /// Preparation runs **outside any OpenRaft call**, which is the point: a
    /// refusal of the sender's bytes is answered to that sender and this voter
    /// keeps serving, where the same refusal inside `install_snapshot` becomes
    /// an OpenRaft `Fatal` that ends the state-machine worker.
    pub(crate) async fn prepare(
        self,
        state: &StateHandle,
    ) -> Result<SnapshotHandoff, PrepareInstallError> {
        let SnapshotReceipt { vote, meta, body } = self;
        let supplied = SnapshotMetaV1::from(&meta);
        let prepared = state.prepare_install(body, supplied).await?;
        Ok(SnapshotHandoff {
            vote,
            meta,
            prepared,
        })
    }
}

/// A proved body, the metadata proved against it, and the vote that authorized
/// the transfer — bound together for the one call that needs all three.
///
/// Constructed only by [`SnapshotReceipt::prepare`]. No public constructor, and
/// no accessor yielding the three parts separately.
#[derive(Debug)]
pub(crate) struct SnapshotHandoff {
    vote: Vote<CoordinationNodeId>,
    meta: Meta,
    prepared: PreparedSnapshot,
}

impl SnapshotHandoff {
    /// Hand the snapshot to OpenRaft.
    ///
    /// **The sole production call site of `install_full_snapshot`**, held there
    /// by the source guard in this module's tests. Consuming `self` is what
    /// makes that meaningful: the three values go in together and there is no
    /// way to obtain them apart.
    ///
    /// The state machine still compares metadata independently, because this
    /// token cannot reach across OpenRaft's call — `install_snapshot` receives
    /// `meta` as its own argument, and OpenRaft is free to hand over a
    /// different one. That comparison is a fatal invariant rather than a
    /// rejection: by that point both values are ours.
    pub(crate) async fn install(
        self,
        raft: &Raft<CoordinationTypeConfig>,
    ) -> Result<
        openraft::raft::SnapshotResponse<CoordinationNodeId>,
        openraft::error::Fatal<CoordinationNodeId>,
    > {
        let SnapshotHandoff {
            vote,
            meta,
            prepared,
        } = self;
        raft.install_full_snapshot(
            vote,
            Snapshot {
                meta,
                snapshot: Box::new(CoordinationSnapshotFile::Prepared(Box::new(prepared))),
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The **local** obligations. The subsystem-wide ones — one constructor for
    /// each sealed value, one production call site for `install_full_snapshot`,
    /// two construction sites for `SnapshotData::Prepared` — live in
    /// [`coordination::guards`](crate::coordination::guards), because a guard
    /// that scans only this file is exempt from its own rule the moment the
    /// construct appears in another. An earlier revision of these guards did
    /// scan only this file, and the Raft service handler was outside all of
    /// them.
    ///
    /// What is left here is the shape of *this* module's API, which no other
    /// file can affect.
    #[test]
    fn no_constructor_assembles_the_parts_separately() {
        let source = include_str!("handoff.rs");
        let production = &source[..crate::coordination::guards::production_end(source)];

        // No function anywhere in this module takes a vote and a metadata as
        // separate parameters. That signature is what made the first revision
        // forgeable: `begin(header_a, vote_b, meta_a, body_a)` compiled.
        for forbidden in [
            "vote: Vote<CoordinationNodeId>,\n        meta: Meta,",
            "fn new(vote",
            "fn from_parts(",
            "impl From<(",
        ] {
            assert!(
                !production.contains(forbidden),
                "a {forbidden:?} constructor would let separately chosen parts be assembled, \
                 which relocates the substitution rather than removing it"
            );
        }

        // `begin` takes the sealed header and a layout — never a body, so no
        // caller can hand this receiver another stream's temporary.
        let begin = production
            .find("pub(crate) fn begin(")
            .expect("the receiver has a constructor");
        let signature = &production[begin..begin + 200];
        assert!(
            signature.contains("header: AdmittedHeader"),
            "begin must take the sealed header: {signature}"
        );
        assert!(
            !signature.contains("body:"),
            "begin must create its own temporary rather than accept one: {signature}"
        );
        assert!(
            !signature.contains("vote:") && !signature.contains("meta:"),
            "begin must not take a vote or metadata separately: {signature}"
        );
    }

    /// The production `install_snapshot` has no `cfg(test)` raw-role arm.
    ///
    /// An arm inside the production method would delete the invariant from the
    /// very type the real-Raft boundary test exercises, so that test would be
    /// proving a property the shipped build does not have. The suite reaches
    /// production installation through an adapter instead.
    #[test]
    fn the_production_install_has_no_test_only_raw_role_arm() {
        let machine = include_str!("../storage/machine.rs");
        let production = &machine[..crate::coordination::guards::production_end(machine)];
        let install = production
            .find("async fn install_snapshot(")
            .expect("the state machine implements install_snapshot");
        let rest = &production[install..];
        let body_end = rest[1..]
            .find("\n    async fn ")
            .map(|at| at + 1)
            .unwrap_or(rest.len());
        let body = &rest[..body_end];
        assert!(
            !body.contains("cfg(test)"),
            "install_snapshot must have no cfg(test) arm; the suite uses an adapter"
        );
        assert!(
            body.contains("raw.role()"),
            "the raw-role arm must remain fatal and name the role it refused"
        );
    }

    /// A header's two payloads are canonicality-checked, not merely decoded.
    ///
    /// Fory tolerates trailing bytes, so a vote payload with one appended byte
    /// decodes to the same vote and is a *second spelling* of it. §5.1 refuses
    /// trailing internal-codec bytes before dispatch, and the outer envelope
    /// check cannot see inside a nested payload.
    #[test]
    fn a_header_payload_with_a_second_spelling_is_refused() {
        use crate::coordination::contract::payload_fory;
        use crate::coordination::raft::RaftSnapshotHeaderWireV1;

        let vote = VoteV1::from(&Vote::new_committed(
            7,
            CoordinationNodeId::from_array([1u8; 32]),
        ));
        // Built from a real `SnapshotMeta` with a non-empty membership: the
        // wire form's `TryFrom` refuses an empty one, and a baseline that could
        // not decode would make every negative case below pass for the wrong
        // reason.
        let meta = SnapshotMetaV1::from(&Meta {
            last_log_id: None,
            last_membership: openraft::StoredMembership::new(
                None,
                openraft::Membership::new(
                    vec![[CoordinationNodeId::from_array([1u8; 32])]
                        .into_iter()
                        .collect()],
                    None,
                ),
            ),
            snapshot_id: "s1".into(),
        });
        let good_vote = payload_fory::<VoteV1>().serialize(&vote).expect("encode");
        let good_meta = payload_fory::<SnapshotMetaV1>()
            .serialize(&meta)
            .expect("encode");

        let header = |vote_payload: Vec<u8>, meta_payload: Vec<u8>| RaftSnapshotHeaderWireV1 {
            protocol_version: 1,
            raft_codec_version: 1,
            group_id: vec![0xAB; 32],
            sender_node_id: vec![2u8; 32],
            vote_payload,
            snapshot_meta_payload: meta_payload,
            total_len: 0,
            blake3_checksum: vec![0u8; 32],
        };

        // The honest baseline: the untouched header decodes.
        AdmittedHeader::decode(&header(good_vote.clone(), good_meta.clone()))
            .expect("the untouched header must decode, or every case below passes vacuously");

        let mut trailing_vote = good_vote.clone();
        trailing_vote.push(0);
        // It still *decodes* — which is what makes the canonicality check the
        // thing doing the work here.
        assert!(payload_fory::<VoteV1>()
            .deserialize::<VoteV1>(&trailing_vote)
            .is_ok());
        let refusal = AdmittedHeader::decode(&header(trailing_vote, good_meta.clone()))
            .expect_err("a second spelling of the vote must be refused");
        assert!(refusal.to_string().contains("canonical"), "{refusal}");

        let mut trailing_meta = good_meta;
        trailing_meta.push(0);
        let refusal = AdmittedHeader::decode(&header(good_vote, trailing_meta))
            .expect_err("a second spelling of the metadata must be refused");
        assert!(refusal.to_string().contains("canonical"), "{refusal}");
    }

    /// A checksum that is not 32 bytes is refused before any chunk is
    /// accepted, so the receiver never runs without a comparison to make.
    #[test]
    fn a_header_without_a_full_checksum_is_refused() {
        use crate::coordination::contract::payload_fory;
        use crate::coordination::raft::RaftSnapshotHeaderWireV1;

        let header = RaftSnapshotHeaderWireV1 {
            protocol_version: 1,
            raft_codec_version: 1,
            group_id: vec![0xAB; 32],
            sender_node_id: vec![2u8; 32],
            vote_payload: payload_fory::<VoteV1>()
                .serialize(&VoteV1::from(&Vote::new_committed(
                    7,
                    CoordinationNodeId::from_array([1u8; 32]),
                )))
                .expect("encode"),
            snapshot_meta_payload: payload_fory::<SnapshotMetaV1>()
                .serialize(&SnapshotMetaV1::from(&Meta {
                    last_log_id: None,
                    last_membership: openraft::StoredMembership::new(
                        None,
                        openraft::Membership::new(
                            vec![[CoordinationNodeId::from_array([1u8; 32])]
                                .into_iter()
                                .collect()],
                            None,
                        ),
                    ),
                    snapshot_id: "s1".into(),
                }))
                .expect("encode"),
            total_len: 0,
            blake3_checksum: vec![0u8; 31],
        };
        let refusal = AdmittedHeader::decode(&header).expect_err("31 bytes is not a checksum");
        assert!(refusal.to_string().contains("32 bytes"), "{refusal}");
    }
}
