//! ABI 3.3 (additive, tranche F3): data-plane parity with the Rust SDK.
//!
//! Blobs (ordered multi-provider fetch with a [`aster::FetchReport`], ranged
//! reads, store-only downloads, tag prefix delete, one-shot GC), docs
//! (authority report / fingerprint, retiring local write authority, role-swap
//! shutdown, durable writes, `entry_exists`, entry content status, open and
//! import of write/read namespaces) and net (connection paths, selected path,
//! send-stream priority).
//!
//! Blob and node-level docs operations go through the `aster` crate facade
//! (`aster::Node::from_core_node(..).blobs()` / `.docs()`), so they share its
//! argument validation and semantics. Operations on an open document handle
//! and on connections/streams use the transport-core objects the C ABI
//! already holds (the `aster` wrappers over them are one-line delegations and
//! cannot be built from a core handle).
//!
//! # Encodings
//!
//! Every multi-field result is either a `#[repr(C)]` struct (paths) or a
//! fixed, little-endian byte layout in the event payload, documented on the
//! function and below. No JSON.
//!
//! **Fetch report v1** (`ASTER_FETCH_REPORT_VERSION` = 1), at the start of the
//! `ASTER_EVENT_BLOB_FETCHED` payload:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 1 | version (= 1) |
//! | 1 | 3 | reserved (0) |
//! | 4 | 4 | `N` = provider count (u32) |
//! | 8 | 8 | bytes transferred (u64) |
//! | 16 | 44 × N | provider records, in first-attempt order |
//!
//! Each provider record (`ASTER_FETCH_PROVIDER_RECORD_SIZE` = 44 bytes): the
//! 32-byte raw node id, then `attempts`, `failures`, `completed_requests`
//! (u32 each). Without `ASTER_FETCH_TO_STORE`, the blob bytes follow the
//! report directly (offset `16 + 44 × N` to the end of the payload).

use std::ffi::c_void;
use std::future::Future;
use std::slice;
use std::sync::Arc;
use std::time::Duration;

use aster::{
    AuthorityFingerprint, BlobFormat, FetchReport, FetchStrategy, Hash, LocalAccess, NamespaceId,
    NamespaceSecret, NodeId, PathInfo, PathRemote, RetireOutcome,
};
use aster_transport_core::CoreBlobStatus;

use crate::{
    aster_bytes_list_t, aster_bytes_t, aster_connection_t, aster_event_kind_t, aster_node_t,
    aster_operation_t, aster_path_kind_t, aster_runtime_t, aster_send_stream_t, aster_status_t,
    check_cancelled, load_runtime, set_last_error, BridgeRuntime, EventInternal,
};

// ── enums and constants ─────────────────────────────────────────────────────

/// Blob wire format (`format` argument of the 3.3 blob downloads).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_blob_format_t {
    ASTER_BLOB_FORMAT_RAW = 0,
    /// A HashSeq (collection).
    ASTER_BLOB_FORMAT_HASH_SEQ = 1,
}

/// How a multi-provider fetch decomposes a HashSeq (`aster::FetchStrategy`).
/// The provider list is walked in order with failover in both variants.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_fetch_strategy_t {
    /// One request; providers tried strictly in order.
    ASTER_FETCH_STRATEGY_WHOLE = 0,
    /// Root, then each child as its own request (up to 32 concurrently); the
    /// order applies per child. Needs `ASTER_BLOB_FORMAT_HASH_SEQ`.
    ASTER_FETCH_STRATEGY_SPLIT_CHILDREN = 1,
}

/// Local storage status of a blob (same codes as `aster_blobs_status`).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_blob_status_t {
    ASTER_BLOB_STATUS_NOT_FOUND = 0,
    ASTER_BLOB_STATUS_PARTIAL = 1,
    ASTER_BLOB_STATUS_COMPLETE = 2,
}

/// What access a namespace grants this node (`aster::LocalAccess`). The
/// values equal the access byte of the authority fingerprint.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_local_access_t {
    ASTER_LOCAL_ACCESS_WRITE = 1,
    ASTER_LOCAL_ACCESS_READ = 2,
}

/// `event.flags` of `ASTER_EVENT_DOC_RETIRE_OUTCOME` (`aster::RetireOutcome`).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_retire_outcome_t {
    /// Retired; payload carries the access now stored.
    ASTER_RETIRE_RETIRED = 0,
    /// Refused: references to the document are still open; payload carries
    /// how many. Normally fail-closed, not something to retry through.
    ASTER_RETIRE_BUSY = 1,
}

/// `flags` bit of the 3.3 blob downloads: leave the blob in the local store
/// and return no bytes (avoids the whole-blob copy on large blobs).
pub const ASTER_FETCH_TO_STORE: u32 = 1;
/// Version byte of the fetch-report encoding (v1, all integers
/// little-endian): byte 0 version, bytes 1..4 reserved (0), u32 provider
/// count `N` at 4, u64 bytes transferred at 8, then `N` provider records of
/// `ASTER_FETCH_PROVIDER_RECORD_SIZE` bytes in first-attempt order: 32-byte
/// raw node id, u32 attempts, u32 failures, u32 completed_requests. A blob's
/// bytes, when returned, follow the last record.
pub const ASTER_FETCH_REPORT_VERSION: u8 = 1;
/// Size of the fetch-report header (before the provider records).
pub const ASTER_FETCH_REPORT_HEADER_SIZE: usize = 16;
/// Size of one provider record in a fetch report.
pub const ASTER_FETCH_PROVIDER_RECORD_SIZE: usize = 44;
/// Size of an authority fingerprint (BLAKE3).
pub const ASTER_AUTHORITY_FINGERPRINT_SIZE: usize = 32;

// ── structs ─────────────────────────────────────────────────────────────────

/// One network path of a connection (`aster::PathInfo`). Strings point into
/// the owning `aster_path_list_t`'s storage; `{NULL, 0}` when absent; not
/// NUL-terminated.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_path_info_t {
    /// `sizeof(aster_path_info_t)`.
    pub struct_size: u32,
    /// One of `aster_path_kind_t` (`DIRECT`, `RELAY` or `OTHER`).
    pub path_kind: u32,
    /// 1 if this path currently carries application data.
    pub is_selected: u32,
    /// DIRECT: the peer's UDP port.
    pub peer_port: u16,
    pub _pad: u16,
    /// DIRECT: the peer's IP address.
    pub peer_host: aster_bytes_t,
    /// RELAY: the relay URL.
    pub relay_url: aster_bytes_t,
    /// OTHER: a description of a custom/future transport.
    pub other: aster_bytes_t,
    /// RTT estimate in microseconds; `UINT64_MAX` when not yet measured.
    pub rtt_micros: u64,
}

/// A list of paths filled by `aster_connection_paths` /
/// `aster_connection_selected_path`. Release with `aster_path_list_release`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_path_list_t {
    /// `sizeof(aster_path_list_t)`; written by the library.
    pub struct_size: u32,
    pub _pad: u32,
    /// `len` entries, owned by `storage`; NULL when `len == 0`.
    pub items: *const aster_path_info_t,
    pub len: usize,
    /// Library-owned backing store; NULL when nothing was allocated.
    pub storage: *mut c_void,
}

// ── helpers ─────────────────────────────────────────────────────────────────

const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;

const EMPTY: aster_bytes_t = aster_bytes_t {
    ptr: std::ptr::null(),
    len: 0,
};

pub(crate) fn invalid(msg: impl ToString) -> i32 {
    set_last_error(msg);
    aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
}

pub(crate) fn not_found(msg: impl ToString) -> i32 {
    set_last_error(msg);
    aster_status_t::ASTER_STATUS_NOT_FOUND as i32
}

macro_rules! try_status {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(s) => return s,
        }
    };
}

pub(crate) fn bridge_for(runtime: aster_runtime_t) -> Result<Arc<BridgeRuntime>, i32> {
    load_runtime(runtime).map_err(|s| s as i32)
}

pub(crate) unsafe fn in_slice<'a>(b: &aster_bytes_t, what: &str) -> Result<&'a [u8], i32> {
    if b.len == 0 {
        Ok(&[])
    } else if b.ptr.is_null() {
        Err(invalid(format!(
            "{what}: NULL pointer with length {}",
            b.len
        )))
    } else {
        Ok(slice::from_raw_parts(b.ptr, b.len))
    }
}

pub(crate) unsafe fn in_string(b: &aster_bytes_t, what: &str) -> Result<String, i32> {
    let s = in_slice(b, what)?;
    std::str::from_utf8(s)
        .map(str::to_owned)
        .map_err(|e| invalid(format!("{what}: invalid UTF-8: {e}")))
}

/// A 64-char hex id (hash, node id, author, namespace), lower-cased.
pub(crate) unsafe fn in_hex32(b: &aster_bytes_t, what: &str) -> Result<String, i32> {
    let s = in_slice(b, what)?;
    if s.len() != 64 || !s.iter().all(u8::is_ascii_hexdigit) {
        return Err(invalid(format!("{what}: expected 64 hex characters")));
    }
    Ok(String::from_utf8_lossy(s).to_ascii_lowercase())
}

/// A node id: 64 hex characters that decode to a valid ed25519 public key.
unsafe fn in_node_id(b: &aster_bytes_t, what: &str) -> Result<NodeId, i32> {
    let hex = in_hex32(b, what)?;
    hex.parse::<iroh::EndpointId>()
        .map_err(|e| invalid(format!("{what}: not a valid node id: {e}")))?;
    Ok(NodeId::from_hex(hex))
}

unsafe fn in_namespace(b: &aster_bytes_t) -> Result<NamespaceId, i32> {
    let hex = in_hex32(b, "namespace_id")?;
    NamespaceId::from_hex(&hex).map_err(invalid)
}

pub(crate) unsafe fn in_node_list(l: &aster_bytes_list_t, what: &str) -> Result<Vec<NodeId>, i32> {
    if l.len == 0 {
        return Ok(Vec::new());
    }
    if l.items.is_null() {
        return Err(invalid(format!("{what}: NULL items with length {}", l.len)));
    }
    slice::from_raw_parts(l.items, l.len)
        .iter()
        .enumerate()
        .map(|(i, b)| in_node_id(b, &format!("{what}[{i}]")))
        .collect()
}

pub(crate) fn blob_format(v: u32) -> Result<BlobFormat, i32> {
    match v {
        0 => Ok(BlobFormat::Raw),
        1 => Ok(BlobFormat::HashSeq),
        _ => Err(invalid(format!("unknown aster_blob_format_t {v}"))),
    }
}

pub(crate) fn fetch_strategy(v: u32) -> Result<FetchStrategy, i32> {
    match v {
        0 => Ok(FetchStrategy::Whole),
        1 => Ok(FetchStrategy::SplitChildren),
        _ => Err(invalid(format!("unknown aster_fetch_strategy_t {v}"))),
    }
}

pub(crate) fn fetch_flags(v: u32) -> Result<bool, i32> {
    if v & !ASTER_FETCH_TO_STORE != 0 {
        return Err(invalid(format!("unknown fetch flags {v:#x}")));
    }
    Ok(v & ASTER_FETCH_TO_STORE != 0)
}

fn local_access_code(a: LocalAccess) -> u32 {
    match a {
        LocalAccess::Write => aster_local_access_t::ASTER_LOCAL_ACCESS_WRITE as u32,
        LocalAccess::Read => aster_local_access_t::ASTER_LOCAL_ACCESS_READ as u32,
    }
}

fn blob_status_parts(s: CoreBlobStatus) -> (u32, u64) {
    match s {
        CoreBlobStatus::NotFound => (aster_blob_status_t::ASTER_BLOB_STATUS_NOT_FOUND as u32, 0),
        CoreBlobStatus::Partial { size } => {
            (aster_blob_status_t::ASTER_BLOB_STATUS_PARTIAL as u32, size)
        }
        CoreBlobStatus::Complete { size } => {
            (aster_blob_status_t::ASTER_BLOB_STATUS_COMPLETE as u32, size)
        }
    }
}

fn node_facade(bridge: &BridgeRuntime, node: aster_node_t) -> Result<aster::Node, i32> {
    match bridge.nodes.get(node) {
        Some(core) => Ok(aster::Node::from_core_node(core.as_ref().clone())),
        None => Err(not_found(format!("unknown node handle {node}"))),
    }
}

/// Encode a fetch report (v1, see the module docs) followed by `data`.
pub fn encode_fetch_report(report: &FetchReport, data: &[u8]) -> Vec<u8> {
    let providers = report.providers();
    let mut out = Vec::with_capacity(
        ASTER_FETCH_REPORT_HEADER_SIZE
            + ASTER_FETCH_PROVIDER_RECORD_SIZE * providers.len()
            + data.len(),
    );
    out.push(ASTER_FETCH_REPORT_VERSION);
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&(providers.len() as u32).to_le_bytes());
    out.extend_from_slice(&report.bytes_transferred().to_le_bytes());
    for p in providers {
        let mut id = [0u8; 32];
        // Ids come from iroh endpoint ids, so they are always 32-byte hex.
        let _ = hex::decode_to_slice(p.node_id().as_str(), &mut id);
        out.extend_from_slice(&id);
        out.extend_from_slice(&p.attempts().to_le_bytes());
        out.extend_from_slice(&p.failures().to_le_bytes());
        out.extend_from_slice(&p.completed_requests().to_le_bytes());
    }
    out.extend_from_slice(data);
    out
}

/// The outcome of one async operation, turned into a completion event.
pub(crate) struct Done {
    kind: aster_event_kind_t,
    status: aster_status_t,
    handle: u64,
    related: u64,
    flags: u32,
    data: Option<Vec<u8>>,
    /// `error_code` of the event; `None`: derived from `status`.
    code: Option<i32>,
    /// Registry owning `handle`, when success inserted a resource before the
    /// cancellation/completion race was resolved.
    owned_handle: Option<OwnedHandle>,
}

enum OwnedHandle {
    Doc,
    /// Frees a resource registered outside the runtime's own registries.
    Discard(Box<dyn FnOnce() + Send>),
}

impl Done {
    pub(crate) fn ok(kind: aster_event_kind_t, handle: u64) -> Self {
        Done {
            kind,
            status: aster_status_t::ASTER_STATUS_OK,
            handle,
            related: 0,
            flags: 0,
            data: None,
            code: None,
            owned_handle: None,
        }
    }
    pub(crate) fn owned_doc(mut self) -> Self {
        self.owned_handle = Some(OwnedHandle::Doc);
        self
    }
    /// `free` releases the resource success registered under `handle` if the
    /// completion loses to cancellation (the caller never sees the handle).
    pub(crate) fn on_discard(mut self, free: impl FnOnce() + Send + 'static) -> Self {
        self.owned_handle = Some(OwnedHandle::Discard(Box::new(free)));
        self
    }
    pub(crate) fn related(mut self, related: u64) -> Self {
        self.related = related;
        self
    }
    pub(crate) fn flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }
    pub(crate) fn data(mut self, data: Vec<u8>) -> Self {
        self.data = Some(data);
        self
    }
    pub(crate) fn status(mut self, status: aster_status_t) -> Self {
        self.status = status;
        self
    }
    /// Set the event's `error_code` explicitly.
    pub(crate) fn code(mut self, code: i32) -> Self {
        self.code = Some(code);
        self
    }
}

/// A failed operation with a specific status: `ASTER_EVENT_ERROR` with
/// `status`, `error_code` and the message as payload.
pub(crate) struct OpError {
    pub(crate) status: aster_status_t,
    pub(crate) code: i32,
    /// The event's `flags` (a detail code; 0 by default).
    pub(crate) flags: u32,
    pub(crate) message: String,
}

impl From<String> for OpError {
    fn from(message: String) -> Self {
        OpError {
            status: aster_status_t::ASTER_STATUS_INTERNAL,
            code: -1,
            flags: 0,
            message,
        }
    }
}

/// Register an operation, write its id to `*out_operation`, and run `fut` on
/// the runtime; its result becomes one completion event (`ASTER_EVENT_ERROR`
/// with the message as payload on `Err`).
pub(crate) unsafe fn spawn_op<F>(
    bridge: &Arc<BridgeRuntime>,
    user_data: u64,
    out_operation: *mut aster_operation_t,
    fut: F,
) -> i32
where
    F: Future<Output = Result<Done, String>> + Send + 'static,
{
    spawn_op_coded(bridge, user_data, out_operation, async move {
        fut.await.map_err(OpError::from)
    })
}

/// [`spawn_op`] whose failures carry their own status ([`OpError`]).
pub(crate) unsafe fn spawn_op_coded<F>(
    bridge: &Arc<BridgeRuntime>,
    user_data: u64,
    out_operation: *mut aster_operation_t,
    fut: F,
) -> i32
where
    F: Future<Output = Result<Done, OpError>> + Send + 'static,
{
    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    *out_operation = op_id;
    run_op(bridge, op_id, cancelled, user_data, fut);
    OK
}

/// Wakes an operation's future when the host cancels it or the runtime
/// closes, so a long wait (a revocation, a renewal outcome) gives up without
/// consuming what it waited for.
pub(crate) struct CancelSignal {
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
    closing: Arc<std::sync::atomic::AtomicBool>,
    close_notify: Arc<tokio::sync::Notify>,
}

impl CancelSignal {
    /// Resolves once the operation is cancelled or the runtime is closing.
    pub(crate) async fn wait(&self) {
        let cancelled = self.notify.notified();
        let closed = self.close_notify.notified();
        tokio::pin!(cancelled, closed);
        // `notify_waiters` keeps no permit: register before testing the flags.
        cancelled.as_mut().enable();
        closed.as_mut().enable();
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire)
            || self.closing.load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        tokio::select! {
            _ = cancelled => {}
            _ = closed => {}
        }
    }
}

/// [`spawn_op_coded`] whose future is built from the operation's
/// [`CancelSignal`]. A future that returns after the signal fires completes
/// nothing (the cancellation event was already queued).
pub(crate) unsafe fn spawn_op_cancellable<M, F>(
    bridge: &Arc<BridgeRuntime>,
    user_data: u64,
    out_operation: *mut aster_operation_t,
    make: M,
) -> i32
where
    M: FnOnce(CancelSignal) -> F,
    F: Future<Output = Result<Done, OpError>> + Send + 'static,
{
    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    // Shutdown can clear the registry right after admission.
    let Some(notify) = bridge
        .operations
        .get(op_id)
        .map(|operation| operation.cancel_notify.clone())
    else {
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    };
    let signal = CancelSignal {
        cancelled: cancelled.clone(),
        notify,
        closing: bridge.closing.clone(),
        close_notify: bridge.close_notify.clone(),
    };
    *out_operation = op_id;
    run_op(bridge, op_id, cancelled, user_data, make(signal));
    OK
}

fn run_op<F>(
    bridge: &Arc<BridgeRuntime>,
    op_id: u64,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    user_data: u64,
    fut: F,
) where
    F: Future<Output = Result<Done, OpError>> + Send + 'static,
{
    let b = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &b, op_id, user_data) {
            return;
        }
        match fut.await {
            Ok(d) => {
                let code = d
                    .code
                    .unwrap_or(if d.status == aster_status_t::ASTER_STATUS_OK {
                        0
                    } else {
                        d.status as i32
                    });
                let mut ev = EventInternal::new(
                    d.kind, d.status, op_id, d.handle, d.related, user_data, code,
                );
                ev.flags = d.flags;
                let completed = match d.data {
                    Some(data) => b.emit_with_data(ev, data),
                    None => b.complete(ev, None),
                };
                if !completed {
                    match d.owned_handle {
                        Some(OwnedHandle::Doc) => {
                            b.docs.remove(d.handle);
                        }
                        Some(OwnedHandle::Discard(free)) => free(),
                        None => {}
                    }
                }
            }
            Err(e) => b.emit_error_flagged(op_id, user_data, e.status, e.code, e.flags, &e.message),
        }
    });
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// ── Blobs ───────────────────────────────────────────────────────────────────

/// Download a blob by hash from an **ordered** list of providers
/// (`aster::Blobs::download_hash_from` / `download_hash_to_store_from`).
///
/// Providers (64-hex node ids, at least one) are tried strictly in the given
/// order, moving on only when one fails to connect or transfer; each attempt
/// asks only for the ranges still missing. Duplicates are legal and are
/// aggregated in the report. The node must be able to reach each provider
/// (relay, discovery, or an address added with `aster_add_node_addr`).
///
/// `format`: `aster_blob_format_t`; `strategy`: `aster_fetch_strategy_t`
/// (`SPLIT_CHILDREN` requires `HASH_SEQ`); `flags`: 0 or
/// `ASTER_FETCH_TO_STORE`. Bad arguments return
/// `ASTER_STATUS_INVALID_ARGUMENT` before anything starts.
///
/// Emits `ASTER_EVENT_BLOB_FETCHED` (`handle` = node, `flags` = provider
/// count); payload = fetch report v1 then, without `ASTER_FETCH_TO_STORE`, the
/// blob bytes. A failed fetch emits `ASTER_EVENT_ERROR`.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_blobs_download_hash_from(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex: aster_bytes_t,
    providers: aster_bytes_list_t,
    format: u32,
    strategy: u32,
    flags: u32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_blobs_download_hash_from: out_operation is NULL");
    }
    let hash = Hash::from_hex(try_status!(in_hex32(&hash_hex, "hash")));
    let from = try_status!(in_node_list(&providers, "providers"));
    if from.is_empty() {
        return invalid("providers: at least one provider is required");
    }
    let format = try_status!(blob_format(format));
    let strategy = try_status!(fetch_strategy(strategy));
    if strategy == FetchStrategy::SplitChildren && format != BlobFormat::HashSeq {
        return invalid("ASTER_FETCH_STRATEGY_SPLIT_CHILDREN needs ASTER_BLOB_FORMAT_HASH_SEQ");
    }
    let to_store = try_status!(fetch_flags(flags));
    let bridge = try_status!(bridge_for(runtime));
    let blobs = try_status!(node_facade(&bridge, node)).blobs();
    spawn_op(&bridge, user_data, out_operation, async move {
        let (data, report) = if to_store {
            let r = blobs
                .download_hash_to_store_from(&hash, &from, format, strategy)
                .await
                .map_err(err)?;
            (Vec::new(), r)
        } else {
            blobs
                .download_hash_from(&hash, &from, format, strategy)
                .await
                .map_err(err)?
        };
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_BLOB_FETCHED, node)
            .flags(report.providers().len() as u32)
            .data(encode_fetch_report(&report, &data)))
    })
}

/// Download a blob by hash from one provider (`aster::Blobs::download_hash` /
/// `download_hash_to_store`). `format`: `aster_blob_format_t`; `flags`: 0 or
/// `ASTER_FETCH_TO_STORE`.
///
/// Emits `ASTER_EVENT_BLOB_DOWNLOADED` (`handle` = node) with the blob bytes
/// as payload (empty with `ASTER_FETCH_TO_STORE`).
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_blobs_download_hash(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex: aster_bytes_t,
    provider: aster_bytes_t,
    format: u32,
    flags: u32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_blobs_download_hash: out_operation is NULL");
    }
    let hash = Hash::from_hex(try_status!(in_hex32(&hash_hex, "hash")));
    let from = try_status!(in_node_id(&provider, "provider"));
    let format = try_status!(blob_format(format));
    let to_store = try_status!(fetch_flags(flags));
    let bridge = try_status!(bridge_for(runtime));
    let blobs = try_status!(node_facade(&bridge, node)).blobs();
    spawn_op(&bridge, user_data, out_operation, async move {
        let data = if to_store {
            blobs
                .download_hash_to_store(&hash, &from, format)
                .await
                .map_err(err)?;
            Vec::new()
        } else {
            blobs
                .download_hash(&hash, &from, format)
                .await
                .map_err(err)?
        };
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_BLOB_DOWNLOADED, node).data(data))
    })
}

/// Read `len` bytes at `offset` of a blob in the local store
/// (`aster::Blobs::read_range`). Emits `ASTER_EVENT_BLOB_READ` with the bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_read_range(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex: aster_bytes_t,
    offset: u64,
    len: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_blobs_read_range: out_operation is NULL");
    }
    let hash = Hash::from_hex(try_status!(in_hex32(&hash_hex, "hash")));
    let bridge = try_status!(bridge_for(runtime));
    let blobs = try_status!(node_facade(&bridge, node)).blobs();
    spawn_op(&bridge, user_data, out_operation, async move {
        let data = blobs.read_range(&hash, offset, len).await.map_err(err)?;
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_BLOB_READ, node).data(data))
    })
}

/// Delete every tag whose name starts with `prefix` (UTF-8;
/// `aster::Blobs::tag_delete_prefix`). An empty prefix deletes **all** tags.
/// Emits `ASTER_EVENT_TAG_DELETED`: payload = removed count (u64 LE),
/// `flags` = the count saturated to u32.
#[no_mangle]
pub unsafe extern "C" fn aster_tags_delete_prefix(
    runtime: aster_runtime_t,
    node: aster_node_t,
    prefix: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_tags_delete_prefix: out_operation is NULL");
    }
    let prefix = try_status!(in_string(&prefix, "prefix"));
    let bridge = try_status!(bridge_for(runtime));
    let blobs = try_status!(node_facade(&bridge, node)).blobs();
    spawn_op(&bridge, user_data, out_operation, async move {
        let n = blobs.tag_delete_prefix(prefix).await.map_err(err)?;
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_TAG_DELETED, node)
            .flags(n.min(u32::MAX as u64) as u32)
            .data(n.to_le_bytes().to_vec()))
    })
}

/// Run exactly one blob GC mark+sweep pass now (`aster::Blobs::gc_run_once`).
/// Node-wide: reclaims every blob not protected by a tag. Emits
/// `ASTER_EVENT_UNIT_RESULT` when the pass completes.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_gc_run_once(
    runtime: aster_runtime_t,
    node: aster_node_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_blobs_gc_run_once: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let blobs = try_status!(node_facade(&bridge, node)).blobs();
    spawn_op(&bridge, user_data, out_operation, async move {
        blobs.gc_run_once().await.map_err(err)?;
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_UNIT_RESULT, node))
    })
}

// ── Docs: authority ─────────────────────────────────────────────────────────

fn authority_done(handle: u64, report: aster::AuthorityReport) -> Done {
    let mut payload = Vec::with_capacity(64);
    payload.extend_from_slice(&report.namespace().to_bytes());
    payload.extend_from_slice(report.fingerprint().as_bytes());
    Done::ok(aster_event_kind_t::ASTER_EVENT_DOC_AUTHORITY_REPORT, handle)
        .flags(local_access_code(report.access()))
        .data(payload)
}

/// Report the authority this node holds over the namespace `namespace_id`
/// (64 hex) **names** (`aster::Docs::authority_report`) — read back from the
/// store, so it is the access the node has, not what was requested.
///
/// Emits `ASTER_EVENT_DOC_AUTHORITY_REPORT` (`handle` = node, `flags` =
/// `aster_local_access_t`); payload = 32-byte namespace id then the 32-byte
/// authority fingerprint. A namespace not in the local store is an
/// `ASTER_EVENT_ERROR` ("cannot prove" never looks like "proved read-only").
/// To gate on an open document use `aster_doc_authority_report`.
#[no_mangle]
pub unsafe extern "C" fn aster_docs_authority_report(
    runtime: aster_runtime_t,
    node: aster_node_t,
    namespace_id: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_docs_authority_report: out_operation is NULL");
    }
    let ns = try_status!(in_namespace(&namespace_id));
    let bridge = try_status!(bridge_for(runtime));
    let docs = try_status!(node_facade(&bridge, node)).docs();
    spawn_op(&bridge, user_data, out_operation, async move {
        let report = docs.authority_report(ns).await.map_err(err)?;
        Ok(authority_done(node, report))
    })
}

/// As `aster_docs_authority_report`, for the namespace of the open document
/// `doc` (taken from the document, not from an argument; `aster::Doc::
/// authority_report`). `node` is the node that opened `doc`.
/// `event.handle` = doc.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_authority_report(
    runtime: aster_runtime_t,
    node: aster_node_t,
    doc: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_doc_authority_report: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let docs = try_status!(node_facade(&bridge, node)).docs();
    let Some(d) = bridge.docs.get(doc) else {
        return not_found(format!("unknown doc handle {doc}"));
    };
    let ns = try_status!(NamespaceId::from_hex(&d.doc_id()).map_err(invalid));
    spawn_op(&bridge, user_data, out_operation, async move {
        let report = docs.authority_report(ns).await.map_err(err)?;
        Ok(authority_done(doc, report))
    })
}

/// The authority fingerprint of `(namespace_id, access)` — the value an
/// authority report carries (`aster::AuthorityFingerprint::of`), so a caller
/// can derive it from its own grant record. A correlation value, not proof
/// over secret material. Writes 32 bytes to `out`. Synchronous and pure.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_authority_fingerprint(
    namespace_id: aster_bytes_t,
    access: u32,
    out: *mut u8,
) -> i32 {
    if out.is_null() {
        return invalid("aster_doc_authority_fingerprint: out is NULL");
    }
    let ns = try_status!(in_namespace(&namespace_id));
    let access = match access {
        1 => LocalAccess::Write,
        2 => LocalAccess::Read,
        v => return invalid(format!("unknown aster_local_access_t {v}")),
    };
    let fp = AuthorityFingerprint::of(ns, access);
    std::ptr::copy_nonoverlapping(fp.as_bytes().as_ptr(), out, 32);
    OK
}

/// Retire this node's **local** write authority over a namespace, keeping
/// every entry (`aster::Docs::retire_local_write_authority`). Local only:
/// revokes nothing cryptographically. Release every reference first
/// (`aster_doc_shutdown_for_role_swap`), otherwise the outcome is BUSY.
///
/// Emits `ASTER_EVENT_DOC_RETIRE_OUTCOME` (`handle` = node, `flags` =
/// `aster_retire_outcome_t`); 16-byte payload: u32 access now stored
/// (`aster_local_access_t`, 0 when BUSY), u32 reserved, u64 open references
/// (0 when RETIRED). Busy is a successful event, not an error.
#[no_mangle]
pub unsafe extern "C" fn aster_docs_retire_local_write_authority(
    runtime: aster_runtime_t,
    node: aster_node_t,
    namespace_id: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_docs_retire_local_write_authority: out_operation is NULL");
    }
    let ns = try_status!(in_namespace(&namespace_id));
    let bridge = try_status!(bridge_for(runtime));
    let docs = try_status!(node_facade(&bridge, node)).docs();
    spawn_op(&bridge, user_data, out_operation, async move {
        let outcome = docs.retire_local_write_authority(ns).await.map_err(err)?;
        let (kind, access, handles) = match outcome {
            RetireOutcome::Retired(a) => (
                aster_retire_outcome_t::ASTER_RETIRE_RETIRED,
                local_access_code(a),
                0u64,
            ),
            RetireOutcome::Busy { handles } => {
                (aster_retire_outcome_t::ASTER_RETIRE_BUSY, 0, handles)
            }
        };
        let mut payload = Vec::with_capacity(16);
        payload.extend_from_slice(&access.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&handles.to_le_bytes());
        Ok(
            Done::ok(aster_event_kind_t::ASTER_EVENT_DOC_RETIRE_OUTCOME, node)
                .flags(kind as u32)
                .data(payload),
        )
    })
}

/// Release **both** references of the open document `doc` (live sync and
/// the handle itself) under one deadline of `budget_ms`, so its namespace
/// can change role (`aster::Doc::shutdown_for_role_swap`). The handle is
/// freed immediately (as by `aster_doc_free`) and is invalid afterwards;
/// other handles to the same namespace keep it open. Emits
/// `ASTER_EVENT_UNIT_RESULT` (`handle` = doc) or `ASTER_EVENT_ERROR`.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_shutdown_for_role_swap(
    runtime: aster_runtime_t,
    doc: u64,
    budget_ms: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_doc_shutdown_for_role_swap: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let Some(d) = bridge.docs.remove(doc) else {
        return not_found(format!("unknown doc handle {doc}"));
    };
    bridge
        .registry_acls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&doc);
    let d = d.as_ref().clone();
    spawn_op(&bridge, user_data, out_operation, async move {
        d.shutdown_for_role_swap(Duration::from_millis(budget_ms))
            .await
            .map_err(err)?;
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_UNIT_RESULT, doc))
    })
}

// ── Docs: open / import ─────────────────────────────────────────────────────

/// Reopen a namespace already in the local replica (`aster::Docs::open`).
/// Emits `ASTER_EVENT_DOC_OPENED`: `handle` = new doc handle, `related` =
/// node; or, if the namespace is not present, the same kind with status
/// `ASTER_STATUS_NOT_FOUND` and `handle` = 0.
#[no_mangle]
pub unsafe extern "C" fn aster_docs_open(
    runtime: aster_runtime_t,
    node: aster_node_t,
    namespace_id: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_docs_open: out_operation is NULL");
    }
    let ns = try_status!(in_namespace(&namespace_id));
    let bridge = try_status!(bridge_for(runtime));
    let Some(core) = bridge.nodes.get(node) else {
        return not_found(format!("unknown node handle {node}"));
    };
    let docs = core.docs_client();
    let b = bridge.clone();
    spawn_op(&bridge, user_data, out_operation, async move {
        Ok(match docs.open(ns.to_hex()).await.map_err(err)? {
            Some(d) => Done::ok(aster_event_kind_t::ASTER_EVENT_DOC_OPENED, b.docs.insert(d))
                .related(node)
                .owned_doc(),
            None => Done::ok(aster_event_kind_t::ASTER_EVENT_DOC_OPENED, 0)
                .related(node)
                .status(aster_status_t::ASTER_STATUS_NOT_FOUND),
        })
    })
}

/// Open-or-import a **writable** namespace from its 32-byte secret (raw
/// bytes; `aster::Docs::open_or_import_write_namespace`). Idempotent;
/// upgrades a prior read-only import. Emits `ASTER_EVENT_DOC_OPENED`
/// (`handle` = doc, `related` = node).
#[no_mangle]
pub unsafe extern "C" fn aster_docs_import_write_namespace(
    runtime: aster_runtime_t,
    node: aster_node_t,
    secret: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_docs_import_write_namespace: out_operation is NULL");
    }
    let raw = try_status!(in_slice(&secret, "secret"));
    let Ok(arr) = <[u8; 32]>::try_from(raw) else {
        return invalid("secret: expected 32 bytes");
    };
    let secret = NamespaceSecret::from_bytes(arr);
    let bridge = try_status!(bridge_for(runtime));
    let Some(core) = bridge.nodes.get(node) else {
        return not_found(format!("unknown node handle {node}"));
    };
    let docs = core.docs_client();
    let b = bridge.clone();
    spawn_op(&bridge, user_data, out_operation, async move {
        let d = docs
            .import_write_namespace(secret.to_bytes().to_vec())
            .await
            .map_err(err)?;
        Ok(
            Done::ok(aster_event_kind_t::ASTER_EVENT_DOC_OPENED, b.docs.insert(d))
                .related(node)
                .owned_doc(),
        )
    })
}

/// Open-or-import a **read-only** namespace from its id (64 hex;
/// `aster::Docs::open_or_import_read_namespace`). Idempotent; never narrows
/// an existing write capability. Emits `ASTER_EVENT_DOC_OPENED`.
#[no_mangle]
pub unsafe extern "C" fn aster_docs_import_read_namespace(
    runtime: aster_runtime_t,
    node: aster_node_t,
    namespace_id: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_docs_import_read_namespace: out_operation is NULL");
    }
    let ns = try_status!(in_namespace(&namespace_id));
    let bridge = try_status!(bridge_for(runtime));
    let Some(core) = bridge.nodes.get(node) else {
        return not_found(format!("unknown node handle {node}"));
    };
    let docs = core.docs_client();
    let b = bridge.clone();
    spawn_op(&bridge, user_data, out_operation, async move {
        let d = docs
            .import_read_namespace(ns.to_bytes().to_vec())
            .await
            .map_err(err)?;
        Ok(
            Done::ok(aster_event_kind_t::ASTER_EVENT_DOC_OPENED, b.docs.insert(d))
                .related(node)
                .owned_doc(),
        )
    })
}

// ── Docs: entries ───────────────────────────────────────────────────────────

/// Write `key` = `value` as `author` (64 hex) after committing the blob
/// metadata to disk (`aster::Doc::set_bytes_durable`): the crash-durable
/// publication boundary (covers a process kill, not power loss). Emits
/// `ASTER_EVENT_DOC_SET` with the content hash (hex) as payload, like
/// `aster_doc_set_bytes`.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_set_bytes_durable(
    runtime: aster_runtime_t,
    doc: u64,
    author_hex: aster_bytes_t,
    key: aster_bytes_t,
    value: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_doc_set_bytes_durable: out_operation is NULL");
    }
    let author = try_status!(in_hex32(&author_hex, "author"));
    let key = try_status!(in_slice(&key, "key")).to_vec();
    let value = try_status!(in_slice(&value, "value")).to_vec();
    let bridge = try_status!(bridge_for(runtime));
    let Some(d) = bridge.docs.get(doc) else {
        return not_found(format!("unknown doc handle {doc}"));
    };
    spawn_op(&bridge, user_data, out_operation, async move {
        let hash = d.set_bytes_durable(author, key, value).await.map_err(err)?;
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_DOC_SET, doc).data(hash.into_bytes()))
    })
}

/// Whether an entry exists for exactly `(author, key)` — **including** a
/// deletion tombstone (`aster::Doc::entry_exists`; `aster_doc_get_exact`
/// reports a tombstone as absent). Emits `ASTER_EVENT_DOC_ENTRY_EXISTS` with
/// `flags` = 1 (exists) or 0.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_entry_exists(
    runtime: aster_runtime_t,
    doc: u64,
    author_hex: aster_bytes_t,
    key: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_doc_entry_exists: out_operation is NULL");
    }
    let author = try_status!(in_hex32(&author_hex, "author"));
    let key = try_status!(in_slice(&key, "key")).to_vec();
    let bridge = try_status!(bridge_for(runtime));
    let Some(d) = bridge.docs.get(doc) else {
        return not_found(format!("unknown doc handle {doc}"));
    };
    spawn_op(&bridge, user_data, out_operation, async move {
        let exists = d.entry_exists(author, key).await.map_err(err)?;
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_DOC_ENTRY_EXISTS, doc).flags(exists as u32))
    })
}

/// Local storage status of a document entry's value blob
/// (`aster::Doc::entry_content_status`). Emits `ASTER_EVENT_DOC_CONTENT_STATUS`:
/// `flags` = `aster_blob_status_t`, payload = size in bytes (u64 LE; 0 when
/// not found).
#[no_mangle]
pub unsafe extern "C" fn aster_doc_entry_content_status(
    runtime: aster_runtime_t,
    doc: u64,
    content_hash_hex: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_doc_entry_content_status: out_operation is NULL");
    }
    let hash = try_status!(in_hex32(&content_hash_hex, "content_hash"));
    let bridge = try_status!(bridge_for(runtime));
    let Some(d) = bridge.docs.get(doc) else {
        return not_found(format!("unknown doc handle {doc}"));
    };
    spawn_op(&bridge, user_data, out_operation, async move {
        let (code, size) = blob_status_parts(d.entry_content_status(hash).await.map_err(err)?);
        Ok(
            Done::ok(aster_event_kind_t::ASTER_EVENT_DOC_CONTENT_STATUS, doc)
                .flags(code)
                .data(size.to_le_bytes().to_vec()),
        )
    })
}

/// Read ordered content hashes through one local blob-store batch. No network
/// fetch, document-membership check, tags, or retention mutation is performed.
/// Missing/partial content returns an absent slot; present empty bytes remain
/// distinct. Duplicates and input order are preserved. This is not an atomic
/// snapshot: GC or I/O failure during a large validated export fails the batch.
/// Use temporary blob custody when content must remain protected across reads.
///
/// Emits ASTER_EVENT_DOC_CONTENTS_BATCH (handle=doc, flags=0). Payload: u32 LE
/// count, then one u8 presence per slot. 0 has no following bytes; 1 is followed
/// by u64 LE byte length and that many bytes. No padding or trailing bytes.
/// Empty input emits four zero bytes. Release the event buffer normally.
/// All hashes must be 64 ASCII hex characters; invalid input is rejected before
/// operation admission. The list count must fit u32.
/// # Safety
/// Nonempty input lists and spans must be readable/aligned for this call;
/// out_operation must be writable. Input is copied before this function returns.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_read_entry_contents_if_complete(
    runtime: aster_runtime_t,
    doc: u64,
    content_hashes: aster_bytes_list_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("batch read: out_operation is NULL");
    }
    *out_operation = 0;
    if content_hashes.len > u32::MAX as usize
        || content_hashes.len > isize::MAX as usize / std::mem::size_of::<aster_bytes_t>()
    {
        return invalid("batch read: too many hashes");
    }
    let items = if content_hashes.len == 0 {
        &[][..]
    } else {
        if content_hashes.items.is_null() {
            return invalid("batch read: NULL hash list");
        }
        slice::from_raw_parts(content_hashes.items, content_hashes.len)
    };
    let mut hashes = Vec::with_capacity(items.len());
    for item in items {
        hashes.push(try_status!(in_hex32(item, "content hash")));
    }
    let bridge = try_status!(bridge_for(runtime));
    let Some(d) = bridge.docs.get(doc) else {
        return not_found(format!("unknown doc handle {doc}"));
    };
    spawn_op(&bridge, user_data, out_operation, async move {
        let values = d
            .read_entry_contents_if_complete(hashes)
            .await
            .map_err(err)?;
        Ok(
            Done::ok(aster_event_kind_t::ASTER_EVENT_DOC_CONTENTS_BATCH, doc)
                .data(encode_doc_contents_batch(values)?),
        )
    })
}

fn encode_doc_contents_batch(values: Vec<Option<Vec<u8>>>) -> Result<Vec<u8>, String> {
    let count = u32::try_from(values.len()).map_err(err)?;
    let mut size = 4usize;
    for value in &values {
        let slot = match value {
            None => 1,
            Some(bytes) => bytes.len().checked_add(9).ok_or("batch payload overflow")?,
        };
        size = size.checked_add(slot).ok_or("batch payload overflow")?;
    }
    let mut out = Vec::new();
    out.try_reserve_exact(size).map_err(err)?;
    out.extend_from_slice(&count.to_le_bytes());
    for value in values {
        match value {
            None => out.push(0),
            Some(bytes) => {
                out.push(1);
                out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                out.extend_from_slice(&bytes);
            }
        }
    }
    Ok(out)
}

// ── Net: paths ──────────────────────────────────────────────────────────────

struct PathStorage {
    _strings: Vec<String>,
    items: Vec<aster_path_info_t>,
}

fn view(s: &str) -> aster_bytes_t {
    if s.is_empty() {
        EMPTY
    } else {
        aster_bytes_t {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }
}

fn empty_list() -> aster_path_list_t {
    aster_path_list_t {
        struct_size: std::mem::size_of::<aster_path_list_t>() as u32,
        _pad: 0,
        items: std::ptr::null(),
        len: 0,
        storage: std::ptr::null_mut(),
    }
}

/// Lower paths into `out` (one allocation owning every string and entry).
fn fill_paths(paths: Vec<PathInfo>, out: &mut aster_path_list_t) {
    *out = empty_list();
    if paths.is_empty() {
        return;
    }
    // Strings first: their heap buffers do not move when the Vec is boxed.
    let mut strings: Vec<String> = Vec::with_capacity(paths.len());
    let mut parts = Vec::with_capacity(paths.len());
    for p in &paths {
        let (kind, port, text) = match &p.remote {
            PathRemote::Direct(addr) => (
                aster_path_kind_t::ASTER_PATH_KIND_DIRECT,
                addr.port(),
                addr.ip().to_string(),
            ),
            PathRemote::Relay(url) => (aster_path_kind_t::ASTER_PATH_KIND_RELAY, 0, url.clone()),
            PathRemote::Other(s) => (aster_path_kind_t::ASTER_PATH_KIND_OTHER, 0, s.clone()),
        };
        parts.push((kind, port, p.is_selected, p.rtt_micros));
        strings.push(text);
    }
    let items = parts
        .iter()
        .zip(&strings)
        .map(|(&(kind, port, selected, rtt), text)| {
            let s = view(text);
            aster_path_info_t {
                struct_size: std::mem::size_of::<aster_path_info_t>() as u32,
                path_kind: kind as u32,
                is_selected: selected as u32,
                peer_port: port,
                _pad: 0,
                peer_host: if kind == aster_path_kind_t::ASTER_PATH_KIND_DIRECT {
                    s
                } else {
                    EMPTY
                },
                relay_url: if kind == aster_path_kind_t::ASTER_PATH_KIND_RELAY {
                    s
                } else {
                    EMPTY
                },
                other: if kind == aster_path_kind_t::ASTER_PATH_KIND_OTHER {
                    s
                } else {
                    EMPTY
                },
                rtt_micros: rtt.unwrap_or(u64::MAX),
            }
        })
        .collect();
    let storage = Box::new(PathStorage {
        _strings: strings,
        items,
    });
    out.items = storage.items.as_ptr();
    out.len = storage.items.len();
    out.storage = Box::into_raw(storage) as *mut c_void;
}

unsafe fn connection_paths(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    out: *mut aster_path_list_t,
    selected_only: bool,
) -> i32 {
    if out.is_null() {
        return invalid("out is NULL");
    }
    *out = empty_list();
    let bridge = try_status!(bridge_for(runtime));
    let Some(conn) = bridge.connections.get(connection) else {
        return not_found(format!("unknown connection handle {connection}"));
    };
    let mut paths = conn.paths();
    if selected_only {
        paths.retain(|p| p.is_selected);
        paths.truncate(1);
    }
    fill_paths(paths, &mut *out);
    OK
}

/// Snapshot every currently open network path of `connection`
/// (`aster::Connection::paths`): typically a relay path plus a direct one
/// after holepunching. Synchronous. Fills `*out`; release it with
/// `aster_path_list_release` (also when `len == 0`).
#[no_mangle]
pub unsafe extern "C" fn aster_connection_paths(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    out: *mut aster_path_list_t,
) -> i32 {
    connection_paths(runtime, connection, out, false)
}

/// The path currently carrying application data
/// (`aster::Connection::selected_path`): `*out` holds 0 or 1 entries.
/// Synchronous; release with `aster_path_list_release`.
#[no_mangle]
pub unsafe extern "C" fn aster_connection_selected_path(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    out: *mut aster_path_list_t,
) -> i32 {
    connection_paths(runtime, connection, out, true)
}

/// Free a path list filled by `aster_connection_paths` /
/// `aster_connection_selected_path` and zero it (`struct_size` kept). NULL or
/// an already released list is a no-op.
#[no_mangle]
pub unsafe extern "C" fn aster_path_list_release(list: *mut aster_path_list_t) -> i32 {
    if list.is_null() {
        return OK;
    }
    let l = &mut *list;
    if !l.storage.is_null() {
        drop(Box::from_raw(l.storage as *mut PathStorage));
    }
    l.storage = std::ptr::null_mut();
    l.items = std::ptr::null();
    l.len = 0;
    OK
}

// ── Net: stream priority ────────────────────────────────────────────────────

/// Set the send priority of `stream` (higher is scheduled first when send
/// streams on one connection compete; default 0; `aster::SendStream::
/// set_priority`). Emits `ASTER_EVENT_UNIT_RESULT` (`handle` = stream), or
/// `ASTER_EVENT_ERROR` if the stream is already closed.
#[no_mangle]
pub unsafe extern "C" fn aster_send_stream_set_priority(
    runtime: aster_runtime_t,
    stream: aster_send_stream_t,
    priority: i32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_send_stream_set_priority: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let Some(s) = bridge.send_streams.get(stream) else {
        return not_found(format!("unknown send stream handle {stream}"));
    };
    spawn_op(&bridge, user_data, out_operation, async move {
        s.set_priority(priority).await.map_err(err)?;
        Ok(Done::ok(
            aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
            stream,
        ))
    })
}

/// The current send priority of `stream`. Emits `ASTER_EVENT_STREAM_PRIORITY`
/// (`handle` = stream): payload = the priority (i32 LE); `flags` = the same
/// value's bit pattern.
#[no_mangle]
pub unsafe extern "C" fn aster_send_stream_priority(
    runtime: aster_runtime_t,
    stream: aster_send_stream_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_send_stream_priority: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let Some(s) = bridge.send_streams.get(stream) else {
        return not_found(format!("unknown send stream handle {stream}"));
    };
    spawn_op(&bridge, user_data, out_operation, async move {
        let p = s.priority().await.map_err(err)?;
        Ok(
            Done::ok(aster_event_kind_t::ASTER_EVENT_STREAM_PRIORITY, stream)
                .flags(p as u32)
                .data(p.to_le_bytes().to_vec()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_report_encoding_is_stable() {
        let report = FetchReport::from(aster_transport_core::CoreFetchReport {
            providers: vec![aster_transport_core::CoreProviderOutcome {
                node_id: "0a".repeat(32),
                attempts: 3,
                failures: 2,
                completed_requests: 1,
                plain_fallbacks: 0,
                in_flight: 0,
                bytes_transferred: 0x0102_0304_0506_0708,
                wire_bytes: None,
            }],
            bytes_transferred: 0x0102_0304_0506_0708,
        });
        let enc = encode_fetch_report(&report, b"xyz");
        let mut want = vec![1u8, 0, 0, 0, 1, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1];
        want.extend_from_slice(&[0x0a; 32]);
        want.extend_from_slice(&[3, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0]);
        want.extend_from_slice(b"xyz");
        assert_eq!(enc, want);
        assert_eq!(
            enc.len(),
            ASTER_FETCH_REPORT_HEADER_SIZE + ASTER_FETCH_PROVIDER_RECORD_SIZE + 3
        );
    }

    #[test]
    fn hex_and_enum_validation() {
        let good = "AB".repeat(32);
        let b = aster_bytes_t {
            ptr: good.as_ptr(),
            len: good.len(),
        };
        assert_eq!(unsafe { in_hex32(&b, "x") }.unwrap(), "ab".repeat(32));
        let short = aster_bytes_t {
            ptr: good.as_ptr(),
            len: 10,
        };
        assert!(unsafe { in_hex32(&short, "x") }.is_err());
        assert!(blob_format(2).is_err());
        assert!(fetch_strategy(7).is_err());
        assert!(fetch_flags(2).is_err());
        assert!(fetch_flags(ASTER_FETCH_TO_STORE).unwrap());
    }

    #[test]
    fn path_list_lowering_and_release() {
        let paths = vec![
            PathInfo {
                remote: PathRemote::Direct("127.0.0.1:4242".parse().unwrap()),
                is_selected: true,
                rtt_micros: Some(12),
            },
            PathInfo {
                remote: PathRemote::Relay("https://relay.example/".into()),
                is_selected: false,
                rtt_micros: None,
            },
            PathInfo {
                remote: PathRemote::Other("custom".into()),
                is_selected: false,
                rtt_micros: None,
            },
        ];
        let mut out = empty_list();
        fill_paths(paths, &mut out);
        assert_eq!(out.len, 3);
        let items = unsafe { slice::from_raw_parts(out.items, out.len) };
        let s = |b: &aster_bytes_t| unsafe { slice::from_raw_parts(b.ptr, b.len) }.to_vec();
        assert_eq!(
            items[0].path_kind,
            aster_path_kind_t::ASTER_PATH_KIND_DIRECT as u32
        );
        assert_eq!(s(&items[0].peer_host), b"127.0.0.1");
        assert_eq!(items[0].peer_port, 4242);
        assert_eq!(items[0].is_selected, 1);
        assert_eq!(items[0].rtt_micros, 12);
        assert_eq!(
            items[1].path_kind,
            aster_path_kind_t::ASTER_PATH_KIND_RELAY as u32
        );
        assert_eq!(s(&items[1].relay_url), b"https://relay.example/");
        assert!(items[1].peer_host.ptr.is_null());
        assert_eq!(items[1].rtt_micros, u64::MAX);
        assert_eq!(
            items[2].path_kind,
            aster_path_kind_t::ASTER_PATH_KIND_OTHER as u32
        );
        assert_eq!(s(&items[2].other), b"custom");
        assert_eq!(unsafe { aster_path_list_release(&mut out) }, OK);
        assert!(out.storage.is_null() && out.items.is_null() && out.len == 0);
        assert_eq!(unsafe { aster_path_list_release(&mut out) }, OK);
    }

    #[test]
    fn fingerprint_matches_aster() {
        let ns = "11".repeat(32);
        let b = aster_bytes_t {
            ptr: ns.as_ptr(),
            len: ns.len(),
        };
        let mut out = [0u8; 32];
        assert_eq!(
            unsafe { aster_doc_authority_fingerprint(b, 1, out.as_mut_ptr()) },
            OK
        );
        let want =
            AuthorityFingerprint::of(NamespaceId::from_bytes([0x11; 32]), LocalAccess::Write);
        assert_eq!(&out, want.as_bytes());
        assert_ne!(
            unsafe { aster_doc_authority_fingerprint(b, 3, out.as_mut_ptr()) },
            OK
        );
    }
}
