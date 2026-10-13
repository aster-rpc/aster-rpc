//! Versioned Exec v1 request and result types. IDs are not bearer authority.
//!
//! Byte fields (`stdin_bytes`, output chunk `bytes`) are standard base64
//! strings in JSON.

use serde::{Deserialize, Serialize};

/// Bound for one local helper or supervisor message: an encoded request
/// frame plus sealed launch metadata.
pub const MAX_LOCAL_MESSAGE_BYTES: usize = MAX_REQUEST_FRAME_BYTES + 256 * 1024;

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
/// Bound on a request's raw content: identifiers, paths, argv, environment
/// and stdin.
pub const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
/// Bound on an encoded request frame. It covers base64 stdin and JSON string
/// escaping of up to six bytes per byte of argv and environment.
pub const MAX_REQUEST_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// Output paging charges each chunk this much beyond its bytes, so a page's
/// encoded size stays within 4/3 of its byte budget however small its chunks.
pub const OUTPUT_CHUNK_OVERHEAD_BYTES: usize = 96;
/// Bound on an on-behalf-of chain's text; equals Aster's delegation
/// `MAX_CHAIN_BYTES`, which verification enforces again.
pub const MAX_ON_BEHALF_OF_CHAIN_BYTES: usize = 24 * 1024;
/// Domain separator for [`ExecRequestV1::on_behalf_digest`].
pub const REQUEST_DIGEST_DOMAIN: &[u8] = b"aster.exec.request.v1\0";

/// Serde adapters that carry bytes as standard base64 strings rather than
/// JSON number arrays, which cost up to four bytes per byte.
pub mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        STANDARD.decode(text).map_err(D::Error::custom)
    }

    pub mod option {
        use super::*;

        pub fn serialize<S: Serializer>(
            bytes: &Option<Vec<u8>>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match bytes {
                Some(bytes) => super::serialize(bytes, serializer),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Vec<u8>>, D::Error> {
            Option::<String>::deserialize(deserializer)?
                .map(|text| STANDARD.decode(text).map_err(D::Error::custom))
                .transpose()
        }
    }
}

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
    #[serde(default, with = "base64_bytes::option")]
    pub stdin_bytes: Option<Vec<u8>>,
    pub execution_timeout_ms: Option<u64>,
    pub output_limit_bytes: Option<u64>,
    /// Submit for another principal under its Exec grant, narrowed by an
    /// action chain the principal (or its delegate) issued to this caller.
    /// Absent means the caller submits under its own grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<OnBehalfOfV1>,
}

/// The principal whose grant applies and the action chain that lets the
/// authenticated caller use it. `principal` only selects which grant to
/// read; the chain must be anchored at that key or the request is denied.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OnBehalfOfV1 {
    /// Principal node identity: 64 lowercase hex characters.
    pub principal: String,
    /// Action chain text, `aster-chain-v1~<leaf>~...~<top>`: the leaf names
    /// the caller and the top link is issued by `principal`.
    pub chain: String,
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
    #[serde(with = "base64_bytes")]
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
    /// The authenticated submitter, who may observe and cancel the job.
    pub caller_id: String,
    /// For an on-behalf-of submission, the principal whose grant the job runs
    /// under. Its revocation cancels the job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
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
        if let Some(on_behalf) = &self.on_behalf_of
            && (on_behalf.principal.len() != 64
                || !on_behalf
                    .principal
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                || on_behalf.chain.is_empty()
                || on_behalf.chain.len() > MAX_ON_BEHALF_OF_CHAIN_BYTES)
        {
            return Err(ExecErrorV1::invalid("invalid on-behalf-of authority"));
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
            + self.stdin_bytes.as_ref().map_or(0, Vec::len)
            + self.on_behalf_of.as_ref().map_or(0, |on_behalf| {
                on_behalf.principal.len() + on_behalf.chain.len()
            });
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

    /// Submit for `principal` under the action chain it (or its delegate)
    /// issued to the submitting node.
    pub fn on_behalf(mut self, principal: impl Into<String>, chain: impl Into<String>) -> Self {
        self.on_behalf_of = Some(OnBehalfOfV1 {
            principal: principal.into(),
            chain: chain.into(),
        });
        self
    }

    /// The digest an action link's `request` claim binds, so a principal can
    /// authorize exactly one request: BLAKE3 over `"aster.exec.request.v1\0"`
    /// followed by the compact `serde_json` encoding of the request exactly as
    /// the client sends it, without `on_behalf_of`. That encoding has fields
    /// in declaration order, `null` for absent options, sorted environment
    /// keys and stdin as standard base64. The server computes it before
    /// resolving defaults, so a client digests the same field values it
    /// submits, including `request_id`.
    pub fn on_behalf_digest(&self) -> Result<[u8; 32], ExecErrorV1> {
        let mut unsigned = self.clone();
        unsigned.on_behalf_of = None;
        let bytes = serde_json::to_vec(&unsigned)
            .map_err(|_| ExecErrorV1::invalid("invalid request encoding"))?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(REQUEST_DIGEST_DOMAIN);
        hasher.update(&bytes);
        Ok(*hasher.finalize().as_bytes())
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
    fn worst_case_request_fits_its_frame_bound() {
        // Control characters escape to six bytes in JSON; 0xFF stdin is the
        // longest base64.
        let escaped = |len: usize| "\u{1}".repeat(len);
        let mut environment = std::collections::BTreeMap::new();
        for index in 0..MAX_ENV_ENTRIES {
            let key = format!("K{index:02}");
            let value = escaped(MAX_ENV_BYTES / MAX_ENV_ENTRIES - key.len());
            environment.insert(key, value);
        }
        let executable = "/usr/bin/cat".to_string();
        let bytes = vec![255u8; MAX_STDIN_BYTES];
        let request = ExecRequestV1 {
            request_id: uuid::Uuid::new_v4().to_string(),
            target_id: "22".repeat(32),
            profile: Some("open".into()),
            cwd: "/tmp".into(),
            argv: vec![escaped(MAX_ARGV_BYTES - executable.len())],
            executable,
            environment_overrides: environment,
            stdin_bytes: Some(bytes.clone()),
            execution_timeout_ms: Some(MAX_TIMEOUT_MS),
            output_limit_bytes: Some(MAX_OUTPUT_BYTES),
            on_behalf_of: Some(OnBehalfOfV1 {
                principal: "11".repeat(32),
                chain: "x".repeat(MAX_ON_BEHALF_OF_CHAIN_BYTES),
            }),
        };
        request.validate().unwrap();
        let encoded = serde_json::to_vec(&request).unwrap();
        assert!(
            encoded.len() <= MAX_REQUEST_FRAME_BYTES,
            "{} > {MAX_REQUEST_FRAME_BYTES}",
            encoded.len()
        );
        assert!(encoded.len() < MAX_LOCAL_MESSAGE_BYTES);
        let decoded: ExecRequestV1 = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.stdin_bytes.unwrap(), bytes);
    }

    #[test]
    fn output_pages_encode_within_their_byte_budget() {
        // Paging charges OUTPUT_CHUNK_OVERHEAD_BYTES per chunk; even one-byte
        // chunks keep the encoded page within 4/3 of the budget.
        let budget = MAX_PAGE_BYTES as usize;
        for size in [1usize, 3, 4096] {
            let count = budget / (size + OUTPUT_CHUNK_OVERHEAD_BYTES);
            let page = OutputPageV1 {
                chunks: (0..count as u64)
                    .map(|offset| OutputChunkV1 {
                        sequence: u64::MAX - offset,
                        stream: OutputStreamV1::Stderr,
                        bytes: vec![255; size],
                    })
                    .collect(),
                next_cursor: u64::MAX,
                truncated: true,
                eof: false,
            };
            let encoded = serde_json::to_vec(&page).unwrap();
            assert!(
                encoded.len() <= budget * 4 / 3 + 1024,
                "{size}: {}",
                encoded.len()
            );
            assert_eq!(
                serde_json::from_slice::<OutputPageV1>(&encoded).unwrap(),
                page
            );
        }
    }

    #[test]
    fn byte_fields_are_base64_and_stdin_may_be_omitted() {
        let chunk: OutputChunkV1 =
            serde_json::from_str(r#"{"sequence":0,"stream":"stdout","bytes":"aGk="}"#).unwrap();
        assert_eq!(chunk.bytes, b"hi");
        assert!(
            serde_json::from_str::<OutputChunkV1>(
                r#"{"sequence":0,"stream":"stdout","bytes":[104]}"#
            )
            .is_err()
        );
        let request = serde_json::json!({
            "request_id": uuid::Uuid::new_v4().to_string(),
            "target_id": "t",
            "profile": null,
            "cwd": "/tmp",
            "executable": "/bin/true",
            "argv": [],
            "environment_overrides": {},
            "execution_timeout_ms": null,
            "output_limit_bytes": null,
        });
        let request: ExecRequestV1 = serde_json::from_value(request).unwrap();
        assert!(request.stdin_bytes.is_none());
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
            on_behalf_of: None,
        };
        let first = request.canonical_digest("open").unwrap();
        request.profile = Some("open".into());
        assert_eq!(first, request.canonical_digest("open").unwrap());
        request.argv.push("different".into());
        assert_ne!(first, request.canonical_digest("open").unwrap());
    }

    #[test]
    fn on_behalf_digest_ignores_the_chain_and_binds_every_other_field() {
        let mut request = ExecRequestV1 {
            request_id: "6f1f1e1e-0000-4000-8000-000000000000".into(),
            target_id: "22".repeat(32),
            profile: None,
            cwd: "/tmp".into(),
            executable: "/bin/true".into(),
            argv: vec![],
            environment_overrides: Default::default(),
            stdin_bytes: None,
            execution_timeout_ms: None,
            output_limit_bytes: None,
            on_behalf_of: None,
        };
        let plain = request.on_behalf_digest().unwrap();
        // The documented canonical form, written out independently.
        let mut expected = blake3::Hasher::new();
        expected.update(b"aster.exec.request.v1\0");
        expected.update(
            br#"{"request_id":"6f1f1e1e-0000-4000-8000-000000000000","target_id":"2222222222222222222222222222222222222222222222222222222222222222","profile":null,"cwd":"/tmp","executable":"/bin/true","argv":[],"environment_overrides":{},"stdin_bytes":null,"execution_timeout_ms":null,"output_limit_bytes":null}"#,
        );
        assert_eq!(plain, *expected.finalize().as_bytes());
        request.on_behalf_of = Some(OnBehalfOfV1 {
            principal: "11".repeat(32),
            chain: "aster-chain-v1~x".into(),
        });
        assert_eq!(request.on_behalf_digest().unwrap(), plain);
        request.validate().unwrap();
        request.argv.push("changed".into());
        assert_ne!(request.on_behalf_digest().unwrap(), plain);
        for principal in ["11".repeat(31), "AB".repeat(32)] {
            let mut invalid = request.clone();
            invalid.on_behalf_of.as_mut().unwrap().principal = principal;
            assert!(invalid.validate().is_err());
        }
        let mut oversized = request.clone();
        oversized.on_behalf_of.as_mut().unwrap().chain =
            "x".repeat(MAX_ON_BEHALF_OF_CHAIN_BYTES + 1);
        assert!(oversized.validate().is_err());
    }
}
