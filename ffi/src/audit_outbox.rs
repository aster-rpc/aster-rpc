//! Thin durable audit outbox ABI (4.9). Storage, delivery and policy stay in Rust.
//! See `ffi_spec/Aster-audit-outbox-spec.md` for the bounded JSON v1 contract.

use std::sync::{atomic::Ordering, Arc};
use std::time::Duration;

use aster::audit_outbox::{
    AuditError, AuditErrorKind, AuditEvent, AuditOutbox, Destination, OutboxConfig, MAX_EVENT_BYTES,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::data::{bridge_for, in_slice, invalid, not_found, spawn_op_coded, Done, OpError};
use crate::pure::try_status;
use crate::{
    aster_bytes_t, aster_event_kind_t, aster_operation_t, aster_runtime_t, aster_status_t,
    next_handle_id, BridgeRuntime,
};

/// Runtime-owned reference to a shared Rust outbox. Clone shares workers and budget.
pub type aster_audit_outbox_t = u64;
/// Version of all audit outbox JSON request/status/export wrappers.
pub const ASTER_AUDIT_OUTBOX_JSON_VERSION: u32 = 1;
/// Maximum encoded UTF-8 config or destination request (before reading/copying it).
pub const ASTER_AUDIT_OUTBOX_MAX_CONFIG_JSON: usize = 32 * 1024;
/// Maximum encoded UTF-8 quarantine/export/replay request.
pub const ASTER_AUDIT_OUTBOX_MAX_OPERATOR_JSON: usize = 8 * 1024;
/// Additional encoded JSON framing allowed above the configured envelope limit.
/// Raw input whitespace/escaping also counts; native envelope validation remains authoritative.
pub const ASTER_AUDIT_OUTBOX_EVENT_JSON_OVERHEAD: usize = 1024;

/// `error_code` on audit outbox `ASTER_EVENT_ERROR`. Switch on this enum,
/// never on error text. Synchronous malformed ABI input returns INVALID_ARGUMENT.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_audit_outbox_error_t {
    ASTER_AUDIT_OUTBOX_ERROR_NONE = 0,
    ASTER_AUDIT_OUTBOX_ERROR_INVALID_CONFIG = 1,
    ASTER_AUDIT_OUTBOX_ERROR_INVALID_EVENT = 2,
    ASTER_AUDIT_OUTBOX_ERROR_EVENT_TOO_LARGE = 3,
    ASTER_AUDIT_OUTBOX_ERROR_STORAGE_FULL = 4,
    ASTER_AUDIT_OUTBOX_ERROR_STORAGE = 5,
    /// The commit outcome is uncertain; retry may duplicate an accepted event.
    ASTER_AUDIT_OUTBOX_ERROR_COMMIT_UNCERTAIN = 6,
    ASTER_AUDIT_OUTBOX_ERROR_QUEUE_OVERLOADED = 7,
    ASTER_AUDIT_OUTBOX_ERROR_MAINTENANCE = 8,
    ASTER_AUDIT_OUTBOX_ERROR_CLOSED = 9,
    ASTER_AUDIT_OUTBOX_ERROR_WORKER_FAILED = 10,
    ASTER_AUDIT_OUTBOX_ERROR_TIMEOUT = 11,
    ASTER_AUDIT_OUTBOX_ERROR_DELIVERY_STALLED = 12,
    /// The flushed prefix includes a quarantined disposition, not delivery.
    ASTER_AUDIT_OUTBOX_ERROR_DISPOSITION = 13,
    ASTER_AUDIT_OUTBOX_ERROR_NOT_FOUND = 14,
    ASTER_AUDIT_OUTBOX_ERROR_DESTINATION_MISMATCH = 15,
    ASTER_AUDIT_OUTBOX_ERROR_INTEGRITY_CONFLICT = 16,
    ASTER_AUDIT_OUTBOX_ERROR_STALE_DELIVERY = 17,
}

pub(crate) struct OutboxState {
    outbox: AuditOutbox,
    max_event_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigRequest {
    version: u32,
    config: OutboxConfig,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventRequest {
    version: u32,
    event: AuditEvent,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DestinationRequest {
    version: u32,
    destination: Destination,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdRequest {
    version: u32,
    event_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuarantineRequest {
    version: u32,
    quarantine: QuarantineAction,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuarantineAction {
    event_id: String,
    reason: String,
    operator_reference: String,
}

/// Validate length BEFORE constructing a foreign slice, let alone copying/parsing.
/// No parser error details are returned: they may contain caller payload/credentials.
unsafe fn request<T: DeserializeOwned>(bytes: aster_bytes_t, limit: usize) -> Result<T, i32> {
    if bytes.len > limit {
        return Err(invalid("audit outbox JSON exceeds its encoded input limit"));
    }
    let bytes = in_slice(&bytes, "audit outbox JSON")?;
    serde_json::from_slice(bytes).map_err(|_| invalid("invalid audit outbox JSON request"))
}

fn version(version: u32) -> Result<(), i32> {
    if version == ASTER_AUDIT_OUTBOX_JSON_VERSION {
        Ok(())
    } else {
        Err(invalid("unsupported audit outbox JSON version"))
    }
}

fn id(id: &str) -> Result<(), i32> {
    if id.len() > 160 || id.is_empty() {
        Err(invalid("invalid audit event ID length"))
    } else {
        Ok(())
    }
}

fn outbox_for(bridge: &BridgeRuntime, handle: u64) -> Result<Arc<OutboxState>, i32> {
    bridge
        .audit_outboxes
        .get(handle)
        .ok_or_else(|| not_found("unknown audit outbox handle for this runtime"))
}

/// Closing and registration synchronize through the runtime's registry lock:
/// shutdown sets `closing` first and then takes this same lock.
fn register(bridge: &BridgeRuntime, state: Arc<OutboxState>) -> Result<u64, OpError> {
    let mut entries = bridge
        .audit_outboxes
        .items
        .write()
        .unwrap_or_else(|e| e.into_inner());
    if bridge.closing.load(Ordering::Acquire) {
        return Err(closed());
    }
    let handle = next_handle_id();
    let mut lifetimes = bridge
        .audit_outbox_lifetimes
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    lifetimes.retain(|weak| weak.strong_count() > 0);
    let weak = Arc::downgrade(&state);
    if !lifetimes.iter().any(|existing| existing.ptr_eq(&weak)) {
        lifetimes.push(weak);
    }
    entries.insert(handle, state);
    Ok(handle)
}

fn closed() -> OpError {
    OpError {
        status: aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
        code: aster_audit_outbox_error_t::ASTER_AUDIT_OUTBOX_ERROR_CLOSED as i32,
        flags: 0,
        message: "audit outbox runtime is closing".into(),
    }
}

fn error(error: AuditError) -> OpError {
    use aster_audit_outbox_error_t::*;
    use aster_status_t::*;
    let (code, status) = match error.kind() {
        AuditErrorKind::InvalidConfig => (
            ASTER_AUDIT_OUTBOX_ERROR_INVALID_CONFIG,
            ASTER_STATUS_INVALID_ARGUMENT,
        ),
        AuditErrorKind::InvalidEvent => (
            ASTER_AUDIT_OUTBOX_ERROR_INVALID_EVENT,
            ASTER_STATUS_INVALID_ARGUMENT,
        ),
        AuditErrorKind::EventTooLarge => (
            ASTER_AUDIT_OUTBOX_ERROR_EVENT_TOO_LARGE,
            ASTER_STATUS_INVALID_ARGUMENT,
        ),
        AuditErrorKind::StorageFull => (
            ASTER_AUDIT_OUTBOX_ERROR_STORAGE_FULL,
            ASTER_STATUS_QUEUE_FULL,
        ),
        AuditErrorKind::Storage => (ASTER_AUDIT_OUTBOX_ERROR_STORAGE, ASTER_STATUS_INTERNAL),
        AuditErrorKind::CommitUncertain => (
            ASTER_AUDIT_OUTBOX_ERROR_COMMIT_UNCERTAIN,
            ASTER_STATUS_INTERNAL,
        ),
        AuditErrorKind::QueueOverloaded => (
            ASTER_AUDIT_OUTBOX_ERROR_QUEUE_OVERLOADED,
            ASTER_STATUS_QUEUE_FULL,
        ),
        AuditErrorKind::Maintenance => (
            ASTER_AUDIT_OUTBOX_ERROR_MAINTENANCE,
            ASTER_STATUS_QUEUE_FULL,
        ),
        AuditErrorKind::Closed => (ASTER_AUDIT_OUTBOX_ERROR_CLOSED, ASTER_STATUS_ALREADY_CLOSED),
        AuditErrorKind::WorkerFailed => (
            ASTER_AUDIT_OUTBOX_ERROR_WORKER_FAILED,
            ASTER_STATUS_INTERNAL,
        ),
        AuditErrorKind::Timeout => (ASTER_AUDIT_OUTBOX_ERROR_TIMEOUT, ASTER_STATUS_TIMEOUT),
        AuditErrorKind::DeliveryStalled => (
            ASTER_AUDIT_OUTBOX_ERROR_DELIVERY_STALLED,
            ASTER_STATUS_CONNECTION_REFUSED,
        ),
        AuditErrorKind::Disposition => {
            (ASTER_AUDIT_OUTBOX_ERROR_DISPOSITION, ASTER_STATUS_INTERNAL)
        }
        AuditErrorKind::NotFound => (ASTER_AUDIT_OUTBOX_ERROR_NOT_FOUND, ASTER_STATUS_NOT_FOUND),
        AuditErrorKind::DestinationMismatch => (
            ASTER_AUDIT_OUTBOX_ERROR_DESTINATION_MISMATCH,
            ASTER_STATUS_INVALID_ARGUMENT,
        ),
        AuditErrorKind::IntegrityConflict => (
            ASTER_AUDIT_OUTBOX_ERROR_INTEGRITY_CONFLICT,
            ASTER_STATUS_VERIFICATION_FAILED,
        ),
        AuditErrorKind::StaleDelivery => (
            ASTER_AUDIT_OUTBOX_ERROR_STALE_DELIVERY,
            ASTER_STATUS_INTERNAL,
        ),
    };
    OpError {
        status,
        code: code as i32,
        flags: 0,
        message: error.to_string(),
    }
}

fn output(value: serde_json::Value) -> Result<Vec<u8>, OpError> {
    serde_json::to_vec(&value).map_err(|_| OpError {
        status: aster_status_t::ASTER_STATUS_INTERNAL,
        code: aster_audit_outbox_error_t::ASTER_AUDIT_OUTBOX_ERROR_WORKER_FAILED as i32,
        flags: 0,
        message: "could not encode audit outbox result".into(),
    })
}

/// Close all registered workers asynchronously before runtime teardown. Pending
/// records remain durable. In-flight opens register under the same lock or fail.
pub(crate) fn shutdown_runtime(bridge: &Arc<BridgeRuntime>) {
    let states = std::mem::take(
        &mut *bridge
            .audit_outboxes
            .items
            .write()
            .unwrap_or_else(|e| e.into_inner()),
    );
    let lifetimes = std::mem::take(
        &mut *bridge
            .audit_outbox_lifetimes
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    let mut seen = std::collections::HashSet::new();
    for state in states
        .into_values()
        .chain(lifetimes.into_iter().filter_map(|weak| weak.upgrade()))
    {
        if seen.insert(Arc::as_ptr(&state) as usize) {
            let bridge_owned = bridge.clone();
            bridge.runtime.spawn(async move {
                let _ = state.outbox.close().await;
                drop(bridge_owned);
            });
        }
    }
}

/// Open JSON v1 `{ "version":1, "config":{...} }`; configuration and storage
/// failures complete asynchronously. Emits AUDIT_OUTBOX_OPENED with the handle.
/// Cancelled successful opens are closed/freed even when completion arrives late.
#[no_mangle]
pub unsafe extern "C" fn aster_audit_outbox_open(
    runtime: aster_runtime_t,
    json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("out_operation is NULL");
    }
    *out_operation = 0;
    let req: ConfigRequest = try_status!(request(json, ASTER_AUDIT_OUTBOX_MAX_CONFIG_JSON));
    try_status!(version(req.version));
    let bridge = try_status!(bridge_for(runtime));
    let owned = bridge.clone();
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let max_event_bytes = req.config.max_event_bytes;
        let outbox = AuditOutbox::open(req.config).await.map_err(error)?;
        #[cfg(test)]
        let gate = test_hooks::before_open(user_data).await;
        let state = Arc::new(OutboxState {
            outbox,
            max_event_bytes,
        });
        let handle = match register(&owned, state.clone()) {
            Ok(handle) => handle,
            Err(e) => {
                let _ = state.outbox.close().await;
                return Err(e);
            }
        };
        Ok(
            Done::ok(aster_event_kind_t::ASTER_EVENT_AUDIT_OUTBOX_OPENED, handle).on_discard(
                move || {
                    if let Some(state) = owned.audit_outboxes.remove(handle) {
                        let keep_runtime = owned.clone();
                        owned.runtime.spawn(async move {
                            let _ = state.outbox.close().await;
                            #[cfg(test)]
                            test_hooks::discarded(gate);
                            drop(keep_runtime);
                        });
                    }
                },
            ),
        )
    })
}

/// Clone a handle without reopening storage or allocating workers/queues.
/// Closing any clone stops the shared engine; free releases only this reference.
#[no_mangle]
pub unsafe extern "C" fn aster_audit_outbox_clone(
    runtime: aster_runtime_t,
    outbox: aster_audit_outbox_t,
    out_clone: *mut aster_audit_outbox_t,
) -> i32 {
    if out_clone.is_null() {
        return invalid("out_clone is NULL");
    }
    *out_clone = 0;
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(outbox_for(&bridge, outbox));
    match register(&bridge, state) {
        Ok(handle) => {
            *out_clone = handle;
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(e) => {
            crate::set_last_error(e.message);
            e.status as i32
        }
    }
}

/// Release a handle. Last-reference drop signals worker shutdown without waiting.
/// Prefer asynchronous close before free when immediate reopen is required.
#[no_mangle]
pub unsafe extern "C" fn aster_audit_outbox_free(
    runtime: aster_runtime_t,
    outbox: aster_audit_outbox_t,
) -> i32 {
    let bridge = try_status!(bridge_for(runtime));
    if bridge.audit_outboxes.remove(outbox).is_some() {
        bridge
            .audit_outbox_lifetimes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|weak| weak.strong_count() > 0);
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        not_found("unknown audit outbox handle for this runtime")
    }
}

/// Submit JSON v1 `{ "version":1, "event":{ "timestamp_ms":..., "kind":...,
/// "payload":... } }`. Emits SUBMITTED with the full ID after durable commit.
/// Cancellation can lose the result of an accepted event; it never deletes it.
#[no_mangle]
pub unsafe extern "C" fn aster_audit_outbox_submit(
    runtime: aster_runtime_t,
    outbox: aster_audit_outbox_t,
    json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("out_operation is NULL");
    }
    *out_operation = 0;
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(outbox_for(&bridge, outbox));
    let limit = state.max_event_bytes.min(MAX_EVENT_BYTES) + ASTER_AUDIT_OUTBOX_EVENT_JSON_OVERHEAD;
    if json.ptr.is_null() && json.len != 0 {
        return invalid("audit outbox JSON: NULL pointer with nonzero length");
    }
    if json.len > limit {
        // Preserve typed domain rejection without reading/copying foreign bytes.
        return spawn_op_coded(&bridge, user_data, out_operation, async move {
            Err(error(AuditError {
                code: AuditErrorKind::EventTooLarge,
                message: "encoded audit event exceeds its input limit".into(),
            }))
        });
    }
    let req: EventRequest = try_status!(request(json, limit));
    try_status!(version(req.version));
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let event_id = state.outbox.submit(req.event).await.map_err(error)?;
        #[cfg(test)]
        test_hooks::after_submit(user_data).await;
        Ok(Done::ok(
            aster_event_kind_t::ASTER_EVENT_AUDIT_OUTBOX_SUBMITTED,
            outbox,
        )
        .data(event_id.to_string().into_bytes()))
    })
}

/// Obtain JSON v1 `{ "version":1, "status":{...} }` asynchronously.
#[no_mangle]
pub unsafe extern "C" fn aster_audit_outbox_status(
    runtime: aster_runtime_t,
    outbox: aster_audit_outbox_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("out_operation is NULL");
    }
    *out_operation = 0;
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(outbox_for(&bridge, outbox));
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let status = state.outbox.status().await.map_err(error)?;
        Ok(
            Done::ok(aster_event_kind_t::ASTER_EVENT_AUDIT_OUTBOX_STATUS, outbox)
                .data(output(serde_json::json!({"version":1,"status":status}))?),
        )
    })
}

/// Wait for the committed prefix captured by Rust. Timeout/cancellation keeps
/// pending events. Quarantine of a prefix returns DISPOSITION, never success.
#[no_mangle]
pub unsafe extern "C" fn aster_audit_outbox_flush(
    runtime: aster_runtime_t,
    outbox: aster_audit_outbox_t,
    timeout_ms: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("out_operation is NULL");
    }
    *out_operation = 0;
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(outbox_for(&bridge, outbox));
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        state
            .outbox
            .flush(Duration::from_millis(timeout_ms))
            .await
            .map_err(error)?;
        Ok(Done::ok(
            aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
            outbox,
        ))
    })
}

/// Stop shared workers and release storage asynchronously, retaining pending
/// records. The handle remains registered until free; all clones become closed.
#[no_mangle]
pub unsafe extern "C" fn aster_audit_outbox_close(
    runtime: aster_runtime_t,
    outbox: aster_audit_outbox_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("out_operation is NULL");
    }
    *out_operation = 0;
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(outbox_for(&bridge, outbox));
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        state.outbox.close().await.map_err(error)?;
        Ok(Done::ok(
            aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
            outbox,
        ))
    })
}

/// Rotate credentials or explicitly repair destination JSON v1. Identity changes
/// while pending/dead-letter data exist are rejected by the shared Rust engine.
#[no_mangle]
pub unsafe extern "C" fn aster_audit_outbox_reconfigure(
    runtime: aster_runtime_t,
    outbox: aster_audit_outbox_t,
    json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("out_operation is NULL");
    }
    *out_operation = 0;
    let req: DestinationRequest = try_status!(request(json, ASTER_AUDIT_OUTBOX_MAX_CONFIG_JSON));
    try_status!(version(req.version));
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(outbox_for(&bridge, outbox));
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        state
            .outbox
            .reconfigure(req.destination)
            .await
            .map_err(error)?;
        Ok(Done::ok(
            aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
            outbox,
        ))
    })
}

/// Explicit, evidence-preserving quarantine JSON v1. Never frees capacity or
/// marks delivery successful. No purge operation exists in v1.
#[no_mangle]
pub unsafe extern "C" fn aster_audit_outbox_quarantine(
    runtime: aster_runtime_t,
    outbox: aster_audit_outbox_t,
    json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("out_operation is NULL");
    }
    *out_operation = 0;
    let req: QuarantineRequest = try_status!(request(json, ASTER_AUDIT_OUTBOX_MAX_OPERATOR_JSON));
    try_status!(version(req.version));
    try_status!(id(&req.quarantine.event_id));
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(outbox_for(&bridge, outbox));
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let q = req.quarantine;
        state
            .outbox
            .quarantine(q.event_id, q.reason, q.operator_reference)
            .await
            .map_err(error)?;
        Ok(Done::ok(
            aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
            outbox,
        ))
    })
}

/// Read-only export JSON v1 `{ "version":1, "event_id":"..." }`. Emits
/// EXPORTED with `{ "version":1, "export":{...} }` including original bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_audit_outbox_export(
    runtime: aster_runtime_t,
    outbox: aster_audit_outbox_t,
    json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("out_operation is NULL");
    }
    *out_operation = 0;
    let req: IdRequest = try_status!(request(json, ASTER_AUDIT_OUTBOX_MAX_OPERATOR_JSON));
    try_status!(version(req.version));
    try_status!(id(&req.event_id));
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(outbox_for(&bridge, outbox));
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        let export = state.outbox.export(req.event_id).await.map_err(error)?;
        Ok(Done::ok(
            aster_event_kind_t::ASTER_EVENT_AUDIT_OUTBOX_EXPORTED,
            outbox,
        )
        .data(output(serde_json::json!({"version":1,"export":export}))?))
    })
}

/// Explicit replay JSON v1, preserving the original event ID and disposition
/// trail. Emits UNIT_RESULT; remote acceptance remains separately flushable.
#[no_mangle]
pub unsafe extern "C" fn aster_audit_outbox_replay(
    runtime: aster_runtime_t,
    outbox: aster_audit_outbox_t,
    json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("out_operation is NULL");
    }
    *out_operation = 0;
    let req: IdRequest = try_status!(request(json, ASTER_AUDIT_OUTBOX_MAX_OPERATOR_JSON));
    try_status!(version(req.version));
    try_status!(id(&req.event_id));
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(outbox_for(&bridge, outbox));
    spawn_op_coded(&bridge, user_data, out_operation, async move {
        state.outbox.replay(req.event_id).await.map_err(error)?;
        Ok(Done::ok(
            aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
            outbox,
        ))
    })
}

// Deterministic cancellation seams are unit-test-only and absent from the ABI.
#[cfg(test)]
mod test_hooks {
    use std::collections::HashMap;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    pub(super) struct Gate {
        reached: Mutex<bool>,
        discarded: Mutex<bool>,
        cv: Condvar,
        discarded_cv: Condvar,
        release: tokio::sync::Semaphore,
    }
    impl Gate {
        pub(super) fn wait(&self, discarded: bool) {
            let state = if discarded {
                &self.discarded
            } else {
                &self.reached
            };
            let cv = if discarded {
                &self.discarded_cv
            } else {
                &self.cv
            };
            let state = state.lock().unwrap();
            let (state, _) = cv
                .wait_timeout_while(state, Duration::from_secs(10), |done| !*done)
                .unwrap();
            assert!(*state, "audit cancellation gate did not complete");
        }
        pub(super) fn release(&self) {
            self.release.add_permits(1);
        }
    }
    static OPEN: std::sync::LazyLock<Mutex<HashMap<u64, Arc<Gate>>>> =
        std::sync::LazyLock::new(Mutex::default);
    static SUBMIT: std::sync::LazyLock<Mutex<HashMap<u64, Arc<Gate>>>> =
        std::sync::LazyLock::new(Mutex::default);
    pub(super) fn arm(user_data: u64, open: bool) -> Arc<Gate> {
        let gate = Arc::new(Gate {
            reached: Mutex::new(false),
            discarded: Mutex::new(false),
            cv: Condvar::new(),
            discarded_cv: Condvar::new(),
            release: tokio::sync::Semaphore::new(0),
        });
        (if open { &OPEN } else { &SUBMIT })
            .lock()
            .unwrap()
            .insert(user_data, gate.clone());
        gate
    }
    async fn pass(gate: Option<Arc<Gate>>) -> Option<Arc<Gate>> {
        let gate = gate?;
        *gate.reached.lock().unwrap() = true;
        gate.cv.notify_all();
        gate.release.acquire().await.unwrap().forget();
        Some(gate)
    }
    pub(super) async fn before_open(user_data: u64) -> Option<Arc<Gate>> {
        let gate = OPEN.lock().unwrap().remove(&user_data);
        pass(gate).await
    }
    pub(super) async fn after_submit(user_data: u64) {
        let gate = SUBMIT.lock().unwrap().remove(&user_data);
        pass(gate).await;
    }
    pub(super) fn discarded(gate: Option<Arc<Gate>>) {
        if let Some(gate) = gate {
            *gate.discarded.lock().unwrap() = true;
            gate.discarded_cv.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        aster_buffer_release, aster_operation_cancel, aster_runtime_close,
        aster_runtime_close_and_wait, aster_runtime_config_t, aster_runtime_new,
        ASTER_ABI_VERSION_MAJOR,
    };
    use serde_json::{json, Value};
    use std::path::Path;
    use std::ptr;

    use crate::audit_test_common as common;

    // Docker exec (including Forgejo steps) can start with umask 0000.
    // Configure the test-owned directory itself, never the parallel process.
    fn private_directory() -> tempfile::TempDir {
        let mut builder = tempfile::Builder::new();
        builder.prefix("aster-ffi-audit-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        builder.tempdir().expect("private audit test directory")
    }

    #[cfg(unix)]
    #[test]
    fn private_fixture_opens_and_reopens_under_permissive_umask() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child.args([
            "--exact",
            "audit_outbox::tests::clones_are_runtime_scoped_and_reopen_keeps_durable_ids_and_errors",
            "--nocapture",
        ]);
        // SAFETY: umask is an async-signal-safe syscall in the child after fork;
        // it never changes the parent test process or another parallel test.
        unsafe {
            child.pre_exec(|| {
                libc::umask(0);
                Ok(())
            });
        }
        let mut child = child
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("private fixture child exceeded its 60-second deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "private fixture failed under umask 0000: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;
    fn bytes(json: &[u8]) -> aster_bytes_t {
        aster_bytes_t {
            ptr: json.as_ptr(),
            len: json.len(),
        }
    }
    unsafe fn runtime(limit: u32) -> u64 {
        let config = aster_runtime_config_t {
            struct_size: std::mem::size_of::<aster_runtime_config_t>() as u32,
            worker_threads: 1,
            event_queue_capacity: 16,
            max_inflight_operations: limit,
        };
        let mut rt = 0;
        assert_eq!(
            aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &config, &mut rt),
            OK
        );
        rt
    }
    fn config(path: &Path, max: usize) -> Vec<u8> {
        serde_json::to_vec(&json!({"version":1,"config":{"directory":path,
            "destination":{"endpoint":"http://127.0.0.1:1/audit"},
            "max_event_bytes":max}}))
        .unwrap()
    }
    unsafe fn open(rt: u64, path: &Path, max: usize) -> u64 {
        let cfg = config(path, max);
        let mut op = 0;
        assert_eq!(aster_audit_outbox_open(rt, bytes(&cfg), 0, &mut op), OK);
        common::wait_for_successful_operation(
            rt,
            op,
            aster_event_kind_t::ASTER_EVENT_AUDIT_OUTBOX_OPENED,
        )
        .handle
    }
    unsafe fn payload(rt: u64, event: crate::aster_event_t) -> Vec<u8> {
        let data = if event.data_len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(event.data_ptr, event.data_len).to_vec()
        };
        if event.buffer != 0 {
            assert_eq!(aster_buffer_release(rt, event.buffer), OK);
        }
        data
    }
    unsafe fn status(rt: u64, handle: u64) -> Value {
        let mut op = 0;
        assert_eq!(aster_audit_outbox_status(rt, handle, 0, &mut op), OK);
        let event = common::wait_for_successful_operation(
            rt,
            op,
            aster_event_kind_t::ASTER_EVENT_AUDIT_OUTBOX_STATUS,
        );
        let value: Value = serde_json::from_slice(&payload(rt, event)).unwrap();
        assert_eq!(value["version"], 1);
        value["status"].clone()
    }
    unsafe fn close(rt: u64, handle: u64) {
        let mut op = 0;
        assert_eq!(aster_audit_outbox_close(rt, handle, 0, &mut op), OK);
        common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_UNIT_RESULT);
    }
    fn event(size: usize) -> Vec<u8> {
        serde_json::to_vec(&json!({"version":1,"event":{"timestamp_ms":1,"kind":"test",
            "payload":{"body":"x".repeat(size)}}}))
        .unwrap()
    }

    #[test]
    fn typed_native_errors_keep_the_published_numeric_contract() {
        use AuditErrorKind::*;
        let kinds = [
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
        ];
        for (index, kind) in kinds.into_iter().enumerate() {
            let mapped = error(AuditError {
                code: kind,
                message: "redacted native failure".into(),
            });
            assert_eq!(mapped.code, index as i32 + 1);
            assert_eq!(mapped.flags, 0);
        }
    }

    #[test]
    fn requests_reject_bounds_before_reading_foreign_pointers_and_hide_values() {
        unsafe {
            let unreadable = aster_bytes_t {
                ptr: ptr::dangling(),
                len: usize::MAX,
            };
            let mut op = 123;
            assert_eq!(
                aster_audit_outbox_open(0, unreadable, 0, &mut op),
                aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
            );
            assert_eq!(op, 0);
            assert_eq!(
                aster_audit_outbox_quarantine(0, 0, unreadable, 0, &mut op),
                aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
            );
            assert_eq!(
                aster_audit_outbox_export(0, 0, unreadable, 0, &mut op),
                aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
            );
            let secret = br#"{"version":1,"config":{"directory":".","destination":{"endpoint":"http://x","authorization":"DO_NOT_PRINT"},"extra":"DO_NOT_PRINT"}}"#;
            assert_eq!(
                aster_audit_outbox_open(0, bytes(secret), 0, &mut op),
                aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
            );
            let mut last = [0u8; 256];
            let len = crate::aster_last_error_message(last.as_mut_ptr(), last.len());
            assert!(!String::from_utf8_lossy(&last[..len]).contains("DO_NOT_PRINT"));
        }
    }

    #[test]
    fn clones_are_runtime_scoped_and_reopen_keeps_durable_ids_and_errors() {
        let dir = private_directory();
        unsafe {
            let rt = runtime(16);
            let other = runtime(16);
            let handle = open(rt, dir.path(), 512);
            let mut clone = 0;
            assert_eq!(aster_audit_outbox_clone(rt, handle, &mut clone), OK);
            let mut op = 0;
            assert_eq!(
                aster_audit_outbox_status(other, clone, 0, &mut op),
                aster_status_t::ASTER_STATUS_NOT_FOUND as i32
            );
            assert_eq!(aster_audit_outbox_free(rt, handle), OK);
            assert_eq!(
                aster_audit_outbox_submit(rt, clone, bytes(&event(1)), 0, &mut op),
                OK
            );
            let submitted = common::wait_for_successful_operation(
                rt,
                op,
                aster_event_kind_t::ASTER_EVENT_AUDIT_OUTBOX_SUBMITTED,
            );
            let full_id = String::from_utf8(payload(rt, submitted)).unwrap();
            assert!(full_id.starts_with("aster-audit-v1:"));
            assert_eq!(status(rt, clone)["pending_records"], 1);
            assert_eq!(
                aster_audit_outbox_submit(rt, clone, bytes(&event(600)), 0, &mut op),
                OK
            );
            let failed = common::wait_for_operation(rt, op);
            assert_eq!(failed.kind, aster_event_kind_t::ASTER_EVENT_ERROR as u32);
            assert_eq!(
                failed.error_code,
                aster_audit_outbox_error_t::ASTER_AUDIT_OUTBOX_ERROR_EVENT_TOO_LARGE as i32
            );
            payload(rt, failed);
            let unreadable = aster_bytes_t {
                ptr: ptr::dangling(),
                len: usize::MAX,
            };
            assert_eq!(
                aster_audit_outbox_submit(rt, clone, unreadable, 0, &mut op),
                OK
            );
            let too_large = common::wait_for_operation(rt, op);
            assert_eq!(
                too_large.error_code,
                aster_audit_outbox_error_t::ASTER_AUDIT_OUTBOX_ERROR_EVENT_TOO_LARGE as i32
            );
            payload(rt, too_large);
            assert_eq!(
                aster_audit_outbox_submit(rt, clone, bytes(&event(70 * 1024)), 0, &mut op),
                OK
            );
            let too_large = common::wait_for_operation(rt, op);
            assert_eq!(
                too_large.error_code,
                aster_audit_outbox_error_t::ASTER_AUDIT_OUTBOX_ERROR_EVENT_TOO_LARGE as i32
            );
            payload(rt, too_large);
            close(rt, clone);
            assert_eq!(aster_audit_outbox_free(rt, clone), OK);
            let reopened = open(rt, dir.path(), 512);
            let req = serde_json::to_vec(&json!({"version":1,"event_id":full_id})).unwrap();
            assert_eq!(
                aster_audit_outbox_export(rt, reopened, bytes(&req), 0, &mut op),
                OK
            );
            let exported = common::wait_for_successful_operation(
                rt,
                op,
                aster_event_kind_t::ASTER_EVENT_AUDIT_OUTBOX_EXPORTED,
            );
            let value: Value = serde_json::from_slice(&payload(rt, exported)).unwrap();
            assert_eq!(value["export"]["event_id"], full_id);
            assert_eq!(value["export"]["pending"], true);
            assert_eq!(
                aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
                OK
            );
            assert_eq!(
                aster_runtime_close_and_wait(other, Duration::from_secs(10)),
                OK
            );
        }
    }

    #[test]
    fn cancelled_late_open_closes_unobserved_handle_and_releases_directory() {
        let dir = private_directory();
        unsafe {
            let rt = runtime(8);
            let user_data = next_handle_id();
            let gate = test_hooks::arm(user_data, true);
            let mut op = 0;
            assert_eq!(
                aster_audit_outbox_open(rt, bytes(&config(dir.path(), 4096)), user_data, &mut op),
                OK
            );
            gate.wait(false);
            assert_eq!(aster_operation_cancel(rt, op), OK);
            let cancelled = common::wait_for_operation(rt, op);
            assert_eq!(
                cancelled.kind,
                aster_event_kind_t::ASTER_EVENT_OPERATION_CANCELLED as u32
            );
            gate.release();
            gate.wait(true);
            assert_eq!(crate::load_runtime(rt).unwrap().audit_outboxes.count(), 0);
            let reopened = open(rt, dir.path(), 4096);
            close(rt, reopened);
            assert_eq!(
                aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
                OK
            );
        }
    }

    #[test]
    fn cancelling_committed_submit_never_retracts_event_and_shutdown_closes_clones() {
        let dir = private_directory();
        unsafe {
            let rt = runtime(8);
            let handle = open(rt, dir.path(), 4096);
            let mut clone = 0;
            assert_eq!(aster_audit_outbox_clone(rt, handle, &mut clone), OK);
            let user_data = next_handle_id();
            let gate = test_hooks::arm(user_data, false);
            let mut op = 0;
            assert_eq!(
                aster_audit_outbox_submit(rt, clone, bytes(&event(1)), user_data, &mut op),
                OK
            );
            gate.wait(false); // native durable completion happened before this gate
            assert_eq!(aster_operation_cancel(rt, op), OK);
            assert_eq!(
                common::wait_for_operation(rt, op).kind,
                aster_event_kind_t::ASTER_EVENT_OPERATION_CANCELLED as u32
            );
            gate.release();
            assert_eq!(status(rt, handle)["pending_records"], 1);
            assert_eq!(
                aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
                OK
            );
            let rt = runtime(8);
            let reopened = open(rt, dir.path(), 4096);
            assert_eq!(status(rt, reopened)["pending_records"], 1);
            assert_eq!(
                aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
                OK
            );
        }
    }

    #[test]
    fn runtime_shutdown_rejects_late_registration_and_operation_limit_starts_no_open() {
        let dir = private_directory();
        let unused = private_directory();
        unsafe {
            let rt = runtime(1);
            let user_data = next_handle_id();
            let gate = test_hooks::arm(user_data, true);
            let mut op = 0;
            assert_eq!(
                aster_audit_outbox_open(rt, bytes(&config(dir.path(), 4096)), user_data, &mut op),
                OK
            );
            gate.wait(false);
            let mut refused = 7;
            assert_eq!(
                aster_audit_outbox_open(rt, bytes(&config(unused.path(), 4096)), 0, &mut refused),
                aster_status_t::ASTER_STATUS_OPERATION_LIMIT as i32
            );
            assert_eq!(refused, 0);
            assert_eq!(std::fs::read_dir(unused.path()).unwrap().count(), 0);
            let teardown = crate::load_runtime(rt).unwrap().teardown.clone();
            assert_eq!(aster_runtime_close(rt), OK);
            gate.release();
            assert!(teardown.wait(Duration::from_secs(10)));
            let rt = runtime(8);
            let reopened = open(rt, dir.path(), 4096);
            close(rt, reopened);
            assert_eq!(
                aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
                OK
            );
        }
    }

    #[test]
    fn runtime_shutdown_closes_engines_retained_only_by_freed_handle_operations() {
        let dir = private_directory();
        unsafe {
            let rt = runtime(8);
            let handle = open(rt, dir.path(), 4096);
            let mut op = 0;
            assert_eq!(
                aster_audit_outbox_submit(rt, handle, bytes(&event(1)), 0, &mut op),
                OK
            );
            let submitted = common::wait_for_successful_operation(
                rt,
                op,
                aster_event_kind_t::ASTER_EVENT_AUDIT_OUTBOX_SUBMITTED,
            );
            payload(rt, submitted);
            assert_eq!(
                aster_audit_outbox_flush(rt, handle, 3_600_000, 0, &mut op),
                OK
            );
            assert_eq!(aster_audit_outbox_free(rt, handle), OK);
            assert_eq!(
                aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
                OK
            );
            let rt = runtime(8);
            let reopened = open(rt, dir.path(), 4096);
            assert_eq!(status(rt, reopened)["pending_records"], 1);
            assert_eq!(
                aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
                OK
            );
        }
    }
}
