//! Intersection of an exact-root Exec grant and the owner-installed local ceiling.

use crate::contract::{
    DEFAULT_OUTPUT_BYTES, DEFAULT_TIMEOUT_MS, ExecErrorCodeV1, ExecErrorV1, ExecRequestV1,
};
use crate::policy::{ExecGrantV1, ExecOperationV1, GrantStatusV1};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalCeilingV1 {
    pub version: u32,
    pub target_id: String,
    pub account_alias: String,
    pub default_profile: String,
    pub enabled_profiles: BTreeSet<String>,
    pub enabled_operations: BTreeSet<ExecOperationV1>,
    pub cwd_roots: Vec<PathBuf>,
    pub max_timeout_ms: u64,
    pub max_output_bytes: u64,
    pub denied_callers: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedSubmit {
    pub profile: String,
    pub canonical_cwd: PathBuf,
    pub timeout_ms: u64,
    pub output_limit_bytes: u64,
    pub account_alias: String,
}

fn denied(message: &'static str) -> ExecErrorV1 {
    ExecErrorV1 {
        code: ExecErrorCodeV1::PermissionDenied,
        safe_message: message.into(),
        request_id: None,
        job_id: None,
        retryable: false,
        retry_after_ms: None,
    }
}

fn under_any_root(path: &Path, roots: impl IntoIterator<Item = PathBuf>) -> bool {
    roots.into_iter().any(|root| path.starts_with(root))
}

impl LocalCeilingV1 {
    pub fn authorize_submit(
        &self,
        caller_id: &str,
        grant: &ExecGrantV1,
        request: &ExecRequestV1,
    ) -> Result<AuthorizedSubmit, ExecErrorV1> {
        request.validate()?;
        let profile = request.profile.as_deref().unwrap_or(&self.default_profile);
        let timeout_ms = request.execution_timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let output_limit_bytes = request.output_limit_bytes.unwrap_or(DEFAULT_OUTPUT_BYTES);
        if self.version != 1
            || request.target_id != self.target_id
            || grant.target != self.target_id
            || grant.caller != caller_id
            || grant.status != GrantStatusV1::Active
            || grant.account_alias != self.account_alias
            || self.denied_callers.contains(caller_id)
            || !self.enabled_operations.contains(&ExecOperationV1::Submit)
            || !grant.operations.contains(&ExecOperationV1::Submit)
            || !self.enabled_profiles.contains(profile)
            || !grant.profiles.iter().any(|allowed| allowed == profile)
            || timeout_ms > self.max_timeout_ms
            || timeout_ms > grant.max_timeout_ms
            || output_limit_bytes > self.max_output_bytes
            || output_limit_bytes > grant.max_output_bytes
        {
            return Err(denied(
                "Exec submission is outside the active grant or local ceiling",
            ));
        }
        let cwd = fs::canonicalize(&request.cwd)
            .map_err(|_| ExecErrorV1::invalid("working directory is unavailable"))?;
        if !cwd.is_dir() {
            return Err(ExecErrorV1::invalid("working directory is not a directory"));
        }
        if self.cwd_roots.iter().any(|root| !root.is_absolute())
            || grant
                .cwd_roots
                .iter()
                .any(|root| !Path::new(root).is_absolute())
        {
            return Err(denied("working-directory policy contains a relative root"));
        }
        // The owner file and grant must both name existing absolute roots.
        // Canonicalization resolves symlinks before comparing path components.
        let local_roots = self
            .cwd_roots
            .iter()
            .map(fs::canonicalize)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| denied("local working-directory policy is unavailable"))?;
        let grant_roots = grant
            .cwd_roots
            .iter()
            .map(fs::canonicalize)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| denied("grant working-directory policy is unavailable"))?;
        if !under_any_root(&cwd, local_roots) || !under_any_root(&cwd, grant_roots) {
            return Err(denied("working directory is outside the authorized roots"));
        }
        Ok(AuthorizedSubmit {
            profile: profile.into(),
            canonical_cwd: cwd,
            timeout_ms,
            output_limit_bytes,
            account_alias: self.account_alias.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::GrantStatusV1;

    #[test]
    fn owner_ceiling_and_root_grant_must_both_allow_submit() {
        let root = tempfile::tempdir().unwrap();
        let allowed = root.path().join("allowed");
        let other = root.path().join("other");
        fs::create_dir(&allowed).unwrap();
        fs::create_dir(&other).unwrap();
        let caller = "11".repeat(32);
        let target = "22".repeat(32);
        let grant = ExecGrantV1 {
            version: 1,
            issuer: "33".repeat(32),
            audience: "fleet".into(),
            caller: caller.clone(),
            target: target.clone(),
            revision: 1,
            issued_at: 1,
            not_before: None,
            expires_at: None,
            status: GrantStatusV1::Active,
            account_alias: "builder".into(),
            profiles: vec!["open".into()],
            operations: vec![ExecOperationV1::Submit],
            cwd_roots: vec![allowed.to_string_lossy().into()],
            max_timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: DEFAULT_OUTPUT_BYTES,
            cancel_on_revocation: true,
            reauthorizes_revision: None,
        };
        let ceiling = LocalCeilingV1 {
            version: 1,
            target_id: target.clone(),
            account_alias: "builder".into(),
            default_profile: "open".into(),
            enabled_profiles: ["open".into()].into(),
            enabled_operations: [ExecOperationV1::Submit].into(),
            cwd_roots: vec![root.path().into()],
            max_timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: DEFAULT_OUTPUT_BYTES,
            denied_callers: BTreeSet::new(),
        };
        let mut request = ExecRequestV1 {
            request_id: uuid::Uuid::new_v4().to_string(),
            target_id: target.clone(),
            profile: None,
            cwd: allowed.to_string_lossy().into(),
            executable: "/bin/true".into(),
            argv: vec![],
            environment_overrides: Default::default(),
            stdin_bytes: None,
            execution_timeout_ms: None,
            output_limit_bytes: None,
        };
        assert!(ceiling.authorize_submit(&caller, &grant, &request).is_ok());
        request.cwd = other.to_string_lossy().into();
        assert!(ceiling.authorize_submit(&caller, &grant, &request).is_err());
        request.cwd = allowed.to_string_lossy().into();
        request.profile = Some("offline".into());
        assert!(ceiling.authorize_submit(&caller, &grant, &request).is_err());
    }
}
