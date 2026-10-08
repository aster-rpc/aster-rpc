use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf};

pub const MAX_EVENT_BYTES: usize = 1024 * 1024;
pub const DEFAULT_EVENT_BYTES: usize = 64 * 1024;
pub const DEFAULT_RETAINED_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_BATCH_EVENTS: usize = 64;
pub const MAX_BATCH_BYTES: usize = 1024 * 1024;
pub(crate) const QUEUE_EVENTS: usize = 1024;
pub(crate) const QUEUE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const CONTROL_CAPACITY: usize = 64;

/// Stable language-neutral error codes. Messages never contain event bodies or credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditErrorKind {
    InvalidConfig,
    InvalidEvent,
    EventTooLarge,
    StorageFull,
    Storage,
    CommitUncertain,
    QueueOverloaded,
    Maintenance,
    Closed,
    WorkerFailed,
    Timeout,
    DeliveryStalled,
    Disposition,
    NotFound,
    DestinationMismatch,
    IntegrityConflict,
    StaleDelivery,
}

#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
#[error("{code:?}: {message}")]
pub struct AuditError {
    pub code: AuditErrorKind,
    pub message: String,
}
impl AuditError {
    pub fn kind(&self) -> AuditErrorKind {
        self.code
    }
    pub(crate) fn new(code: AuditErrorKind, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub(crate) fn storage(_: impl fmt::Display) -> Self {
        Self::new(
            AuditErrorKind::Storage,
            "local audit storage operation failed",
        )
    }
    pub(crate) fn worker() -> Self {
        Self::new(
            AuditErrorKind::WorkerFailed,
            "audit worker stopped before completing the operation",
        )
    }
}
pub type Result<T> = std::result::Result<T, AuditError>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub endpoint: String,
    #[serde(default)]
    pub receiver_identity: Option<String>,
    #[serde(default)]
    pub authorization: Option<String>,
}
impl fmt::Debug for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Destination").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboxConfig {
    pub directory: PathBuf,
    pub destination: Destination,
    #[serde(default = "default_retained")]
    pub max_retained_bytes: u64,
    #[serde(default = "default_event")]
    pub max_event_bytes: usize,
}
fn default_retained() -> u64 {
    DEFAULT_RETAINED_BYTES
}
fn default_event() -> usize {
    DEFAULT_EVENT_BYTES
}
impl OutboxConfig {
    pub fn new(directory: impl Into<PathBuf>, destination: Destination) -> Self {
        Self {
            directory: directory.into(),
            destination,
            max_retained_bytes: DEFAULT_RETAINED_BYTES,
            max_event_bytes: DEFAULT_EVENT_BYTES,
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if self.directory.as_os_str().is_empty()
            || self.max_event_bytes < 256
            || self.max_event_bytes > MAX_EVENT_BYTES
            || self.max_retained_bytes < 256
        {
            return Err(AuditError::new(
                AuditErrorKind::InvalidConfig,
                "invalid directory or storage/event limits",
            ));
        }
        self.destination.fingerprint()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditEvent {
    pub timestamp_ms: u64,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// The full content-bound ID, including identity and sequence. Never deduplicate by sequence alone.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub(crate) String);
impl EventId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryErrorKind {
    Transport,
    Protocol,
    Credentials,
    Rejected,
    Integrity,
    Configuration,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryError {
    pub kind: DeliveryErrorKind,
    pub http_status: Option<u16>,
    pub sequence: u64,
    pub immediate_flush_failure: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxStatus {
    pub outbox_id: String,
    pub committed_sequence: u64,
    pub pending_records: u64,
    pub pending_bytes: u64,
    pub dead_letter_records: u64,
    pub dead_letter_bytes: u64,
    pub disposition_receipts: u64,
    pub disposition_bytes: u64,
    pub retained_bytes: u64,
    pub max_retained_bytes: u64,
    pub queued_records: usize,
    pub queued_bytes: usize,
    pub oldest_age_ms: Option<u64>,
    pub accepting: bool,
    pub ready: bool,
    pub delivery_state: String,
    pub last_delivery_error: Option<DeliveryError>,
    pub storage_worker_alive: bool,
    /// This open's physical integrity check repaired a pre-existing database.
    pub storage_recovered: bool,
    pub delivery_worker_alive: bool,
    pub maintenance: bool,
    pub maintenance_duration_ms: Option<u64>,
    pub closed: bool,
    pub durable_commits: u64,
    pub last_group_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispositionReceipt {
    pub event_id: EventId,
    pub action: String,
    pub timestamp_ms: u64,
    pub reason: String,
    pub operator_reference: String,
    pub destination_fingerprint: String,
    pub rejection: Option<DeliveryError>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineExport {
    pub event_id: EventId,
    pub envelope_base64: String,
    pub pending: bool,
    pub receipts: Vec<DispositionReceipt>,
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
