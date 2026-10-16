//! Dynamic dispatch server (ABI 3.5): a binding registers services as JSON
//! and answers calls by id; Rust serves them through `aster::rpc::Server`.
//!
//! Design: `docs/_internal/bindings/dynamic-dispatch-design.md` §2 and §4.4.
//!
//! ```text
//!   aster_server_builder_new(rt, node)            builder over a started node
//!   aster_server_add_service_json(b, json) → idx  DynamicService JSON (§1.5)
//!   aster_server_set_option(b, key, value)        baseline services, caps
//!   aster_server_start(b, ring_capacity) → server serve: reactor → Server
//!                                                 (Gate 3, sessions, deadlines,
//!                                                 frame limits) → RingSink
//!   aster_server_poll(server, events[], max, ms)  drain DispatchEvents
//!   aster_dispatch_*(server, call_id, …)          answer by call id
//!   aster_server_destroy(server)
//! ```
//!
//! Since 4.6, `aster_server_attach_health(builder, health)` ties an ops
//! listener's readiness to the server (ready once serving, not ready first on
//! destroy), and a call whose server span is exported carries
//! `ASTER_DISPATCH_FLAG_HAS_TRACE`: `aster_dispatch_trace_context` returns
//! the span's context for the binding's handler spans and logs. A call that
//! carried W3C `baggage` or `tracestate` (hidden from its metadata) has
//! `ASTER_DISPATCH_FLAG_HAS_INBOUND_TRACE`, exported span or not:
//! `aster_dispatch_inbound_trace` returns them as a k/v list.
//!
//! Rust does all framing, compression and status encoding: the binding sees
//! request payloads (already decompressed) and passes response payloads plus
//! AEv1 `RpcStatus` bytes (`NULL` = OK). Every call in a `CALL` event is
//! detached into the server's `CallTable`: it stays there until a terminal
//! `respond` / `finish`, `abandon`, a peer cancel or the deadline, so a
//! binding that drops a call id leaks it only until its deadline.

use std::collections::HashMap;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster::binding_support::dispatch::{
    CallTable, CancelReason, DispatchEvent, ForeignCall, RingConsumer, RingSink, SubmitError,
};
use aster::ops::HealthState;
use aster::rpc::{
    decode_rpc_status, AttributeStore, Dispatcher, DynamicService, MethodPattern, RpcStatus,
    Server, ServerHandle,
};
use aster_transport_core::{CoreConnection, CoreNode};

use crate::health::aster_health_t;
use crate::observability::aster_span_context_t;
use crate::{
    aster_buffer_t, aster_bytes_t, aster_node_t, aster_runtime_t, aster_status_t, load_runtime,
    set_last_error, BridgeRuntime, HandleRegistry,
};

// ============================================================================
// Types
// ============================================================================

/// A server builder (`aster_server_builder_new`); consumed by
/// `aster_server_start`.
pub type aster_server_builder_t = u64;
/// A running dispatch server (`aster_server_start`).
pub type aster_server_t = u64;

/// `aster_dispatch_event_t.kind`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_dispatch_event_kind_t {
    /// A call that passed every gate. Fields: all except `reason`.
    ASTER_DISPATCH_EVENT_CALL = 0,
    /// The call ended without the binding finishing it (`reason`). Rust
    /// already answered the peer (deadline, corrupt request) or there is no
    /// one to answer (peer cancel). Later operations return `NOT_FOUND`.
    ASTER_DISPATCH_EVENT_CANCELLED = 1,
    /// A reservation (`aster_dispatch_reserve`) is held: retry the exact
    /// operation that returned `QUEUE_FULL`.
    ASTER_DISPATCH_EVENT_RESPONSE_WRITABLE = 2,
    /// A pending or held reservation ended because the call did.
    ASTER_DISPATCH_EVENT_RESPONSE_CLOSED = 3,
    /// A session was reaped (`session_key`): drop its instance.
    ASTER_DISPATCH_EVENT_SESSION_CLOSED = 4,
    /// An `aster/1` connection closed (`connection_id`, `peer`). Its
    /// `SESSION_CLOSED` events come first. `connection_handle` is the handle
    /// the connection's calls carried, already invalid.
    ASTER_DISPATCH_EVENT_CONNECTION_CLOSED = 5,
    /// A connection on a non-RPC ALPN that no Rust handler claimed. `request` = the ALPN; `connection_handle` is
    /// **owned** by the binding (free it with `aster_connection_free`).
    ASTER_DISPATCH_EVENT_NON_RPC = 6,
}

/// `aster_dispatch_event_t.reason` on `CANCELLED`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_cancel_reason_t {
    /// The peer sent `FLAG_CANCEL`, or the stream failed. No trailer is sent.
    ASTER_CANCEL_REASON_PEER = 0,
    /// The deadline passed; Rust sent `DEADLINE_EXCEEDED`.
    ASTER_CANCEL_REASON_DEADLINE = 1,
    /// A later request frame was corrupt; Rust sent `INVALID_ARGUMENT`.
    ASTER_CANCEL_REASON_INVALID_REQUEST = 2,
}

/// `aster_dispatch_event_t.pattern` (= `MethodPattern`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_method_pattern_t {
    ASTER_METHOD_PATTERN_UNARY = 0,
    ASTER_METHOD_PATTERN_SERVER_STREAM = 1,
    ASTER_METHOD_PATTERN_CLIENT_STREAM = 2,
    ASTER_METHOD_PATTERN_BIDI_STREAM = 3,
}

/// Keys of `aster_server_set_option`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_server_option_t {
    /// Serve `aster.ops.NodeInfo` (0/1; default 1).
    ASTER_SERVER_OPTION_BUILTIN_NODE_INFO = 1,
    /// Serve `aster.net.Topology` (0/1; default 0). Empty views unless the
    /// node runs with monitoring.
    ASTER_SERVER_OPTION_BUILTIN_TOPOLOGY = 2,
    /// Active sessions one connection may hold (default 1024; ≥ 1).
    ASTER_SERVER_OPTION_MAX_SESSIONS_PER_CONNECTION = 3,
    /// Cap on any handler's run time in ms (default 300000); 0 = no cap.
    ASTER_SERVER_OPTION_HANDLER_TIMEOUT_MS = 4,
    /// Per-kind bound for live foreign calls, sessions, RPC connections and
    /// transferred non-RPC connections (default 4096; at least 1).
    ASTER_SERVER_OPTION_MAX_FOREIGN_INFLIGHT = 5,
}

/// `aster_dispatch_event_t.flags`: the first request frame ended the request
/// stream; `aster_dispatch_recv` has nothing more.
pub const ASTER_DISPATCH_FLAG_FIRST_IS_END: u32 = 0x1;
/// First call of a new session (`session_key`).
pub const ASTER_DISPATCH_FLAG_NEW_SESSION: u32 = 0x2;
/// The peer id was authenticated by the iroh handshake.
pub const ASTER_DISPATCH_FLAG_IROH_AUTHENTICATED: u32 = 0x4;
/// The call header carries metadata (`aster_dispatch_metadata`).
pub const ASTER_DISPATCH_FLAG_HAS_METADATA: u32 = 0x100;
/// The caller has attributes (`aster_dispatch_attributes`).
pub const ASTER_DISPATCH_FLAG_HAS_ATTRIBUTES: u32 = 0x200;
/// ABI 4.6: the call's server span is exported; its context is available
/// from `aster_dispatch_trace_context`.
pub const ASTER_DISPATCH_FLAG_HAS_TRACE: u32 = 0x400;
/// ABI 4.6: the call carried W3C `baggage` and/or `tracestate`, which are
/// hidden from its metadata; read them with `aster_dispatch_inbound_trace`.
pub const ASTER_DISPATCH_FLAG_HAS_INBOUND_TRACE: u32 = 0x800;

/// `timeout_ms` value for "wait without a limit" (`aster_dispatch_recv`,
/// `aster_call_recv_payload`).
pub const ASTER_TIMEOUT_INFINITE: u32 = u32::MAX;

/// One dispatch event, filled by `aster_server_poll`. 104 bytes (LP64).
///
/// `request` and `peer` point into one lease, `buffer`, valid until
/// `aster_dispatch_buffer_release(server, buffer)` (0 = nothing to release).
/// Fields a kind does not use are zero / NULL.
#[repr(C)]
pub struct aster_dispatch_event_t {
    /// `aster_dispatch_event_kind_t`.
    pub kind: u8,
    /// `CALL`: `aster_method_pattern_t`.
    pub pattern: u8,
    /// `CANCELLED`: `aster_cancel_reason_t`.
    pub reason: u8,
    pub _p0: u8,
    /// `CALL`: `ASTER_DISPATCH_FLAG_*`.
    pub flags: u32,
    /// `CALL`, `CANCELLED`, `RESPONSE_*`: the key for every
    /// `aster_dispatch_*` operation. Unique per server, from 1.
    pub call_id: u64,
    /// `CALL`: registration index (`aster_server_add_service_json` order).
    pub service_idx: u32,
    /// `CALL`: index into the service's `methods`.
    pub method_idx: u32,
    /// `CALL` (0 = shared service), `SESSION_CLOSED`.
    pub session_key: u64,
    /// `CALL`, `CONNECTION_CLOSED`.
    pub connection_id: u64,
    /// `CALL`: borrowed `aster_connection_t` (do not free), the same for
    /// every call of the connection. `CONNECTION_CLOSED`: the same value,
    /// no longer valid. `NON_RPC`: owned by the binding.
    pub connection_handle: u64,
    /// `CALL`: when Rust cancels the call with `DEADLINE_EXCEEDED`, Unix ms;
    /// 0 = none.
    pub deadline_unix_ms: i64,
    /// `CALL`: `StreamHeader.call_id` (logging / request id).
    pub wire_call_id: i32,
    /// `CALL`: `StreamHeader.session_id` (the wire id; 0 = none). Rust keys
    /// the session by `session_key`; this is for logs and diagnostics.
    pub session_id: u32,
    /// `CALL`: first request payload, decompressed. `NON_RPC`: the ALPN.
    pub request_ptr: *const u8,
    pub request_len: u32,
    pub _p2: u32,
    /// `CALL`, `CONNECTION_CLOSED`: peer id (UTF-8, not NUL-terminated).
    pub peer_ptr: *const u8,
    pub peer_len: u32,
    pub _p3: u32,
    /// Lease for `request` + `peer`; 0 when both are empty.
    pub buffer: u64,
}

impl aster_dispatch_event_t {
    fn empty(kind: aster_dispatch_event_kind_t) -> Self {
        Self {
            kind: kind as u8,
            pattern: 0,
            reason: 0,
            _p0: 0,
            flags: 0,
            call_id: 0,
            service_idx: 0,
            method_idx: 0,
            session_key: 0,
            connection_id: 0,
            connection_handle: 0,
            deadline_unix_ms: 0,
            wire_call_id: 0,
            session_id: 0,
            request_ptr: ptr::null(),
            request_len: 0,
            _p2: 0,
            peer_ptr: ptr::null(),
            peer_len: 0,
            _p3: 0,
            buffer: 0,
        }
    }
}

// ============================================================================
// State
// ============================================================================

struct BuilderSpec {
    node: CoreNode,
    services: Vec<DynamicService>,
    node_info: bool,
    topology: bool,
    topology_requires: Option<aster::rpc::CapabilityRequirement>,
    max_sessions: Option<usize>,
    handler_timeout: Option<Option<Duration>>,
    max_foreign_inflight: usize,
    health: Option<HealthState>,
    /// ABI 5.1: the Enrollment the node was started with
    /// (`aster_node_start_enrolled`), wired at start.
    enrollment: Option<Arc<crate::enrollment::EnrollmentState>>,
    /// ABI 5.1: `aster.lease.Grantor` services (`aster_server_add_lease_grantor`).
    lease_grantor: Option<aster::rpc::lease::LeaseGrantor>,
}

pub(crate) struct BuilderState {
    spec: Mutex<Option<BuilderSpec>>,
}

/// Bytes an event points into, alive until released. Moving a `Lease` (the
/// map rehashing) does not move the heap data the pointers refer to.
struct Lease {
    _request: Vec<u8>,
    _peer: Arc<str>,
}

const LEASE_SHARDS: usize = 16;

struct Leases {
    shards: [Mutex<HashMap<u64, Lease>>; LEASE_SHARDS],
    next: AtomicU64,
}

impl Leases {
    fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| Mutex::new(HashMap::new())),
            next: AtomicU64::new(1),
        }
    }

    /// Lease `request` and `peer`; returns the id (0 when both are empty)
    /// and the pointers.
    fn insert(&self, request: Vec<u8>, peer: Arc<str>) -> (u64, *const u8, u32, *const u8, u32) {
        if request.is_empty() && peer.is_empty() {
            return (0, ptr::null(), 0, ptr::null(), 0);
        }
        self.insert_always(request, peer)
    }

    /// As [`insert`](Self::insert), but always a real lease.
    fn insert_always(
        &self,
        request: Vec<u8>,
        peer: Arc<str>,
    ) -> (u64, *const u8, u32, *const u8, u32) {
        let (rp, rl) = slice_ptr(&request);
        let (pp, pl) = slice_ptr(peer.as_bytes());
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.shards[(id as usize) % LEASE_SHARDS]
            .lock()
            .unwrap()
            .insert(
                id,
                Lease {
                    _request: request,
                    _peer: peer,
                },
            );
        (id, rp, rl, pp, pl)
    }

    fn release(&self, id: u64) -> bool {
        self.shards[(id as usize) % LEASE_SHARDS]
            .lock()
            .unwrap()
            .remove(&id)
            .is_some()
    }

    fn clear(&self) {
        for shard in &self.shards {
            shard.lock().unwrap().clear();
        }
    }
}

fn slice_ptr(b: &[u8]) -> (*const u8, u32) {
    if b.is_empty() {
        (ptr::null(), 0)
    } else {
        (b.as_ptr(), b.len() as u32)
    }
}

/// The poll side: the ring's single consumer plus the connection-handle
/// cache (one borrowed `aster_connection_t` per `connection_id`).
struct PollState {
    consumer: RingConsumer,
    connections: HashMap<u64, u64>,
}

pub(crate) struct ServerState {
    pub(crate) runtime_owner: aster_runtime_t,
    pub(crate) dispatcher: Dispatcher,
    pub(crate) http_children: Mutex<HttpChildren>,
    poll: Mutex<PollState>,
    table: CallTable,
    attributes: AttributeStore,
    leases: Leases,
    pub(crate) handle: Mutex<Option<ServerHandle>>,
    rt: tokio::runtime::Handle,
    /// An attached ops listener's state (`aster_server_attach_health`).
    pub(crate) health: Option<HealthState>,
    /// Dropped with the server: ends Enrollment's per-connection tasks, which
    /// would otherwise keep the node (and its runtime) alive.
    _enrollment_stop: Option<tokio::sync::watch::Sender<()>>,
    /// The served `aster.ops.NodeInfo` identity (ABI 5.1,
    /// `aster_server_set_node_attribute`); `None` with the baseline off.
    node_identity: Option<aster::rpc::baseline::NodeIdentityHandle>,
}

#[derive(Default)]
pub(crate) struct HttpChildren {
    pub(crate) closed: bool,
    pub(crate) handles: Vec<u64>,
}

pub(crate) static BUILDERS: once_cell::sync::Lazy<HandleRegistry<BuilderState>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);
pub(crate) static SERVERS: once_cell::sync::Lazy<HandleRegistry<ServerState>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);

fn status(s: aster_status_t) -> i32 {
    s as i32
}

fn fail(s: aster_status_t, msg: impl ToString) -> i32 {
    set_last_error(msg);
    s as i32
}

unsafe fn bytes_in<'a>(p: *const u8, len: u32) -> &'a [u8] {
    if p.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: the caller passes `len` readable bytes at `p`.
        unsafe { std::slice::from_raw_parts(p, len as usize) }
    }
}

// ── k/v lists ───────────────────────────────────────────────────────────────

/// Encode `(key, value)` pairs as a k/v list: `u32 LE count`, then per entry
/// `u32 LE key_len`, key, `u32 LE value_len`, value (UTF-8). The format of
/// `aster_dispatch_metadata` / `_attributes`, `aster_call_options_t.metadata`
/// and `aster_server_set_peer_attributes`.
pub(crate) fn encode_kv<'a>(pairs: impl Iterator<Item = (&'a str, &'a str)>) -> Vec<u8> {
    let pairs: Vec<_> = pairs.collect();
    let mut out = Vec::with_capacity(
        4 + pairs
            .iter()
            .map(|(k, v)| 8 + k.len() + v.len())
            .sum::<usize>(),
    );
    out.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
    for (k, v) in pairs {
        out.extend_from_slice(&(k.len() as u32).to_le_bytes());
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(&(v.len() as u32).to_le_bytes());
        out.extend_from_slice(v.as_bytes());
    }
    out
}

/// Decode a k/v list (see [`encode_kv`]). Empty input = no entries.
pub(crate) fn decode_kv(mut b: &[u8]) -> Result<Vec<(String, String)>, String> {
    if b.is_empty() {
        return Ok(Vec::new());
    }
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8], String> {
        if b.len() < n {
            return Err("k/v list truncated".into());
        }
        let (head, rest) = b.split_at(n);
        *b = rest;
        Ok(head)
    }
    fn take_u32(b: &mut &[u8]) -> Result<usize, String> {
        let h = take(b, 4)?;
        Ok(u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize)
    }
    fn take_str(b: &mut &[u8]) -> Result<String, String> {
        let n = take_u32(b)?;
        let s = take(b, n)?;
        String::from_utf8(s.to_vec()).map_err(|_| "k/v list: invalid UTF-8".to_string())
    }
    let count = take_u32(&mut b)?;
    if count > b.len() / 8 {
        return Err("k/v list count exceeds its length".into());
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let k = take_str(&mut b)?;
        let v = take_str(&mut b)?;
        out.push((k, v));
    }
    if !b.is_empty() {
        return Err("k/v list has trailing bytes".into());
    }
    Ok(out)
}

// ============================================================================
// Builder
// ============================================================================

/// Start building a dispatch server on `node` (a node from
/// `aster_node_start` or `aster_node_start_enrolled`, which must have
/// `aster/1` among its ALPNs). Gate 0 stays with the node's gate; the server
/// runs Gate 3 (`requires`). On a node started by `aster_node_start_enrolled`
/// (server side) `aster_server_start` also runs the Enrollment server
/// sequence (design §15.4). `INVALID_ARGUMENT` for a client-side enrollment.
#[no_mangle]
pub unsafe extern "C" fn aster_server_builder_new(
    runtime: aster_runtime_t,
    node: aster_node_t,
    out_builder: *mut aster_server_builder_t,
) -> i32 {
    if out_builder.is_null() {
        return status(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT);
    }
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return status(s),
    };
    let Some(core) = bridge.nodes.get(node) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    let enrollment = match crate::enrollment::server_enrollment_for_node(&bridge, node) {
        Ok(enrollment) => enrollment,
        Err(code) => return code,
    };
    let id = BUILDERS.insert(BuilderState {
        spec: Mutex::new(Some(BuilderSpec {
            node: (*core).clone(),
            services: Vec::new(),
            node_info: true,
            topology: false,
            topology_requires: None,
            max_sessions: None,
            handler_timeout: None,
            max_foreign_inflight: aster::binding_support::dispatch::DEFAULT_MAX_FOREIGN_INFLIGHT,
            health: None,
            enrollment,
            lease_grantor: None,
        })),
    });
    // SAFETY: checked non-null above.
    unsafe { ptr::write(out_builder, id) };
    status(aster_status_t::ASTER_STATUS_OK)
}

/// Register one service from its JSON registration form (design §1.5;
/// `DynamicService`). Validated here; a `(name, version)` already added is
/// refused. `*out_idx` = the service's index in `CALL` events.
#[no_mangle]
pub unsafe extern "C" fn aster_server_add_service_json(
    builder: aster_server_builder_t,
    json: *const u8,
    json_len: u32,
    out_idx: *mut u32,
) -> i32 {
    let Some(state) = BUILDERS.get(builder) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    // SAFETY: caller passes `json_len` readable bytes.
    let raw = unsafe { bytes_in(json, json_len) };
    let Ok(text) = std::str::from_utf8(raw) else {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "service JSON is not UTF-8",
        );
    };
    let svc = match DynamicService::from_json(text) {
        Ok(s) => s,
        Err(e) => return fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, e),
    };
    let mut guard = state.spec.lock().unwrap();
    let Some(spec) = guard.as_mut() else {
        return fail(
            aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
            "builder already started",
        );
    };
    if spec.services.iter().any(|s| s.key() == svc.key()) {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            format!(
                "service '{}' v{} is already registered",
                svc.name, svc.version
            ),
        );
    }
    let idx = spec.services.len() as u32;
    spec.services.push(svc);
    if !out_idx.is_null() {
        // SAFETY: checked non-null.
        unsafe { ptr::write(out_idx, idx) };
    }
    status(aster_status_t::ASTER_STATUS_OK)
}

/// Configure the opt-in topology baseline service and its native Gate-3
/// capability requirement atomically (ABI 4.16). Enabling requires monitoring.
/// An empty span or JSON null means no additional capability requirement;
/// otherwise use the native CapabilityRequirement JSON (kind and roles).
/// The default is disabled. Remote peer views never expose remote_addr.
/// This does not change the legacy set_option behavior for existing callers.
/// # Safety
/// requirement_json must be readable for its advertised length during the call.
#[no_mangle]
pub unsafe extern "C" fn aster_server_set_topology(
    builder: aster_server_builder_t,
    enabled: u32,
    requirement_json: aster_bytes_t,
) -> i32 {
    if enabled > 1 {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "enabled must be 0 or 1",
        );
    }
    if requirement_json.len > 1024 * 1024 {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "topology requirement exceeds 1 MiB",
        );
    }
    let input = match unsafe { crate::data::in_slice(&requirement_json, "requirement_json") } {
        Ok(input) => input,
        Err(status) => return status,
    };
    let requirement = if input.is_empty() {
        None
    } else {
        match serde_json::from_slice::<Option<aster::rpc::CapabilityRequirement>>(input) {
            Ok(requirement) => requirement,
            Err(error) => {
                return fail(
                    aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
                    format!("invalid topology requirement: {error}"),
                )
            }
        }
    };
    let Some(state) = BUILDERS.get(builder) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    let mut guard = state.spec.lock().unwrap();
    let Some(spec) = guard.as_mut() else {
        return fail(
            aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
            "builder already started",
        );
    };
    if enabled == 1 && !spec.node.has_monitoring() {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "builtin topology requires monitoring",
        );
    }
    spec.topology = enabled == 1;
    spec.topology_requires = requirement;
    status(aster_status_t::ASTER_STATUS_OK)
}

/// Set one `aster_server_option_t`. Unknown keys and out-of-range values →
/// `INVALID_ARGUMENT`.
#[no_mangle]
pub extern "C" fn aster_server_set_option(
    builder: aster_server_builder_t,
    key: u32,
    value: u64,
) -> i32 {
    let Some(state) = BUILDERS.get(builder) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    let mut guard = state.spec.lock().unwrap();
    let Some(spec) = guard.as_mut() else {
        return fail(
            aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
            "builder already started",
        );
    };
    const NODE_INFO: u32 = aster_server_option_t::ASTER_SERVER_OPTION_BUILTIN_NODE_INFO as u32;
    const TOPOLOGY: u32 = aster_server_option_t::ASTER_SERVER_OPTION_BUILTIN_TOPOLOGY as u32;
    const MAX_SESSIONS: u32 =
        aster_server_option_t::ASTER_SERVER_OPTION_MAX_SESSIONS_PER_CONNECTION as u32;
    const TIMEOUT: u32 = aster_server_option_t::ASTER_SERVER_OPTION_HANDLER_TIMEOUT_MS as u32;
    const MAX_FOREIGN: u32 = aster_server_option_t::ASTER_SERVER_OPTION_MAX_FOREIGN_INFLIGHT as u32;
    match key {
        NODE_INFO | TOPOLOGY if value > 1 => {
            return fail(
                aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
                "boolean option must be 0 or 1",
            )
        }
        NODE_INFO => spec.node_info = value == 1,
        TOPOLOGY => spec.topology = value == 1,
        MAX_SESSIONS => {
            if value == 0 || value > u32::MAX as u64 {
                return fail(
                    aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
                    "max_sessions_per_connection must be in 1..=u32::MAX",
                );
            }
            spec.max_sessions = Some(value as usize);
        }
        TIMEOUT => {
            spec.handler_timeout = Some((value != 0).then(|| Duration::from_millis(value)));
        }
        MAX_FOREIGN => {
            if value == 0 || value > u32::MAX as u64 {
                return fail(
                    aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
                    "max_foreign_inflight must be in 1..=u32::MAX",
                );
            }
            spec.max_foreign_inflight = value as usize;
        }
        other => {
            return fail(
                aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
                format!("unknown server option {other}"),
            )
        }
    }
    status(aster_status_t::ASTER_STATUS_OK)
}

/// Start serving. Consumes `builder` (freed whatever the outcome). Events
/// flow through an SPSC ring of `ring_capacity` (rounded up to a power of
/// two; 0 = 256) drained by `aster_server_poll`. Fails with
/// `INVALID_ARGUMENT` when a service collides with a baseline one.
#[no_mangle]
pub unsafe extern "C" fn aster_server_start(
    runtime: aster_runtime_t,
    builder: aster_server_builder_t,
    ring_capacity: u32,
    out_server: *mut aster_server_t,
) -> i32 {
    if out_server.is_null() {
        return status(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT);
    }
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return status(s),
    };
    // Attaching Enrollment's policy blocks (below); check before removing
    // the builder so a misplaced call leaves it intact.
    let enrolled = BUILDERS.get(builder).is_some_and(|b| {
        b.spec
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|s| s.enrollment.is_some())
    });
    if enrolled {
        if let Some(error) =
            crate::http_server::blocking_on_runtime("aster_server_start (enrolled)")
        {
            return error;
        }
    }
    let Some(state) = BUILDERS.remove(builder) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    let Some(spec) = state.spec.lock().unwrap().take() else {
        return fail(
            aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
            "builder already started",
        );
    };
    let capacity = if ring_capacity == 0 {
        256
    } else {
        ring_capacity as usize
    };
    let rt = bridge.runtime.handle().clone();
    let _enter = rt.enter();

    let node = aster::Node::from_core_node(spec.node);
    let attributes = AttributeStore::new();
    let (sink, consumer) = RingSink::new_on_with_limit(&rt, capacity, spec.max_foreign_inflight);
    let mut server = Server::new(&node)
        .attributes(attributes.clone())
        .dispatch_sink(sink);
    if let Some(max) = spec.max_sessions {
        server = server.max_sessions_per_connection(max);
    }
    if let Some(timeout) = spec.handler_timeout {
        server = server.handler_timeout(timeout);
    }
    let mut node_identity = None;
    if spec.node_info {
        let (with_info, handle) = server.with_builtin_node_info(&node, Default::default(), None);
        server = with_info;
        node_identity = Some(handle);
    }
    if spec.topology {
        server = server.with_builtin_topology(&node, spec.topology_requires);
    }
    let mut enrollment_stop = None;
    if let Some(enrollment) = &spec.enrollment {
        let (stop, stopped) = tokio::sync::watch::channel(());
        server = enrollment.apply(server, &attributes, stopped);
        enrollment_stop = Some(stop);
    }
    if let Some(grantor) = spec.lease_grantor {
        server = server.register(grantor);
    }
    let services = spec.services.len() as u32;
    for svc in spec.services {
        server = match server.dynamic_service(svc) {
            Ok(s) => s,
            Err(e) => return fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, e),
        };
    }
    let table = server.call_table();
    let dispatcher = server.dispatcher();
    let handle = server.serve();

    // The acceptor runs: hold or synchronize the fleet namespace, then renew
    // (design §15.4 steps 5 and 6). Blocks up to `sync_timeout_ms` with
    // replicas; a failure stops the server (the binding closes the node).
    if let Some(enrollment) = &spec.enrollment {
        drop(_enter);
        if let Err(error) = bridge.runtime.block_on(enrollment.attach_server()) {
            handle.abort();
            return fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, error);
        }
    }

    let state = ServerState {
        runtime_owner: runtime,
        dispatcher,
        http_children: Mutex::new(HttpChildren::default()),
        poll: Mutex::new(PollState {
            consumer,
            connections: HashMap::new(),
        }),
        table,
        attributes,
        leases: Leases::new(),
        handle: Mutex::new(Some(handle)),
        rt: rt.clone(),
        health: spec.health.clone(),
        _enrollment_stop: enrollment_stop,
        node_identity,
    };
    let mut tracked = bridge
        .server_handles
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if bridge.closing.load(Ordering::Acquire) {
        if let Some(handle) = state.handle.lock().unwrap().take() {
            handle.abort();
        }
        return status(aster_status_t::ASTER_STATUS_ALREADY_CLOSED);
    }
    let id = SERVERS.insert(state);
    tracked.push(id);
    // Ready once serving (design §6), as `AsterServer` does.
    if let Some(h) = &spec.health {
        h.set_services(services);
        h.set_ready(true);
    }
    drop(tracked);
    // SAFETY: checked non-null.
    unsafe { ptr::write(out_server, id) };
    status(aster_status_t::ASTER_STATUS_OK)
}

/// Attach an ops listener (`aster_health_start`) to a server before
/// `aster_server_start`: a successful start sets it ready (and reports the
/// registered service count), `aster_server_destroy` sets it not ready first.
/// The built-in metrics record without it; this ties readiness only.
/// `NOT_FOUND` for an unknown builder or listener; `ALREADY_CLOSED` for a
/// builder already started.
#[no_mangle]
pub extern "C" fn aster_server_attach_health(
    builder: aster_server_builder_t,
    health: aster_health_t,
) -> i32 {
    let Some(state) = BUILDERS.get(builder) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    let Some(entry) = crate::health::attachable(health) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    let mut spec = state.spec.lock().unwrap();
    let Some(spec) = spec.as_mut() else {
        return fail(
            aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
            "builder already started",
        );
    };
    spec.health = Some(entry.state());
    status(aster_status_t::ASTER_STATUS_OK)
}

/// Parse an optional `CapabilityRequirement` JSON (`aster_server_set_topology`
/// format): an empty span keeps `default`, JSON `null` is none.
unsafe fn requirement_arg(
    json: &aster_bytes_t,
    what: &str,
    default: Option<aster::rpc::CapabilityRequirement>,
) -> Result<Option<aster::rpc::CapabilityRequirement>, i32> {
    if json.len > 1024 * 1024 {
        return Err(fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            format!("{what} exceeds 1 MiB"),
        ));
    }
    let input = crate::data::in_slice(json, what)?;
    if input.is_empty() {
        return Ok(default);
    }
    serde_json::from_slice::<Option<aster::rpc::CapabilityRequirement>>(input).map_err(|error| {
        fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            format!("invalid {what}: {error}"),
        )
    })
}

/// Serve `aster.lease.Grantor` (ABI 5.1; Rust `aster::rpc::lease::LeaseGrantor`)
/// on this server, backed by `serializer` (any `aster_lease_serializer_t`:
/// a foreign one answered by the binding over upcalls, or the in-memory
/// one). The grantor binds every operation to the caller's authenticated
/// node id, never a request field. `requires_json` gates every method
/// (CapabilityRequirement JSON; empty or `null` = none, admission is the
/// boundary); `revoke_requires_json` gates `revoke` (empty = the default
/// `operator` role, `null` = only `requires_json`). Clients acquire through
/// it with `aster_lease_serializer_grantor_new`. A later call replaces the
/// earlier one. `NOT_FOUND` for an unknown builder or serializer;
/// `ALREADY_CLOSED` for a builder already started.
///
/// # Safety
/// The JSON spans must be readable for their advertised lengths.
#[no_mangle]
pub unsafe extern "C" fn aster_server_add_lease_grantor(
    builder: aster_server_builder_t,
    serializer: crate::upcall::aster_lease_serializer_t,
    requires_json: aster_bytes_t,
    revoke_requires_json: aster_bytes_t,
) -> i32 {
    let requires = match requirement_arg(&requires_json, "requires_json", None) {
        Ok(requirement) => requirement,
        Err(code) => return code,
    };
    let revoke = match requirement_arg(
        &revoke_requires_json,
        "revoke_requires_json",
        Some(aster::rpc::require_role("operator")),
    ) {
        Ok(requirement) => requirement,
        Err(code) => return code,
    };
    let backend = match crate::upcall::serializer_for(serializer) {
        Ok(backend) => backend,
        Err(code) => return code,
    };
    let Some(state) = BUILDERS.get(builder) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    let mut spec = state.spec.lock().unwrap();
    let Some(spec) = spec.as_mut() else {
        return fail(
            aster_status_t::ASTER_STATUS_ALREADY_CLOSED,
            "builder already started",
        );
    };
    let mut grantor = aster::rpc::lease::LeaseGrantor::new(backend).revoke_requires(revoke);
    if let Some(requires) = requires {
        grantor = grantor.requires(requires);
    }
    spec.lease_grantor = Some(grantor);
    status(aster_status_t::ASTER_STATUS_OK)
}

/// Free a builder that was never started.
#[no_mangle]
pub extern "C" fn aster_server_builder_free(builder: aster_server_builder_t) -> i32 {
    match BUILDERS.remove(builder) {
        Some(_) => status(aster_status_t::ASTER_STATUS_OK),
        None => status(aster_status_t::ASTER_STATUS_NOT_FOUND),
    }
}

/// Stop serving and free the server. Events still in the ring are dropped
/// (their calls are abandoned with `INTERNAL`); calls the binding still holds
/// return `NOT_FOUND` afterwards; borrowed connection handles are freed.
/// Joins owned HTTP/WebTransport child listeners with zero grace, releasing
/// their ports on success. Blocks and is UNSUPPORTED on a tokio runtime thread
/// (checked before removal). Does not close the node.
#[no_mangle]
pub extern "C" fn aster_server_destroy(runtime: aster_runtime_t, server: aster_server_t) -> i32 {
    if let Some(error) = crate::http_server::blocking_on_runtime("aster_server_destroy") {
        return error;
    }
    let bridge = match load_runtime(runtime) {
        Ok(bridge) => bridge,
        Err(s) => return status(s),
    };
    let Some(owner) = SERVERS.get(server) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    if owner.runtime_owner != runtime {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    }
    let Some(state) = SERVERS.remove(server) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    bridge
        .server_handles
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|id| *id != server);
    // Not ready first (design §6): probes stop routing before calls fail.
    if let Some(h) = &state.health {
        h.set_ready(false);
    }
    // Seal listener admission before joining any child; a start that was
    // binding concurrently must recheck this flag before publishing its owner.
    let children = {
        let mut children = state
            .http_children
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        children.closed = true;
        std::mem::take(&mut children.handles)
    };
    let mut child_error = None;
    for child in children {
        if let Err(error) = crate::http_server::stop_child(&bridge, child, Duration::ZERO) {
            child_error.get_or_insert(error);
        }
    }
    if let Some(handle) = state.handle.lock().unwrap().take() {
        handle.abort();
    }
    {
        let _enter = state.rt.enter();
        let mut poll = state.poll.lock().unwrap();
        // Abandon what is still queued (CallHandle drop answers the peer).
        poll.consumer.drain(usize::MAX, drop);
        if let Ok(bridge) = load_runtime(runtime) {
            for (_, handle) in poll.connections.drain() {
                bridge.connections.remove(handle);
            }
        }
    }
    state.leases.clear();
    if let Some(error) = child_error {
        return fail(aster_status_t::ASTER_STATUS_INTERNAL, error);
    }
    status(aster_status_t::ASTER_STATUS_OK)
}

/// Runtime shutdown cannot block on its own workers. Remove global server
/// owners, seal their listener admission, abort the Iroh loops and drop state;
/// the runtime's listener registry is cleared next to force child cleanup.
pub(crate) fn shutdown_runtime(bridge: &BridgeRuntime) {
    let handles = std::mem::take(
        &mut *bridge
            .server_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    for id in handles {
        let Some(state) = SERVERS.remove(id) else {
            continue;
        };
        if let Some(health) = &state.health {
            health.set_ready(false);
        }
        state
            .http_children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed = true;
        if let Some(handle) = state
            .handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            handle.abort();
        }
        state
            .poll
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .consumer
            .drain(usize::MAX, drop);
        state.leases.clear();
    }
}

/// Attribute bridge (design §2.6): set the attributes Gate 3 sees for
/// `peer` (64-hex node id). `kv` is a k/v list; empty clears the entry.
/// For a peer with a live enrollment session the credential sets the maximum
/// roles: a list carrying `aster.role` is `INVALID_ARGUMENT` (use
/// `aster_server_restrict_peer_roles`).
#[no_mangle]
pub unsafe extern "C" fn aster_server_set_peer_attributes(
    server: aster_server_t,
    peer: *const u8,
    peer_len: u32,
    kv: *const u8,
    kv_len: u32,
) -> i32 {
    let Some(state) = SERVERS.get(server) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    // SAFETY: caller-provided lengths.
    let (peer, kv) = unsafe { (bytes_in(peer, peer_len), bytes_in(kv, kv_len)) };
    let Ok(peer) = std::str::from_utf8(peer) else {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "peer not UTF-8",
        );
    };
    if peer.is_empty() {
        return fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, "empty peer");
    }
    let pairs = match decode_kv(kv) {
        Ok(p) => p,
        Err(e) => return fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, e),
    };
    if pairs.is_empty() {
        state.attributes.remove(peer);
    } else if let Err(e) = state.attributes.set(peer, pairs.into_iter().collect()) {
        return fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, e);
    }
    status(aster_status_t::ASTER_STATUS_OK)
}

fn peer_arg(peer: *const u8, peer_len: u32) -> Result<String, i32> {
    // SAFETY: caller-provided length.
    let peer = unsafe { bytes_in(peer, peer_len) };
    match std::str::from_utf8(peer) {
        Ok("") => Err(fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "empty peer",
        )),
        Ok(peer) => Ok(peer.to_string()),
        Err(_) => Err(fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "peer not UTF-8",
        )),
    }
}

/// ABI 5.1: narrow an enrolled `peer`'s roles to `roles` (comma-separated
/// UTF-8; empty = no roles) intersected with its credential roles, from its
/// next call. The credential sets the maximum: a role it lacks is never
/// granted. Replaces any earlier restriction; it ends with the peer's
/// enrollment session and survives renewals. `NOT_FOUND` when the server or
/// the peer's live enrollment session is unknown.
#[no_mangle]
pub unsafe extern "C" fn aster_server_restrict_peer_roles(
    server: aster_server_t,
    peer: *const u8,
    peer_len: u32,
    roles: *const u8,
    roles_len: u32,
) -> i32 {
    let Some(state) = SERVERS.get(server) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    let peer = match peer_arg(peer, peer_len) {
        Ok(peer) => peer,
        Err(code) => return code,
    };
    // SAFETY: caller-provided length.
    let Ok(roles) = std::str::from_utf8(unsafe { bytes_in(roles, roles_len) }) else {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "roles not UTF-8",
        );
    };
    match state.attributes.restrict_roles(&peer, roles.split(',')) {
        Ok(()) => status(aster_status_t::ASTER_STATUS_OK),
        Err(e) => fail(aster_status_t::ASTER_STATUS_NOT_FOUND, e),
    }
}

/// ABI 5.1: remove `peer`'s role restriction, restoring its credential roles
/// from its next call. `OK` whether or not one was set; `NOT_FOUND` for an
/// unknown server.
#[no_mangle]
pub unsafe extern "C" fn aster_server_clear_peer_restriction(
    server: aster_server_t,
    peer: *const u8,
    peer_len: u32,
) -> i32 {
    let Some(state) = SERVERS.get(server) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    let peer = match peer_arg(peer, peer_len) {
        Ok(peer) => peer,
        Err(code) => return code,
    };
    state.attributes.clear_restriction(&peer);
    status(aster_status_t::ASTER_STATUS_OK)
}

/// ABI 5.1: set (or, with an empty `value`, remove) a text attribute of the
/// server's `aster.ops.NodeInfo` identity, e.g. `aster.registry_namespace`
/// once the binding has published its contracts (clients read it to find
/// the contract registry, as the retired consumer-admission response
/// carried it). `NOT_FOUND` for an unknown server; `UNSUPPORTED` when the
/// server was built with `ASTER_SERVER_OPTION_BUILTIN_NODE_INFO` = 0.
#[no_mangle]
pub unsafe extern "C" fn aster_server_set_node_attribute(
    server: aster_server_t,
    name: *const u8,
    name_len: u32,
    value: *const u8,
    value_len: u32,
) -> i32 {
    let Some(state) = SERVERS.get(server) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    // SAFETY: caller-provided lengths.
    let (name, value) = unsafe { (bytes_in(name, name_len), bytes_in(value, value_len)) };
    let (Ok(name), Ok(value)) = (std::str::from_utf8(name), std::str::from_utf8(value)) else {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "attribute name and value must be UTF-8",
        );
    };
    if name.is_empty() {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "empty attribute name",
        );
    }
    let Some(identity) = &state.node_identity else {
        return fail(
            aster_status_t::ASTER_STATUS_UNSUPPORTED,
            "the server does not serve aster.ops.NodeInfo",
        );
    };
    identity.update(|identity| {
        identity.attributes.retain(|attr| attr.name != name);
        if !value.is_empty() {
            identity
                .attributes
                .push(aster::rpc::baseline::Attr::new(name, value));
        }
    });
    status(aster_status_t::ASTER_STATUS_OK)
}

// ============================================================================
// Poll
// ============================================================================

/// Drain up to `max` events into `out`. `timeout_ms` 0 = return at once;
/// otherwise wait up to that long for the first event. Returns the count
/// (0 on timeout, unknown server or bad arguments). Single consumer: call it
/// from one thread at a time.
#[no_mangle]
pub unsafe extern "C" fn aster_server_poll(
    runtime: aster_runtime_t,
    server: aster_server_t,
    out: *mut aster_dispatch_event_t,
    max: u32,
    timeout_ms: u32,
) -> u32 {
    if out.is_null() || max == 0 {
        return 0;
    }
    let Ok(bridge) = load_runtime(runtime) else {
        return 0;
    };
    let Some(state) = SERVERS.get(server) else {
        return 0;
    };
    let mut poll = state.poll.lock().unwrap();
    let mut written = drain_into(&state, &bridge, &mut poll, out, max);
    if written == 0 && timeout_ms > 0 {
        poll.consumer
            .wait_blocking(Duration::from_millis(timeout_ms as u64));
        written = drain_into(&state, &bridge, &mut poll, out, max);
    }
    written
}

fn drain_into(
    state: &ServerState,
    bridge: &BridgeRuntime,
    poll: &mut PollState,
    out: *mut aster_dispatch_event_t,
    max: u32,
) -> u32 {
    let mut written = 0u32;
    let PollState {
        consumer,
        connections,
    } = poll;
    consumer.drain(max as usize, |event| {
        let ev = lower(state, bridge, connections, event);
        // SAFETY: the caller provides room for `max` events; `drain` yields
        // at most `max`.
        unsafe { ptr::write(out.add(written as usize), ev) };
        written += 1;
    });
    written
}

fn lower(
    state: &ServerState,
    bridge: &BridgeRuntime,
    connections: &mut HashMap<u64, u64>,
    event: DispatchEvent,
) -> aster_dispatch_event_t {
    use aster_dispatch_event_kind_t as K;
    match event {
        DispatchEvent::Call(call) => lower_call(state, bridge, connections, call),
        DispatchEvent::Cancelled { call_id, reason } => {
            let mut ev = aster_dispatch_event_t::empty(K::ASTER_DISPATCH_EVENT_CANCELLED);
            ev.call_id = call_id;
            ev.reason = match reason {
                CancelReason::Deadline => aster_cancel_reason_t::ASTER_CANCEL_REASON_DEADLINE,
                CancelReason::InvalidRequest => {
                    aster_cancel_reason_t::ASTER_CANCEL_REASON_INVALID_REQUEST
                }
                _ => aster_cancel_reason_t::ASTER_CANCEL_REASON_PEER,
            } as u8;
            ev
        }
        DispatchEvent::ResponseWritable { call_id } => {
            let mut ev = aster_dispatch_event_t::empty(K::ASTER_DISPATCH_EVENT_RESPONSE_WRITABLE);
            ev.call_id = call_id;
            ev
        }
        DispatchEvent::ResponseClosed { call_id } => {
            let mut ev = aster_dispatch_event_t::empty(K::ASTER_DISPATCH_EVENT_RESPONSE_CLOSED);
            ev.call_id = call_id;
            ev
        }
        DispatchEvent::SessionClosed { session_key } => {
            let mut ev = aster_dispatch_event_t::empty(K::ASTER_DISPATCH_EVENT_SESSION_CLOSED);
            ev.session_key = session_key;
            ev
        }
        DispatchEvent::ConnectionClosed {
            connection_id,
            peer_id,
        } => {
            let mut ev = aster_dispatch_event_t::empty(K::ASTER_DISPATCH_EVENT_CONNECTION_CLOSED);
            ev.connection_id = connection_id;
            if let Some(handle) = connections.remove(&connection_id) {
                bridge.connections.remove(handle);
                ev.connection_handle = handle;
            }
            let (id, _, _, pp, pl) = state.leases.insert(Vec::new(), peer_id);
            ev.peer_ptr = pp;
            ev.peer_len = pl;
            ev.buffer = id;
            ev
        }
        DispatchEvent::NonRpc { alpn, connection } => {
            let mut ev = aster_dispatch_event_t::empty(K::ASTER_DISPATCH_EVENT_NON_RPC);
            ev.connection_handle = bridge.connections.insert(connection);
            let (id, rp, rl, _, _) = state.leases.insert(alpn, Arc::from(""));
            ev.request_ptr = rp;
            ev.request_len = rl;
            ev.buffer = id;
            ev
        }
        // `DispatchEvent` is non_exhaustive; a future kind is dropped.
        _ => aster_dispatch_event_t::empty(K::ASTER_DISPATCH_EVENT_CANCELLED),
    }
}

fn lower_call(
    state: &ServerState,
    bridge: &BridgeRuntime,
    connections: &mut HashMap<u64, u64>,
    call: ForeignCall,
) -> aster_dispatch_event_t {
    let ForeignCall {
        call_id,
        service_idx,
        method_idx,
        pattern,
        mut flags,
        session_key,
        connection_id,
        wire_call_id,
        deadline_unix_ms,
        peer_id,
        request,
        header,
        attributes,
        connection,
        handle,
        trace_context,
        inbound_trace,
        ..
    } = call;
    // Answered by id from now on.
    handle.detach();
    if trace_context.is_some() {
        flags |= ASTER_DISPATCH_FLAG_HAS_TRACE;
    }
    if inbound_trace.is_some() {
        flags |= ASTER_DISPATCH_FLAG_HAS_INBOUND_TRACE;
    }
    if !header.metadata_keys.is_empty() {
        flags |= ASTER_DISPATCH_FLAG_HAS_METADATA;
    }
    if !attributes.is_empty() {
        flags |= ASTER_DISPATCH_FLAG_HAS_ATTRIBUTES;
    }
    let connection_handle = match connection {
        Some(conn) if connection_id != 0 => *connections
            .entry(connection_id)
            .or_insert_with(|| bridge.connections.insert(conn)),
        Some(conn) => bridge.connections.insert(conn),
        None => 0,
    };
    let (buffer, rp, rl, pp, pl) = state.leases.insert(request, peer_id);
    aster_dispatch_event_t {
        kind: aster_dispatch_event_kind_t::ASTER_DISPATCH_EVENT_CALL as u8,
        pattern: pattern_code(pattern),
        reason: 0,
        _p0: 0,
        flags,
        call_id,
        service_idx,
        method_idx,
        session_key,
        connection_id,
        connection_handle,
        deadline_unix_ms: deadline_unix_ms as i64,
        wire_call_id,
        session_id: header.session_id as u32,
        request_ptr: rp,
        request_len: rl,
        _p2: 0,
        peer_ptr: pp,
        peer_len: pl,
        _p3: 0,
        buffer,
    }
}

fn pattern_code(p: MethodPattern) -> u8 {
    match p {
        MethodPattern::Unary => 0,
        MethodPattern::ServerStream => 1,
        MethodPattern::ClientStream => 2,
        MethodPattern::BidiStream => 3,
    }
}

// ============================================================================
// Response operations
// ============================================================================

fn submit_result(r: Result<(), SubmitError>, out_required_len: *mut u64) -> i32 {
    match r {
        Ok(()) => status(aster_status_t::ASTER_STATUS_OK),
        Err(SubmitError::QueueFull { required_len }) => {
            if !out_required_len.is_null() {
                // SAFETY: checked non-null.
                unsafe { ptr::write(out_required_len, required_len as u64) };
            }
            status(aster_status_t::ASTER_STATUS_QUEUE_FULL)
        }
        Err(SubmitError::NotFound) => status(aster_status_t::ASTER_STATUS_NOT_FOUND),
        Err(SubmitError::Closed) => status(aster_status_t::ASTER_STATUS_ALREADY_CLOSED),
        Err(SubmitError::TooLarge) => fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "response frame exceeds the frame limit",
        ),
        Err(e) => fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, e),
    }
}

/// `NULL` / empty = OK; otherwise AEv1 `RpcStatus` bytes.
unsafe fn status_in(p: *const u8, len: u32) -> Result<RpcStatus, i32> {
    // SAFETY: caller-provided length.
    let b = unsafe { bytes_in(p, len) };
    if b.is_empty() {
        return Ok(RpcStatus::ok());
    }
    decode_rpc_status(b).map_err(|e| {
        fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            format!("status: {e}"),
        )
    })
}

fn server(server: aster_server_t) -> Result<Arc<ServerState>, i32> {
    SERVERS
        .get(server)
        .ok_or(aster_status_t::ASTER_STATUS_NOT_FOUND as i32)
}

/// Terminal single-shot response: `payload` (omitted when empty) plus the
/// trailer, written together (the unary fast path; also the client-stream
/// answer). `status_aev1` NULL = OK. Never blocks: `QUEUE_FULL` writes
/// `*out_required_len`; retry the exact call, or `aster_dispatch_reserve` it
/// and retry on `RESPONSE_WRITABLE`. `NOT_FOUND`: the call is over.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_dispatch_respond(
    runtime: aster_runtime_t,
    server_h: aster_server_t,
    call_id: u64,
    payload: *const u8,
    payload_len: u32,
    status_aev1: *const u8,
    status_len: u32,
    out_required_len: *mut u64,
) -> i32 {
    let _ = runtime;
    let state = match server(server_h) {
        Ok(s) => s,
        Err(e) => return e,
    };
    // SAFETY: caller-provided lengths.
    let st = match unsafe { status_in(status_aev1, status_len) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    // SAFETY: caller-provided length.
    let payload = unsafe { bytes_in(payload, payload_len) };
    submit_result(
        state.table.try_respond(call_id, payload, &st),
        out_required_len,
    )
}

/// One response data frame (server-stream / bidi). `payload` must not be
/// empty. `QUEUE_FULL` as for `aster_dispatch_respond`.
#[no_mangle]
pub unsafe extern "C" fn aster_dispatch_send(
    runtime: aster_runtime_t,
    server_h: aster_server_t,
    call_id: u64,
    payload: *const u8,
    payload_len: u32,
    out_required_len: *mut u64,
) -> i32 {
    let _ = runtime;
    let state = match server(server_h) {
        Ok(s) => s,
        Err(e) => return e,
    };
    // SAFETY: caller-provided length.
    let payload = unsafe { bytes_in(payload, payload_len) };
    submit_result(state.table.try_send(call_id, payload), out_required_len)
}

/// The terminal trailer. `status_aev1` NULL = OK.
#[no_mangle]
pub unsafe extern "C" fn aster_dispatch_finish(
    runtime: aster_runtime_t,
    server_h: aster_server_t,
    call_id: u64,
    status_aev1: *const u8,
    status_len: u32,
    out_required_len: *mut u64,
) -> i32 {
    let _ = runtime;
    let state = match server(server_h) {
        Ok(s) => s,
        Err(e) => return e,
    };
    // SAFETY: caller-provided length.
    let st = match unsafe { status_in(status_aev1, status_len) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    submit_result(state.table.try_finish(call_id, &st), out_required_len)
}

/// After `QUEUE_FULL`: reserve `required_len` bytes in the background;
/// `RESPONSE_WRITABLE` (or `RESPONSE_CLOSED`) follows through the poll. One
/// reservation per call (a second → `INVALID_ARGUMENT`).
#[no_mangle]
pub extern "C" fn aster_dispatch_reserve(
    runtime: aster_runtime_t,
    server_h: aster_server_t,
    call_id: u64,
    required_len: u64,
) -> i32 {
    let _ = runtime;
    let state = match server(server_h) {
        Ok(s) => s,
        Err(e) => return e,
    };
    submit_result(
        state.table.reserve(call_id, required_len as usize),
        ptr::null_mut(),
    )
}

/// Drop a pending or held reservation. `NOT_FOUND` when there is none.
#[no_mangle]
pub extern "C" fn aster_dispatch_cancel_reservation(
    runtime: aster_runtime_t,
    server_h: aster_server_t,
    call_id: u64,
) -> i32 {
    let _ = runtime;
    let state = match server(server_h) {
        Ok(s) => s,
        Err(e) => return e,
    };
    submit_result(state.table.cancel_reservation(call_id), ptr::null_mut())
}

/// The next request payload after the first (client-stream / bidi),
/// decompressed. Waits up to `timeout_ms` (0 = do not wait,
/// `ASTER_TIMEOUT_INFINITE` = no limit).
///
/// - `OK` with `*out_buffer != 0`: a payload at `*out_ptr`/`*out_len`
///   (release with `aster_dispatch_buffer_release`);
/// - `OK` with `*out_buffer == 0`: end of the request stream;
/// - `TIMEOUT`: nothing yet;
/// - `NOT_FOUND`: the call is over (cancelled, finished, unknown).
///
/// Blocks the calling thread: run it off the runtime's threads (and off
/// virtual-thread carriers).
#[no_mangle]
pub unsafe extern "C" fn aster_dispatch_recv(
    runtime: aster_runtime_t,
    server_h: aster_server_t,
    call_id: u64,
    timeout_ms: u32,
    out_ptr: *mut *const u8,
    out_len: *mut u32,
    out_buffer: *mut aster_buffer_t,
) -> i32 {
    let _ = runtime;
    if out_ptr.is_null() || out_len.is_null() || out_buffer.is_null() {
        return status(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT);
    }
    let state = match server(server_h) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let Some(call) = state.table.get(call_id) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    let next = if timeout_ms == 0 {
        use std::future::Future;
        let fut = call.recv();
        let mut fut = std::pin::pin!(fut);
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let _enter = state.rt.enter();
        match fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => Some(v),
            std::task::Poll::Pending => None,
        }
    } else if timeout_ms == ASTER_TIMEOUT_INFINITE {
        Some(state.rt.block_on(call.recv()))
    } else {
        // The timer must be created inside the runtime: `timeout()` registers
        // with the runtime's time driver when constructed, so building it
        // outside `block_on` panics ("there is no reactor running").
        state
            .rt
            .block_on(async {
                tokio::time::timeout(Duration::from_millis(timeout_ms as u64), call.recv()).await
            })
            .ok()
    };
    // SAFETY: out pointers checked non-null.
    unsafe {
        match next {
            None => status(aster_status_t::ASTER_STATUS_TIMEOUT),
            Some(None) => {
                ptr::write(out_ptr, ptr::null());
                ptr::write(out_len, 0);
                ptr::write(out_buffer, 0);
                status(aster_status_t::ASTER_STATUS_OK)
            }
            Some(Some(payload)) => {
                // Always a real lease: `*out_buffer != 0` is what marks a
                // payload (even an empty one) apart from end of stream.
                let (id, rp, rl, _, _) = state.leases.insert_always(payload, Arc::from(""));
                ptr::write(out_ptr, rp);
                ptr::write(out_len, rl);
                ptr::write(out_buffer, id);
                status(aster_status_t::ASTER_STATUS_OK)
            }
        }
    }
}

/// 0 = live; 1 = cancelled, finished or unknown (stop producing).
#[no_mangle]
pub extern "C" fn aster_dispatch_is_cancelled(
    runtime: aster_runtime_t,
    server_h: aster_server_t,
    call_id: u64,
) -> i32 {
    let _ = runtime;
    match SERVERS.get(server_h) {
        Some(state) => state.table.is_cancelled(call_id) as i32,
        None => 1,
    }
}

/// The call header's metadata as a k/v list (empty when there is none;
/// `*out_buffer` 0). Valid until the call ends.
#[no_mangle]
pub unsafe extern "C" fn aster_dispatch_metadata(
    runtime: aster_runtime_t,
    server_h: aster_server_t,
    call_id: u64,
    out_ptr: *mut *const u8,
    out_len: *mut u32,
    out_buffer: *mut aster_buffer_t,
) -> i32 {
    let _ = runtime;
    // SAFETY: forwarded caller pointers.
    unsafe {
        kv_out(server_h, call_id, out_ptr, out_len, out_buffer, |call| {
            let h = call.header();
            encode_kv(
                h.metadata_keys
                    .iter()
                    .map(String::as_str)
                    .zip(h.metadata_values.iter().map(String::as_str)),
            )
        })
    }
}

/// The caller's attributes (what Gate 3 saw) as a k/v list, sorted by key.
#[no_mangle]
pub unsafe extern "C" fn aster_dispatch_attributes(
    runtime: aster_runtime_t,
    server_h: aster_server_t,
    call_id: u64,
    out_ptr: *mut *const u8,
    out_len: *mut u32,
    out_buffer: *mut aster_buffer_t,
) -> i32 {
    let _ = runtime;
    // SAFETY: forwarded caller pointers.
    unsafe {
        kv_out(server_h, call_id, out_ptr, out_len, out_buffer, |call| {
            encode_kv(call.attributes().iter())
        })
    }
}

unsafe fn kv_out(
    server_h: aster_server_t,
    call_id: u64,
    out_ptr: *mut *const u8,
    out_len: *mut u32,
    out_buffer: *mut aster_buffer_t,
    encode: impl FnOnce(&aster::binding_support::dispatch::CallRef) -> Vec<u8>,
) -> i32 {
    if out_ptr.is_null() || out_len.is_null() || out_buffer.is_null() {
        return status(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT);
    }
    let state = match server(server_h) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let Some(call) = state.table.get(call_id) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    let bytes = encode(&call);
    let (id, rp, rl, _, _) = state.leases.insert(bytes, Arc::from(""));
    // SAFETY: checked non-null.
    unsafe {
        ptr::write(out_ptr, rp);
        ptr::write(out_len, rl);
        ptr::write(out_buffer, id);
    }
    status(aster_status_t::ASTER_STATUS_OK)
}

/// The server span context of a live call (design §5.2), for the binding's
/// handler spans and logs: set with `ASTER_DISPATCH_FLAG_HAS_TRACE` on its
/// `CALL` event. A call whose span is not exported gets an all-zero context
/// (`OK`). `NOT_FOUND` once the call is over or for an unknown server.
#[no_mangle]
pub unsafe extern "C" fn aster_dispatch_trace_context(
    runtime: aster_runtime_t,
    server_h: aster_server_t,
    call_id: u64,
    out: *mut aster_span_context_t,
) -> i32 {
    if out.is_null() {
        return status(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT);
    }
    if let Err(s) = load_runtime(runtime) {
        return status(s);
    }
    let state = match server(server_h) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let Some(call) = state.table.get(call_id) else {
        return status(aster_status_t::ASTER_STATUS_NOT_FOUND);
    };
    let cx = call
        .trace_context()
        .map(|t| aster_span_context_t {
            trace_id: t.trace_id,
            span_id: t.span_id,
            trace_flags: t.trace_flags,
            is_remote: 0,
            _p0: 0,
            _p1: 0,
        })
        .unwrap_or_default();
    // SAFETY: checked non-null.
    unsafe { ptr::write(out, cx) };
    status(aster_status_t::ASTER_STATUS_OK)
}

/// The inbound W3C `baggage` and `tracestate` of a live call as a k/v list
/// (keys `baggage` and `tracestate`, the raw header values; a key is present
/// only when the caller sent it), for the binding's `current_baggage`. Set
/// with `ASTER_DISPATCH_FLAG_HAS_INBOUND_TRACE` on its `CALL` event, whether
/// or not the call's span is exported. `baggage` is within the W3C limits
/// (8192 bytes, 64 members); a larger header was dropped. Same lease
/// contract as `aster_dispatch_metadata`. `NOT_FOUND` once the call is over.
#[no_mangle]
pub unsafe extern "C" fn aster_dispatch_inbound_trace(
    runtime: aster_runtime_t,
    server_h: aster_server_t,
    call_id: u64,
    out_ptr: *mut *const u8,
    out_len: *mut u32,
    out_buffer: *mut aster_buffer_t,
) -> i32 {
    let _ = runtime;
    // SAFETY: forwarded caller pointers.
    unsafe {
        kv_out(server_h, call_id, out_ptr, out_len, out_buffer, |call| {
            let mut pairs: Vec<(&str, &str)> = Vec::new();
            if let Some(t) = call.inbound_trace() {
                if let Some(b) = &t.baggage {
                    pairs.push(("baggage", b));
                }
                if let Some(ts) = &t.tracestate {
                    pairs.push(("tracestate", ts));
                }
            }
            encode_kv(pairs.into_iter())
        })
    }
}

/// End the call with `INTERNAL "handler dropped call"` unless it already
/// ended. `NOT_FOUND` when it had.
#[no_mangle]
pub extern "C" fn aster_dispatch_abandon(
    runtime: aster_runtime_t,
    server_h: aster_server_t,
    call_id: u64,
) -> i32 {
    let _ = runtime;
    let state = match server(server_h) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let _enter = state.rt.enter();
    submit_result(state.table.abandon(call_id), ptr::null_mut())
}

/// Release one lease (`aster_dispatch_event_t.buffer`, `aster_dispatch_recv`,
/// `_metadata`, `_attributes`). 0 is a no-op; an unknown id is `NOT_FOUND`.
#[no_mangle]
pub extern "C" fn aster_dispatch_buffer_release(
    runtime: aster_runtime_t,
    server_h: aster_server_t,
    buffer: aster_buffer_t,
) -> i32 {
    let _ = runtime;
    if buffer == 0 {
        return status(aster_status_t::ASTER_STATUS_OK);
    }
    match SERVERS.get(server_h) {
        Some(state) if state.leases.release(buffer) => status(aster_status_t::ASTER_STATUS_OK),
        _ => status(aster_status_t::ASTER_STATUS_NOT_FOUND),
    }
}

/// `contract_id` (64 lowercase hex) of a service's JSON registration form,
/// as the server will publish it. `*out` is freed with
/// `aster_string_release`.
#[no_mangle]
pub unsafe extern "C" fn aster_dynamic_service_contract_id(
    json: *const u8,
    json_len: u32,
    out: *mut aster_bytes_t,
) -> i32 {
    if out.is_null() {
        return status(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT);
    }
    // SAFETY: caller-provided length.
    let raw = unsafe { bytes_in(json, json_len) };
    let Ok(text) = std::str::from_utf8(raw) else {
        return fail(
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT,
            "service JSON is not UTF-8",
        );
    };
    let id = match DynamicService::from_json(text).and_then(|s| s.contract_id()) {
        Ok(id) => id,
        Err(e) => return fail(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT, e),
    };
    // SAFETY: checked non-null.
    unsafe { ptr::write(out, crate::alloc_string(id)) };
    status(aster_status_t::ASTER_STATUS_OK)
}

/// Live calls in the server's table (diagnostics / tests).
#[no_mangle]
pub extern "C" fn aster_server_live_calls(server_h: aster_server_t) -> u64 {
    SERVERS.get(server_h).map_or(0, |s| s.table.len() as u64)
}

/// Rust-side handle on a connection a binding received, for the tests.
#[doc(hidden)]
pub fn debug_connection(rt: aster_runtime_t, handle: u64) -> Option<CoreConnection> {
    load_runtime(rt)
        .ok()?
        .connections
        .get(handle)
        .map(|c| (*c).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_round_trip() {
        let pairs = [("a", "1"), ("", ""), ("ключ", "значение")];
        let enc = encode_kv(pairs.iter().copied());
        let dec = decode_kv(&enc).unwrap();
        assert_eq!(
            dec,
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>()
        );
        assert_eq!(decode_kv(&[]).unwrap(), vec![]);
        assert_eq!(encode_kv(std::iter::empty()), vec![0, 0, 0, 0]);
    }

    #[test]
    fn kv_rejects_malformed() {
        let enc = encode_kv([("key", "value")].into_iter());
        assert!(decode_kv(&enc[..enc.len() - 1]).is_err());
        let mut trailing = enc.clone();
        trailing.push(0);
        assert!(decode_kv(&trailing).is_err());
        assert!(decode_kv(&[0xff, 0xff, 0xff, 0x7f]).is_err());
        let bad_utf8 = [1, 0, 0, 0, 1, 0, 0, 0, 0xff, 0, 0, 0, 0];
        assert!(decode_kv(&bad_utf8).is_err());
    }
}
