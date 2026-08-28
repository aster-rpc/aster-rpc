//! Resolving a configured [`CoordinationStart`] into a storage binding (§7.2).
//!
//! This is the step §3.3 puts between "the Aster node has started and has its
//! stable NodeId" and "open the storage directory": every signature is checked
//! here, against the local identity that did not exist when the config was
//! built.
//!
//! ## What each mode can and cannot check
//!
//! The three modes hold different amounts of independent knowledge, and the
//! checks differ accordingly rather than being made to look uniform:
//!
//! | Mode | Group id and genesis hash come from | Profile comes from |
//! |---|---|---|
//! | `Bootstrap` | the verified genesis | the verified genesis |
//! | `Existing` | the configured expectation | the configured expectation |
//! | `Join` | the verified permit | the configured expectation |
//!
//! `Bootstrap` needs no separately configured profile: the signed genesis *is*
//! the independent input, and a second copy would only create a way to disagree
//! with a document the operator just signed. `Existing` and `Join` both require
//! one with no default, because in those two the value the store will be
//! checked against would otherwise come from the store itself.
//!
//! ## The identity is built from verified values only
//!
//! [`StorageIdentity`] is what binds a directory to a group forever, so nothing
//! reaches it that was not either verified here or configured as a pin. The
//! source guard in this module's tests asserts that no production code outside
//! [`model::authority`](crate::coordination::model::authority) names `GenesisV1`
//! at all: the sealed [`VerifiedGenesis`] is the only way to read a genesis
//! field, and a second decode beside it is precisely what the seal exists to
//! prevent.
//!
//! ## Unreached-by-shipping-code allowance
//!
//! Nothing in a shipping build calls [`resolve`] yet: the caller is the voter
//! runtime, which opens storage from a plan, and that is the next checkpoint.
//! The allowance is scoped to this module for the same reason every one in
//! `storage` is — a crate-root allowance would silently cover everything added
//! after it — and it comes off when the runtime lands, not at some later
//! convenient point.

#![allow(dead_code)]

use std::path::PathBuf;

use super::host::{CoordinationConfig, CoordinationStart, UnacknowledgedProfile};
use super::ids::{CoordinationGroupId, CoordinationNodeId};
use super::model::authority::{
    verify_genesis, verify_join_permit_for_start, GenesisError, PermitError, VerifiedGenesis,
    VerifiedJoinPermit,
};
use super::storage::identity::{OpenPolicy, StorageIdentity};
use super::storage::schema::{OpenOutcome, StorageLayout};

/// Why a configured start could not be resolved against this node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StartError {
    /// The signed genesis did not verify.
    Genesis(GenesisError),
    /// The signed join permit did not verify.
    Permit(PermitError),
    /// This node is not one of the genesis's signed initial voters.
    ///
    /// §7.2 allows only an initial voter to bootstrap. A node that is not one
    /// would be founding a group it is not a member of, and would then be
    /// unable to speak to it.
    NotAnInitialVoter {
        /// This node.
        local: CoordinationNodeId,
    },
    /// The group's profile has no local acknowledgement.
    Unacknowledged(UnacknowledgedProfile),
    /// The genesis binds a recovery export and no path was configured.
    RecoveryExportMissing,
    /// A recovery export path was configured for a genesis that binds none.
    RecoveryExportUnexpected,
    /// The signed genesis supplied to `Existing` is not this directory's.
    ///
    /// The group id, genesis hash or profile disagrees with the independently
    /// configured expectation. Refused rather than preferred either way: a
    /// restart that accepted a different document could propose a second
    /// genesis into a running group.
    GenesisNotThisGroups,
    /// The storage directory's state does not match the mode.
    ///
    /// `Bootstrap` requires an empty directory and `Existing` a bound one;
    /// `Join` admits either, because its own first start binds one. Reported as
    /// one variant carrying both facts because
    /// the fix is always the same shape: the operator pointed a mode at the
    /// wrong directory, or restarted with the wrong mode.
    WrongDirectoryState {
        /// The mode that was configured.
        mode: &'static str,
        /// What opening the directory found.
        outcome: OpenOutcome,
    },
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::Genesis(e) => write!(f, "the signed genesis did not verify: {e:?}"),
            StartError::Permit(e) => write!(f, "the signed join permit did not verify: {e:?}"),
            StartError::NotAnInitialVoter { local } => write!(
                f,
                "this node ({local}) is not one of the genesis's signed initial voters; only an \
                 initial voter may bootstrap"
            ),
            StartError::GenesisNotThisGroups => f.write_str(
                "the signed genesis supplied to CoordinationStart::Existing is not the \
                 document this directory is bound to",
            ),
            StartError::Unacknowledged(e) => write!(f, "{e}"),
            StartError::RecoveryExportMissing => f.write_str(
                "the genesis binds a recovery export, so CoordinationStart::Bootstrap requires \
                 recovery_export_path",
            ),
            StartError::RecoveryExportUnexpected => f.write_str(
                "the genesis binds no recovery export, so supplying recovery_export_path is an \
                 error rather than a no-op",
            ),
            StartError::WrongDirectoryState { mode, outcome } => write!(
                f,
                "CoordinationStart::{mode} does not match the storage directory, which opened as \
                 {outcome:?}"
            ),
        }
    }
}

impl std::error::Error for StartError {}

impl From<StartError> for crate::error::Error {
    fn from(err: StartError) -> Self {
        crate::error::Error::InvalidArgument(err.to_string())
    }
}

/// The mode-specific material a resolved start carries forward.
///
/// Each variant holds the *sealed* verified value rather than the bytes it came
/// from, so the corridor that runs next cannot decode a second time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResolvedStart {
    Bootstrap {
        genesis: VerifiedGenesis,
        /// Present exactly when the genesis binds an export; the pairing is
        /// checked before this value exists.
        recovery_export_path: Option<PathBuf>,
    },
    Existing {
        /// The same signed genesis, verified and bound to this directory's
        /// expected hash. Carried because a restart may still have to propose
        /// it: an interrupted bootstrap comes back at `admin_revision = 0`.
        genesis: VerifiedGenesis,
    },
    Join {
        permit: VerifiedJoinPermit,
    },
}

impl ResolvedStart {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            ResolvedStart::Bootstrap { .. } => "Bootstrap",
            ResolvedStart::Existing { .. } => "Existing",
            ResolvedStart::Join { .. } => "Join",
        }
    }

    /// Which opened-directory states this mode admits (§7.2).
    ///
    /// Three answers, not two, and the third is the one an earlier revision got
    /// wrong.
    ///
    /// * `Bootstrap` requires an **empty** directory: it founds a group, and
    ///   founding one over an existing group is how a directory ends up bound
    ///   to two.
    /// * `Existing` requires a **bound** one: its whole premise is that the
    ///   binding is already there.
    /// * `Join` admits **either**, because a joining node restarts. Its first
    ///   start binds the directory; every start after that opens a bound one,
    ///   and requiring `Fresh` refused the restart outright. §S3-JOIN is
    ///   explicit that "restart retains the same restricted state until
    ///   membership lands", and deleting the directory is not a workaround: the
    ///   group's quorum may already count the candidate a voter, so a wiped
    ///   store rejoins as a member with no log.
    ///
    /// A `Join` into a directory bound to *another* group is still refused —
    /// by the storage identity check, which compares the permit's group id and
    /// genesis hash with what the store holds. That check is the one that
    /// matters here, and it runs either way.
    pub(crate) fn admits(&self, outcome: OpenOutcome) -> bool {
        let empty = matches!(outcome, OpenOutcome::Fresh);
        match self {
            ResolvedStart::Bootstrap { .. } => empty,
            ResolvedStart::Existing { .. } => !empty,
            ResolvedStart::Join { .. } => true,
        }
    }
}

/// A configured start, resolved against this node's identity: everything needed
/// to open storage, and nothing that was not verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StartPlan {
    identity: StorageIdentity,
    resolved: ResolvedStart,
    layout: StorageLayout,
    policy: OpenPolicy,
}

impl StartPlan {
    pub(crate) fn identity(&self) -> &StorageIdentity {
        &self.identity
    }

    pub(crate) fn resolved(&self) -> &ResolvedStart {
        &self.resolved
    }

    /// Where the databases live.
    pub(crate) fn layout(&self) -> &StorageLayout {
        &self.layout
    }

    pub(crate) fn policy(&self) -> OpenPolicy {
        self.policy
    }

    /// The §13 import request, when this start is a recovery bootstrap.
    ///
    /// Built from [`VerifiedGenesis`] accessors only: the exact hash the new
    /// signed genesis binds, and its `initial_admins` for the administrator
    /// union. `resolve` proved the pairing, so a `Bootstrap` with a path
    /// always has the hash.
    pub(crate) fn recovery_import(
        &self,
    ) -> Option<crate::coordination::storage::import::RecoveryImport> {
        let ResolvedStart::Bootstrap {
            genesis,
            recovery_export_path: Some(path),
        } = &self.resolved
        else {
            return None;
        };
        let expected_hash = *genesis
            .recovery_export_hash()
            .expect("resolve pairs the path with the bound hash");
        Some(crate::coordination::storage::import::RecoveryImport {
            path: path.clone(),
            expected_hash,
            genesis_admins: genesis.initial_admins().to_vec(),
        })
    }

    /// Check the opened directory's state against the mode (§7.2).
    ///
    /// Runs *after* the open rather than before it, because "is this directory
    /// initialized?" is a question only the open coordinator can answer
    /// correctly — a pre-check on file existence would race with another opener
    /// and would misread a half-created directory. The open itself is
    /// idempotent for a `Fresh` result, so the cost of learning this late is a
    /// directory created and then abandoned, which is the safe direction.
    pub(crate) fn check_directory_state(&self, outcome: OpenOutcome) -> Result<(), StartError> {
        if !self.resolved.admits(outcome) {
            return Err(StartError::WrongDirectoryState {
                mode: self.resolved.name(),
                outcome,
            });
        }
        Ok(())
    }
}

/// Resolve a configured start against this node's verified identity.
///
/// No ticket is threaded in. A `Join` permit binds the candidate's **NodeId**,
/// and the `candidate_ticket` field it carries is derived from that NodeId by
/// [`canonical_candidate_ticket`](crate::coordination::model::authority::canonical_candidate_ticket)
/// — an Aster node's address is ephemeral, so a permit that bound one could
/// never survive the candidate's own restart.
///
/// `root` is the deployment's **ROOT public key**, already reconciled by
/// [`CoordinationConfig::resolved_root_key`] against `AsterConfig`'s
/// `ASTER_ROOT_PUBKEY`. Passed in rather than read from the coordination
/// config, because one deployment has one ROOT and a second place to configure
/// it is a second place for it to be wrong.
pub(crate) fn resolve(
    config: &CoordinationConfig,
    root: &[u8; 32],
    local: CoordinationNodeId,
) -> Result<StartPlan, StartError> {
    let key = root;
    let domain = config.authority_domain();

    let (group_id, genesis_hash, profile, resolved) = match config.start_mode() {
        CoordinationStart::Bootstrap {
            signed_genesis,
            recovery_export_path,
        } => {
            let genesis =
                verify_genesis(key, domain, signed_genesis).map_err(StartError::Genesis)?;

            // §7.2: only a signed initial voter may bootstrap.
            if !genesis.is_initial_voter(local.as_bytes()) {
                return Err(StartError::NotAnInitialVoter { local });
            }

            // The pairing is exact in both directions. A missing path for a
            // recovery-bound genesis would create empty state for a group that
            // is supposed to be a continuation; a supplied path for an ordinary
            // genesis is an operator who believes they are recovering and is
            // not.
            match (genesis.recovery_export_hash(), recovery_export_path) {
                (Some(_), None) => return Err(StartError::RecoveryExportMissing),
                (None, Some(_)) => return Err(StartError::RecoveryExportUnexpected),
                _ => {}
            }

            let group_id = CoordinationGroupId::from_array(*genesis.group_id());
            let genesis_hash = *genesis.genesis_hash();
            let profile = genesis.deployment_profile_code();
            (
                group_id,
                genesis_hash,
                profile,
                ResolvedStart::Bootstrap {
                    genesis,
                    recovery_export_path: recovery_export_path.clone(),
                },
            )
        }
        CoordinationStart::Existing {
            signed_genesis,
            expected_group_id,
            expected_genesis_hash,
            expected_deployment_profile_code,
        } => {
            // The genesis is verified here too, and bound to the hash this
            // directory is expected to carry. A restart may still have to
            // *propose* it — a voter that crashed between `Raft::initialize`
            // and the genesis entry comes back at revision 0 — so the document
            // has to be present and has to be the right one. Verifying it
            // against the configured hash is what makes "present" mean the
            // right one rather than any document that happens to verify.
            let genesis =
                verify_genesis(key, domain, signed_genesis).map_err(StartError::Genesis)?;
            if genesis.genesis_hash() != expected_genesis_hash {
                return Err(StartError::GenesisNotThisGroups);
            }
            if genesis.group_id() != expected_group_id.as_bytes() {
                return Err(StartError::GenesisNotThisGroups);
            }
            if genesis.deployment_profile_code() != *expected_deployment_profile_code {
                return Err(StartError::GenesisNotThisGroups);
            }
            (
                *expected_group_id,
                *expected_genesis_hash,
                *expected_deployment_profile_code,
                ResolvedStart::Existing { genesis },
            )
        }
        CoordinationStart::Join {
            signed_join_permit,
            expected_deployment_profile_code,
        } => {
            let permit =
                verify_join_permit_for_start(key, domain, signed_join_permit, local.as_bytes())
                    .map_err(StartError::Permit)?;
            let group_id = CoordinationGroupId::from_array(*permit.group_id());
            let genesis_hash = *permit.genesis_hash();
            (
                group_id,
                genesis_hash,
                *expected_deployment_profile_code,
                ResolvedStart::Join { permit },
            )
        }
    };

    // §7.1: the one- and two-voter profiles each need their own local
    // acknowledgement, **every start**. Checked here rather than at build
    // because a `Join` or `Existing` config names its profile as an
    // expectation, and a `Bootstrap` learns it from the genesis -- so this is
    // the first point at which all three know which profile they are running.
    config
        .development()
        .permit(profile)
        .map_err(StartError::Unacknowledged)?;

    let policy = if config.allow_broad_storage_permissions() {
        OpenPolicy::production().allowing_broad_permissions()
    } else {
        OpenPolicy::production()
    };

    Ok(StartPlan {
        identity: StorageIdentity::new(
            group_id,
            genesis_hash,
            *key,
            config.authority_domain(),
            local,
            profile,
        ),
        resolved,
        layout: StorageLayout::new(config.storage_dir()),
        policy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::admission::testing::source_admitting;
    use crate::coordination::model::authority::testing::{
        authority, genesis_for, permit_for, sign, sign_genesis,
    };
    use crate::coordination::model::membership::test_ticket;
    use crate::coordination::model::shape;
    use crate::coordination::raft::{GenesisV1, JoinPermitV1};
    use std::sync::Arc;

    const LOCAL: [u8; 32] = [1u8; 32];

    /// [`super::resolve`] under this fixture's ROOT key.
    ///
    /// A shim rather than the key threaded through every call: these tests are
    /// about the *modes*, and the deployment's root key is a constant for all
    /// of them. Naming it once here keeps a change of subject out of fifteen
    /// call sites — and a test that meant to vary the key would call
    /// `super::resolve` directly, which is then visibly the unusual thing.
    fn resolve(
        config: &CoordinationConfig,
        local: CoordinationNodeId,
    ) -> Result<StartPlan, StartError> {
        super::resolve(config, &authority().public_key, local)
    }

    fn node() -> CoordinationNodeId {
        CoordinationNodeId::from_array(LOCAL)
    }

    fn ticket() -> Vec<u8> {
        test_ticket(&LOCAL)
    }

    fn config(start: CoordinationStart) -> CoordinationConfig {
        config_with(start, |b| b)
    }

    fn config_with(
        start: CoordinationStart,
        tweak: impl FnOnce(
            crate::coordination::host::CoordinationConfigBuilder,
        ) -> crate::coordination::host::CoordinationConfigBuilder,
    ) -> CoordinationConfig {
        let a = authority();
        tweak(
            CoordinationConfig::builder()
                .storage_dir("/tmp/does-not-need-to-exist")
                .authority_domain(a.domain)
                .start_mode(start)
                .verified_admission(Arc::new(source_admitting(&[LOCAL]))),
        )
        .build()
        .expect("a complete config")
    }

    /// The `Existing` mode for a group founded on `signed`.
    ///
    /// Derived from the document rather than assembled from separate literals:
    /// `Existing` now verifies the genesis against its expectation, so a fixture
    /// whose two halves were independent constants would be testing that
    /// mismatch rather than the mode. A test that means to supply the wrong
    /// document overrides a field explicitly, which is then visibly the point.
    fn existing_of(signed: Vec<u8>) -> CoordinationStart {
        use crate::coordination::raft::SignedCoordinationDocumentV1;
        let envelope: SignedCoordinationDocumentV1 =
            crate::coordination::contract::payload_fory::<SignedCoordinationDocumentV1>()
                .deserialize(&signed)
                .expect("the fixture's own genesis decodes");
        let expected_genesis_hash =
            crate::coordination::model::authority::genesis_hash(&envelope.document_bytes);
        CoordinationStart::Existing {
            signed_genesis: signed,
            expected_group_id: CoordinationGroupId::from_array(authority().group_id),
            expected_genesis_hash,
            expected_deployment_profile_code: shape::PROFILE_PROD,
        }
    }

    fn three_voter_genesis() -> Vec<u8> {
        sign_genesis(&genesis_for(
            &[LOCAL, [2u8; 32], [3u8; 32]],
            shape::PROFILE_PROD,
        ))
    }

    fn bootstrap(signed_genesis: Vec<u8>, recovery: Option<PathBuf>) -> CoordinationStart {
        CoordinationStart::Bootstrap {
            signed_genesis,
            recovery_export_path: recovery,
        }
    }

    /// Bootstrap binds storage to exactly what the verified genesis says, and
    /// to the configured pins for the two values a genesis does not carry.
    #[test]
    fn bootstrap_binds_storage_to_the_verified_genesis() {
        let plan =
            resolve(&config(bootstrap(three_voter_genesis(), None)), node()).expect("resolve");

        let a = authority();
        assert_eq!(plan.identity().group_id().as_bytes(), &a.group_id);
        assert_eq!(plan.identity().authority_public_key(), &a.public_key);
        assert_eq!(plan.identity().authority_domain(), a.domain);
        assert_eq!(plan.identity().local_node_id(), &node());
        assert_eq!(
            plan.identity().deployment_profile_code(),
            shape::PROFILE_PROD
        );

        // The genesis hash is the derived one, not a field of the document.
        // Recomputed here from the signed bytes so the assertion has its own
        // oracle.
        let ResolvedStart::Bootstrap { genesis, .. } = plan.resolved() else {
            panic!("bootstrap resolves to Bootstrap");
        };
        let mut preimage = Vec::from(&b"aster-coordination/genesis/v1\0"[..]);
        preimage.extend_from_slice(genesis.document_bytes());
        assert_eq!(
            plan.identity().genesis_hash(),
            blake3::hash(&preimage).as_bytes()
        );
    }

    /// §7.2: only a signed initial voter may bootstrap. A node that is not one
    /// would found a group it cannot then speak to.
    #[test]
    fn a_node_outside_the_initial_voters_cannot_bootstrap() {
        let outsider = CoordinationNodeId::from_array([9u8; 32]);
        assert_eq!(
            resolve(&config(bootstrap(three_voter_genesis(), None)), outsider),
            Err(StartError::NotAnInitialVoter { local: outsider })
        );
    }

    /// A bad signature stops the resolution rather than producing a plan whose
    /// identity came from unverified bytes.
    #[test]
    fn an_unverifiable_genesis_produces_no_plan() {
        let elsewhere = sign_genesis(&GenesisV1 {
            authority_domain: "aster1.net".into(),
            ..genesis_for(&[LOCAL, [2u8; 32], [3u8; 32]], shape::PROFILE_PROD)
        });
        assert_eq!(
            resolve(&config(bootstrap(elsewhere, None)), node()),
            Err(StartError::Genesis(GenesisError::WrongDomain))
        );
    }

    /// The recovery pairing is exact in both directions.
    #[test]
    fn a_recovery_bound_genesis_requires_its_path_and_an_ordinary_one_refuses_it() {
        let bound = sign_genesis(&GenesisV1 {
            recovery_export_hash: Some(vec![0xEEu8; 32]),
            ..genesis_for(&[LOCAL, [2u8; 32], [3u8; 32]], shape::PROFILE_PROD)
        });
        assert_eq!(
            resolve(&config(bootstrap(bound.clone(), None)), node()),
            Err(StartError::RecoveryExportMissing)
        );
        assert!(resolve(
            &config(bootstrap(bound, Some(PathBuf::from("/tmp/export.aster")))),
            node()
        )
        .is_ok());

        assert_eq!(
            resolve(
                &config(bootstrap(
                    three_voter_genesis(),
                    Some(PathBuf::from("/tmp/export.aster"))
                )),
                node()
            ),
            Err(StartError::RecoveryExportUnexpected)
        );
    }

    /// Each development profile needs its acknowledgement at **every** start,
    /// including this one — the genesis being signed is not consent to run a
    /// group that cannot survive a loss.
    #[test]
    fn a_development_profile_needs_its_acknowledgement_to_resolve() {
        let single = sign_genesis(&genesis_for(&[LOCAL], shape::PROFILE_DEV_SINGLE));
        assert_eq!(
            resolve(&config(bootstrap(single.clone(), None)), node()),
            Err(StartError::Unacknowledged(UnacknowledgedProfile {
                profile: shape::PROFILE_DEV_SINGLE
            }))
        );
        assert!(resolve(
            &config_with(bootstrap(single, None), |b| b
                .acknowledge_single_voter_development_group(true)),
            node()
        )
        .is_ok());

        let even = sign_genesis(&genesis_for(&[LOCAL, [2u8; 32]], shape::PROFILE_DEV_EVEN));
        assert_eq!(
            resolve(&config(bootstrap(even.clone(), None)), node()),
            Err(StartError::Unacknowledged(UnacknowledgedProfile {
                profile: shape::PROFILE_DEV_EVEN
            }))
        );
        // And the *other* acknowledgement does not admit it.
        assert!(resolve(
            &config_with(bootstrap(even.clone(), None), |b| b
                .acknowledge_single_voter_development_group(true)),
            node()
        )
        .is_err());
        assert!(resolve(
            &config_with(bootstrap(even, None), |b| b
                .acknowledge_even_quorum_development_group(true)),
            node()
        )
        .is_ok());
    }

    /// `Existing` binds from the configured expectation, which is the point:
    /// the store is about to be checked against it, so it must not come from
    /// the store.
    #[test]
    fn existing_binds_from_the_configured_expectation() {
        let plan = resolve(&config(existing_of(three_voter_genesis())), node()).expect("resolve");
        // Both from the configured expectation, which `existing_of` derives from
        // the document it supplies — the two are now checked against each other
        // at resolve, so a fixture with unrelated literals could not get here.
        let CoordinationStart::Existing {
            expected_group_id,
            expected_genesis_hash,
            ..
        } = existing_of(three_voter_genesis())
        else {
            unreachable!("the fixture is an existing start")
        };
        assert_eq!(plan.identity().group_id(), &expected_group_id);
        assert_eq!(plan.identity().genesis_hash(), &expected_genesis_hash);
        assert!(matches!(plan.resolved(), ResolvedStart::Existing { .. }));
    }

    /// `Join` learns the group from the permit and takes the profile from the
    /// independent expectation. Both halves asserted, because taking either
    /// from the other source is the failure mode.
    #[test]
    fn join_learns_the_group_from_the_permit_and_the_profile_from_config() {
        let plan = resolve(
            &config(CoordinationStart::Join {
                signed_join_permit: sign(&permit_for(&LOCAL, &ticket())),
                expected_deployment_profile_code: shape::PROFILE_PROD,
            }),
            node(),
        )
        .expect("resolve");

        let a = authority();
        assert_eq!(plan.identity().group_id().as_bytes(), &a.group_id);
        assert_eq!(plan.identity().genesis_hash(), &a.genesis_hash);
        assert_eq!(
            plan.identity().deployment_profile_code(),
            shape::PROFILE_PROD
        );
        let ResolvedStart::Join { permit } = plan.resolved() else {
            panic!("join resolves to Join");
        };
        assert_eq!(permit.candidate(), &LOCAL);
    }

    /// A permit carrying anything but this candidate's canonical addressless
    /// ticket does not resolve. Checked here rather than at `build()` because
    /// the local NodeId the derivation needs is not known until the endpoint
    /// has started — not because the binding depends on an address.
    #[test]
    fn a_permit_bound_to_another_ticket_does_not_resolve() {
        assert_eq!(
            resolve(
                &config(CoordinationStart::Join {
                    signed_join_permit: sign(&JoinPermitV1 {
                        candidate_ticket: String::from_utf8(test_ticket(&[7u8; 32]))
                            .expect("ascii"),
                        ..permit_for(&LOCAL, &ticket())
                    }),
                    expected_deployment_profile_code: shape::PROFILE_PROD,
                }),
                node()
            ),
            Err(StartError::Permit(PermitError::TicketMismatch))
        );
    }

    /// Bootstrap and Join create a binding, so both require an empty
    /// directory; Existing reopens one, so it requires the opposite. Every
    /// `OpenOutcome` is exercised against every mode, so a fifth outcome added
    /// later cannot silently be accepted by all three.
    #[test]
    fn each_mode_admits_exactly_the_directory_states_it_creates_or_reopens() {
        let outcomes = [
            OpenOutcome::Fresh,
            OpenOutcome::Initialized,
            OpenOutcome::RecreateState,
            OpenOutcome::RecreateRaft,
        ];
        let plans = [
            (
                "Bootstrap",
                resolve(&config(bootstrap(three_voter_genesis(), None)), node()).expect("resolve"),
            ),
            (
                "Join",
                resolve(
                    &config(CoordinationStart::Join {
                        signed_join_permit: sign(&permit_for(&LOCAL, &ticket())),
                        expected_deployment_profile_code: shape::PROFILE_PROD,
                    }),
                    node(),
                )
                .expect("resolve"),
            ),
            (
                "Existing",
                resolve(&config(existing_of(three_voter_genesis())), node()).expect("resolve"),
            ),
        ];

        for (name, plan) in &plans {
            for outcome in outcomes {
                let fresh = matches!(outcome, OpenOutcome::Fresh);
                // Three answers, not two. `Bootstrap` founds a group and needs
                // an empty directory; `Existing` reopens one and needs a bound
                // directory; **`Join` takes either**, because a joining node
                // restarts — its first start binds the directory and every
                // later one opens a bound one. Requiring `Fresh` there refused
                // the restart outright, and wiping the directory is not the
                // workaround: the group's quorum may already count the
                // candidate a voter.
                let expected = match *name {
                    "Bootstrap" => fresh,
                    "Existing" => !fresh,
                    "Join" => true,
                    other => unreachable!("unknown mode {other}"),
                };
                assert_eq!(
                    plan.check_directory_state(outcome).is_ok(),
                    expected,
                    "{name} against {outcome:?}"
                );
            }
        }
    }

    /// The development escape reaches the open policy, and nothing else does.
    #[test]
    fn the_permission_escape_is_the_only_negotiable_open_rule() {
        let strict =
            resolve(&config(bootstrap(three_voter_genesis(), None)), node()).expect("resolve");
        assert!(strict.policy().enforces_permissions());

        let relaxed = resolve(
            &config_with(bootstrap(three_voter_genesis(), None), |b| {
                b.allow_broad_storage_permissions(true)
            }),
            node(),
        )
        .expect("resolve");
        assert!(!relaxed.policy().enforces_permissions());
    }

    /// Slice 3's genesis guard: outside `model::authority`, no production code
    /// names `GenesisV1`.
    ///
    /// The sealed [`VerifiedGenesis`] exists so a caller cannot decode the
    /// presented bytes a second time and act on a document that was never
    /// verified. That property is about *where a decode may appear*, which no
    /// behavioural test can observe — a second decode of the same bytes usually
    /// produces the same value, so the bug is invisible until the two inputs
    /// differ. It is therefore asserted as a statement about the source, the
    /// same instrument `storage::durability_guard` uses for the deferred-fsync
    /// rule.
    ///
    /// `authority.rs` is excluded because it is the module that *does* the
    /// verifying, and `start.rs` is excluded because this test names the type
    /// in its own prose.
    #[test]
    fn no_production_code_outside_the_verifier_names_the_raw_genesis() {
        const SOURCES: &[(&str, &str)] = &[
            ("host.rs", include_str!("host.rs")),
            ("admission.rs", include_str!("admission.rs")),
            ("service.rs", include_str!("service.rs")),
            ("start.rs", include_str!("start.rs")),
        ];
        for (name, source) in SOURCES {
            let production = &source[..production_end(source)];
            for (n, line) in production.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                assert!(
                    !code.contains("GenesisV1"),
                    "{name}:{} reads a raw GenesisV1; every genesis field must be read \
                     through the sealed VerifiedGenesis",
                    n + 1
                );
            }
        }
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

    /// And the guard is not vacuous: the string it forbids is one the verifier
    /// module really does contain, so a guard pointed at the wrong files would
    /// be visible here.
    #[test]
    fn the_genesis_guard_looks_for_a_string_that_exists() {
        let verifier = include_str!("model/authority.rs");
        assert!(
            verifier.contains("GenesisV1"),
            "the verifier must name the raw type; if it stopped, this guard is checking \
             for a token nothing produces"
        );
    }
}
