//! The one pinned `openraft::Config` (§7.5) and the cross-constant invariants
//! that hold it together.
//!
//! ## Why the values are constants and the config is built rather than defaulted
//!
//! OpenRaft's own defaults are chosen for a general library. Several of these
//! values are not tuning at all — `max_payload_entries` is pinned by the §12.3
//! frame arithmetic, and `snapshot_max_chunk_size` is the §11.3 chunk bound —
//! so inheriting an upstream default would silently change a protocol
//! constraint on a dependency bump.
//!
//! ## The invariants OpenRaft cannot check
//!
//! [`Config::validate`] enforces the three it knows about: the two election
//! timeouts are ordered, the heartbeat sits below the minimum, and the payload
//! is non-zero. The rest are relationships between OpenRaft's config and
//! Aster's §12 limits, which OpenRaft has never heard of.
//!
//! §12.3 declares three, each with **named** operands rather than an
//! approximation — "a startup check whose inputs are about this big is not a
//! check". [`check_frame_invariants`] is them:
//!
//! ```text
//! (1) max_encoded_command + STORE_PREFIX_MAX + FRAME_OVERHEAD  <= store_frame_max
//! (2) (max_encoded_command + ENTRY_OVERHEAD_MAX) * max_payload_entries
//!         + RAFT_ENVELOPE_OVERHEAD_MAX + FRAME_OVERHEAD        <= raft_frame_max
//! (3) chunk_data_max + CHUNK_HEADER_MAX + FRAME_OVERHEAD       <= chunk_frame_max
//! ```
//!
//! **(2) is why `max_payload_entries` is 28 rather than 32.** At 32 it computes
//! to about 8.2 MiB against an 8 MiB frame, which does not close — and a batch
//! that does not fit is not a throughput problem: §5.4 explains that
//! `PayloadTooLarge::new_entries_hint` asserts a hint above zero, so there is no
//! way to tell a peer "one entry is too large". The failure surfaces as a
//! stalled follower rather than as a configuration error, which is precisely
//! why it is checked at boot.
//!
//! 28 is **not** the arithmetic boundary — that is 31 — and the test says so
//! rather than implying otherwise. The pinned value keeps about 780 KiB of
//! headroom on purpose: the overhead constants are upper bounds, so a value
//! sitting exactly on the boundary would turn any future widening of
//! `RaftEntryV1` into a boot failure instead of absorbing it.
//!
//! The overhead constants are upper bounds on fixed-shape encodings, so they
//! are checkable rather than estimated: [`tests`] encodes the largest legal
//! instance of each envelope with an empty payload and asserts the result is at
//! or below its constant. If Fory's framing changes under a version bump, that
//! test fails rather than the invariant silently becoming false.
//!
//! The lag threshold is checked here too, though §12.3 does not list it: a
//! threshold at or below the snapshot trigger sends a snapshot to a follower
//! that did not need one, which then falls behind again.
//!
//! ## `leader_liveness_timeout` is derived here and configured nowhere
//!
//! §7.6 fixes it as `election_timeout_max`. It is exposed as a function of the
//! config rather than as a field for the reason §7.6 gives: a field could be
//! set inconsistently with the timeouts it must relate to, and there is no
//! value it could take that is right independently of them.

use std::sync::Arc;
use std::time::Duration;

use openraft::{Config, SnapshotPolicy};

use crate::coordination::ids::CoordinationGroupId;
use crate::coordination::model::limits::MAX_ENCODED_COMMAND;
use crate::coordination::service::{
    RAFT_UNARY_FRAME_MAX, SNAPSHOT_CHUNK_FRAME_MAX, STORE_FRAME_MAX,
};

/// §7.5's pinned timings and counts, in one place so a diff shows a change to
/// any of them.
pub(crate) const HEARTBEAT_INTERVAL_MS: u64 = 250;
pub(crate) const ELECTION_TIMEOUT_MIN_MS: u64 = 1_500;
pub(crate) const ELECTION_TIMEOUT_MAX_MS: u64 = 3_000;
pub(crate) const INSTALL_SNAPSHOT_TIMEOUT_MS: u64 = 30_000;
/// Pinned by the §12.3 frame arithmetic, not chosen for throughput.
pub(crate) const MAX_PAYLOAD_ENTRIES: u64 = 28;
pub(crate) const REPLICATION_LAG_THRESHOLD: u64 = 20_000;
pub(crate) const SNAPSHOT_LOGS_SINCE_LAST: u64 = 10_000;
pub(crate) const SNAPSHOT_MAX_CHUNK_SIZE: u64 = 1024 * 1024;
pub(crate) const MAX_IN_SNAPSHOT_LOG_TO_KEEP: u64 = 1_000;
pub(crate) const PURGE_BATCH_SIZE: u64 = 500;

// ── §12.3's named overhead constants ─────────────────────────────────────────
//
// Each is an upper bound on a *fixed-shape* encoding — an envelope or header
// with an empty payload — which is what makes it checkable rather than
// estimated. The test module encodes the largest legal instance of each and
// asserts it fits, so a Fory framing change fails there rather than making an
// invariant quietly false.

/// The transport frame prefix: a 4-byte little-endian length plus one flags
/// byte.
pub(crate) const FRAME_OVERHEAD: u64 = 5;
/// A Store request's fixed prefix: protocol version, group id, client id,
/// sequence, and Fory struct framing.
pub(crate) const STORE_PREFIX_MAX: u64 = 256;
/// `RaftEntryV1` minus its `CommandV1` payload.
pub(crate) const ENTRY_OVERHEAD_MAX: u64 = 512;
/// `RaftEnvelopeV1` minus its payload.
pub(crate) const RAFT_ENVELOPE_OVERHEAD_MAX: u64 = 1024;
/// A snapshot chunk frame's fixed part: offset, final flag, and framing.
pub(crate) const CHUNK_HEADER_MAX: u64 = 256;

/// Why the pinned configuration is not internally consistent.
///
/// Every variant is a startup failure rather than a warning: each one describes
/// a group that would run and then misbehave in a way that reads as a
/// replication bug.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RaftConfigError {
    /// OpenRaft's own validation refused the values.
    Upstream(String),
    /// One of §12.3's three frame invariants does not close.
    ///
    /// One variant for all three, carrying **both operands named** as §12.3
    /// requires: an operator reading it needs the computed size and the cap it
    /// exceeded, and which of the three it was. Three variants carrying the
    /// same two numbers would say nothing more.
    Frame {
        /// Which invariant: `"store frame"`, `"raft entry batch"`, or
        /// `"chunk frame"`.
        invariant: &'static str,
        /// The largest legal payload plus its declared overheads.
        computed: u64,
        /// The frame cap it must fit inside.
        cap: u64,
    },
    /// The replication-lag threshold does not exceed the snapshot trigger.
    LagBelowSnapshotTrigger { lag: u64, snapshot_after: u64 },
}

impl std::fmt::Display for RaftConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RaftConfigError::Upstream(detail) => {
                write!(f, "openraft refused the pinned configuration: {detail}")
            }
            RaftConfigError::Frame {
                invariant,
                computed,
                cap,
            } => write!(
                f,
                "the {invariant} invariant does not close: {computed} bytes against a {cap}-byte \
                 cap. A frame that cannot hold its own largest legal payload surfaces as a \
                 stalled peer rather than as a configuration error"
            ),
            RaftConfigError::LagBelowSnapshotTrigger {
                lag,
                snapshot_after,
            } => write!(
                f,
                "the replication lag threshold ({lag}) must exceed the snapshot trigger \
                 ({snapshot_after}), or a follower is sent a snapshot it did not need"
            ),
        }
    }
}

impl std::error::Error for RaftConfigError {}

impl From<RaftConfigError> for crate::error::Error {
    fn from(err: RaftConfigError) -> Self {
        crate::error::Error::InvalidArgument(err.to_string())
    }
}

/// Build and validate the one `openraft::Config` this voter runs.
///
/// `cluster_name` carries the group id, per §7.5. It is a diagnostic — OpenRaft
/// does not enforce it across peers — and every real group binding is checked by
/// §5.3's envelope group field and by storage identity, so nothing here relies
/// on it.
pub(crate) fn validated(group: &CoordinationGroupId) -> Result<Arc<Config>, RaftConfigError> {
    let config = Config {
        cluster_name: format!("aster-coordination/{}", group.to_hex()),
        heartbeat_interval: HEARTBEAT_INTERVAL_MS,
        election_timeout_min: ELECTION_TIMEOUT_MIN_MS,
        election_timeout_max: ELECTION_TIMEOUT_MAX_MS,
        install_snapshot_timeout: INSTALL_SNAPSHOT_TIMEOUT_MS,
        max_payload_entries: MAX_PAYLOAD_ENTRIES,
        replication_lag_threshold: REPLICATION_LAG_THRESHOLD,
        snapshot_policy: SnapshotPolicy::LogsSinceLast(SNAPSHOT_LOGS_SINCE_LAST),
        snapshot_max_chunk_size: SNAPSHOT_MAX_CHUNK_SIZE,
        max_in_snapshot_log_to_keep: MAX_IN_SNAPSHOT_LOG_TO_KEEP,
        purge_batch_size: PURGE_BATCH_SIZE,
        enable_tick: true,
        enable_heartbeat: true,
        enable_elect: true,
        ..Default::default()
    };

    let config = config
        .validate()
        .map_err(|e| RaftConfigError::Upstream(e.to_string()))?;

    check_frame_invariants(&config)?;
    check_lag_threshold(&config)?;
    Ok(Arc::new(config))
}

/// §12.3's three frame invariants, computed from named operands.
///
/// Separate from [`validated`] so it can be pointed at a deliberately broken
/// config in tests: a check that can only ever be run against the one correct
/// value proves that the value is itself, not that the check works.
///
/// Each uses `<=`, per §12.3: a payload that exactly fills its frame *with* its
/// overheads accounted for does fit. The overheads are what make that safe —
/// they are already upper bounds on everything that wraps the payload.
pub(crate) fn check_frame_invariants(config: &Config) -> Result<(), RaftConfigError> {
    let command = MAX_ENCODED_COMMAND as u64;

    // (1) Store frame.
    let store = command + STORE_PREFIX_MAX + FRAME_OVERHEAD;
    fits("store frame", store, u64::from(STORE_FRAME_MAX))?;

    // (2) Raft entry batch. The one that pins `max_payload_entries`: this is a
    //     *batch* bound, not a single-entry one, so it scales with the count
    //     OpenRaft is allowed to pack.
    let batch = (command + ENTRY_OVERHEAD_MAX).saturating_mul(config.max_payload_entries)
        + RAFT_ENVELOPE_OVERHEAD_MAX
        + FRAME_OVERHEAD;
    fits("raft entry batch", batch, u64::from(RAFT_UNARY_FRAME_MAX))?;

    // (3) Chunk frame.
    let chunk = config.snapshot_max_chunk_size + CHUNK_HEADER_MAX + FRAME_OVERHEAD;
    fits("chunk frame", chunk, u64::from(SNAPSHOT_CHUNK_FRAME_MAX))?;

    Ok(())
}

fn fits(invariant: &'static str, computed: u64, cap: u64) -> Result<(), RaftConfigError> {
    if computed > cap {
        return Err(RaftConfigError::Frame {
            invariant,
            computed,
            cap,
        });
    }
    Ok(())
}

/// The replication-lag threshold against the snapshot trigger.
///
/// Not one of §12.3's three — it is about work, not about bytes — but it fails
/// the same way: a follower is sent a snapshot it did not need, falls behind
/// again, and the group looks like it has a slow peer rather than a
/// misconfiguration.
pub(crate) fn check_lag_threshold(config: &Config) -> Result<(), RaftConfigError> {
    let snapshot_after = match config.snapshot_policy {
        SnapshotPolicy::LogsSinceLast(n) => n,
        // `SnapshotPolicy` is `#[non_exhaustive]` upstream. A policy this build
        // does not recognise has no trigger count to compare, and inventing one
        // would make the check report success it did not establish.
        _ => return Ok(()),
    };
    if config.replication_lag_threshold <= snapshot_after {
        return Err(RaftConfigError::LagBelowSnapshotTrigger {
            lag: config.replication_lag_threshold,
            snapshot_after,
        });
    }

    Ok(())
}

/// §7.6's leader-liveness timeout: **derived**, never configured.
///
/// `Config::validate` guarantees
/// `heartbeat_interval < election_timeout_min < election_timeout_max`, so this
/// value exceeds the heartbeat and metrics cadence by construction and needs no
/// cross-constant check of its own.
///
/// A smaller value reports `NoQuorum` during ordinary heartbeat jitter; a
/// larger one delays the report past the point where the group can already have
/// elected a replacement.
pub(crate) fn leader_liveness_timeout(config: &Config) -> Duration {
    Duration::from_millis(config.election_timeout_max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group() -> CoordinationGroupId {
        CoordinationGroupId::from_array([0xABu8; 32])
    }

    /// The pinned configuration builds, and every §7.5 value survives into it.
    ///
    /// Asserted as literals rather than against the constants they came from:
    /// an oracle that reads the same constant proves only that the constant
    /// equals itself, and these are values a dependency bump could otherwise
    /// change silently.
    #[test]
    fn the_pinned_configuration_is_the_one_section_seven_five_declares() {
        let config = validated(&group()).expect("the pinned config must validate");
        assert_eq!(config.heartbeat_interval, 250);
        assert_eq!(config.election_timeout_min, 1_500);
        assert_eq!(config.election_timeout_max, 3_000);
        assert_eq!(config.install_snapshot_timeout, 30_000);
        assert_eq!(config.max_payload_entries, 28);
        assert_eq!(config.replication_lag_threshold, 20_000);
        assert_eq!(config.snapshot_max_chunk_size, 1_048_576);
        assert_eq!(config.max_in_snapshot_log_to_keep, 1_000);
        assert_eq!(config.purge_batch_size, 500);
        assert!(matches!(
            config.snapshot_policy,
            SnapshotPolicy::LogsSinceLast(10_000)
        ));
        assert!(config.enable_tick && config.enable_heartbeat && config.enable_elect);
        assert_eq!(
            config.cluster_name,
            format!("aster-coordination/{}", "ab".repeat(32))
        );
    }

    /// The timeout is `election_timeout_max` and is derived from the config
    /// rather than read from a field, so it cannot disagree with the timeouts
    /// it must relate to.
    #[test]
    fn the_leader_liveness_timeout_is_the_maximum_election_timeout() {
        let config = validated(&group()).expect("validate");
        assert_eq!(
            leader_liveness_timeout(&config),
            Duration::from_millis(3_000)
        );

        // And it exceeds the heartbeat cadence by construction, which is the
        // property §7.6 relies on instead of a separate cross-constant check.
        assert!(
            leader_liveness_timeout(&config) > Duration::from_millis(config.heartbeat_interval)
        );
        assert!(
            leader_liveness_timeout(&config) > Duration::from_millis(config.election_timeout_min)
        );
    }

    /// The three §12.3 invariants close at the pinned values, and each one's
    /// computed size is the number the plan writes down.
    ///
    /// The sums are recomputed from the shipped constants and compared against
    /// **literals**, so this fails if any operand moves — a check that read the
    /// same constants on both sides would prove only that they equal
    /// themselves, and calling `check_frame_invariants` for the oracle would
    /// prove only that the function agrees with itself.
    #[test]
    fn the_three_frame_invariants_close_at_the_pinned_values() {
        let config = validated(&group()).expect("validate");
        assert!(check_frame_invariants(&config).is_ok());

        let command = MAX_ENCODED_COMMAND as u64;
        assert_eq!(command, 262_144, "§12.1's max_encoded_command");

        // (1) store frame
        let store = command + STORE_PREFIX_MAX + FRAME_OVERHEAD;
        assert_eq!(store, 262_405);
        assert_eq!(u64::from(STORE_FRAME_MAX), 2_097_152);
        assert!(store <= u64::from(STORE_FRAME_MAX));

        // (2) raft entry batch
        let batch = (command + ENTRY_OVERHEAD_MAX) * config.max_payload_entries
            + RAFT_ENVELOPE_OVERHEAD_MAX
            + FRAME_OVERHEAD;
        assert_eq!(batch, 7_355_397);
        assert_eq!(u64::from(RAFT_UNARY_FRAME_MAX), 8_388_608);
        assert!(batch <= u64::from(RAFT_UNARY_FRAME_MAX));

        // (3) chunk frame
        let chunk = config.snapshot_max_chunk_size + CHUNK_HEADER_MAX + FRAME_OVERHEAD;
        assert_eq!(chunk, 1_048_837);
        assert_eq!(u64::from(SNAPSHOT_CHUNK_FRAME_MAX), 2_097_152);
        assert!(chunk <= u64::from(SNAPSHOT_CHUNK_FRAME_MAX));
    }

    /// **This is what rejected 32 and admits 28**, and it also records that 28
    /// is not sitting on the cliff.
    ///
    /// §12.3 says 32 computes to about 8.2 MiB against an 8 MiB frame and does
    /// not close, so `max_payload_entries` is "pinned at 28 rather than 32". It
    /// is easy to read that as *28 is the maximum*, and it is not: the
    /// arithmetic boundary is 31. The pinned value carries about 780 KiB of
    /// deliberate headroom, which matters because the overhead constants are
    /// upper bounds — a value at the exact boundary would turn any future
    /// widening of `RaftEntryV1` into a boot failure.
    ///
    /// The boundary is asserted here rather than assumed, so a change to
    /// `MAX_ENCODED_COMMAND` or to the overheads that moves it below the pinned
    /// value fails as a test rather than at boot.
    #[test]
    fn the_entry_batch_bound_rejects_thirty_two_and_admits_the_pinned_count() {
        let base = (*validated(&group()).expect("validate")).clone();
        let at = |n: u64| {
            let mut c = base.clone();
            c.max_payload_entries = n;
            check_frame_invariants(&c)
        };

        assert!(
            matches!(
                at(32),
                Err(RaftConfigError::Frame {
                    invariant: "raft entry batch",
                    ..
                })
            ),
            "32 entries is the mismatch §12.3 records; it must not pass"
        );

        assert_eq!(base.max_payload_entries, 28, "the §7.5 pinned value");
        assert!(at(28).is_ok(), "the pinned value must close");

        // The real boundary, from both sides. 28 must sit strictly below it.
        let largest = (1..=64u64)
            .filter(|n| at(*n).is_ok())
            .max()
            .expect("some count fits");
        assert_eq!(largest, 31, "the entry-batch arithmetic admits at most 31");
        assert!(
            base.max_payload_entries < largest,
            "the pinned count must keep headroom below the boundary, not sit on it"
        );
    }

    /// Each frame invariant fails on its own when broken, naming which one.
    ///
    /// Driven by mutating a *validated* config, which is the only way to
    /// exercise a check whose real inputs are compile-time constants.
    #[test]
    fn each_frame_invariant_fails_on_its_own_and_names_itself() {
        let base = (*validated(&group()).expect("validate")).clone();

        let mut oversized_chunk = base.clone();
        oversized_chunk.snapshot_max_chunk_size = u64::from(SNAPSHOT_CHUNK_FRAME_MAX);
        assert!(
            matches!(
                check_frame_invariants(&oversized_chunk),
                Err(RaftConfigError::Frame {
                    invariant: "chunk frame",
                    ..
                })
            ),
            "a chunk exactly filling its frame leaves no room for the chunk header"
        );

        // The real boundary: the largest chunk that fits with its header.
        let mut exact = base.clone();
        exact.snapshot_max_chunk_size =
            u64::from(SNAPSHOT_CHUNK_FRAME_MAX) - CHUNK_HEADER_MAX - FRAME_OVERHEAD;
        assert!(check_frame_invariants(&exact).is_ok());

        let mut batch = base.clone();
        batch.max_payload_entries = 1_000;
        assert!(matches!(
            check_frame_invariants(&batch),
            Err(RaftConfigError::Frame {
                invariant: "raft entry batch",
                ..
            })
        ));
    }

    /// The error names both operands, as §12.3 requires. An operator reading
    /// "does not close" with no numbers cannot tell which constant to change.
    #[test]
    fn a_frame_failure_names_both_operands() {
        let mut broken = (*validated(&group()).expect("validate")).clone();
        broken.max_payload_entries = 32;
        let err = check_frame_invariants(&broken).expect_err("32 does not close");
        let rendered = err.to_string();
        assert!(rendered.contains("raft entry batch"), "{rendered}");
        assert!(
            rendered.contains(&((262_144u64 + 512) * 32 + 1029).to_string()),
            "{rendered}"
        );
        assert!(rendered.contains("8388608"), "{rendered}");
    }

    /// The declared overheads are upper bounds on real encodings, not
    /// estimates.
    ///
    /// §12.3: "a test encodes the largest legal instance of each envelope with
    /// an empty payload and asserts the result is at or below the constant. If
    /// Fory's framing changes under a version bump, that test fails rather than
    /// the invariant silently becoming false."
    #[test]
    fn each_declared_overhead_bounds_its_real_empty_payload_encoding() {
        use crate::coordination::contract::{payload_fory, LogIdV1};
        use crate::coordination::raft::{
            EntryPayloadV1, RaftEntryV1, RaftEnvelopeV1, RaftSnapshotChunkWireV1,
            RaftSnapshotFrameV1,
        };

        // The largest legal envelope with an empty payload: full-width group
        // and sender ids, and the widest version numbers.
        let envelope = payload_fory::<RaftEnvelopeV1>()
            .serialize(&RaftEnvelopeV1 {
                protocol_version: i32::MAX,
                raft_codec_version: i32::MAX,
                group_id: vec![0xFFu8; 32],
                sender_node_id: vec![0xFFu8; 32],
                payload: Vec::new(),
            })
            .expect("encode");
        assert!(
            envelope.len() as u64 <= RAFT_ENVELOPE_OVERHEAD_MAX,
            "RaftEnvelopeV1 with an empty payload encodes to {} bytes, above the declared \
             {RAFT_ENVELOPE_OVERHEAD_MAX}",
            envelope.len()
        );

        // The largest legal entry with an empty command: a full log id and the
        // blank payload variant, which is the entry shape with no command at
        // all.
        let entry = payload_fory::<RaftEntryV1>()
            .serialize(&RaftEntryV1 {
                log_id: LogIdV1 {
                    term: u64::MAX,
                    leader_node_id: vec![0xFFu8; 32],
                    index: u64::MAX,
                },
                payload: EntryPayloadV1::Blank(Default::default()),
            })
            .expect("encode");
        assert!(
            entry.len() as u64 <= ENTRY_OVERHEAD_MAX,
            "RaftEntryV1 with no command encodes to {} bytes, above the declared \
             {ENTRY_OVERHEAD_MAX}",
            entry.len()
        );

        // The largest legal chunk frame with no data.
        let chunk = payload_fory::<RaftSnapshotFrameV1>()
            .serialize(&RaftSnapshotFrameV1::Chunk(RaftSnapshotChunkWireV1 {
                offset: u64::MAX,
                data: Vec::new(),
                final_chunk: true,
            }))
            .expect("encode");
        assert!(
            chunk.len() as u64 <= CHUNK_HEADER_MAX,
            "an empty snapshot chunk frame encodes to {} bytes, above the declared \
             {CHUNK_HEADER_MAX}",
            chunk.len()
        );
    }

    /// The lag threshold fails on its own, and its boundary is exercised from
    /// both sides.
    #[test]
    fn the_lag_threshold_must_exceed_the_snapshot_trigger() {
        let base = (*validated(&group()).expect("validate")).clone();

        let mut lagging = base.clone();
        lagging.replication_lag_threshold = SNAPSHOT_LOGS_SINCE_LAST;
        assert_eq!(
            check_lag_threshold(&lagging),
            Err(RaftConfigError::LagBelowSnapshotTrigger {
                lag: SNAPSHOT_LOGS_SINCE_LAST,
                snapshot_after: SNAPSHOT_LOGS_SINCE_LAST,
            })
        );

        let mut just_above = base.clone();
        just_above.replication_lag_threshold = SNAPSHOT_LOGS_SINCE_LAST + 1;
        assert!(check_lag_threshold(&just_above).is_ok());
    }

    /// A snapshot policy this build does not recognise is not silently reported
    /// as satisfying the lag invariant it cannot be compared against.
    ///
    /// `SnapshotPolicy` is `#[non_exhaustive]` upstream, so this arm is
    /// reachable on a dependency bump. The test states the chosen behaviour —
    /// the frame invariants still apply, and only the lag comparison is
    /// skipped — so that a later change to make it an error is a deliberate one.
    #[test]
    fn an_unrecognised_snapshot_policy_skips_only_the_comparison_it_cannot_make() {
        let mut never = (*validated(&group()).expect("validate")).clone();
        never.snapshot_policy = SnapshotPolicy::Never;
        assert!(check_lag_threshold(&never).is_ok());

        // But an oversized chunk is still caught under the same policy, so the
        // early return skips one comparison rather than the whole check.
        never.snapshot_max_chunk_size = u64::from(SNAPSHOT_CHUNK_FRAME_MAX) + 1;
        assert!(check_frame_invariants(&never).is_err());
    }

    /// OpenRaft's own three invariants are exercised through the same entry
    /// point, so a future edit to the pinned values that breaks one of them
    /// fails here rather than at first election.
    #[test]
    fn openraft_still_refuses_a_configuration_that_breaks_its_own_ordering() {
        let broken = Config {
            election_timeout_min: 3_000,
            election_timeout_max: 1_500,
            heartbeat_interval: 250,
            ..Default::default()
        };
        assert!(broken.validate().is_err());

        let heartbeat_too_slow = Config {
            election_timeout_min: 200,
            election_timeout_max: 400,
            heartbeat_interval: 250,
            ..Default::default()
        };
        assert!(heartbeat_too_slow.validate().is_err());
    }

    /// The features §7.5 says v1 does not enable are not enabled.
    ///
    /// Asserted through behaviour rather than a `cfg!` string: with
    /// `single-term-leader` on, a `LeaderId` carries no `voted_for`, so this
    /// construction would not compile. It is the same instrument
    /// `typeconfig::openraft_serde_is_off` uses.
    #[test]
    fn the_v1_feature_set_is_the_one_the_type_config_assumes() {
        let leader = openraft::LeaderId::<crate::coordination::ids::CoordinationNodeId>::new(
            7,
            crate::coordination::ids::CoordinationNodeId::from_array([1u8; 32]),
        );
        assert_eq!(leader.term, 7);
        assert!(
            leader.voted_for().is_some(),
            "single-term-leader would remove voted_for entirely"
        );
    }
}
