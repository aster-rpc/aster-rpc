//! The test fixture OpenRaft's storage suite is built from (§11.1).
//!
//! ## The invariant this exists to hold
//!
//! `testing::Suite::transfer_snapshot` builds a *leader* and a *follower* from
//! one `StoreBuilder` and installs the leader's snapshot on the follower. Our
//! snapshot install validates the group/genesis/authority binding and refuses
//! a mismatch, correctly.
//!
//! So a builder that sampled a fresh group identity on every `build()` — the
//! obvious implementation, and the one a per-test-directory helper naturally
//! becomes — would make Aster refuse the suite's transfer *because Aster is
//! right*, and the failure would read as a storage bug rather than a fixture
//! bug. The fixture is therefore sampled once for its whole lifetime, and only
//! the storage directory and local NodeId vary per store.
//!
//! Identity-mismatch negatives use a **different** fixture rather than
//! weakening install validation.
//!
//! ## Why the `StoreBuilder` impl is not here yet
//!
//! `StoreBuilder<C, LS, SM, G>` has to produce a state machine, and Checkpoint
//! C is what writes one. The half that is at risk — the fixture invariant — is
//! testable now and is tested below; the trait impl is a wrapper around this
//! once `SM` exists.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::identity::{OpenPolicy, StorageIdentity};
use super::schema::StorageLayout;
use super::VoterStorage;
use crate::coordination::ids::{CoordinationGroupId, CoordinationNodeId};
use crate::coordination::model::shape;

/// One group's worth of fixed identity, plus a per-store counter.
pub(crate) struct SuiteFixture {
    root: PathBuf,
    group_id: CoordinationGroupId,
    genesis_hash: [u8; 32],
    authority_public_key: [u8; 32],
    authority_domain: String,
    deployment_profile_code: i32,
    next: AtomicU64,
}

impl SuiteFixture {
    /// Sample one group fixture. Every store built from it shares this
    /// identity.
    pub(crate) fn new(root: impl Into<PathBuf>, seed: u8) -> Self {
        Self {
            root: root.into(),
            group_id: CoordinationGroupId::from_array([seed; 32]),
            genesis_hash: [seed.wrapping_add(1); 32],
            // A key the Ed25519 verifier accepts, derived from the seed so two
            // fixtures are two authorities. Arbitrary bytes used to do here,
            // and stopped when the state machine started checking that a
            // replicated authority key is a curve point -- correctly: a group
            // whose key verifies nothing can never accept a join permit.
            authority_public_key: ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
                .verifying_key()
                .to_bytes(),
            authority_domain: format!("fixture-{seed}.test"),
            deployment_profile_code: shape::PROFILE_PROD,
            next: AtomicU64::new(0),
        }
    }

    /// The identity for the `n`th store: group fields fixed, NodeId varying.
    pub(crate) fn identity_for(&self, n: u64) -> StorageIdentity {
        let mut node = [0u8; 32];
        node[24..].copy_from_slice(&n.to_be_bytes());
        // A NodeId of all zeros is the OpenRaft `Default` placeholder rather
        // than a real identity, so the first store is 1 rather than 0.
        node[0] = 0xA0;
        StorageIdentity::new(
            self.group_id,
            self.genesis_hash,
            self.authority_public_key,
            self.authority_domain.clone(),
            CoordinationNodeId::from_array(node),
            self.deployment_profile_code,
        )
    }

    /// Open the next store: a new directory and NodeId under the one fixed
    /// group identity.
    pub(crate) fn build(&self) -> VoterStorage {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let dir = self.root.join(format!("store-{n}"));
        std::fs::create_dir_all(&dir).expect("create store dir");
        VoterStorage::open(
            StorageLayout::new(&dir),
            self.identity_for(n),
            OpenPolicy::production(),
        )
        .expect("open a fixture store")
    }
}

// ── Maximal legal values ─────────────────────────────────────────────────────
//
// Here rather than in one module's test block because three modules need the
// same fixtures and they must not drift: `schema` sizes the metadata ceiling
// against them, `rows` checks that the metadata-local validator accepts the
// widest legal binding, and `machine` builds a coherent complete state from
// the same caps. A second copy of "the maximal legal meta" would be a second
// answer to what the caps are.
//
// Every bound below comes from a declared cap constant rather than a literal,
// and every field is named -- no `..Default::default()` -- so a field added to
// `CoordinationMetaV1` breaks the construction and forces whoever adds it to
// decide its maximum.

use super::identity::LOGICAL_SCHEMA_VERSION;
use crate::coordination::raft::CoordinationMetaV1;

/// The widest membership a production group can actually reach.
///
/// Not "two disjoint five-voter configs". That shape is representable in
/// OpenRaft and unreachable in Aster: it introduces five new voters at
/// once, and `MAX_NEW_VOTERS` is two. An OpenRaft round trip proves
/// OpenRaft representability, which is not the same claim.
///
/// The reachable maximum is one `ChangeMembership` step at its widest:
///
/// - old config: `MAX_VOTERS` voters, `0..5`
/// - new config: the three retained from old, plus `MAX_NEW_VOTERS` new
/// - node map: the seven-node union, **plus** up to
///   `MAX_RETAINED_LEARNERS` learners
///
/// Learners are the reason the node map is not simply the union: OpenRaft
/// represents them as nodes outside every config, so an exact-union
/// assertion would have excluded them and understated the maximum.
pub(crate) fn maximal_reachable_membership() -> crate::coordination::raft::MembershipV1 {
    use crate::coordination::model::shape::{MAX_NEW_VOTERS, MAX_RETAINED_LEARNERS, MAX_VOTERS};
    use crate::coordination::raft::{MembershipV1, NodeEntryV1, NodeIdSetV1, NodeIdV1};

    let node_id = |n: usize| vec![n as u8; 32];
    let set = |ids: Vec<usize>| NodeIdSetV1 {
        ids: ids
            .into_iter()
            .map(|n| NodeIdV1 {
                node_id: node_id(n),
            })
            .collect(),
    };

    let retained = MAX_VOTERS - MAX_NEW_VOTERS;
    let old: Vec<usize> = (0..MAX_VOTERS).collect();
    // The retained tail of `old`, then the new voters.
    let new: Vec<usize> = (MAX_NEW_VOTERS..MAX_VOTERS)
        .chain(MAX_VOTERS..MAX_VOTERS + MAX_NEW_VOTERS)
        .collect();
    debug_assert_eq!(new.len(), MAX_VOTERS);
    debug_assert_eq!(
        old.iter().filter(|n| new.contains(n)).count(),
        retained,
        "exactly the retained voters overlap"
    );

    let configs = vec![set(old.clone()), set(new.clone())];

    // Voters first, then the learners that sit outside every config.
    let mut members: Vec<usize> = old.into_iter().chain(new).collect();
    members.sort_unstable();
    members.dedup();
    let learners: Vec<usize> = (100..100 + MAX_RETAINED_LEARNERS).collect();

    MembershipV1 {
        configs,
        nodes: members
            .into_iter()
            .chain(learners)
            .map(|n| NodeEntryV1 {
                node_id: node_id(n),
                format_version: crate::coordination::convert::NODE_FORMAT_VERSION,
            })
            .collect(),
    }
}

/// The largest `CoordinationMetaV1` a production group can legally hold.
///
/// Every field is named — no `..Default::default()` — so adding a field
/// breaks this construction and forces whoever adds it to decide its
/// maximum rather than inherit a default and leave the sizing proof stale.
///
/// Every bound comes from the declared cap constant rather than a literal,
/// so this cannot drift from the limits the model enforces.
pub(crate) fn maximal_legal_meta() -> CoordinationMetaV1 {
    use crate::coordination::model::authority::MAX_AUTHORITY_DOMAIN_BYTES;
    use crate::coordination::model::limits::{GROWTH_BUDGET, RESERVE_BUDGET};
    use crate::coordination::model::shape::PROFILE_PROD;
    use crate::coordination::raft::{RecoveredFromV1, StoredMembershipV1};

    let log_id = |index: u64| {
        Some(crate::coordination::contract::LogIdV1 {
            term: 9,
            index,
            leader_node_id: vec![3u8; 32],
        })
    };

    CoordinationMetaV1 {
        schema_version: LOGICAL_SCHEMA_VERSION,
        group_id: vec![0xFF; 32],
        genesis_hash: vec![0xFF; 32],
        // A key the Ed25519 verifier actually accepts. Arbitrary bytes are
        // not a public key: most 32-byte strings decode to nothing on the
        // curve, so sizing against one proves nothing about a real
        // authority binding.
        authority_public_key: authority_public_key().to_vec(),
        authority_domain: "d".repeat(MAX_AUTHORITY_DOMAIN_BYTES),
        deployment_profile_code: PROFILE_PROD,
        last_applied: log_id(9_000_000),
        membership: StoredMembershipV1 {
            log_id: log_id(8_999_000),
            membership: maximal_reachable_membership(),
        },
        admin_revision: 1_000_000,
        recovery_export_hash: Some(vec![0xFF; 32]),
        recovered_from: Some(RecoveredFromV1 {
            source_group_id: vec![0xFF; 32],
            source_genesis_hash: vec![0xFF; 32],
            // A source authority is recorded, never matched: §13 imports
            // routinely carry a different binding from the importing voter's.
            source_authority_public_key: vec![0xFF; 32],
            source_authority_domain: "s".repeat(MAX_AUTHORITY_DOMAIN_BYTES),
            // The same hash `recovery_export_hash` records above; §13 binds one
            // BLAKE3 and both fields name it.
            export_hash: vec![0xFF; 32],
            source_last_applied: log_id(8_800_000),
        }),
        // The declared budgets, not a round number above them.
        aggregate_growth_bytes: GROWTH_BUDGET,
        aggregate_reserve_bytes: RESERVE_BUDGET,
        audit_floor: log_id(8_900_000),
    }
}

/// The row counts a group can legally reach, each from its own cap.
pub(crate) fn maximal_legal_row_counts() -> crate::coordination::rowstream::RowCountsV1 {
    use crate::coordination::model::limits::{
        MAX_ACL_ROWS_PER_GROUP, MAX_ADMINS_PER_GROUP, MAX_AUDIT_ROWS, MAX_RESOURCES_PER_GROUP,
        MAX_RETAINED_INTENTS, MAX_RETAINED_ROUTES, MAX_RETAINED_SESSIONS_PER_GROUP,
    };
    crate::coordination::rowstream::RowCountsV1 {
        resources: MAX_RESOURCES_PER_GROUP,
        grants: MAX_ACL_ROWS_PER_GROUP,
        admins: MAX_ADMINS_PER_GROUP,
        sessions: MAX_RETAINED_SESSIONS_PER_GROUP,
        intents: MAX_RETAINED_INTENTS,
        routes: MAX_RETAINED_ROUTES,
        audit: MAX_AUDIT_ROWS,
    }
}

/// A verifier-accepted authority public key, deterministically derived.
pub(crate) fn authority_public_key() -> [u8; 32] {
    crate::coordination::model::authority::testing::signing_key()
        .verifying_key()
        .to_bytes()
}

/// A deliberately *over*-sized meta, used only to measure the header
/// envelope conservatively.
///
/// Not legal state, and labelled so. Its scalars sit at their type maxima
/// and its domain at the generic string ceiling, which makes every
/// variable-length encoding at least as wide as anything real, so the
/// envelope measured from it cannot be understated.
pub(crate) fn oversized_meta_for_envelope() -> CoordinationMetaV1 {
    use crate::coordination::contract::MAX_WIRE_STRING;

    let mut meta = maximal_legal_meta();
    meta.schema_version = u32::MAX;
    meta.deployment_profile_code = i32::MAX;
    meta.authority_domain = "d".repeat(MAX_WIRE_STRING);
    meta.admin_revision = u64::MAX;
    meta.aggregate_growth_bytes = u64::MAX;
    meta.aggregate_reserve_bytes = u64::MAX;
    meta
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64 as Counter;

    static COUNTER: Counter = Counter::new(0);

    fn scratch() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "aster-coordination-fixture-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// One fixture, many stores: group identity fixed, NodeId and directory
    /// varying.
    ///
    /// This is the property `transfer_snapshot` depends on. A fixture that
    /// re-sampled the group per `build()` would make our own — correct —
    /// snapshot identity check refuse the suite's transfer, and the failure
    /// would be read as a storage bug.
    #[test]
    fn one_fixture_holds_its_group_identity_across_every_store() {
        let fixture = SuiteFixture::new(scratch(), 7);

        let first = fixture.identity_for(0);
        let second = fixture.identity_for(1);

        assert_eq!(first.group_id(), second.group_id());
        assert_eq!(first.genesis_hash(), second.genesis_hash());
        assert_eq!(first.authority_public_key(), second.authority_public_key());
        assert_eq!(first.authority_domain(), second.authority_domain());
        assert_eq!(
            first.deployment_profile_code(),
            second.deployment_profile_code()
        );

        assert_ne!(
            first.local_node_id(),
            second.local_node_id(),
            "each store is still its own node"
        );
        assert!(
            !first.local_node_id().is_placeholder(),
            "and never the all-zero Default placeholder"
        );
    }

    /// Two stores really do open side by side, each with its own lock.
    #[test]
    fn two_stores_from_one_fixture_open_concurrently() {
        let fixture = SuiteFixture::new(scratch(), 11);

        let leader = fixture.build();
        let follower = fixture.build();

        assert_ne!(leader.layout().root(), follower.layout().root());
        assert_eq!(leader.identity().group_id(), follower.identity().group_id());
        assert_ne!(
            leader.identity().local_node_id(),
            follower.identity().local_node_id()
        );
    }

    /// A different fixture is a different group, which is what identity
    /// negatives are built from.
    #[test]
    fn a_second_fixture_is_a_different_group() {
        let one = SuiteFixture::new(scratch(), 1);
        let two = SuiteFixture::new(scratch(), 2);
        assert_ne!(
            one.identity_for(0).group_id(),
            two.identity_for(0).group_id(),
            "negatives come from a separate fixture, never from weakening validation"
        );
    }
}
