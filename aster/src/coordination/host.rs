//! Host-side voter configuration (§3.3).
//!
//! Slice 0 carries only what Slice 0 can validate: the persistent storage
//! directory, and -- enforced by
//! [`builtin_coordination`](crate::rpc::AsterServerBuilder::builtin_coordination)
//! rather than by this type -- a restart-stable endpoint identity.
//!
//! ## What is deliberately absent
//!
//! §3.3 also requires a pinned `authority_public_key`, an `authority_domain`, a
//! `CoordinationStart` mode, a `VerifiedAdmissionSource`, and an explicit
//! acknowledgement for a development-only one- or two-voter group. None of them
//! are here yet: every one is verified against a signed document or an
//! admission fact, and Slice 3 owns that parsing and its canonical vectors. A
//! field accepted now and checked later is worse than an absent field, because
//! it reads as configured while nothing enforces it.
//!
//! The consequence is that this type will gain required fields. It is
//! constructed only through [`CoordinationConfig::builder`], so adding one is a
//! compile error at the call site rather than a silently defaulted value.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Configuration for hosting a coordination voter.
///
/// ```no_run
/// # fn run() -> aster::Result<()> {
/// use aster::coordination::host::CoordinationConfig;
///
/// let config = CoordinationConfig::builder()
///     .storage_dir("/var/lib/my-app/aster-coordination")
///     .build()?;
/// # let _ = config; Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct CoordinationConfig {
    storage_dir: PathBuf,
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
}

/// Builder for a [`CoordinationConfig`].
#[derive(Default, Clone, Debug)]
pub struct CoordinationConfigBuilder {
    storage_dir: Option<PathBuf>,
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

    /// Validate and produce the configuration.
    pub fn build(self) -> Result<CoordinationConfig> {
        let storage_dir = self.storage_dir.ok_or_else(|| {
            Error::InvalidArgument(
                "CoordinationConfig requires a storage_dir: a voter's log and state machine \
                 must survive restart"
                    .into(),
            )
        })?;
        if storage_dir.as_os_str().is_empty() {
            return Err(Error::InvalidArgument(
                "CoordinationConfig storage_dir must not be empty".into(),
            ));
        }
        Ok(CoordinationConfig { storage_dir })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_storage_dir_is_required() {
        let err = CoordinationConfig::builder()
            .build()
            .expect_err("a voter with nowhere to store its log must not build");
        assert!(
            err.to_string().contains("storage_dir"),
            "the error must name the missing field, got: {err}"
        );
    }

    #[test]
    fn an_empty_storage_dir_is_rejected() {
        let err = CoordinationConfig::builder()
            .storage_dir("")
            .build()
            .expect_err("an empty path is not a directory");
        assert!(err.to_string().contains("storage_dir"), "got: {err}");
    }

    #[test]
    fn a_supplied_storage_dir_survives_the_builder() {
        let config = CoordinationConfig::builder()
            .storage_dir("/var/lib/example/coordination")
            .build()
            .expect("build");
        assert_eq!(
            config.storage_dir(),
            Path::new("/var/lib/example/coordination")
        );
    }

    /// A relative path is the "application-resolved" case §3.3 allows; the
    /// builder must not quietly reject or rewrite it.
    #[test]
    fn a_relative_storage_dir_is_accepted_unchanged() {
        let config = CoordinationConfig::builder()
            .storage_dir("state/coordination")
            .build()
            .expect("build");
        assert_eq!(config.storage_dir(), Path::new("state/coordination"));
    }
}
