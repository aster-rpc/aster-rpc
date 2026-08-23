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

/// The receiving half of exactly one `install_snapshot` stream.
///
/// Owns the vote and metadata from its single header frame together with the
/// body those chunks are being written into, and hands all three on as one
/// value. **This is the only place a [`SnapshotReceipt`] is built**, and it is
/// built by consuming this receiver — so the three parts are never separately
/// selectable, not even inside this module.
#[derive(Debug)]
pub(crate) struct SnapshotStreamReceiver {
    vote: Vote<CoordinationNodeId>,
    meta: Meta,
    body: Box<CoordinationSnapshotFile>,
    writer: IncomingWriter,
    /// The next offset a chunk must start at. §5.3 requires exact contiguity.
    expected_offset: u64,
    declared_len: u64,
    declared_checksum: [u8; 32],
    hasher: blake3::Hasher,
    complete: bool,
}

impl SnapshotStreamReceiver {
    /// Begin receiving, from the one header frame and the receiver-owned
    /// temporary the chunks will land in.
    ///
    /// `vote` and `meta` are the *decoded* header payloads. They are passed in
    /// already decoded because decoding needs the §5.3 bounds check that runs
    /// before any payload is interpreted, which is the caller's step — but they
    /// come from **one** header, and this is the only constructor, so there is
    /// no path by which they can come from two.
    pub(crate) fn begin(
        header: &RaftSnapshotHeaderWireV1,
        vote: Vote<CoordinationNodeId>,
        meta: Meta,
        body: Box<CoordinationSnapshotFile>,
    ) -> Result<Self, StreamRefusal> {
        let declared_checksum: [u8; 32] = header
            .blake3_checksum
            .as_slice()
            .try_into()
            .map_err(|_| StreamRefusal::Malformed("the checksum is not 32 bytes".into()))?;
        let writer = body
            .writer()
            .map_err(|e| StreamRefusal::Storage(e.to_string()))?;
        Ok(Self {
            vote,
            meta,
            body,
            writer,
            expected_offset: 0,
            declared_len: header.total_len,
            declared_checksum,
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
        if after > self.declared_len {
            return Err(StreamRefusal::LengthMismatch {
                declared: self.declared_len,
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

    /// The vote this stream arrived under, for the admission recheck §8.4
    /// requires before install.
    pub(crate) fn vote(&self) -> &Vote<CoordinationNodeId> {
        &self.vote
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
            vote,
            meta,
            body,
            writer,
            expected_offset,
            declared_len,
            declared_checksum,
            hasher,
            complete: _,
        } = self;

        let written = writer
            .finish()
            .map_err(|e| StreamRefusal::Storage(e.to_string()))?;
        if written != declared_len || expected_offset != declared_len {
            return Err(StreamRefusal::LengthMismatch {
                declared: declared_len,
                received: written,
            });
        }
        if hasher.finalize().as_bytes() != &declared_checksum {
            return Err(StreamRefusal::ChecksumMismatch);
        }

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

/// Decode the vote a header carries.
///
/// Separate from the receiver's constructor so the receiver takes one already-
/// decoded value per field: decoding needs §5.3's bounds check, which runs
/// before any payload is interpreted at all.
pub(crate) fn decode_header_vote(
    header: &RaftSnapshotHeaderWireV1,
) -> Result<Vote<CoordinationNodeId>, StreamRefusal> {
    use crate::coordination::contract::payload_fory;
    let wire: VoteV1 = payload_fory::<VoteV1>()
        .deserialize(&header.vote_payload)
        .map_err(|e| StreamRefusal::Malformed(format!("the header vote will not decode: {e}")))?;
    Vote::try_from(&wire)
        .map_err(|e| StreamRefusal::Malformed(format!("the header vote is not a vote: {e:?}")))
}

/// Decode the metadata a header carries.
pub(crate) fn decode_header_meta(header: &RaftSnapshotHeaderWireV1) -> Result<Meta, StreamRefusal> {
    use crate::coordination::contract::payload_fory;
    let wire: SnapshotMetaV1 = payload_fory::<SnapshotMetaV1>()
        .deserialize(&header.snapshot_meta_payload)
        .map_err(|e| {
            StreamRefusal::Malformed(format!("the header metadata will not decode: {e}"))
        })?;
    Meta::try_from(&wire).map_err(|e| {
        StreamRefusal::Malformed(format!("the header metadata is not metadata: {e:?}"))
    })
}

#[cfg(test)]
mod tests {

    /// The structural obligations, stated as source guards because **no
    /// runtime test can hold them**.
    ///
    /// The vote-only substitution is unobservable: after
    /// [`SnapshotReceipt::prepare`] the vote is inside the token with no
    /// setter, and before it the vote arrived inside the receiver with the
    /// bytes. There is no value downstream to compare a vote against, so a test
    /// that appeared to exercise the crossing at runtime would be asserting a
    /// property no code path has. What *can* be checked is that the code has no
    /// shape in which the crossing could be expressed, and that is a statement
    /// about the source — the same instrument `storage::durability_guard` uses
    /// for the deferred-fsync rule.
    #[test]
    fn the_handoff_has_one_constructor_and_one_call_site() {
        let source = include_str!("handoff.rs");
        let end = production_end(source);
        let production = &source[..end];

        // Exactly one call to `install_full_snapshot`, and it is inside the
        // token's consuming method.
        let call_sites: Vec<usize> = production
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                let code = line.split("//").next().unwrap_or("");
                code.contains("install_full_snapshot(")
            })
            .map(|(n, _)| n + 1)
            .collect();
        assert_eq!(
            call_sites.len(),
            1,
            "install_full_snapshot must have exactly one call site; found at {call_sites:?}"
        );

        // Exactly one *construction* of each sealed value, and exactly one
        // destructuring. A struct literal and a `let` pattern are the only two
        // shapes either name can appear in as a value, so counting them
        // separately is what makes this a statement about construction rather
        // than about how often the word occurs.
        let count = |needle: &str| production.matches(needle).count();

        assert_eq!(
            count("Ok(SnapshotHandoff {"),
            1,
            "SnapshotHandoff must be constructed in exactly one place"
        );
        assert_eq!(
            count("let SnapshotHandoff {"),
            1,
            "and destructured in exactly one place — its consuming install"
        );
        assert_eq!(
            count("Ok(SnapshotReceipt {"),
            1,
            "SnapshotReceipt must be constructed in exactly one place"
        );
        assert_eq!(
            count("let SnapshotReceipt {"),
            1,
            "and destructured in exactly one place — prepare, which consumes it"
        );

        // And there is no constructor function for either: the only way in is
        // the two struct literals above, both inside methods that consume the
        // value that produced them.
        for forbidden in [
            "fn new(vote",
            "pub(crate) fn new(",
            "impl From<(",
            "fn from_parts(",
        ] {
            assert!(
                !production.contains(forbidden),
                "a {forbidden:?} constructor would let three separately chosen parts be \
                 assembled, which relocates the substitution rather than removing it"
            );
        }
    }

    /// `CoordinationSnapshotFile::Prepared` is constructed in exactly two
    /// places: [`SnapshotHandoff::install`] and the test-only suite adapter.
    ///
    /// `prepare_install` is deliberately **not** one of them: it returns a bare
    /// `PreparedSnapshot` and never wraps it. A guard naming it as a producer
    /// would readmit exactly the assembled-token path this design removes —
    /// obtain a `PreparedSnapshot`, wrap it yourself, and supply your own vote
    /// and metadata.
    ///
    /// A *pattern* is not a construction, so match arms and `let` bindings that
    /// destructure the variant are excluded. Counting them would make the guard
    /// fail for the one thing it wants: `install_snapshot` destructuring the
    /// role it accepts.
    #[test]
    fn the_prepared_variant_is_constructed_in_exactly_two_places() {
        const SOURCES: &[(&str, &str)] = &[
            ("network/handoff.rs", include_str!("handoff.rs")),
            ("network/errors.rs", include_str!("errors.rs")),
            (
                "storage/snapshot.rs",
                include_str!("../storage/snapshot.rs"),
            ),
            ("storage/machine.rs", include_str!("../storage/machine.rs")),
        ];
        let mut sites = Vec::new();
        for (name, source) in SOURCES {
            // Up to each file's trailing test module. A test that drives the
            // boundary directly — handing `install_snapshot` a `Prepared` it
            // built itself — is exercising the rule rather than escaping it,
            // and counting those would make the guard fail for the tests that
            // prove the property. What must stay bounded is the *shipped*
            // build, plus the one test-only adapter above the boundary.
            for (n, line) in source[..production_end(source)].lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                let names_it = code.contains("CoordinationSnapshotFile::Prepared(");
                // A match arm or a binding pattern destructures; it does not
                // build one.
                let is_pattern = code.contains("=>") || code.trim_start().starts_with("let ");
                if names_it && !is_pattern && !code.contains("Prepared(_)") {
                    sites.push((*name, n + 1, code.trim().to_string()));
                }
            }
        }

        let names: Vec<String> = sites
            .iter()
            .map(|(name, line, _)| format!("{name}:{line}"))
            .collect();
        assert_eq!(
            sites.len(),
            2,
            "the Prepared variant must be constructed in exactly two places — the handoff's \
             install and the test-only suite adapter; found: {names:?}"
        );
        assert_eq!(sites[0].0, "network/handoff.rs", "{names:?}");
        assert_eq!(sites[1].0, "storage/machine.rs", "{names:?}");

        // And the second one really is test-only. Without this the guard would
        // accept a *production* second producer as long as it lived in
        // `machine.rs`, which is the whole thing it is meant to forbid.
        let machine = include_str!("../storage/machine.rs");
        let before = &machine[..machine
            .lines()
            .take(sites[1].1 - 1)
            .map(|l| l.len() + 1)
            .sum::<usize>()];
        let guard = before
            .rfind("#[cfg(test)]")
            .expect("the second producer must sit under a #[cfg(test)] item");
        assert!(
            before[guard..].matches("\nimpl ").count() <= 1
                && before[guard..].matches("\nfn ").count() == 0,
            "the second producer must be inside the #[cfg(test)] item that immediately \
             precedes it, not merely somewhere after one"
        );
    }

    /// The production `install_snapshot` has no non-fatal raw-role arm.
    ///
    /// A `cfg(test)` arm inside it would delete the invariant from the very
    /// type the real-Raft boundary test exercises, so that test would prove a
    /// property the shipped build does not have. The suite reaches production
    /// installation through an adapter instead.
    #[test]
    fn the_production_install_has_no_test_only_raw_role_arm() {
        let machine = include_str!("../storage/machine.rs");
        let production = &machine[..production_end(machine)];
        let install = production
            .find("async fn install_snapshot(")
            .expect("the state machine implements install_snapshot");
        // The body ends at the next `async fn` at the same indentation.
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

    /// Where a file's production code ends: at its **last** `#[cfg(test)]`
    /// module. The same rule, and the same reasons, as
    /// `storage::durability_guard::production_end`.
    fn production_end(source: &str) -> usize {
        source
            .rfind("\n#[cfg(test)]\nmod ")
            .map(|at| at + 1)
            .unwrap_or(source.len())
    }

    /// The guards above search for strings the code really contains, so a guard
    /// pointed at the wrong token cannot pass by finding nothing.
    #[test]
    fn the_guards_look_for_tokens_that_exist() {
        let handoff = include_str!("handoff.rs");
        assert!(handoff.contains("install_full_snapshot("));
        // Built rather than written, so this line does not itself become a
        // third "construction site" for the guard above to find — the same
        // reason `storage::durability_guard` assembles the level name it
        // forbids.
        let variant = format!("{}::{}(", "CoordinationSnapshotFile", "Prepared");
        assert!(handoff.contains(&variant));
        assert!(include_str!("../storage/machine.rs").contains("async fn install_snapshot("));
    }
}
