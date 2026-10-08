//! Version-one durable receiver contract. Verify opaque bytes before parsing the envelope.
use super::types::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;

pub const MAX_WIRE_BYTES: usize = 1_420_000;
pub const MAX_RESPONSE_BYTES: usize = 4096;
pub const BATCH_ID_DOMAIN: &[u8] = b"aster-audit-batch-v1\0";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvelopeV1 {
    pub version: u8,
    pub outbox_id: String,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub kind: String,
    pub payload: serde_json::Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireEvent {
    pub event_id: String,
    pub envelope_base64: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRequest {
    pub version: u8,
    pub batch_id: String,
    pub events: Vec<WireEvent>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableAck {
    pub version: u8,
    pub batch_id: String,
    pub durable: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IntegrityResponse {
    pub version: u8,
    pub error: String,
}

/// SHA-256(domain || each ordered ID's u32 big-endian byte length || ID bytes).
pub fn batch_id<'a>(ids: impl IntoIterator<Item = &'a str>) -> String {
    let mut hash = Sha256::new();
    hash.update(BATCH_ID_DOMAIN);
    for id in ids {
        hash.update((id.len() as u32).to_be_bytes());
        hash.update(id.as_bytes());
    }
    hex::encode(hash.finalize())
}

pub(crate) fn id_for(identity: &str, sequence: u64, bytes: &[u8]) -> EventId {
    EventId(format!(
        "aster-audit-v1:{identity}:{sequence}:{}",
        hex::encode(Sha256::digest(bytes))
    ))
}
pub(crate) fn parse_id(id: &str) -> Result<(&str, u64, &str)> {
    if id.len() > 133 {
        return Err(integrity("invalid full event ID"));
    }
    let parts: Vec<_> = id.split(':').collect();
    if parts.len() != 4
        || parts[0] != "aster-audit-v1"
        || parts[1].len() != 32
        || parts[3].len() != 64
        || !parts[1]
            .bytes()
            .chain(parts[3].bytes())
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(integrity("invalid full event ID"));
    }
    let sequence = parts[2]
        .parse::<u64>()
        .map_err(|_| integrity("invalid full event ID"))?;
    if sequence == 0 || sequence.to_string() != parts[2] {
        return Err(integrity("noncanonical sequence"));
    }
    Ok((parts[1], sequence, parts[3]))
}
fn integrity(message: &str) -> AuditError {
    AuditError::new(AuditErrorKind::IntegrityConflict, message)
}

/// Receiver-side verification. The caller must bound outer JSON before parsing it.
/// Returns the original bytes, which the receiver must persist and compare on duplicates.
/// This verifies transport integrity and identity. The receiver must separately validate
/// its event-kind/payload acceptance schema before durable acceptance; producers require
/// a printable kind of 1..128 bytes and an object payload. This helper alone does not
/// implement that schema policy, durable deduplication, persistence, or ACK generation.
pub fn verify_wire_event(
    event: &WireEvent,
    max_event_bytes: usize,
) -> Result<(EnvelopeV1, Vec<u8>)> {
    if max_event_bytes == 0
        || max_event_bytes > MAX_EVENT_BYTES
        || event.envelope_base64.len() > max_event_bytes.div_ceil(3) * 4
    {
        return Err(AuditError::new(
            AuditErrorKind::EventTooLarge,
            "encoded envelope exceeds receiver limit",
        ));
    }
    let (identity, sequence, digest) = parse_id(&event.event_id)?;
    // Encoded input is bounded above. Allocate only that bound and check exact decoded size.
    let bytes = STANDARD
        .decode(&event.envelope_base64)
        .map_err(|_| integrity("malformed padded base64"))?;
    if bytes.len() > max_event_bytes {
        return Err(AuditError::new(
            AuditErrorKind::EventTooLarge,
            "decoded envelope exceeds receiver limit",
        ));
    }
    if hex::encode(Sha256::digest(&bytes)) != digest {
        return Err(integrity("envelope digest mismatch"));
    }
    let envelope: EnvelopeV1 =
        serde_json::from_slice(&bytes).map_err(|_| integrity("invalid versioned envelope"))?;
    if envelope.version != 1 || envelope.outbox_id != identity || envelope.sequence != sequence {
        return Err(integrity("envelope identity or sequence mismatch"));
    }
    Ok((envelope, bytes))
}

pub(crate) struct LimitedWriter {
    pub bytes: Vec<u8>,
    pub max: usize,
}
impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > self.max {
            return Err(std::io::Error::other("bounded serialization limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(crate) fn bounded_json(value: &impl Serialize, max: usize) -> Result<Vec<u8>> {
    let mut writer = LimitedWriter {
        bytes: Vec::with_capacity(max.min(4096)),
        max,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| {
        AuditError::new(
            AuditErrorKind::EventTooLarge,
            "serialized event exceeds limit",
        )
    })?;
    Ok(writer.bytes)
}

pub(crate) fn validate_event(event: &AuditEvent, max: usize) -> Result<usize> {
    if event.kind.is_empty()
        || event.kind.len() > 128
        || event.kind.chars().any(char::is_control)
        || !event.payload.is_object()
    {
        return Err(AuditError::new(
            AuditErrorKind::InvalidEvent,
            "kind must be 1..128 printable bytes and payload must be an object",
        ));
    }
    // Bound depth before serde's recursive serializer and avoid allocating a second event body.
    fn depth(value: &serde_json::Value, level: usize) -> bool {
        if level > 64 {
            return false;
        }
        match value {
            serde_json::Value::Array(a) => a.iter().all(|v| depth(v, level + 1)),
            serde_json::Value::Object(o) => o.values().all(|v| depth(v, level + 1)),
            _ => true,
        }
    }
    if !depth(&event.payload, 0) {
        return Err(AuditError::new(
            AuditErrorKind::InvalidEvent,
            "payload nesting exceeds 64 levels",
        ));
    }
    // Reserve fixed envelope framing, 32-byte identity and longest u64 sequence.
    let size = bounded_json(event, max)?.len().saturating_add(128);
    if size > max {
        return Err(AuditError::new(
            AuditErrorKind::EventTooLarge,
            "envelope exceeds event limit",
        ));
    }
    Ok(size)
}

impl Destination {
    pub(crate) fn validated_url(&self) -> Result<url::Url> {
        let bad = || {
            AuditError::new(
                AuditErrorKind::InvalidConfig,
                "invalid audit destination or credentials",
            )
        };
        if self.endpoint.len() > 4096
            || self.endpoint.trim() != self.endpoint
            || self
                .receiver_identity
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 128)
            || self.authorization.as_ref().is_some_and(|s| {
                s.len() > 4096 || reqwest::header::HeaderValue::from_str(s).is_err()
            })
        {
            return Err(bad());
        }
        let url = url::Url::parse(&self.endpoint).map_err(|_| bad())?;
        let authority = self
            .endpoint
            .split_once("://")
            .map(|(_, r)| r.split(['/', '?', '#']).next().unwrap_or(""))
            .unwrap_or("");
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || authority.contains('@')
        {
            return Err(bad());
        }
        Ok(url)
    }
    pub fn fingerprint(&self) -> Result<String> {
        let url = self.validated_url()?;
        let canonical = serde_json::json!({"version":1,"scheme":url.scheme(),"host":url.host_str().unwrap().to_ascii_lowercase(),
            "port":url.port_or_known_default().unwrap(),"path":url.path(),"query":url.query(),"receiver_identity":self.receiver_identity});
        Ok(hex::encode(Sha256::digest(
            serde_json::to_vec(&canonical).expect("JSON value"),
        )))
    }
}
