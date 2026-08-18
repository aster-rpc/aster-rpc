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
            authority_public_key: [seed.wrapping_add(2); 32],
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
