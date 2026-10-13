//! ABI 4.13: owned temporary blob custody and verified retained imports.
//! Scope close seals admission; accepted operations retain all temporary roots.
//! Operation cancellation is cooperative and callers must collect the terminal
//! event: a named-tag commit already in progress reports its actual result.
use crate::data::{
    bridge_for, encode_fetch_report, fetch_flags, fetch_strategy, in_hex32, in_node_list, in_slice,
    in_string, invalid,
};
use crate::*;
use aster_transport_core::{
    CoreBlobCustody, CoreBlobCustodyError, CoreBlobCustodyLease, CoreFetchStrategy,
};
use std::future::Future;
use std::ptr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// Runtime-local owned temporary custody scope. Free with aster_blob_custody_free.
pub type aster_blob_custody_t = u64;

pub(crate) struct CustodyState {
    scope: CoreBlobCustody,
}
impl Drop for CustodyState {
    fn drop(&mut self) {
        self.scope.close();
    }
}

pub(crate) fn shutdown(bridge: &BridgeRuntime) {
    for operation in bridge.operations.items.read().unwrap().values() {
        if operation.cooperative_cancel {
            operation.cancelled.store(true, Ordering::SeqCst);
            operation.cancel_notify.notify_waiters();
        }
    }
    let owners = std::mem::take(&mut *bridge.blob_custody.items.write().unwrap());
    for owner in owners.values() {
        owner.scope.close();
    }
}

macro_rules! attempt {
    ($expr:expr) => {
        match $expr {
            Ok(v) => v,
            Err(e) => return e,
        }
    };
}
fn format_name(value: u32) -> Result<String, i32> {
    match value {
        0 => Ok("raw".into()),
        1 => Ok("hash_seq".into()),
        _ => Err(invalid("unknown blob format")),
    }
}
fn lease(bridge: &BridgeRuntime, scope: u64) -> Result<CoreBlobCustodyLease, i32> {
    if bridge.closing.load(Ordering::Acquire) {
        return Err(aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32);
    }
    let owner = bridge
        .blob_custody
        .get(scope)
        .ok_or(aster_status_t::ASTER_STATUS_NOT_FOUND as i32)?;
    owner.scope.lease().map_err(|e| {
        set_last_error(e);
        aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32
    })
}
struct ResultData {
    kind: aster_event_kind_t,
    bytes: Vec<u8>,
    flags: u32,
}
fn result(kind: aster_event_kind_t, bytes: Vec<u8>) -> ResultData {
    ResultData {
        kind,
        bytes,
        flags: 0,
    }
}
fn cancelled(bridge: &BridgeRuntime, operation: u64, user_data: u64) {
    bridge.emit_simple(
        aster_event_kind_t::ASTER_EVENT_OPERATION_CANCELLED,
        aster_status_t::ASTER_STATUS_CANCELLED,
        operation,
        0,
        0,
        user_data,
        aster_status_t::ASTER_STATUS_CANCELLED as i32,
    );
}

// `commits` operations delegate the precommit cancellation check to the core.
// Every operation joins native work before publishing a cancellation terminal.
unsafe fn spawn<F, Fut>(
    bridge: Arc<BridgeRuntime>,
    handle: u64,
    user_data: u64,
    out: *mut aster_operation_t,
    commits: bool,
    make: F,
) -> i32
where
    F: FnOnce(Arc<AtomicBool>) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<ResultData>> + Send + 'static,
{
    if out.is_null() {
        return invalid("NULL operation output");
    }
    unsafe {
        ptr::write(out, 0);
    }
    let (id, flag) = match bridge.new_operation_with_cancel_mode(user_data, true) {
        Ok(v) => v,
        Err(e) => return e as i32,
    };
    unsafe {
        ptr::write(out, id);
    }
    let b = bridge.clone();
    bridge.runtime.spawn(async move {
        if b.closing.load(Ordering::Acquire) {
            flag.store(true, Ordering::SeqCst);
        }
        if flag.load(Ordering::SeqCst) {
            drop(make);
            cancelled(&b, id, user_data);
            return;
        }
        let work = make(flag.clone());
        let value = work.await;
        if !commits && flag.load(Ordering::SeqCst) {
            cancelled(&b, id, user_data);
            return;
        }
        match value {
            Ok(value) => {
                let mut event = EventInternal::new(
                    value.kind,
                    aster_status_t::ASTER_STATUS_OK,
                    id,
                    handle,
                    0,
                    user_data,
                    0,
                );
                event.flags = value.flags;
                b.emit_with_data(event, value.bytes);
            }
            Err(error) => {
                let status = match error.downcast_ref::<CoreBlobCustodyError>() {
                    Some(CoreBlobCustodyError::Cancelled) => {
                        cancelled(&b, id, user_data);
                        return;
                    }
                    Some(CoreBlobCustodyError::Closed) => {
                        aster_status_t::ASTER_STATUS_ALREADY_CLOSED
                    }
                    Some(CoreBlobCustodyError::HashMismatch { .. }) => {
                        aster_status_t::ASTER_STATUS_VERIFICATION_FAILED
                    }
                    Some(CoreBlobCustodyError::UnprotectedRoot) => {
                        aster_status_t::ASTER_STATUS_INVALID_ARGUMENT
                    }
                    _ => aster_status_t::ASTER_STATUS_INTERNAL,
                };
                b.emit_error_coded(id, user_data, status, status as i32, &error.to_string());
            }
        }
    });
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Create a temporary custody scope for a node. No persistent staging tag is created.
/// # Safety
/// out_scope must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_custody_new(
    runtime: aster_runtime_t,
    node: aster_node_t,
    out_scope: *mut aster_blob_custody_t,
) -> i32 {
    if out_scope.is_null() {
        return invalid("NULL custody output");
    }
    unsafe {
        ptr::write(out_scope, 0);
    }
    let bridge = attempt!(bridge_for(runtime));
    if bridge.closing.load(Ordering::Acquire) {
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    }
    let Some(owner) = bridge.nodes.get(node) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let id = bridge.blob_custody.insert(CustodyState {
        scope: owner.blobs_client().custody(),
    });
    if bridge.closing.load(Ordering::Acquire) {
        bridge.blob_custody.remove(id);
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    }
    unsafe {
        ptr::write(out_scope, id);
    }
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Seal a scope and release its owner. Accepted operations retain its guards
/// until they finish; unaccepted calls return NOT_FOUND. Repeated free is NOT_FOUND.
#[no_mangle]
pub extern "C" fn aster_blob_custody_free(
    runtime: aster_runtime_t,
    scope: aster_blob_custody_t,
) -> i32 {
    let bridge = attempt!(bridge_for(runtime));
    let Some(owner) = bridge.blob_custody.remove(scope) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    owner.scope.close();
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Scope-bound protect. Emits ASTER_EVENT_BLOB_CUSTODY_DONE on success.
/// Formats are RAW=0 or HASH_SEQ=1. Reads and imports return bytes/hash payloads;
/// protect and retain have empty payloads. Protection is temporary until retain.
/// Cancellation requests must collect the terminal event; commit may win the race.
/// # Safety
/// Input spans must be readable and out_operation writable for this call.
#[no_mangle]
pub unsafe extern "C" fn aster_blob_custody_protect(
    runtime: aster_runtime_t,
    scope: aster_blob_custody_t,
    hash: aster_bytes_t,
    format: u32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    let hash = attempt!(in_hex32(&hash, "hash"));
    let format = attempt!(format_name(format));
    let bridge = attempt!(bridge_for(runtime));
    let lease = attempt!(lease(&bridge, scope));
    unsafe {
        spawn(
            bridge,
            scope,
            user_data,
            out_operation,
            false,
            move |_cancel| async move {
                let data = {
                    lease
                        .protect_cancellable(hash, format, Some(_cancel))
                        .await?;
                    Vec::new()
                };
                Ok(result(
                    aster_event_kind_t::ASTER_EVENT_BLOB_CUSTODY_DONE,
                    data,
                ))
            },
        )
    }
}

/// Scope-bound add bytes. Emits ASTER_EVENT_BLOB_ADDED on success.
/// Formats are RAW=0 or HASH_SEQ=1. Reads and imports return bytes/hash payloads;
/// protect and retain have empty payloads. Protection is temporary until retain.
/// Cancellation requests must collect the terminal event; commit may win the race.
/// # Safety
/// Input spans must be readable and out_operation writable for this call.
#[no_mangle]
pub unsafe extern "C" fn aster_blob_custody_add_bytes(
    runtime: aster_runtime_t,
    scope: aster_blob_custody_t,
    data: aster_bytes_t,
    format: u32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    let data = attempt!(in_slice(&data, "data")).to_vec();
    let format = attempt!(format_name(format));
    let bridge = attempt!(bridge_for(runtime));
    let lease = attempt!(lease(&bridge, scope));
    unsafe {
        spawn(
            bridge,
            scope,
            user_data,
            out_operation,
            false,
            move |_cancel| async move {
                let data = {
                    lease
                        .add_bytes_cancellable(data, format, Some(_cancel))
                        .await?
                        .into_bytes()
                };
                Ok(result(aster_event_kind_t::ASTER_EVENT_BLOB_ADDED, data))
            },
        )
    }
}

/// Scope-bound add path. Emits ASTER_EVENT_BLOB_ADDED on success.
/// Formats are RAW=0 or HASH_SEQ=1. Reads and imports return bytes/hash payloads;
/// protect and retain have empty payloads. Protection is temporary until retain.
/// Cancellation requests must collect the terminal event; commit may win the race.
/// # Safety
/// Input spans must be readable and out_operation writable for this call.
#[no_mangle]
pub unsafe extern "C" fn aster_blob_custody_add_path(
    runtime: aster_runtime_t,
    scope: aster_blob_custody_t,
    path: aster_bytes_t,
    format: u32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    let path = attempt!(in_string(&path, "path"));
    let format = attempt!(format_name(format));
    let bridge = attempt!(bridge_for(runtime));
    let lease = attempt!(lease(&bridge, scope));
    unsafe {
        spawn(
            bridge,
            scope,
            user_data,
            out_operation,
            false,
            move |_cancel| async move {
                let data = {
                    lease
                        .add_path_cancellable(path, format, Some(_cancel))
                        .await?
                        .into_bytes()
                };
                Ok(result(aster_event_kind_t::ASTER_EVENT_BLOB_ADDED, data))
            },
        )
    }
}

/// Scope-bound retain. Emits ASTER_EVENT_BLOB_CUSTODY_DONE on success.
/// Formats are RAW=0 or HASH_SEQ=1. Reads and imports return bytes/hash payloads;
/// protect and retain have empty payloads. Protection is temporary until retain.
/// Cancellation requests must collect the terminal event; commit may win the race.
/// # Safety
/// Input spans must be readable and out_operation writable for this call.
#[no_mangle]
pub unsafe extern "C" fn aster_blob_custody_retain(
    runtime: aster_runtime_t,
    scope: aster_blob_custody_t,
    hash: aster_bytes_t,
    format: u32,
    tag: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    let hash = attempt!(in_hex32(&hash, "hash"));
    let format = attempt!(format_name(format));
    let tag = attempt!(in_string(&tag, "tag"));
    if tag.is_empty() {
        return invalid("tag must not be empty");
    }
    let bridge = attempt!(bridge_for(runtime));
    let lease = attempt!(lease(&bridge, scope));
    unsafe {
        spawn(
            bridge,
            scope,
            user_data,
            out_operation,
            true,
            move |cancel| async move {
                let data = {
                    lease.retain(hash, format, tag, Some(cancel)).await?;
                    Vec::new()
                };
                Ok(result(
                    aster_event_kind_t::ASTER_EVENT_BLOB_CUSTODY_DONE,
                    data,
                ))
            },
        )
    }
}

/// Scope-bound read. Emits ASTER_EVENT_BLOB_READ on success.
/// Formats are RAW=0 or HASH_SEQ=1. Reads and imports return bytes/hash payloads;
/// protect and retain have empty payloads. Protection is temporary until retain.
/// Cancellation requests must collect the terminal event; commit may win the race.
/// # Safety
/// Input spans must be readable and out_operation writable for this call.
#[no_mangle]
pub unsafe extern "C" fn aster_blob_custody_read(
    runtime: aster_runtime_t,
    scope: aster_blob_custody_t,
    hash: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    let hash = attempt!(in_hex32(&hash, "hash"));
    let bridge = attempt!(bridge_for(runtime));
    let lease = attempt!(lease(&bridge, scope));
    unsafe {
        spawn(
            bridge,
            scope,
            user_data,
            out_operation,
            false,
            move |_cancel| async move {
                let data = { lease.read_cancellable(hash, Some(_cancel)).await? };
                Ok(result(aster_event_kind_t::ASTER_EVENT_BLOB_READ, data))
            },
        )
    }
}

/// Copy data into the store, verify its expected hash, then commit exactly
/// the named tag with the requested format. No anonymous persistent root is
/// created. Hash mismatch returns VERIFICATION_FAILED and leaves the tag unchanged.
/// Emits BLOB_ADDED with hash hex. Cancellation before commit cleans up; once
/// commit starts, collect its actual success/error rather than assuming cancellation.
/// # Safety
/// Input spans must be readable and out_operation writable for this call.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_blobs_add_bytes_verified(
    runtime: aster_runtime_t,
    node: aster_node_t,
    data: aster_bytes_t,
    expected_hash: aster_bytes_t,
    format: u32,
    tag: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    let data = attempt!(in_slice(&data, "data")).to_vec();
    let expected = attempt!(in_hex32(&expected_hash, "expected hash"));
    let format = attempt!(format_name(format));
    let tag = attempt!(in_string(&tag, "tag"));
    if tag.is_empty() {
        return invalid("tag must not be empty");
    }
    let bridge = attempt!(bridge_for(runtime));
    let Some(owner) = bridge.nodes.get(node) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let client = owner.blobs_client();
    unsafe {
        spawn(
            bridge,
            node,
            user_data,
            out_operation,
            true,
            move |cancel| async move {
                let hash = client
                    .add_bytes_verified(data, expected, format, tag, Some(cancel))
                    .await?;
                Ok(result(
                    aster_event_kind_t::ASTER_EVENT_BLOB_ADDED,
                    hash.into_bytes(),
                ))
            },
        )
    }
}

/// Copy path into the store, verify its expected hash, then commit exactly
/// the named tag with the requested format. No anonymous persistent root is
/// created. Hash mismatch returns VERIFICATION_FAILED and leaves the tag unchanged.
/// Emits BLOB_ADDED with hash hex. Cancellation before commit cleans up; once
/// commit starts, collect its actual success/error rather than assuming cancellation.
/// # Safety
/// Input spans must be readable and out_operation writable for this call.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_blobs_add_path_verified(
    runtime: aster_runtime_t,
    node: aster_node_t,
    path: aster_bytes_t,
    expected_hash: aster_bytes_t,
    format: u32,
    tag: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    let path = attempt!(in_string(&path, "path"));
    let expected = attempt!(in_hex32(&expected_hash, "expected hash"));
    let format = attempt!(format_name(format));
    let tag = attempt!(in_string(&tag, "tag"));
    if tag.is_empty() {
        return invalid("tag must not be empty");
    }
    let bridge = attempt!(bridge_for(runtime));
    let Some(owner) = bridge.nodes.get(node) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let client = owner.blobs_client();
    unsafe {
        spawn(
            bridge,
            node,
            user_data,
            out_operation,
            true,
            move |cancel| async move {
                let hash = client
                    .add_path_verified(path, expected, format, tag, Some(cancel))
                    .await?;
                Ok(result(
                    aster_event_kind_t::ASTER_EVENT_BLOB_ADDED,
                    hash.into_bytes(),
                ))
            },
        )
    }
}

/// Fetch under temporary scope protection. Signature, flags, strategy and
/// BLOB_FETCHED report payload match aster_blobs_download_hash_from, replacing
/// the node handle with a custody scope. Protection is established before the
/// network request and remains in the scope afterward; no named tag is created.
/// # Safety
/// Input spans/list must be readable and out_operation writable for this call.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_blob_custody_download_hash_from(
    runtime: aster_runtime_t,
    scope: aster_blob_custody_t,
    hash: aster_bytes_t,
    providers: aster_bytes_list_t,
    format: u32,
    strategy: u32,
    flags: u32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    let hash = attempt!(in_hex32(&hash, "hash"));
    let providers = attempt!(in_node_list(&providers, "providers"));
    if providers.is_empty() {
        return invalid("at least one provider is required");
    }
    let format = attempt!(format_name(format));
    let strategy = attempt!(fetch_strategy(strategy));
    if strategy == aster::FetchStrategy::SplitChildren && format != "hash_seq" {
        return invalid("split children requires HashSeq format");
    }
    let to_store = attempt!(fetch_flags(flags));
    let providers = providers
        .into_iter()
        .map(|v| v.as_str().to_string())
        .collect();
    let strategy = match strategy {
        aster::FetchStrategy::Whole => CoreFetchStrategy::Whole,
        aster::FetchStrategy::SplitChildren => CoreFetchStrategy::SplitChildren,
    };
    let bridge = attempt!(bridge_for(runtime));
    let lease = attempt!(lease(&bridge, scope));
    unsafe {
        spawn(
            bridge,
            scope,
            user_data,
            out_operation,
            false,
            move |_cancel| async move {
                let report: aster::FetchReport = lease
                    .download_cancellable(
                        hash.clone(),
                        format,
                        providers,
                        strategy,
                        Some(_cancel.clone()),
                    )
                    .await?
                    .into();
                let bytes = if to_store {
                    Vec::new()
                } else {
                    lease.read_cancellable(hash, Some(_cancel)).await?
                };
                Ok(ResultData {
                    kind: aster_event_kind_t::ASTER_EVENT_BLOB_FETCHED,
                    flags: report.providers().len() as u32,
                    bytes: encode_fetch_report(&report, &bytes),
                })
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Gate the worker after admission so cancellation/commit ordering is
    // deterministic. A cancellation request cannot release the terminal slot
    // before native cleanup, and cannot overwrite an already-started commit.
    #[test]
    fn cooperative_cancel_waits_for_cleanup_and_preserves_committed_success() {
        for commits in [false, true] {
            let config = aster_runtime_config_t {
                struct_size: std::mem::size_of::<aster_runtime_config_t>() as u32,
                worker_threads: 1,
                event_queue_capacity: 1,
                max_inflight_operations: 1,
            };
            let mut runtime = 0;
            assert_eq!(
                unsafe { aster_runtime_new(crate::ASTER_ABI_VERSION_MAJOR, &config, &mut runtime) },
                0
            );
            let bridge = load_runtime(runtime).unwrap();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let mut operation = 0;
            assert_eq!(
                unsafe {
                    spawn(
                        bridge.clone(),
                        0,
                        123,
                        &mut operation,
                        commits,
                        move |flag| async move {
                            started_tx.send(()).unwrap();
                            release_rx.await.unwrap();
                            assert!(flag.load(Ordering::SeqCst));
                            Ok(result(
                                aster_event_kind_t::ASTER_EVENT_BLOB_CUSTODY_DONE,
                                Vec::new(),
                            ))
                        },
                    )
                },
                0
            );
            started_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert!(bridge.cancel_operation(operation));
            assert!(bridge.operations.get(operation).is_some());
            assert_eq!(bridge.operation_slots.available_permits(), 0);
            let mut event = std::mem::MaybeUninit::<aster_event_t>::uninit();
            assert_eq!(
                unsafe { aster_poll_events(runtime, event.as_mut_ptr(), 1, 0) },
                0
            );
            release_tx.send(()).unwrap();
            assert_eq!(
                unsafe { aster_poll_events(runtime, event.as_mut_ptr(), 1, 5000) },
                1
            );
            let event = unsafe { event.assume_init() };
            assert_eq!(event.operation, operation);
            assert_eq!(event.user_data, 123);
            assert_eq!(
                event.kind,
                if commits {
                    aster_event_kind_t::ASTER_EVENT_BLOB_CUSTODY_DONE as u32
                } else {
                    aster_event_kind_t::ASTER_EVENT_OPERATION_CANCELLED as u32
                }
            );
            assert!(!bridge.cancel_operation(operation));
            assert_eq!(bridge.operation_slots.available_permits(), 1);
            drop(bridge);
            assert_eq!(
                aster_runtime_close_and_wait(runtime, std::time::Duration::from_secs(10)),
                0
            );
        }
    }
}
