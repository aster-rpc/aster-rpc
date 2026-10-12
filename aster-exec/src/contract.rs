//! Versioned Exec v1 request and result types. IDs are not bearer authority.

/// Bound for worst-case JSON byte arrays and sealed request metadata.
pub const MAX_LOCAL_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

use serde::{Deserialize, Serialize};

pub const SERVICE_NAME: &str = "aster.exec.Exec";
pub const SERVICE_VERSION: u32 = 1;
pub const DEFAULT_TIMEOUT_MS: u64 = 30 * 60 * 1_000;
pub const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1_000;
pub const DEFAULT_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: u64 = 256 * 1024 * 1024;
pub const OUTPUT_RETENTION_MS: u64 = 24 * 60 * 60 * 1_000;
pub const REQUEST_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
pub const DEFAULT_CANCEL_GRACE_MS: u32 = 5_000;
pub const MAX_CANCEL_GRACE_MS: u32 = 60_000;
pub const MAX_PAGE_BYTES: u32 = 1024 * 1024;
pub const MIN_PAGE_BYTES: u32 = 4096;
pub const MAX_STDIN_BYTES: usize = 1024 * 1024;
pub const MAX_ENV_ENTRIES: usize = 64;
pub const MAX_ENV_BYTES: usize = 128 * 1024;
pub const MAX_ARGV_BYTES: usize = 256 * 1024;
pub const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TargetRefV1 {
    pub target_id: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetOsV1 {
    Linux,
    Macos,
    Windows,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum TargetEnvironmentV1 {
    Native,
    WslDistribution(String),
    ExecInsideOci,
    PerJobOci,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PathFlavorV1 {
    Posix,
    Windows,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AvailableProfileV1 {
    pub id: String,
    pub guarantees: GuaranteesV1,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TargetCapabilitiesV1 {
    pub target_id: String,
    pub node_id: String,
    pub os: TargetOsV1,
    pub architecture: String,
    pub environment: TargetEnvironmentV1,
    pub path_flavor: PathFlavorV1,
    pub account_alias: String,
    pub os_principal: String,
    pub profiles: Vec<AvailableProfileV1>,
    pub adapters: Vec<String>,
    pub max_execution_timeout_ms: u64,
    pub max_output_bytes: u64,
    pub max_stdin_bytes: u64,
    pub request_retention_ms: u64,
    pub output_retention_ms: u64,
    pub backend_ready: bool,
    pub unavailable_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecRequestV1 {
    pub request_id: String,
    pub target_id: String,
    pub profile: Option<String>,
    pub cwd: String,
    pub executable: String,
    pub argv: Vec<String>,
    pub environment_overrides: std::collections::BTreeMap<String, String>,
    pub stdin_bytes: Option<Vec<u8>>,
    pub execution_timeout_ms: Option<u64>,
    pub output_limit_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JobRefV1 {
    pub job_id: String,
    pub target_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutputRequestV1 {
    pub job: JobRefV1,
    pub cursor: u64,
    pub max_bytes: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WatchRequestV1 {
    pub job: JobRefV1,
    pub cursor: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CancelRequestV1 {
    pub job: JobRefV1,
    pub grace_ms: Option<u32>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStateV1 {
    Accepted,
    Starting,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Lost,
}

impl JobStateV1 {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::TimedOut | Self::Lost
        )
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Accepted, Self::Starting | Self::Failed | Self::Lost)
                | (Self::Starting, Self::Running | Self::Failed | Self::Lost)
                | (
                    Self::Running,
                    Self::Succeeded | Self::Failed | Self::Cancelled | Self::TimedOut | Self::Lost
                )
        )
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputStreamV1 {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutputChunkV1 {
    pub sequence: u64,
    pub stream: OutputStreamV1,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutputPageV1 {
    pub chunks: Vec<OutputChunkV1>,
    pub next_cursor: u64,
    pub truncated: bool,
    pub eof: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CancelReceiptV1 {
    pub job: JobRefV1,
    pub accepted_at_ms: u64,
    pub already_terminal: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum JobEventV1 {
    OutputChunk(OutputChunkV1),
    StateChanged(Box<JobStatusV1>),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkGuaranteeV1 {
    Inherited,
    Denied,
    ProxyOnly,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemGuaranteeV1 {
    Inherited,
    Allowlist,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DescendantScopeV1 {
    Cgroup,
    JobObject,
    Container,
    ProcessGroupSubreaperBestEffort,
    ProcessGroupBestEffort,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartSurvivalV1 {
    None,
    ClientDisconnect,
    ApiRestart,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimitsV1 {
    pub cpu_millis_per_second: Option<u32>,
    pub memory_bytes: Option<u64>,
    pub processes: Option<u32>,
    pub file_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GuaranteesV1 {
    pub profile: String,
    pub provider: String,
    pub network: NetworkGuaranteeV1,
    pub proxy_id: Option<String>,
    pub filesystem: FilesystemGuaranteeV1,
    pub filesystem_policy_id: Option<String>,
    pub execution_identity: String,
    pub resource_limits: ResourceLimitsV1,
    pub descendant_scope: DescendantScopeV1,
    pub restart_survival: RestartSurvivalV1,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JobStatusV1 {
    pub job: JobRefV1,
    pub state: JobStateV1,
    pub caller_id: String,
    pub resolved_target: String,
    pub execution_identity: String,
    pub accepted_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub reason_code: Option<String>,
    pub output_cursor: u64,
    pub guarantees: GuaranteesV1,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecErrorCodeV1 {
    InvalidArgument,
    Unauthenticated,
    PermissionDenied,
    PolicyUnavailable,
    TargetUnavailable,
    ProfileUnavailable,
    UnsupportedOperation,
    ExecutableNotFound,
    CwdInvalid,
    ResourceExhausted,
    DeadlineExceeded,
    RequestConflict,
    JobNotFound,
    OutputExpired,
    AuditUnavailable,
    SupervisorUnavailable,
    BackendFailure,
    Internal,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecErrorV1 {
    pub code: ExecErrorCodeV1,
    pub safe_message: String,
    pub request_id: Option<String>,
    pub job_id: Option<String>,
    pub retryable: bool,
    pub retry_after_ms: Option<u64>,
}

impl ExecErrorV1 {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: ExecErrorCodeV1::InvalidArgument,
            safe_message: message.into(),
            request_id: None,
            job_id: None,
            retryable: false,
            retry_after_ms: None,
        }
    }
}

impl std::fmt::Display for ExecErrorV1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.safe_message)
    }
}
impl std::error::Error for ExecErrorV1 {}

impl ExecRequestV1 {
    pub fn validate(&self) -> Result<(), ExecErrorV1> {
        if uuid::Uuid::parse_str(&self.request_id).is_err()
            || self.target_id.is_empty()
            || self.executable.is_empty()
            || self.cwd.is_empty()
        {
            return Err(ExecErrorV1::invalid("missing or invalid required field"));
        }
        let timeout = self.execution_timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if !(1_000..=MAX_TIMEOUT_MS).contains(&timeout)
            || self.output_limit_bytes.unwrap_or(DEFAULT_OUTPUT_BYTES) > MAX_OUTPUT_BYTES
            || self
                .stdin_bytes
                .as_ref()
                .is_some_and(|v| v.len() > MAX_STDIN_BYTES)
            || self.environment_overrides.len() > MAX_ENV_ENTRIES
        {
            return Err(ExecErrorV1::invalid("request exceeds v1 limits"));
        }
        let env_bytes: usize = self
            .environment_overrides
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum();
        let argv_bytes: usize =
            self.executable.len() + self.argv.iter().map(String::len).sum::<usize>();
        let request_bytes = self.request_id.len()
            + self.target_id.len()
            + self.profile.as_ref().map_or(0, String::len)
            + self.cwd.len()
            + env_bytes
            + argv_bytes
            + self.stdin_bytes.as_ref().map_or(0, Vec::len);
        if env_bytes > MAX_ENV_BYTES
            || argv_bytes > MAX_ARGV_BYTES
            || request_bytes > MAX_REQUEST_BYTES
            || self.environment_overrides.iter().any(|(key, value)| {
                key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0')
            })
            || self.executable.contains('\0')
            || self.cwd.contains('\0')
            || self.argv.iter().any(|arg| arg.contains('\0'))
        {
            return Err(ExecErrorV1::invalid("invalid or oversized request fields"));
        }
        Ok(())
    }

    pub fn canonical_digest(&self, resolved_profile: &str) -> Result<[u8; 32], ExecErrorV1> {
        self.validate()?;
        let mut canonical = self.clone();
        canonical.profile = Some(resolved_profile.to_owned());
        let bytes = serde_json::to_vec(&canonical)
            .map_err(|_| ExecErrorV1::invalid("invalid request encoding"))?;
        Ok(*blake3::hash(&bytes).as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maximum_byte_arrays_fit_local_protocol_bound() {
        let bytes = vec![255u8; MAX_STDIN_BYTES];
        let request = ExecRequestV1 {
            request_id: uuid::Uuid::new_v4().to_string(),
            target_id: "target".into(),
            profile: None,
            cwd: "/tmp".into(),
            executable: "/usr/bin/cat".into(),
            argv: vec![],
            environment_overrides: Default::default(),
            stdin_bytes: Some(bytes.clone()),
            execution_timeout_ms: None,
            output_limit_bytes: None,
        };
        request.validate().unwrap();
        let encoded = serde_json::to_vec(&request).unwrap();
        assert!(encoded.len() < MAX_LOCAL_MESSAGE_BYTES);
        assert_eq!(
            serde_json::from_slice::<ExecRequestV1>(&encoded)
                .unwrap()
                .stdin_bytes
                .unwrap(),
            bytes
        );
        let page = OutputPageV1 {
            chunks: (0..256)
                .map(|sequence| OutputChunkV1 {
                    sequence,
                    stream: OutputStreamV1::Stdout,
                    bytes: vec![255; 4096],
                })
                .collect(),
            next_cursor: 256,
            truncated: false,
            eof: true,
        };
        let encoded = serde_json::to_vec(&page).unwrap();
        assert!(encoded.len() < MAX_LOCAL_MESSAGE_BYTES);
        let decoded: OutputPageV1 = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            decoded
                .chunks
                .iter()
                .map(|chunk| chunk.bytes.len())
                .sum::<usize>(),
            MAX_PAGE_BYTES as usize
        );
        assert!(
            decoded
                .chunks
                .iter()
                .all(|chunk| chunk.bytes.iter().all(|byte| *byte == 255))
        );
    }

    #[test]
    fn canonical_digest_resolves_default_profile_and_sorts_environment() {
        let mut request = ExecRequestV1 {
            request_id: uuid::Uuid::new_v4().to_string(),
            target_id: "node".into(),
            profile: None,
            cwd: "/tmp".into(),
            executable: "printf".into(),
            argv: vec!["x".into()],
            environment_overrides: [("Z".into(), "z".into()), ("A".into(), "a".into())].into(),
            stdin_bytes: None,
            execution_timeout_ms: None,
            output_limit_bytes: None,
        };
        let first = request.canonical_digest("open").unwrap();
        request.profile = Some("open".into());
        assert_eq!(first, request.canonical_digest("open").unwrap());
        request.argv.push("different".into());
        assert_ne!(first, request.canonical_digest("open").unwrap());
    }
}
