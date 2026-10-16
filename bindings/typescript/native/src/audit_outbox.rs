//! Thin asynchronous bridge to the shared Rust outbox.
use aster::audit_outbox::{
    AuditError, AuditEvent, AuditOutbox, Destination, OutboxConfig, MAX_EVENT_BYTES,
};
use napi::{Error, Result, Status};
use napi_derive::napi;
use serde_json::Value;
use std::time::Duration;

/// The longest acknowledgement wait one call accepts (the C ABI's
/// `ASTER_AUDIT_OUTBOX_MAX_ACK_WAIT_MS`).
const MAX_ACK_WAIT_MS: f64 = 3_600_000.0;

fn native_error(e: AuditError) -> Error {
    let kind = serde_json::to_value(e.kind()).expect("error kind serializes");
    Error::new(
        Status::GenericFailure,
        format!(
            "[audit_outbox:{}] {}",
            kind.as_str().expect("error kind is string"),
            e.message
        ),
    )
}

fn invalid(_: impl std::fmt::Display) -> Error {
    Error::new(
        Status::InvalidArg,
        "[audit_outbox:invalid_event] invalid audit argument",
    )
}

fn invalid_config(_: impl std::fmt::Display) -> Error {
    Error::new(
        Status::InvalidArg,
        "[audit_outbox:invalid_config] invalid audit configuration",
    )
}

// Decimal strings keep u64 values exact across JavaScript's JSON boundary.
// Only bridge metadata is converted; callers' arbitrary payloads are untouched.
fn integer_in(value: &mut Value, field: &str) -> Result<()> {
    if let Some(v) = value.get_mut(field) {
        if let Some(text) = v.as_str() {
            let n: u64 = text.parse().map_err(invalid)?;
            *v = Value::from(n);
        }
    }
    Ok(())
}

fn integers_out(value: &mut Value) {
    match value {
        Value::Number(n) if n.is_u64() || n.is_i64() => *value = Value::String(n.to_string()),
        Value::Array(items) => items.iter_mut().for_each(integers_out),
        Value::Object(items) => items.values_mut().for_each(integers_out),
        _ => {}
    }
}

#[napi]
pub struct NativeAuditOutbox {
    inner: AuditOutbox,
}

impl NativeAuditOutbox {
    /// The outbox, for a server that records into it (`DispatchServer.start`).
    pub(crate) fn outbox(&self) -> AuditOutbox {
        self.inner.clone()
    }
}

#[napi]
impl NativeAuditOutbox {
    #[napi]
    pub fn clone_handle(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }

    #[napi(factory)]
    pub async fn open(config_json: String) -> Result<Self> {
        if config_json.len() > 32 * 1024 {
            return Err(Error::new(
                Status::InvalidArg,
                "[audit_outbox:invalid_config] audit configuration exceeds limit",
            ));
        }
        let mut value: Value = serde_json::from_str(&config_json).map_err(invalid_config)?;
        integer_in(&mut value, "max_retained_bytes")?;
        integer_in(&mut value, "max_event_bytes")?;
        let config: OutboxConfig = serde_json::from_value(value).map_err(invalid_config)?;
        Ok(Self {
            inner: AuditOutbox::open(config).await.map_err(native_error)?,
        })
    }

    #[napi]
    pub async fn submit(&self, event_json: String) -> Result<String> {
        if event_json.len() > MAX_EVENT_BYTES + 1024 {
            return Err(Error::new(
                Status::InvalidArg,
                "[audit_outbox:event_too_large] audit event exceeds limit",
            ));
        }
        let mut value: Value = serde_json::from_str(&event_json).map_err(invalid)?;
        integer_in(&mut value, "timestamp_ms")?;
        let event: AuditEvent = serde_json::from_value(value).map_err(invalid)?;
        Ok(self
            .inner
            .submit(event)
            .await
            .map_err(native_error)?
            .to_string())
    }

    #[napi]
    pub async fn status(&self) -> Result<String> {
        let mut value = serde_json::to_value(self.inner.status().await.map_err(native_error)?)
            .map_err(invalid)?;
        integers_out(&mut value);
        serde_json::to_string(&value).map_err(invalid)
    }

    #[napi]
    pub async fn flush(&self, timeout_ms: f64) -> Result<()> {
        let duration = Duration::try_from_secs_f64(timeout_ms / 1000.0).map_err(invalid)?;
        self.inner.flush(duration).await.map_err(native_error)
    }

    #[napi]
    pub async fn close(&self) -> Result<()> {
        self.inner.close().await.map_err(native_error)
    }

    #[napi]
    pub async fn reconfigure(&self, destination_json: String) -> Result<()> {
        if destination_json.len() > 32 * 1024 {
            return Err(Error::new(
                Status::InvalidArg,
                "[audit_outbox:invalid_config] audit destination exceeds limit",
            ));
        }
        let destination: Destination =
            serde_json::from_str(&destination_json).map_err(invalid_config)?;
        self.inner
            .reconfigure(destination)
            .await
            .map_err(native_error)
    }

    #[napi]
    pub async fn quarantine(
        &self,
        event_id: String,
        reason: String,
        operator_reference: String,
    ) -> Result<()> {
        self.inner
            .quarantine(event_id, reason, operator_reference)
            .await
            .map_err(native_error)
    }

    #[napi]
    pub async fn export(&self, event_id: String) -> Result<String> {
        let mut value =
            serde_json::to_value(self.inner.export(event_id).await.map_err(native_error)?)
                .map_err(invalid)?;
        // Export's envelope is opaque bytes; it must not pass through numeric conversion.
        if let Some(receipts) = value.get_mut("receipts") {
            integers_out(receipts);
        }
        serde_json::to_string(&value).map_err(invalid)
    }

    /// Up to `limit` (1..=1000) acknowledgement-log entries with a cursor
    /// greater than `after` (a decimal u64 string), oldest first, as JSON
    /// with u64 values as decimal strings. With `waitMs`, waits up to that
    /// long (at most one hour) for the next acknowledgement when none follows
    /// `after` yet, and returns an empty page when the wait ends first.
    #[napi]
    pub async fn acknowledgements(
        &self,
        after: String,
        limit: u32,
        wait_ms: Option<f64>,
    ) -> Result<String> {
        let after: u64 = after.parse().map_err(invalid)?;
        let limit = limit as usize;
        let page = match wait_ms {
            None => self.inner.acknowledgements(after, limit).await,
            Some(ms) => {
                if !ms.is_finite() || !(0.0..=MAX_ACK_WAIT_MS).contains(&ms) {
                    return Err(Error::new(
                        Status::InvalidArg,
                        "[audit_outbox:invalid_event] acknowledgement wait must be between 0 and one hour",
                    ));
                }
                self.inner
                    .wait_acknowledgements(after, limit, Duration::from_secs_f64(ms / 1000.0))
                    .await
            }
        }
        .map_err(native_error)?;
        let mut value = serde_json::to_value(page).map_err(invalid)?;
        integers_out(&mut value);
        serde_json::to_string(&value).map_err(invalid)
    }

    #[napi]
    pub async fn replay(&self, event_id: String) -> Result<()> {
        self.inner.replay(event_id).await.map_err(native_error)
    }
}
