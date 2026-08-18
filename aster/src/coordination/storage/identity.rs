//! The crate-private storage-open identity and open policy (§11.1).
//!
//! ## Why this is not `CoordinationConfig`
//!
//! §11.1 requires storage to bind a group id, genesis hash, authority key and
//! domain, local NodeId, and deployment profile, and to refuse to open against
//! a different one. Every one of those values arrives from a *verified* signed
//! document, and Slice 3 owns that parsing.
//!
//! So this type is crate-private and has no `Default`: Slice 2 tests construct
//! it directly, and Slice 3 will construct it from verified bootstrap input.
//! The alternative — adding accepted-but-unverified fields to the public
//! [`CoordinationConfig`](crate::coordination::host::CoordinationConfig) so
//! storage compiles — is worse than an absent field, because it reads as
//! configured while nothing enforces it.
//!
//! ## Versions are constants, not fields
//!
//! [`LOGICAL_SCHEMA_VERSION`] and [`STORAGE_CODEC_VERSION`] are compiled
//! protocol constants and deliberately not constructor parameters. A voter
//! that could be *asked* to write some other version could write a file its
//! own peers cannot read, and the resulting divergence would look like
//! corruption rather than misconfiguration.

use crate::coordination::ids::{CoordinationGroupId, CoordinationNodeId};

/// The logical schema version this build implements (§11.2).
///
/// Versions **this plan's logical schema**, not redb's physical file format.
/// The two are independent: a redb major upgrade may change the file format
/// with no logical change, and a logical migration may occur with the format
/// unchanged. Startup validates and reports them separately.
pub(crate) const LOGICAL_SCHEMA_VERSION: u32 = 1;

/// The internal storage codec version this build implements (§11.2).
///
/// Distinct from the schema version because the value encoding can change
/// while the table shape does not — and because §19 requires a wrong codec to
/// be refused by name rather than surfacing as a decode failure halfway
/// through a load.
pub(crate) const STORAGE_CODEC_VERSION: u32 = 1;

/// Everything storage must bind itself to, immutably, at first open.
///
/// Cloned rather than borrowed because the open coordinator hands it to both
/// databases and then keeps it for revalidation after a snapshot install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StorageIdentity {
    group_id: CoordinationGroupId,
    genesis_hash: [u8; 32],
    authority_public_key: [u8; 32],
    authority_domain: String,
    local_node_id: CoordinationNodeId,
    deployment_profile_code: i32,
}

impl StorageIdentity {
    /// Construct an identity. Every field is required; there is no builder and
    /// no `Default`, so a field added later is a compile error at each call
    /// site rather than a silently zeroed binding.
    pub(crate) fn new(
        group_id: CoordinationGroupId,
        genesis_hash: [u8; 32],
        authority_public_key: [u8; 32],
        authority_domain: impl Into<String>,
        local_node_id: CoordinationNodeId,
        deployment_profile_code: i32,
    ) -> Self {
        Self {
            group_id,
            genesis_hash,
            authority_public_key,
            authority_domain: authority_domain.into(),
            local_node_id,
            deployment_profile_code,
        }
    }

    pub(crate) fn group_id(&self) -> &CoordinationGroupId {
        &self.group_id
    }

    pub(crate) fn genesis_hash(&self) -> &[u8; 32] {
        &self.genesis_hash
    }

    pub(crate) fn authority_public_key(&self) -> &[u8; 32] {
        &self.authority_public_key
    }

    pub(crate) fn authority_domain(&self) -> &str {
        &self.authority_domain
    }

    /// The local NodeId, bound immutably in `raft.redb` only.
    ///
    /// Deliberately absent from `state.redb`: the state machine is replicated,
    /// and a snapshot moves between nodes. Binding a local id into replicated
    /// state would make every installed snapshot look like an identity
    /// mismatch.
    pub(crate) fn local_node_id(&self) -> &CoordinationNodeId {
        &self.local_node_id
    }

    /// The replicated deployment profile (`shape::PROFILE_*`), which decides
    /// the legal voter counts.
    pub(crate) fn deployment_profile_code(&self) -> i32 {
        self.deployment_profile_code
    }
}

/// How strictly an open enforces host-local filesystem policy.
///
/// Only the permission rule is negotiable, and only explicitly. Identity,
/// schema, codec, integrity, and partial-initialization rules have no
/// development mode: relaxing those would let a test prove a property the
/// shipping voter does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PermissionPolicy {
    /// Refuse an existing directory or file that is writable beyond its owner.
    #[default]
    Production,
    /// Accept broader permissions. Explicitly development-only: it exists for
    /// shared build agents and container images whose umask is not ours.
    DevelopmentOnlyAllowBroadPermissions,
}

/// Host-local policy for one storage open.
///
/// `Default` is production. Slice 3 owns making any of this publicly
/// configurable; until then a caller cannot weaken it by accident because it
/// cannot name the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct OpenPolicy {
    permissions: PermissionPolicy,
}

impl OpenPolicy {
    /// The shipping policy.
    pub(crate) fn production() -> Self {
        Self {
            permissions: PermissionPolicy::Production,
        }
    }

    /// Accept directory/file permissions broader than `0700`/`0600`.
    pub(crate) fn allowing_broad_permissions(mut self) -> Self {
        self.permissions = PermissionPolicy::DevelopmentOnlyAllowBroadPermissions;
        self
    }

    pub(crate) fn enforces_permissions(&self) -> bool {
        matches!(self.permissions, PermissionPolicy::Production)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::model::shape;

    fn identity() -> StorageIdentity {
        StorageIdentity::new(
            CoordinationGroupId::from_array([7u8; 32]),
            [9u8; 32],
            [11u8; 32],
            "example.test",
            CoordinationNodeId::from_array([13u8; 32]),
            shape::PROFILE_PROD,
        )
    }

    #[test]
    fn an_identity_round_trips_every_field() {
        let id = identity();
        assert_eq!(id.group_id().as_bytes(), &[7u8; 32]);
        assert_eq!(id.genesis_hash(), &[9u8; 32]);
        assert_eq!(id.authority_public_key(), &[11u8; 32]);
        assert_eq!(id.authority_domain(), "example.test");
        assert_eq!(id.local_node_id().as_bytes(), &[13u8; 32]);
        assert_eq!(id.deployment_profile_code(), shape::PROFILE_PROD);
    }

    /// Production is the default. Asserted rather than assumed: `OpenPolicy`
    /// is `Default`-constructible, so a call site that forgets to choose must
    /// land on the strict side.
    #[test]
    fn the_default_policy_enforces_permissions() {
        assert!(OpenPolicy::default().enforces_permissions());
        assert!(OpenPolicy::production().enforces_permissions());
        assert!(!OpenPolicy::default()
            .allowing_broad_permissions()
            .enforces_permissions());
    }

    /// The versions are constants. This test exists to make a bump visible in
    /// a diff: changing either is an on-disk compatibility decision, not a
    /// refactor.
    #[test]
    fn the_compiled_versions_are_v1() {
        assert_eq!(LOGICAL_SCHEMA_VERSION, 1);
        assert_eq!(STORAGE_CODEC_VERSION, 1);
    }
}
