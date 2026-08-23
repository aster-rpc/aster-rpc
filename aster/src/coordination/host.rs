//! Host-side voter configuration (§3.3) and the three start modes (§7.2).
//!
//! ## Every field is required, and none of them defaults to something weaker
//!
//! Slice 0 carried only the storage directory, on the rule that "a field
//! accepted now and checked later is worse than an absent field, because it
//! reads as configured while nothing enforces it". Slice 3 owns the parsing, so
//! the fields land together with the code that verifies them: the pinned
//! authority key and domain, exactly one [`CoordinationStart`], and a
//! [`VerifiedAdmissionSource`].
//!
//! There is no `Default` for the config, no unset admission source, and no
//! allow-all fallback. A config that omits any of them does not build.
//!
//! ## What `build()` checks, and what it deliberately leaves to `start()`
//!
//! `build()` checks everything that is a property of the configuration alone:
//! field presence, fixed widths, the domain charset, and that a declared
//! profile code is one this build implements.
//!
//! It does **not** verify signatures. §3.3 fixes the startup order as *validate
//! static config → start the Aster node to obtain its stable NodeId → open and
//! validate storage against that NodeId*, and every signature check in §7.2
//! binds something the node does not have yet: bootstrap requires the local
//! NodeId to be a signed initial voter, and a join permit binds the candidate's
//! NodeId **and its ticket**, neither of which exists before the endpoint does.
//! A `build()` that verified what it could would leave the reader unable to tell
//! which half was checked.
//!
//! ## Development acknowledgements are per start, and never replicated
//!
//! §7.1's one- and two-voter profiles each require an explicit local
//! acknowledgement **every start**. They are host-local: they gate whether
//! *this* process is willing to run such a group, they never enter apply, and
//! they never change the replicated `deployment_profile_code`. A group whose
//! genesis says profile 2 is a one-voter group whether or not this node
//! acknowledges it; the acknowledgement decides only whether this node
//! participates.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::admission::VerifiedAdmissionSource;
use super::ids::CoordinationGroupId;
use super::model::authority::MAX_AUTHORITY_DOMAIN_BYTES;
use super::model::shape;
use crate::error::{Error, Result};

/// How this voter starts (§7.2). Exactly three modes exist.
///
/// The pinned authority key and domain are **not** here: they live once on
/// [`CoordinationConfig`] and apply identically to all three. An earlier
/// revision put a key inside `Existing` while also requiring one at the top
/// level, which left it ambiguous which was authoritative and made
/// `Bootstrap`/`Join` read as though they had no pin at all.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum CoordinationStart {
    /// Found an empty directory and found this group.
    ///
    /// The local NodeId must be one of the signed initial voters. Every initial
    /// voter may call `initialize` with the same signed membership; identical
    /// concurrent initialization is tolerated.
    Bootstrap {
        /// The signed genesis envelope (§7.1), as bytes.
        signed_genesis: Vec<u8>,
        /// Required exactly when the genesis binds a recovery export, and an
        /// error otherwise. Slice 5 performs the import; Slice 3 enforces the
        /// pairing so a recovery-bound genesis can never silently create empty
        /// state.
        recovery_export_path: Option<PathBuf>,
    },
    /// Reopen storage this voter already owns.
    ///
    /// Never calls `initialize`. Every configured value must match what is
    /// persisted, including the top-level authority key and domain.
    Existing {
        /// The **same signed genesis** the group was founded on.
        ///
        /// Required, and it is not redundant with the hash below. The genesis
        /// is installed by a committed command, and a voter that crashed after
        /// `Raft::initialize` committed but before that entry applied comes
        /// back at `admin_revision = 0` — with no administrators, so nothing
        /// can ever propose one. Without the document, restarting through
        /// `Existing` could never finish the bootstrap it interrupted, and the
        /// group would be permanently wedged at `NoQuorum` with no error to
        /// explain it.
        ///
        /// Verified against `expected_genesis_hash`, so supplying a *different*
        /// document is a start failure rather than a second genesis proposed
        /// into a running group.
        signed_genesis: Vec<u8>,
        /// The group this directory must be bound to.
        expected_group_id: CoordinationGroupId,
        /// The genesis hash this directory must be bound to.
        expected_genesis_hash: [u8; 32],
        /// **Required with no default.** It is the only input to the binding
        /// check that does not come from the database being validated, so
        /// taking it from that database would make the comparison
        /// tautological — the store would be checked against itself and every
        /// mismatch would agree.
        expected_deployment_profile_code: i32,
    },
    /// Join an existing group with a signed permit, into an empty directory.
    ///
    /// The permit grants **no membership**: an existing administrator must
    /// still commit a membership intent through the current quorum. What it
    /// grants is the right to be spoken to before any committed membership
    /// contains this node.
    Join {
        /// The signed join-permit envelope (§7.1), as bytes.
        signed_join_permit: Vec<u8>,
        /// **Required with no default**, for the reason given under
        /// [`Existing`](Self::Existing): the permit carries no profile, so the
        /// installed state must be checked against an independent expectation.
        expected_deployment_profile_code: i32,
    },
}

impl CoordinationStart {
    /// A short name for logs and errors.
    pub fn name(&self) -> &'static str {
        match self {
            CoordinationStart::Bootstrap { .. } => "Bootstrap",
            CoordinationStart::Existing { .. } => "Existing",
            CoordinationStart::Join { .. } => "Join",
        }
    }

    /// The independently configured profile expectation, where the mode has
    /// one.
    ///
    /// `Bootstrap` has none by design: the profile comes from the verified
    /// genesis, which is itself the independent input. Requiring a second copy
    /// would only create a way to disagree with a document the operator just
    /// signed.
    pub fn expected_deployment_profile_code(&self) -> Option<i32> {
        match self {
            CoordinationStart::Bootstrap { .. } => None,
            CoordinationStart::Existing {
                expected_deployment_profile_code,
                ..
            }
            | CoordinationStart::Join {
                expected_deployment_profile_code,
                ..
            } => Some(*expected_deployment_profile_code),
        }
    }
}

/// The host-local acknowledgements §7.1 requires for a group with no fault
/// tolerance.
///
/// Two independent flags rather than one "development mode", because they admit
/// different things and an operator who wants one should not silently get the
/// other. Neither has a `true` default and neither can be set from a file or an
/// environment variable — the point is that a group that cannot survive a single
/// loss is acknowledged in code the operator wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DevelopmentAcknowledgements {
    single_voter: bool,
    even_quorum: bool,
}

impl DevelopmentAcknowledgements {
    /// Whether this process will run the one-voter development profile.
    pub fn allows_single_voter(&self) -> bool {
        self.single_voter
    }

    /// Whether this process will run the two-voter development profile.
    pub fn allows_even_quorum(&self) -> bool {
        self.even_quorum
    }

    /// Whether `profile` is acknowledged.
    ///
    /// Production needs no acknowledgement; the two development profiles each
    /// need their own. Written as a match over the declared codes rather than
    /// `!= PROFILE_PROD`, so a fourth profile added to
    /// [`shape::PROFILES`](super::model::shape::PROFILES) is a compile-time
    /// question here rather than silently inheriting "no acknowledgement
    /// needed".
    pub fn permit(&self, profile: i32) -> std::result::Result<(), UnacknowledgedProfile> {
        let acknowledged = match profile {
            shape::PROFILE_PROD => true,
            shape::PROFILE_DEV_SINGLE => self.single_voter,
            shape::PROFILE_DEV_EVEN => self.even_quorum,
            // An undeclared profile is refused before this is reached;
            // refusing here too keeps the function total without inventing a
            // permissive default for a code this build cannot evaluate.
            _ => false,
        };
        if acknowledged {
            Ok(())
        } else {
            Err(UnacknowledgedProfile { profile })
        }
    }
}

/// A profile this process has not acknowledged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnacknowledgedProfile {
    /// The profile the group declares.
    pub profile: i32,
}

impl std::fmt::Display for UnacknowledgedProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (name, method) = match self.profile {
            shape::PROFILE_DEV_SINGLE => (
                "a one-voter development group",
                "acknowledge_single_voter_development_group(true)",
            ),
            shape::PROFILE_DEV_EVEN => (
                "a two-voter development group, which stops on either voter's loss",
                "acknowledge_even_quorum_development_group(true)",
            ),
            other => {
                return write!(
                    f,
                    "deployment profile {other} is not one this build implements"
                )
            }
        };
        write!(
            f,
            "this group is {name}; hosting it requires \
             CoordinationConfig::builder().{method}"
        )
    }
}

impl std::error::Error for UnacknowledgedProfile {}

/// Configuration for hosting a coordination voter.
///
/// ```no_run
/// # fn run() -> aster::Result<()> {
/// use std::sync::Arc;
/// use aster::coordination::host::{CoordinationConfig, CoordinationStart};
/// # fn signed_genesis() -> Vec<u8> { Vec::new() }
/// # fn admission() -> Arc<dyn aster::coordination::admission::VerifiedAdmissionSource> {
/// #     unimplemented!()
/// # }
///
/// let config = CoordinationConfig::builder()
///     .storage_dir("/var/lib/my-app/aster-coordination")
///     .authority_domain("example.portal.roaming")
///     .start_mode(CoordinationStart::Bootstrap {
///         signed_genesis: signed_genesis(),
///         recovery_export_path: None,
///     })
///     .verified_admission(admission())
///     .build()?;
/// # let _ = config; Ok(())
/// # }
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub struct CoordinationConfig {
    storage_dir: PathBuf,
    // **There is no ROOT key here, deliberately.** `AsterConfig` resolves
    // `ASTER_ROOT_PUBKEY` / `ASTER_ROOT_PUBKEY_FILE` / `[trust] root_pubkey`,
    // and coordination consumes that. One deployment has one ROOT; a second
    // place to configure it is a second place for it to be wrong, and the
    // disagreement would surface as "this genesis does not verify" — a message
    // about the document, from a mismatch about the key.
    authority_domain: String,
    start_mode: CoordinationStart,
    verified_admission: Arc<dyn VerifiedAdmissionSource>,
    development: DevelopmentAcknowledgements,
    allow_broad_storage_permissions: bool,
}

// Hand-written because `Arc<dyn VerifiedAdmissionSource>` is not `Debug` and
// requiring `Debug` on the trait would push a bound onto every application's
// admission directory for the benefit of a log line. The source is reported as
// present rather than rendered.
impl std::fmt::Debug for CoordinationConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoordinationConfig")
            .field("storage_dir", &self.storage_dir)
            .field("authority_domain", &self.authority_domain)
            .field("start_mode", &self.start_mode.name())
            .field("verified_admission", &"<source>")
            .field("development", &self.development)
            .field(
                "allow_broad_storage_permissions",
                &self.allow_broad_storage_permissions,
            )
            .finish()
    }
}

impl CoordinationConfig {
    /// Start building a configuration.
    pub fn builder() -> CoordinationConfigBuilder {
        CoordinationConfigBuilder::default()
    }

    /// The persistent directory this voter's storage lives in.
    pub fn storage_dir(&self) -> &Path {
        &self.storage_dir
    }

    /// The one ROOT public key this voter runs under.
    ///
    /// `deployment` is `AsterConfig`'s resolved `ASTER_ROOT_PUBKEY`, and it is
    /// the **only** source. There is no coordination-local key to reconcile it
    /// against, because there is no such field.
    ///
    /// Two earlier revisions got this wrong in the same direction: the first
    /// required the key here as well, the second accepted either. Both left
    /// coordination able to run under a key `AsterConfig` had never seen — and,
    /// because every fixture then took that route, left nothing in the suite
    /// proving a voter could start from `ASTER_ROOT_PUBKEY` at all.
    pub fn resolved_root_key(&self, deployment: Option<[u8; 32]>) -> Result<[u8; 32]> {
        deployment.ok_or_else(|| {
            Error::InvalidArgument(
                "coordination requires the deployment ROOT public key: set \
                 ASTER_ROOT_PUBKEY, ASTER_ROOT_PUBKEY_FILE, or [trust] root_pubkey. \
                 It is the key every signed genesis, join permit and recovery \
                 document is verified against, and the identity of the group's \
                 immutable super-administrator"
                    .into(),
            )
        })
    }

    /// The expected `authority_domain`, compared against every signed
    /// document's own.
    pub fn authority_domain(&self) -> &str {
        &self.authority_domain
    }

    /// How this voter starts.
    pub fn start_mode(&self) -> &CoordinationStart {
        &self.start_mode
    }

    /// The application's verified admission directory.
    pub fn verified_admission(&self) -> &Arc<dyn VerifiedAdmissionSource> {
        &self.verified_admission
    }

    /// The host-local development acknowledgements.
    pub fn development(&self) -> DevelopmentAcknowledgements {
        self.development
    }

    /// Whether this process will open a storage directory that is writable
    /// beyond its owner.
    pub fn allow_broad_storage_permissions(&self) -> bool {
        self.allow_broad_storage_permissions
    }
}

/// Builder for a [`CoordinationConfig`].
#[derive(Clone, Default)]
pub struct CoordinationConfigBuilder {
    storage_dir: Option<PathBuf>,
    authority_domain: Option<String>,
    start_mode: Option<CoordinationStart>,
    verified_admission: Option<Arc<dyn VerifiedAdmissionSource>>,
    development: DevelopmentAcknowledgements,
    allow_broad_storage_permissions: bool,
}

impl CoordinationConfigBuilder {
    /// The persistent storage directory. Required.
    ///
    /// §3.3 asks for "an absolute or application-resolved" path, which is not a
    /// distinction a library can make at runtime -- a relative path may be
    /// exactly what the application resolved. So the check here is only that a
    /// non-empty path was supplied; where it lands is the application's choice
    /// and its backup policy's problem.
    pub fn storage_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.storage_dir = Some(dir.into());
        self
    }

    // No `authority_public_key` setter: the ROOT key is the deployment's, set
    // once through `AsterConfig`. Offering one here would make coordination a
    // second place to configure it, which is the thing the contract forbids —
    // and a setter that only cross-checked would still be a field an operator
    // has to keep in step with the real one.

    /// The expected `authority_domain`. Required.
    pub fn authority_domain(mut self, domain: impl Into<String>) -> Self {
        self.authority_domain = Some(domain.into());
        self
    }

    /// Exactly one [`CoordinationStart`] mode. Required.
    pub fn start_mode(mut self, mode: CoordinationStart) -> Self {
        self.start_mode = Some(mode);
        self
    }

    /// The application's verified admission directory. Required, with no
    /// default and no allow-all fallback.
    pub fn verified_admission(mut self, source: Arc<dyn VerifiedAdmissionSource>) -> Self {
        self.verified_admission = Some(source);
        self
    }

    /// Acknowledge a one-voter development group (§7.1 profile 2).
    ///
    /// A group of one has no fault tolerance and no meaningful consensus; it
    /// exists so a single-process test can exercise the real corridor.
    pub fn acknowledge_single_voter_development_group(mut self, yes: bool) -> Self {
        self.development.single_voter = yes;
        self
    }

    /// Acknowledge a two-voter development group (§7.1 profile 3).
    ///
    /// Deliberately even: it has **no fault tolerance at all** — losing either
    /// voter stops progress, which is strictly worse than one voter, and it
    /// exists so the even-quorum path is testable.
    pub fn acknowledge_even_quorum_development_group(mut self, yes: bool) -> Self {
        self.development.even_quorum = yes;
        self
    }

    /// Accept a storage directory or file writable beyond its owner.
    ///
    /// Development-only, and the only §11.1 open rule that is negotiable at
    /// all: it exists for shared build agents and container images whose umask
    /// is not ours. Identity, schema, codec, integrity, and
    /// partial-initialization rules have no development mode, because relaxing
    /// one would let a test prove a property the shipping voter does not have.
    pub fn allow_broad_storage_permissions(mut self, yes: bool) -> Self {
        self.allow_broad_storage_permissions = yes;
        self
    }

    /// Validate and produce the configuration.
    pub fn build(self) -> Result<CoordinationConfig> {
        let storage_dir = self.storage_dir.ok_or_else(|| {
            missing(
                "storage_dir",
                "a voter's log and state machine must survive restart",
            )
        })?;
        if storage_dir.as_os_str().is_empty() {
            return Err(Error::InvalidArgument(
                "CoordinationConfig storage_dir must not be empty".into(),
            ));
        }

        let authority_domain = self.authority_domain.ok_or_else(|| {
            missing(
                "authority_domain",
                "one authority legitimately signs for several deployments, so a document \
                 minted for staging verifies cleanly against production's key without an \
                 independently configured domain to compare",
            )
        })?;
        // The same bound §7.1 puts on the field inside every signed document.
        // Checked here so a domain that could never match one is refused by the
        // builder rather than by every document comparison.
        if authority_domain.is_empty()
            || authority_domain.len() > MAX_AUTHORITY_DOMAIN_BYTES
            || !authority_domain.is_ascii()
        {
            return Err(Error::InvalidArgument(format!(
                "CoordinationConfig authority_domain must be 1..={MAX_AUTHORITY_DOMAIN_BYTES} \
                 ASCII bytes; no signed document can carry the configured value"
            )));
        }

        let start_mode = self.start_mode.ok_or_else(|| {
            missing(
                "start_mode",
                "a voter must say whether it is founding a group, reopening one, or joining one",
            )
        })?;

        let verified_admission = self.verified_admission.ok_or_else(|| {
            missing(
                "verified_admission",
                "consensus accepts only an exact-subject Producer, and there is no allow-all \
                 fallback",
            )
        })?;

        // A profile code this build cannot evaluate is refused by membership in
        // the declared set. `shape::profile_permits_voters` cannot answer this:
        // it reports whether a *count* is permitted, and an unknown profile
        // permits no count -- so a node that has committed no membership yet
        // slips past it entirely.
        if let Some(profile) = start_mode.expected_deployment_profile_code() {
            if !shape::PROFILES.contains(&profile) {
                return Err(Error::InvalidArgument(format!(
                    "CoordinationConfig expected_deployment_profile_code {profile} is not a \
                     profile this build implements; declared profiles are {:?}",
                    shape::PROFILES
                )));
            }
        }

        // The recovery pairing, checked here because it is a property of the
        // configuration alone once the genesis is verified -- except that the
        // genesis is not verified yet, so only the "supplied a path" half is
        // knowable now. The other half (genesis binds an export, no path
        // supplied) is §7.2's, at start.
        if let CoordinationStart::Bootstrap {
            signed_genesis,
            recovery_export_path,
        } = &start_mode
        {
            if signed_genesis.is_empty() {
                return Err(Error::InvalidArgument(
                    "CoordinationStart::Bootstrap requires the signed genesis bytes".into(),
                ));
            }
            if recovery_export_path
                .as_ref()
                .is_some_and(|p| p.as_os_str().is_empty())
            {
                return Err(Error::InvalidArgument(
                    "CoordinationStart::Bootstrap recovery_export_path must not be empty; omit \
                     it entirely when the genesis binds no export"
                        .into(),
                ));
            }
        }
        if let CoordinationStart::Join {
            signed_join_permit, ..
        } = &start_mode
        {
            if signed_join_permit.is_empty() {
                return Err(Error::InvalidArgument(
                    "CoordinationStart::Join requires the signed join permit bytes".into(),
                ));
            }
        }

        Ok(CoordinationConfig {
            storage_dir,
            authority_domain,
            start_mode,
            verified_admission,
            development: self.development,
            allow_broad_storage_permissions: self.allow_broad_storage_permissions,
        })
    }
}

/// One missing-field error, phrased the same way every time: what is absent,
/// and why the absence cannot be defaulted away.
fn missing(field: &str, why: &str) -> Error {
    Error::InvalidArgument(format!("CoordinationConfig requires {field}: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::admission::testing::source_admitting;

    fn source() -> Arc<dyn VerifiedAdmissionSource> {
        Arc::new(source_admitting(&[[1u8; 32]]))
    }

    fn bootstrap() -> CoordinationStart {
        CoordinationStart::Bootstrap {
            signed_genesis: crate::coordination::model::authority::testing::valid_genesis(),
            recovery_export_path: None,
        }
    }

    fn complete() -> CoordinationConfigBuilder {
        CoordinationConfig::builder()
            .storage_dir("/var/lib/example/coordination")
            .authority_domain("example.test")
            .start_mode(bootstrap())
            .verified_admission(source())
    }

    #[test]
    fn a_complete_configuration_builds_and_preserves_every_field() {
        let config = complete().build().expect("build");
        assert_eq!(
            config.storage_dir(),
            Path::new("/var/lib/example/coordination")
        );
        assert_eq!(config.authority_domain(), "example.test");
        assert_eq!(config.start_mode().name(), "Bootstrap");
        assert!(!config.development().allows_single_voter());
        assert!(!config.development().allows_even_quorum());
        assert!(!config.allow_broad_storage_permissions());
    }

    /// Every required field, omitted one at a time from an otherwise complete
    /// builder — so each case fails for exactly the field under test rather
    /// than for being empty.
    #[test]
    fn each_required_field_is_required_on_its_own() {
        let cases: Vec<(&str, CoordinationConfigBuilder)> = vec![
            ("storage_dir", {
                let mut b = complete();
                b.storage_dir = None;
                b
            }),
            ("authority_domain", {
                let mut b = complete();
                b.authority_domain = None;
                b
            }),
            ("start_mode", {
                let mut b = complete();
                b.start_mode = None;
                b
            }),
            ("verified_admission", {
                let mut b = complete();
                b.verified_admission = None;
                b
            }),
        ];
        for (field, builder) in cases {
            let err = builder
                .build()
                .expect_err(&format!("{field} must be required"));
            assert!(
                err.to_string().contains(field),
                "the error must name the missing field {field}, got: {err}"
            );
        }
    }

    /// **One deployment has one ROOT, and coordination is not where it lives.**
    ///
    /// There is no coordination-local key to reconcile: the field and its
    /// setter are gone. Two earlier revisions required the key here as well,
    /// then accepted either — and because every fixture took the local route,
    /// nothing in the suite proved a voter could start from `ASTER_ROOT_PUBKEY`
    /// at all.
    #[test]
    fn coordination_takes_the_root_key_only_from_the_deployment() {
        let config = complete().build().expect("a complete configuration");

        assert_eq!(
            config
                .resolved_root_key(Some([9u8; 32]))
                .expect("deployment"),
            [9u8; 32]
        );

        let err = config
            .resolved_root_key(None)
            .expect_err("a group needs a ROOT and coordination cannot supply one");
        assert!(
            err.to_string().contains("ASTER_ROOT_PUBKEY"),
            "the refusal must name where the key comes from, got: {err}"
        );
    }

    /// There is no allow-all fallback: the source has no `Default`, so a
    /// config without one cannot be built at all. Asserted because "required"
    /// is the whole property — an admission source that could be omitted would
    /// be an admission source that could be forgotten.
    #[test]
    fn a_config_without_an_admission_source_does_not_build() {
        let err = CoordinationConfig::builder()
            .storage_dir("/tmp/x")
            .authority_domain("example.test")
            .start_mode(bootstrap())
            .build()
            .expect_err("no source, no config");
        assert!(err.to_string().contains("verified_admission"), "{err}");
    }

    #[test]
    fn an_empty_storage_dir_is_rejected() {
        let err = complete()
            .storage_dir("")
            .build()
            .expect_err("an empty path is not a directory");
        assert!(err.to_string().contains("storage_dir"), "got: {err}");
    }

    /// A relative path is the "application-resolved" case §3.3 allows; the
    /// builder must not quietly reject or rewrite it.
    #[test]
    fn a_relative_storage_dir_is_accepted_unchanged() {
        let config = complete()
            .storage_dir("state/coordination")
            .build()
            .expect("build");
        assert_eq!(config.storage_dir(), Path::new("state/coordination"));
    }

    /// A configured domain no signed document could ever carry is refused by
    /// the builder rather than by every comparison at start.
    #[test]
    fn a_domain_outside_the_signed_document_bound_is_refused() {
        for bad in [
            String::new(),
            "a".repeat(MAX_AUTHORITY_DOMAIN_BYTES + 1),
            "astér.net".into(),
        ] {
            let err = complete()
                .authority_domain(bad.clone())
                .build()
                .expect_err("a domain no document can carry");
            assert!(err.to_string().contains("authority_domain"), "{err}");
        }
        // And the boundary itself is accepted.
        assert!(complete()
            .authority_domain("a".repeat(MAX_AUTHORITY_DOMAIN_BYTES))
            .build()
            .is_ok());
    }

    /// The expected profile is checked for membership in the declared set, and
    /// the cases are iterated from `shape::PROFILES` rather than matched
    /// against literals.
    #[test]
    fn an_expected_profile_must_be_one_this_build_implements() {
        for profile in shape::PROFILES {
            assert!(
                complete()
                    .start_mode(CoordinationStart::Join {
                        signed_join_permit: vec![1, 2, 3],
                        expected_deployment_profile_code: profile,
                    })
                    .build()
                    .is_ok(),
                "declared profile {profile} must be configurable"
            );
        }
        for profile in [0, 4, -1, i32::MAX] {
            let err = complete()
                .start_mode(CoordinationStart::Join {
                    signed_join_permit: vec![1, 2, 3],
                    expected_deployment_profile_code: profile,
                })
                .build()
                .expect_err("an undeclared profile");
            assert!(
                err.to_string().contains("expected_deployment_profile_code"),
                "{err}"
            );
        }
    }

    /// `Bootstrap` has no expected-profile field, and that is deliberate: the
    /// profile comes from the verified genesis, which is already the
    /// independent input.
    #[test]
    fn bootstrap_carries_no_expected_profile_and_the_other_two_do() {
        assert_eq!(bootstrap().expected_deployment_profile_code(), None);
        assert_eq!(
            CoordinationStart::Existing {
                signed_genesis: crate::coordination::model::authority::testing::valid_genesis(),
                expected_group_id: CoordinationGroupId::from_array([1u8; 32]),
                expected_genesis_hash: [2u8; 32],
                expected_deployment_profile_code: shape::PROFILE_PROD,
            }
            .expected_deployment_profile_code(),
            Some(shape::PROFILE_PROD)
        );
        assert_eq!(
            CoordinationStart::Join {
                signed_join_permit: vec![1],
                expected_deployment_profile_code: shape::PROFILE_DEV_EVEN,
            }
            .expected_deployment_profile_code(),
            Some(shape::PROFILE_DEV_EVEN)
        );
    }

    #[test]
    fn a_start_mode_without_its_signed_bytes_is_refused() {
        let err = complete()
            .start_mode(CoordinationStart::Bootstrap {
                signed_genesis: Vec::new(),
                recovery_export_path: None,
            })
            .build()
            .expect_err("no genesis");
        assert!(err.to_string().contains("signed genesis"), "{err}");

        let err = complete()
            .start_mode(CoordinationStart::Join {
                signed_join_permit: Vec::new(),
                expected_deployment_profile_code: shape::PROFILE_PROD,
            })
            .build()
            .expect_err("no permit");
        assert!(err.to_string().contains("join permit"), "{err}");
    }

    /// An empty recovery path is refused rather than treated as absent: the two
    /// mean opposite things to §7.2, and silently reading one as the other
    /// would turn a recovery-bound bootstrap into an ordinary one.
    #[test]
    fn an_empty_recovery_export_path_is_not_the_same_as_omitting_it() {
        let err = complete()
            .start_mode(CoordinationStart::Bootstrap {
                signed_genesis: crate::coordination::model::authority::testing::valid_genesis(),
                recovery_export_path: Some(PathBuf::new()),
            })
            .build()
            .expect_err("an empty path");
        assert!(err.to_string().contains("recovery_export_path"), "{err}");
    }

    /// Each development profile needs its own acknowledgement, and neither
    /// acknowledgement admits the other. Production needs none.
    #[test]
    fn each_development_profile_needs_its_own_acknowledgement() {
        let none = DevelopmentAcknowledgements::default();
        assert!(none.permit(shape::PROFILE_PROD).is_ok());
        assert_eq!(
            none.permit(shape::PROFILE_DEV_SINGLE),
            Err(UnacknowledgedProfile {
                profile: shape::PROFILE_DEV_SINGLE
            })
        );
        assert_eq!(
            none.permit(shape::PROFILE_DEV_EVEN),
            Err(UnacknowledgedProfile {
                profile: shape::PROFILE_DEV_EVEN
            })
        );

        let single = CoordinationConfig::builder()
            .acknowledge_single_voter_development_group(true)
            .development;
        assert!(single.permit(shape::PROFILE_DEV_SINGLE).is_ok());
        assert!(
            single.permit(shape::PROFILE_DEV_EVEN).is_err(),
            "acknowledging one voter must not admit an even quorum"
        );

        let even = CoordinationConfig::builder()
            .acknowledge_even_quorum_development_group(true)
            .development;
        assert!(even.permit(shape::PROFILE_DEV_EVEN).is_ok());
        assert!(
            even.permit(shape::PROFILE_DEV_SINGLE).is_err(),
            "acknowledging an even quorum must not admit a single voter"
        );
    }

    /// An undeclared profile is never acknowledged, whatever the flags say.
    #[test]
    fn no_acknowledgement_admits_a_profile_this_build_does_not_implement() {
        let both = CoordinationConfig::builder()
            .acknowledge_single_voter_development_group(true)
            .acknowledge_even_quorum_development_group(true)
            .development;
        for profile in [0, 4, -1, i32::MAX] {
            assert_eq!(
                both.permit(profile),
                Err(UnacknowledgedProfile { profile }),
                "profile {profile} must not be acknowledged by any flag"
            );
        }
    }

    /// The acknowledgements default to off. Asserted rather than assumed: they
    /// gate a group that cannot survive a single loss, so landing on the
    /// permissive side by omission would be the whole failure.
    #[test]
    fn the_development_acknowledgements_default_to_off() {
        let config = complete().build().expect("build");
        assert!(!config.development().allows_single_voter());
        assert!(!config.development().allows_even_quorum());
        assert!(config
            .development()
            .permit(shape::PROFILE_DEV_SINGLE)
            .is_err());
        assert!(config
            .development()
            .permit(shape::PROFILE_DEV_EVEN)
            .is_err());
    }

    /// The debug rendering names the mode, never prints the admission source,
    /// and carries **no root key** — because the config no longer holds one.
    #[test]
    fn the_debug_rendering_names_the_mode_without_dumping_the_source() {
        let rendered = format!("{:?}", complete().build().expect("build"));
        assert!(rendered.contains("Bootstrap"), "{rendered}");
        assert!(rendered.contains("example.test"), "{rendered}");
        assert!(!rendered.contains("SignedAdmissionSet"), "{rendered}");
        assert!(
            !rendered.contains("authority_public_key"),
            "the ROOT key is the deployment's and is not a coordination field: {rendered}"
        );
    }
}
