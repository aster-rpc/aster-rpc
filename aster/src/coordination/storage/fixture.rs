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
        self.build_at().1
    }

    /// The next store, and the directory it lives in.
    ///
    /// The directory comes back so a caller can close the store and open the
    /// same files again -- which is the only way to exercise a *restarted*
    /// store rather than one that has only ever existed in this process.
    pub(crate) fn build_at(&self) -> (PathBuf, VoterStorage) {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let dir = self.root.join(format!("store-{n}"));
        std::fs::create_dir_all(&dir).expect("create store dir");
        let store = VoterStorage::open(
            StorageLayout::new(&dir),
            self.identity_for(n),
            OpenPolicy::production(),
        )
        .expect("open a fixture store");
        (dir, store)
    }

    /// Reopen a directory this fixture already made.
    ///
    /// The identity is recovered from the directory name rather than sampled
    /// again: a store must reopen under the identity it was bound with, and
    /// handing it a fresh one would be testing the identity check instead of
    /// the restart.
    pub(crate) fn open_existing(&self, dir: &std::path::Path) -> VoterStorage {
        let n: u64 = dir
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("store-"))
            .and_then(|n| n.parse().ok())
            .expect("a directory this fixture made");
        VoterStorage::open(
            StorageLayout::new(dir),
            self.identity_for(n),
            OpenPolicy::production(),
        )
        .expect("reopen a fixture store")
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

/// The widest membership the **validator admits**, which is deliberately wider
/// than any single transition reaches.
///
/// This is the sizing fixture, and sizing has to be against what can be
/// *loaded* rather than against what can be produced: the ceiling's job is to
/// admit everything the validator does, so measuring it against a narrower
/// shape would leave a gap between "accepted" and "storable".
///
/// - old config: `MAX_VOTERS` voters, `0..5`
/// - new config: the three retained from old, plus `MAX_NEW_VOTERS` new
/// - node map: the seven-node union plus the full `MAX_LEARNER_ENVELOPE`
///
/// Thirteen nodes. **No transition reaches that**, and
/// `the_widest_admitted_membership_round_trips_and_bounds_both_reachable_peaks`
/// states the two
/// reachable peaks separately: five voters with six learners before the joint
/// step, or seven voters with four after it -- eleven either way, because the
/// two new voters are learners in the first and voters in the second.
/// Combining both peaks is what makes this conservative rather than reachable.
///
/// It is still not "two disjoint five-voter configs": that shape introduces
/// five new voters at once and `MAX_NEW_VOTERS` is two.
///
/// Learners are the reason the node map is not simply the union: OpenRaft
/// represents them as nodes outside every config, so an exact-union
/// assertion would have excluded them and understated the maximum.
pub(crate) fn widest_admitted_membership() -> crate::coordination::raft::MembershipV1 {
    use crate::coordination::model::shape::{MAX_LEARNER_ENVELOPE, MAX_NEW_VOTERS, MAX_VOTERS};
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
    // The full §9 envelope, not the retained pair. The widest node map the
    // validator admits is a joint membership carrying every learner a change
    // can transiently hold, and the metadata ceiling has to admit whatever the
    // validator does -- so this is sized against the check rather than against
    // the narrower shape a joint entry happens to reach.
    let learners: Vec<usize> = (100..100 + MAX_LEARNER_ENVELOPE).collect();

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
            membership: widest_admitted_membership(),
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
        MAX_ACL_ROWS_PER_GROUP, MAX_ADMINS_PER_GROUP, MAX_AUDIT_ROWS, MAX_INTENT_ROWS,
        MAX_RESOURCES_PER_GROUP, MAX_RETAINED_ROUTES, MAX_RETAINED_SESSIONS_PER_GROUP,
    };
    crate::coordination::rowstream::RowCountsV1 {
        resources: MAX_RESOURCES_PER_GROUP,
        grants: MAX_ACL_ROWS_PER_GROUP,
        admins: MAX_ADMINS_PER_GROUP,
        sessions: MAX_RETAINED_SESSIONS_PER_GROUP,
        // Rows, not the retention cap: §9 retains 64 terminal intents and
        // never evicts the active one, so a full group streams 65.
        intents: MAX_INTENT_ROWS,
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

// ── §13 recovery export (S5-D) ───────────────────────────────────────────────

/// A canonical §13 recovery export: two resources, two grants on the first, one
/// administrator, and a real three-voter source membership at index 12.
///
/// Lives here rather than inside `import`'s own test module for a reason worth
/// recording. Two consumers need these exact bytes — the import's unit tests and
/// the crash matrix, which crashes an import mid-flight — and the obvious way to
/// share them, widening `import`'s `mod tests` to `pub(crate)`, is not
/// available: the architectural guards locate the production/test split with the
/// literal string `"\n#[cfg(test)]\nmod "`, so widening it reclassifies the whole
/// test module as production and fires guards that have nothing to do with the
/// change. A `#[cfg(test)] pub(crate)` module that already exists for exactly
/// this purpose is the right home.
///
/// The two builders are separate because the crash matrix needs the *bytes* and
/// the import's tests need the *parts* to mutate before encoding.
pub(crate) fn recovery_export_parts() -> (
    crate::coordination::rowstream::RecoveryHeaderV1,
    Vec<crate::coordination::rowstream::RecoveryBodyRowV1>,
) {
    use crate::coordination::contract::{LogIdV1, ResourceKeyV1};
    use crate::coordination::raft::{MembershipV1, StoredMembershipV1};
    use crate::coordination::rowstream::{
        AdminRowV1, GrantRowV1, RecoveryBodyRowV1, RecoveryHeaderV1, RecoveryResourceRowV1,
        RowCountsV1, RECOVERY_FORMAT_VERSION, RECOVERY_SCHEMA_VERSION,
    };
    use crate::coordination::typeconfig::CoordinationNode;

    let log_id = |index: u64| LogIdV1 {
        term: 3,
        leader_node_id: vec![0xDD; 32],
        index,
    };
    let resource = |key: &[u8], epoch: u64| {
        RecoveryBodyRowV1::Resource(RecoveryResourceRowV1 {
            key: ResourceKeyV1 {
                namespace: "portal.roaming".into(),
                key: key.to_vec(),
            },
            source_epoch: epoch,
            ttl_ms: 30_000,
            lease_revision: epoch + 1,
            value_revision: 7,
            value: vec![0x01, 0x02, 0x03],
            acl_revision: 2,
        })
    };
    let grant = |key: &[u8], subject: u8| {
        RecoveryBodyRowV1::Grant(GrantRowV1 {
            key: ResourceKeyV1 {
                namespace: "portal.roaming".into(),
                key: key.to_vec(),
            },
            subject_node_id: [subject; 32].to_vec(),
            rights: 0x3f,
        })
    };
    let rows = vec![
        resource(b"tree-1", 9),
        resource(b"tree-2", 4),
        grant(b"tree-1", 0x21),
        grant(b"tree-1", 0x22),
        RecoveryBodyRowV1::Admin(AdminRowV1 {
            subject_node_id: [0x31u8; 32].to_vec(),
        }),
    ];

    // A membership a running source group could actually have exported, built
    // through the same conversion the loader validates against.
    let voters: std::collections::BTreeSet<CoordinationNodeId> = [0xA1u8, 0xA2, 0xA3]
        .into_iter()
        .map(|b| CoordinationNodeId::from_array([b; 32]))
        .collect();
    let nodes: std::collections::BTreeMap<CoordinationNodeId, CoordinationNode> =
        voters.iter().map(|id| (*id, CoordinationNode)).collect();
    let membership =
        openraft::Membership::<CoordinationNodeId, CoordinationNode>::new(vec![voters], nodes);

    let header = RecoveryHeaderV1 {
        format_version: RECOVERY_FORMAT_VERSION,
        schema_version: RECOVERY_SCHEMA_VERSION,
        source_group_id: vec![0xAA; 32],
        source_genesis_hash: vec![0xBB; 32],
        source_authority_public_key: vec![0xCC; 32],
        source_authority_domain: "old.aster0.net".into(),
        source_last_applied: Some(log_id(40)),
        source_membership: StoredMembershipV1 {
            log_id: Some(log_id(12)),
            membership: MembershipV1::from(&membership),
        },
        source_audit_floor: Some(log_id(10)),
        source_audit_tip: Some(log_id(39)),
        source_admin_revision: 5,
        row_counts: RowCountsV1 {
            resources: 2,
            grants: 2,
            admins: 1,
            ..Default::default()
        },
    };
    (header, rows)
}

/// Encode one recovery export into the canonical §11.3 body bytes.
pub(crate) fn encode_recovery_export(
    header: &crate::coordination::rowstream::RecoveryHeaderV1,
    rows: &[crate::coordination::rowstream::RecoveryBodyRowV1],
) -> Vec<u8> {
    use crate::coordination::rowstream::{
        BodyRow, BodyWriter, RecoveryBodyRowV1, SECTION_ADMINS, SECTION_GRANTS, SECTION_RESOURCES,
    };

    let mut w =
        BodyWriter::<_, RecoveryBodyRowV1>::new(Vec::new(), header).expect("a legal header");
    for section in [SECTION_RESOURCES, SECTION_GRANTS, SECTION_ADMINS] {
        w.begin_section(section).expect("begin");
        for row in rows
            .iter()
            .filter(|r| BodyRow::section(*r) == Some(section))
        {
            w.row(row).expect("row");
        }
    }
    w.finish().expect("finish")
}

/// The canonical export and its BLAKE3, from one definition — so the hash a
/// genesis binds and the bytes an import reads cannot drift apart.
pub(crate) fn canonical_recovery_export() -> (Vec<u8>, [u8; 32]) {
    let (header, rows) = recovery_export_parts();
    let bytes = encode_recovery_export(&header, &rows);
    let hash = *blake3::hash(&bytes).as_bytes();
    (bytes, hash)
}

#[cfg(test)]
mod tests {
    use super::super::scratch::ScratchDir;
    use super::*;

    fn scratch() -> ScratchDir {
        ScratchDir::new("fixture")
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
        let root = scratch();
        let fixture = SuiteFixture::new(root.to_path_buf(), 7);

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
        let root = scratch();
        let fixture = SuiteFixture::new(root.to_path_buf(), 11);

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
        // Bound, not passed inline: the guard is what removes the tree, and a
        // temporary would drop it at the end of its own statement.
        let one_root = scratch();
        let two_root = scratch();
        let one = SuiteFixture::new(one_root.to_path_buf(), 1);
        let two = SuiteFixture::new(two_root.to_path_buf(), 2);
        assert_ne!(
            one.identity_for(0).group_id(),
            two.identity_for(0).group_id(),
            "negatives come from a separate fixture, never from weakening validation"
        );
    }
}
