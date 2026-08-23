//! The Slice 3 internal test harness (contract matrix §S3-E, §S3-F).
//!
//! ## Why this exists rather than a partial Store RPC
//!
//! Several Slice 3 properties are only observable once *committed traffic*
//! exists: a minority that cannot commit, a controller reconciling a durable
//! intent, a follower far enough behind to need a snapshot rather than a log
//! replay. All three need a client write, and the client write is
//! `aster.coordination.Store`, which Slice 4 owns.
//!
//! §6.2 forbids exposing a partial Store RPC to drive them early, and the
//! contract matrix says what to do instead: *"Slice 3 controller tests seed
//! accepted intents through the internal state-machine/test harness. They do
//! not expose a partial Store RPC merely to drive the controller before Slice
//! 4."* This module is that harness.
//!
//! ## What it is, and what it deliberately is not
//!
//! It is **one method**: propose an already-formed [`CommandV1`] through
//! `Raft::client_write`, the same call the Store service will make. Everything
//! else a gate needs — membership intents, log volume, a snapshot trigger —
//! composes from that, because a membership intent *is* a command.
//!
//! It is **not** a way to reach past Raft. There is no method here that writes
//! to the state machine directly, skips replication, or fabricates a commit:
//! a seam that could do any of those would let a test observe an outcome the
//! production path cannot produce, which is the failure mode these gates exist
//! to catch. Every byte still goes through election, replication and quorum.
//!
//! ## It is not compiled into anything that ships
//!
//! Gated on `coordination-testing`, which no shipping profile enables and
//! which `coordination` does **not** imply. `check-coordination-deps.sh`
//! asserts that, so turning it on for convenience would fail the gate rather
//! than quietly widening the public API.

use openraft::error::{ClientWriteError, RaftError};
use openraft::Raft;

use crate::coordination::contract::{CommandReplyV1, CommandV1};
use crate::coordination::typeconfig::CoordinationTypeConfig;
use crate::coordination::Coordination;

/// The `RaftError` a client write can answer with, spelled once.
pub type WriteRejection = RaftError<
    crate::coordination::ids::CoordinationNodeId,
    ClientWriteError<
        crate::coordination::ids::CoordinationNodeId,
        crate::coordination::typeconfig::CoordinationNode,
    >,
>;

/// Why a seeded proposal did not commit.
///
/// The two outcomes are kept apart because a gate asserting "a minority cannot
/// commit" has to distinguish *not committed* from *not attempted*. A single
/// opaque error would let a test that never reached Raft pass as evidence that
/// Raft refused.
#[derive(Debug)]
pub enum ProposeError {
    /// Raft answered, and the answer was a refusal — no leader, a forward, a
    /// rejected membership change. The proposal reached consensus machinery
    /// and consensus machinery declined it. Boxed because it dwarfs the other
    /// variant, which is the one a stalled minority actually produces.
    Rejected(Box<WriteRejection>),
    /// The proposal did not resolve within the caller's window. **Not** proof
    /// of refusal: an entry that times out here may still commit later, which
    /// is exactly why a minority-safety gate must also show that it did not.
    TimedOut,
}

impl std::fmt::Display for ProposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposeError::Rejected(err) => write!(f, "the proposal was refused: {err}"),
            ProposeError::TimedOut => f.write_str("the proposal did not resolve in time"),
        }
    }
}

impl std::error::Error for ProposeError {}

impl Coordination {
    /// Propose one command through the production Raft path.
    ///
    /// Exactly what the Slice 4 Store service will do with a decoded request:
    /// `Raft::client_write`, awaited to commit-and-apply. A leader commits it
    /// once a quorum acknowledges; a follower answers `ForwardToLeader`; a
    /// node without a quorum never commits it at all, which is the point of
    /// the gate that calls this.
    ///
    /// The reply is the state machine's own — not a synthesised acknowledgement
    /// — so a caller can tell "applied" from "accepted for replication".
    pub async fn propose_for_testing(
        &self,
        command: CommandV1,
    ) -> Result<CommandReplyV1, ProposeError> {
        self.raft
            .client_write(command)
            .await
            .map(|response| response.data)
            .map_err(|err| ProposeError::Rejected(Box::new(err)))
    }

    /// Propose, but give up after `within`.
    ///
    /// A proposal into a group that cannot reach a quorum does not fail — it
    /// *waits*, because the entry is legitimately still pending and OpenRaft
    /// has no basis to abandon it. A gate proving a minority cannot commit must
    /// therefore bound its own patience and then check the log, rather than
    /// waiting for an error that never comes.
    pub async fn propose_for_testing_within(
        &self,
        command: CommandV1,
        within: std::time::Duration,
    ) -> Result<CommandReplyV1, ProposeError> {
        match tokio::time::timeout(within, self.propose_for_testing(command)).await {
            Ok(result) => result,
            Err(_) => Err(ProposeError::TimedOut),
        }
    }

    /// The last log index this node has **applied** to its state machine.
    ///
    /// The independent oracle for a commit: a proposal that returned a reply
    /// and a log that never advanced would be a fabricated acknowledgement, and
    /// nothing else in the public surface can tell those apart.
    pub fn last_applied_for_testing(&self) -> Option<u64> {
        self.raft
            .metrics()
            .borrow()
            .last_applied
            .map(|log_id| log_id.index)
    }

    /// The voter ids in the currently committed membership.
    ///
    /// Read from `RaftMetrics`, which is the group's agreed answer rather than
    /// a controller's local view of what it was trying to achieve — the
    /// distinction the §S3-E completion rule turns on.
    pub fn committed_voters_for_testing(
        &self,
    ) -> Vec<crate::coordination::ids::CoordinationNodeId> {
        self.raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .collect()
    }

    /// Every node in the committed membership, voter or learner.
    pub fn committed_nodes_for_testing(&self) -> Vec<crate::coordination::ids::CoordinationNodeId> {
        self.raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .nodes()
            .map(|(id, _)| *id)
            .collect()
    }

    /// Whether the committed membership is a **joint** configuration.
    ///
    /// One of the four `S3-JOINT-RESUME` boundaries is "leadership lost while a
    /// joint entry is committed", and a test cannot arrange to be at that
    /// boundary without being able to see it.
    pub fn membership_is_joint_for_testing(&self) -> bool {
        self.raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .get_joint_config()
            .len()
            > 1
    }

    /// Build a snapshot now, rather than waiting for the log-size policy.
    ///
    /// `trigger().snapshot()` is OpenRaft's own request; the snapshot it
    /// produces is the production builder's, written through the production
    /// storage path. A test that hand-assembled a body would be proving things
    /// about its own fixture.
    pub async fn trigger_snapshot_for_testing(&self) -> Result<(), ProposeError> {
        self.raft
            .trigger()
            .snapshot()
            .await
            .map_err(|err| ProposeError::Rejected(Box::new(RaftError::Fatal(err))))
    }

    /// This node's current snapshot, if it has built one: metadata and body.
    ///
    /// A gate that drives an `install_snapshot` stream needs the *same* values
    /// the production sender puts on the wire. Hand-assembling a body would
    /// make the gate a test of its own fixture — the receiver would be
    /// admitting bytes no builder ever produced, and a change to the body
    /// format would leave the gate passing against a shape nothing writes.
    ///
    /// The body is read whole, which the production sender deliberately never
    /// does (§11.3). That is correct here and only here: a test that means to
    /// corrupt a byte or truncate a stream has to hold the bytes. The gates
    /// using it drive snapshots of a handful of kilobytes.
    pub async fn snapshot_for_testing(
        &self,
    ) -> Option<(
        openraft::SnapshotMeta<
            crate::coordination::ids::CoordinationNodeId,
            crate::coordination::typeconfig::CoordinationNode,
        >,
        Vec<u8>,
    )> {
        use std::io::Read;

        let snapshot = self.raft.get_snapshot().await.ok().flatten()?;
        let meta = snapshot.meta.clone();
        let mut reader = snapshot.snapshot.reader().ok()?;
        let mut body = Vec::new();
        reader.read_to_end(&mut body).ok()?;
        Some((meta, body))
    }

    /// This node's current vote.
    ///
    /// The third value in the handoff triple. A stream carrying anything else
    /// is refused by OpenRaft's `accept_vote`, so a gate has to know it to
    /// build a stream that *should* succeed — and to build one that should not.
    pub fn vote_for_testing(&self) -> openraft::Vote<crate::coordination::ids::CoordinationNodeId> {
        self.raft.metrics().borrow().vote
    }

    /// The `Raft` handle itself, for gates that must reach a boundary no
    /// narrower method exposes.
    ///
    /// Deliberately last and deliberately blunt. Prefer the methods above: each
    /// of them names the property it serves, and a gate written against this
    /// one is a gate whose intent is not in its source.
    pub fn raft_for_testing(&self) -> &Raft<CoordinationTypeConfig> {
        &self.raft
    }
}
