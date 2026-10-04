//! Expose (ABI 4.2, plan F5 / R9): publish local services to Aster peers and
//! relay HTTP to a peer's exposed services, from the C ABI.
//!
//! Design: `docs/_internal/bindings/upcall-design.md` §4 and §6. Payloads are
//! UCv1 messages (vectors `conformance/vectors/upcalls.json`): the binding
//! decodes `C4` (HTTP request) / `C7` (route policy request) and encodes `C5`
//! (HTTP response) / `C6` (trailers) / `C8` (route grant) / `CF` (decision)
//! itself and answers through `aster_upcall_respond` (`upcall.rs`).
//!
//! ```text
//!   aster_expose_new(rt, node) → expose              service registry + reactor
//!   aster_expose_serve(rt, node, expose, alpns)      accept loop (or ...)
//!   aster_expose_handle_connection(rt, expose, conn) ... feed one connection
//!   aster_expose_socket(rt, expose, id, addr, auth)  L4 splice to a loopback server
//!   aster_expose_http_foreign(rt, expose, upcalls,   HTTP handler = upcall kind 49,
//!                             bodies, id, opts)      authorize = kind 48
//!   aster_expose_route_policy_foreign(rt, expose, u) route policy = upcall kind 50
//!   aster_expose_unexpose(rt, expose, id) / aster_expose_free(rt, expose)
//!
//!   aster_http_bodies_new(rt) → bodies               body-handle table
//!   aster_http_body_{read,write,try_write,finish,abort}(rt, bodies, stream_id, ..)
//!
//!   aster_expose_relay_request(rt, conn, c4)         consumer: buffered request
//!   aster_expose_request_route(rt, conn, specs, ..)  consumer: ask an edge for routes
//! ```
//!
//! # Threading
//!
//! The `aster_http_body_*` calls block the calling thread (`read`, `write`)
//! or never block (`try_write`, `finish`, `abort`). They work from any
//! thread that is **not** a tokio runtime thread (`ASTER_HTTP_BODY_BLOCKING_ON_RUNTIME`);
//! in practice that is a binding's own poll or handler thread. Request flow
//! for kind 49: poll → decode `C4` → (read the request body by `stream_id`
//! when the body mode is `Stream`) → `aster_upcall_respond` with `C5` → (write
//! the response body by `stream_id`, then `finish` or `abort` when the
//! response body mode is `Stream`).

use std::net::SocketAddr;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster::binding_support::upcall::codec::{self, ExposeHttpResponse, HttpBodyMode, HttpHeader};
use aster::binding_support::upcall::expose::{
    expose_foreign_http, foreign_authorize_fn, foreign_route_policy, BodyError, ForeignHttpOptions,
    HttpBodies,
};
use aster::expose::{
    http, relay_request, request_route, AuthorizeFn, EdgeRouter, Expose, RouteProtocol,
    RouteRegistration, RouteSpec,
};
use aster::Connection;

use crate::data::{bridge_for, in_slice, in_string, invalid, not_found, spawn_op, Done};
use crate::pure::try_status;
use crate::upcall::{aster_upcalls_t, upcalls_for};
use crate::{
    aster_bytes_list_t, aster_bytes_t, aster_connection_t, aster_event_kind_t, aster_node_t,
    aster_operation_t, aster_runtime_t, aster_status_t, set_last_error, HandleRegistry,
};

const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;

// ============================================================================
// Types
// ============================================================================

/// An expose registry on a node (`aster_expose_new`): the services published
/// to peers, plus the reactor that dispatches their inbound relay streams.
pub type aster_expose_t = u64;
/// A body-handle table for foreign HTTP exchanges (`aster_http_bodies_new`),
/// one per binding runtime, shared by every foreign HTTP service.
///
/// Kept separate from `aster_upcalls_t` (rather than folded into it) because
/// it is a different Rust object with its own lifetime and a different
/// thread-use (handler threads call it, not the poll thread), several
/// `aster_upcalls_t` tables may share one, and keeping it out leaves the 4.1
/// upcall surface untouched.
pub type aster_http_bodies_t = u64;
/// A live route registration (`ASTER_EVENT_BYTES_RESULT` of
/// `aster_expose_request_route`): the edge keeps the granted routes while it
/// is held.
pub type aster_route_registration_t = u64;

/// Return codes of `aster_http_body_*`: `0` OK, `1..=9` are
/// `aster::binding_support::upcall::expose::BodyError::code()` (stable),
/// negative values are argument / handle errors (these functions do not
/// return `aster_status_t`: its small positive values would collide with the
/// body codes). `aster_last_error_message` carries the detail.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_http_body_status_t {
    /// Done. For `read`: bytes in `buf` (`*out_len`) or, with `*out_eof` = 1,
    /// the end of the request body.
    ASTER_HTTP_BODY_OK = 0,
    /// No open exchange has this `stream_id`: never issued, or already over.
    ASTER_HTTP_BODY_UNKNOWN_STREAM = 1,
    /// The peer went away or the request was abandoned: stop producing.
    ASTER_HTTP_BODY_CANCELLED = 2,
    /// A body went over its cap (`max_request_body` for a read: answer `413`;
    /// `max_response_body` for a write: the response was aborted).
    ASTER_HTTP_BODY_TOO_LARGE = 3,
    /// One write was larger than the options' `max_chunk`; split it.
    ASTER_HTTP_BODY_CHUNK_TOO_LARGE = 4,
    /// The response body was already finished or aborted (or it was inline).
    ASTER_HTTP_BODY_FINISHED = 5,
    /// The blocking call timed out; nothing was consumed or queued.
    ASTER_HTTP_BODY_TIMEOUT = 6,
    /// Reading the request body off the wire failed.
    ASTER_HTTP_BODY_READ = 7,
    /// `finish` trailers were not a valid `C6` message / header fields.
    ASTER_HTTP_BODY_INVALID_TRAILERS = 8,
    /// A blocking call was made on a tokio runtime thread.
    ASTER_HTTP_BODY_BLOCKING_ON_RUNTIME = 9,
    /// A NULL pointer with a non-zero length, a zero `cap`, ...
    ASTER_HTTP_BODY_INVALID_ARGUMENT = -1,
    /// Unknown runtime or `aster_http_bodies_t`.
    ASTER_HTTP_BODY_UNKNOWN_HANDLE = -2,
}

/// Options of `aster_expose_http_foreign` (`ForeignHttpOptions`). Initialise
/// with `aster_http_foreign_options_init`; `struct_size` must equal
/// `sizeof(aster_http_foreign_options_t)`. 48 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct aster_http_foreign_options_t {
    pub struct_size: u32,
    /// Reserved, must be 0.
    pub flags: u32,
    /// A request whose `content-length` is at most this travels inline in the
    /// `C4` upcall. 0 = default (64 KiB).
    pub inline_request_max: u64,
    /// Cap on the request body: a declared length over it is answered `413`
    /// without an upcall, a streamed body that grows past it fails the read
    /// with `ASTER_HTTP_BODY_TOO_LARGE`. 0 = unbounded.
    pub max_request_body: u64,
    /// Cap on the response body: an inline body over it becomes `502`, a
    /// streamed one is aborted by the write that crosses it. 0 = unbounded.
    pub max_response_body: u64,
    /// Largest single body write and most one read returns. 0 = default
    /// (1 MiB).
    pub max_chunk: u64,
    /// Response chunks queued before a write waits. 0 = default (8).
    pub response_buffer: u32,
    /// Reserved, must be 0.
    pub reserved: u32,
}

/// One route for `aster_expose_request_route` (`RouteSpec`). 40 bytes (LP64).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct aster_route_spec_t {
    /// Host / SNI to match; empty = any.
    pub host: aster_bytes_t,
    /// Origin-side service id (what was exposed with `aster_expose_*`).
    pub service_id: aster_bytes_t,
    /// Edge-facing port; 0 = the listener's default.
    pub port: u32,
    /// 0 = http, 1 = tcp (`aster_route_protocol_t` values).
    pub protocol: u32,
}

/// `aster_route_spec_t.protocol`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_route_protocol_t {
    ASTER_ROUTE_PROTOCOL_HTTP = 0,
    ASTER_ROUTE_PROTOCOL_TCP = 1,
}

// ============================================================================
// Registries
// ============================================================================

static EXPOSES: once_cell::sync::Lazy<HandleRegistry<Expose>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);
static BODIES: once_cell::sync::Lazy<HandleRegistry<HttpBodies>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);
static REGISTRATIONS: once_cell::sync::Lazy<HandleRegistry<Mutex<Option<RouteRegistration>>>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);

fn expose_for(expose: aster_expose_t) -> Result<Arc<Expose>, i32> {
    EXPOSES
        .get(expose)
        .ok_or_else(|| not_found(format!("unknown expose handle {expose}")))
}

fn bodies_for(bodies: aster_http_bodies_t) -> Result<Arc<HttpBodies>, i32> {
    BODIES
        .get(bodies)
        .ok_or_else(|| not_found(format!("unknown http bodies handle {bodies}")))
}

// ============================================================================
// The expose handle
// ============================================================================

/// Create an expose registry for `node` (which is validated; the registry is
/// independent of it and only binds to connections you feed it). Register
/// services with `aster_expose_socket` / `aster_expose_http_foreign`, then
/// deliver connections with `aster_expose_serve` or
/// `aster_expose_handle_connection`. The node must have been started with the
/// ALPN(s) the consumers connect on.
#[no_mangle]
pub unsafe extern "C" fn aster_expose_new(
    runtime: aster_runtime_t,
    node: aster_node_t,
    out_expose: *mut aster_expose_t,
) -> i32 {
    if out_expose.is_null() {
        return invalid("aster_expose_new: out_expose is NULL");
    }
    *out_expose = 0;
    let bridge = try_status!(bridge_for(runtime));
    if bridge.nodes.get(node).is_none() {
        return not_found(format!("unknown node handle {node}"));
    }
    // `Expose::new` spawns its reactor drain task on the current runtime.
    let _enter = bridge.runtime.enter();
    *out_expose = EXPOSES.insert(Expose::new());
    OK
}

/// Free the handle. Services registered on it keep serving connections that
/// were already fed to it, so `aster_expose_unexpose` first if they must stop.
#[no_mangle]
pub unsafe extern "C" fn aster_expose_free(
    runtime: aster_runtime_t,
    expose: aster_expose_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    match EXPOSES.remove(expose) {
        Some(_) => OK,
        None => not_found(format!("unknown expose handle {expose}")),
    }
}

/// Stop exposing `service_id` (idempotent: an id that was never exposed is
/// `OK`). Streams already being served finish.
#[no_mangle]
pub unsafe extern "C" fn aster_expose_unexpose(
    runtime: aster_runtime_t,
    expose: aster_expose_t,
    service_id: aster_bytes_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    let e = try_status!(expose_for(expose));
    let id = try_status!(in_string(&service_id, "service_id"));
    e.unexpose(&id);
    OK
}

/// Feed one inbound connection (from `aster_accept` / `aster_node_accept_aster`)
/// to the registry, so its relay streams reach the exposed services. Call once
/// per connection, before the peer opens streams on it; the connection handle
/// stays usable and owned by the binding.
#[no_mangle]
pub unsafe extern "C" fn aster_expose_handle_connection(
    runtime: aster_runtime_t,
    expose: aster_expose_t,
    connection: aster_connection_t,
) -> i32 {
    let bridge = try_status!(bridge_for(runtime));
    let e = try_status!(expose_for(expose));
    let Some(core) = bridge.connections.get(connection) else {
        return not_found(format!("unknown connection handle {connection}"));
    };
    // Feeding spawns on the current runtime.
    let _enter = bridge.runtime.enter();
    e.handle(&Connection::from_core_connection((*core).clone()));
    OK
}

/// Own the node's accept loop for expose: accept inbound connections of
/// `node` and feed each whose ALPN is in `alpns` (empty list = every ALPN) to
/// `expose`; others are dropped. Runs until the node closes, then completes
/// with `ASTER_EVENT_ERROR` (the accept error). Do not combine with
/// `aster_accept` / `aster_node_accept_aster` on the same node: they drain
/// the same queue (use `aster_expose_handle_connection` there). Cancel with
/// `aster_operation_cancel` only suppresses the completion event; close the
/// node to stop the loop.
#[no_mangle]
pub unsafe extern "C" fn aster_expose_serve(
    runtime: aster_runtime_t,
    node: aster_node_t,
    expose: aster_expose_t,
    alpns: aster_bytes_list_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_expose_serve: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let Some(core) = bridge.nodes.get(node) else {
        return not_found(format!("unknown node handle {node}"));
    };
    let e = (*try_status!(expose_for(expose))).clone();
    let alpns: Vec<Vec<u8>> = if alpns.len == 0 {
        Vec::new()
    } else if alpns.items.is_null() {
        return invalid("alpns: NULL items with non-zero length");
    } else {
        let mut v = Vec::with_capacity(alpns.len);
        for b in std::slice::from_raw_parts(alpns.items, alpns.len) {
            v.push(try_status!(in_slice(b, "alpns[]")).to_vec());
        }
        v
    };
    spawn_op(&bridge, user_data, out_operation, async move {
        loop {
            let (alpn, conn) = core.accept_aster().await.map_err(|e| format!("{e:#}"))?;
            if alpns.is_empty() || alpns.contains(&alpn) {
                e.handle(&Connection::from_core_connection(conn));
            }
        }
    })
}

/// Allow-all policy for a service registered without a foreign authorizer.
fn allow_all() -> AuthorizeFn {
    Arc::new(|_peer| Box::pin(async { Ok(()) }))
}

unsafe fn authorizer(upcalls: aster_upcalls_t, service_id: &str) -> Result<AuthorizeFn, i32> {
    if upcalls == 0 {
        return Ok(allow_all());
    }
    Ok(foreign_authorize_fn(
        upcalls_for(upcalls)?.upcalls.clone(),
        service_id,
    ))
}

/// Expose a raw L4 splice to `addr` (`"ip:port"`, a loopback server the
/// binding runs in its own language) under `service_id`, node-wide.
///
/// `authorizer_upcalls` != 0 puts every `(connection, service)` admission to
/// the binding as an `EXPOSE_AUTHORIZE` decision (kind 48, `C0` → `CF`; no
/// answer follows the kind's failure mode, fail-closed by default).
/// `authorizer_upcalls` = 0 admits **every** peer that reaches the node's
/// ALPN: only do that when the node itself is access-controlled (Gate 0).
/// Re-registering an id replaces it.
#[no_mangle]
pub unsafe extern "C" fn aster_expose_socket(
    runtime: aster_runtime_t,
    expose: aster_expose_t,
    service_id: aster_bytes_t,
    addr: aster_bytes_t,
    authorizer_upcalls: aster_upcalls_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    let e = try_status!(expose_for(expose));
    let id = try_status!(in_string(&service_id, "service_id"));
    if id.is_empty() {
        return invalid("service_id must be non-empty");
    }
    let addr = try_status!(in_string(&addr, "addr"));
    let addr: SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(err) => return invalid(format!("addr {addr:?} is not ip:port: {err}")),
    };
    let auth = try_status!(authorizer(authorizer_upcalls, &id));
    e.expose_socket(&id, auth, addr);
    OK
}

// ============================================================================
// Foreign HTTP
// ============================================================================

/// Create a body-handle table (one per binding runtime).
#[no_mangle]
pub unsafe extern "C" fn aster_http_bodies_new(
    runtime: aster_runtime_t,
    out_bodies: *mut aster_http_bodies_t,
) -> i32 {
    if out_bodies.is_null() {
        return invalid("aster_http_bodies_new: out_bodies is NULL");
    }
    *out_bodies = 0;
    try_status!(bridge_for(runtime));
    *out_bodies = BODIES.insert(HttpBodies::new());
    OK
}

/// Free the table. Services registered with it keep their own reference and
/// keep working; the binding just cannot reach their bodies any more.
#[no_mangle]
pub unsafe extern "C" fn aster_http_bodies_free(
    runtime: aster_runtime_t,
    bodies: aster_http_bodies_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    match BODIES.remove(bodies) {
        Some(_) => OK,
        None => not_found(format!("unknown http bodies handle {bodies}")),
    }
}

/// Number of open exchanges (for leak checks and tests).
#[no_mangle]
pub unsafe extern "C" fn aster_http_bodies_open_count(
    runtime: aster_runtime_t,
    bodies: aster_http_bodies_t,
    out_count: *mut u64,
) -> i32 {
    if out_count.is_null() {
        return invalid("aster_http_bodies_open_count: out_count is NULL");
    }
    try_status!(bridge_for(runtime));
    *out_count = try_status!(bodies_for(bodies)).open_count() as u64;
    OK
}

/// Fill `*out` with the defaults (`struct_size` set, every limit at its
/// default).
#[no_mangle]
pub unsafe extern "C" fn aster_http_foreign_options_init(
    out: *mut aster_http_foreign_options_t,
) -> i32 {
    let Some(out) = out.as_mut() else {
        return invalid("aster_http_foreign_options_init: out is NULL");
    };
    *out = aster_http_foreign_options_t {
        struct_size: std::mem::size_of::<aster_http_foreign_options_t>() as u32,
        flags: 0,
        inline_request_max: 0,
        max_request_body: 0,
        max_response_body: 0,
        max_chunk: 0,
        response_buffer: 0,
        reserved: 0,
    };
    OK
}

unsafe fn foreign_options(
    opts: *const aster_http_foreign_options_t,
) -> Result<ForeignHttpOptions, i32> {
    let mut o = ForeignHttpOptions::default();
    let Some(c) = opts.as_ref() else {
        return Ok(o);
    };
    let expected = std::mem::size_of::<aster_http_foreign_options_t>() as u32;
    if c.struct_size != expected {
        return Err(invalid(format!(
            "aster_http_foreign_options_t.struct_size is {}, expected {expected}",
            c.struct_size
        )));
    }
    if c.flags != 0 || c.reserved != 0 {
        return Err(invalid(
            "aster_http_foreign_options_t: flags and reserved must be 0",
        ));
    }
    let size = |v: u64, what: &str| {
        usize::try_from(v).map_err(|_| invalid(format!("{what} does not fit this platform")))
    };
    if c.inline_request_max != 0 {
        o.inline_request_max = size(c.inline_request_max, "inline_request_max")?;
    }
    if c.max_request_body != 0 {
        o.max_request_body = Some(c.max_request_body);
    }
    if c.max_response_body != 0 {
        o.max_response_body = Some(c.max_response_body);
    }
    if c.max_chunk != 0 {
        o.max_chunk = size(c.max_chunk, "max_chunk")?;
    }
    if c.response_buffer != 0 {
        o.response_buffer = c.response_buffer as usize;
    }
    Ok(o)
}

/// Expose `service_id` with both halves foreign: admission is an
/// `EXPOSE_AUTHORIZE` decision (kind 48, `C0` → `CF`) and each request one
/// `EXPOSE_HTTP` upcall (kind 49, `C4` → `C5`) on `upcalls`, with bodies moved
/// through `bodies` by the request's `stream_id` (`aster_http_body_*`). See
/// design §6 for the exchange, limits, cancellation and failure statuses
/// (`ERROR` → 500, `UNIMPLEMENTED` → 501, bad `C5` → 502, overload / closed →
/// 503, timeout → 504; configure the kind's timeout with
/// `aster_upcall_set_policy`, default 5 s covers the response head only).
/// `opts` NULL = defaults. Re-registering an id replaces it.
#[no_mangle]
pub unsafe extern "C" fn aster_expose_http_foreign(
    runtime: aster_runtime_t,
    expose: aster_expose_t,
    upcalls: aster_upcalls_t,
    bodies: aster_http_bodies_t,
    service_id: aster_bytes_t,
    opts: *const aster_http_foreign_options_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    let e = try_status!(expose_for(expose));
    let u = try_status!(upcalls_for(upcalls));
    let b = try_status!(bodies_for(bodies));
    let id = try_status!(in_string(&service_id, "service_id"));
    if id.is_empty() {
        return invalid("service_id must be non-empty");
    }
    let options = try_status!(foreign_options(opts));
    if options.max_chunk == 0 || options.response_buffer == 0 {
        return invalid("max_chunk and response_buffer must be > 0");
    }
    expose_foreign_http(&e, &u.upcalls, &b, &id, options);
    OK
}

/// Accept route registrations (act as an edge) node-wide, granting what the
/// binding decides: each request is one `EXPOSE_ROUTE_POLICY` upcall (kind
/// 50, `C7` → `C8`: the indexes of the granted specs, strictly ascending; an
/// index out of range is a malformed answer). With no usable answer the
/// kind's failure mode applies to the whole request: fail-closed (default)
/// grants nothing, fail-open grants every route. The routes land in an
/// internal `EdgeRouter` (the C ABI has no public edge listener).
#[no_mangle]
pub unsafe extern "C" fn aster_expose_route_policy_foreign(
    runtime: aster_runtime_t,
    expose: aster_expose_t,
    upcalls: aster_upcalls_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    let e = try_status!(expose_for(expose));
    let u = try_status!(upcalls_for(upcalls));
    e.serve_routes(EdgeRouter::new(), foreign_route_policy(u.upcalls.clone()));
    OK
}

// ============================================================================
// Body I/O
// ============================================================================

use aster_http_body_status_t as B;

fn body_code(e: &BodyError) -> i32 {
    set_last_error(e.to_string());
    e.code() as i32
}

fn body_invalid(msg: &str) -> i32 {
    set_last_error(msg);
    B::ASTER_HTTP_BODY_INVALID_ARGUMENT as i32
}

/// Resolve the runtime and table; a failure is a body-status code.
fn body_ctx(runtime: aster_runtime_t, bodies: aster_http_bodies_t) -> Result<Arc<HttpBodies>, i32> {
    if crate::load_runtime(runtime).is_err() {
        set_last_error("unknown runtime");
        return Err(B::ASTER_HTTP_BODY_UNKNOWN_HANDLE as i32);
    }
    BODIES.get(bodies).ok_or_else(|| {
        set_last_error(format!("unknown http bodies handle {bodies}"));
        B::ASTER_HTTP_BODY_UNKNOWN_HANDLE as i32
    })
}

unsafe fn chunk_in(ptr: *const u8, len: usize) -> Result<Vec<u8>, i32> {
    if len == 0 {
        Ok(Vec::new())
    } else if ptr.is_null() {
        Err(body_invalid("NULL pointer with non-zero length"))
    } else {
        Ok(std::slice::from_raw_parts(ptr, len).to_vec())
    }
}

/// Read up to `cap` bytes of the request body of exchange `stream_id` into
/// `buf` (a read returns at most `min(cap, max_chunk)`), waiting up to
/// `timeout_ms` (0 = return at once with `ASTER_HTTP_BODY_TIMEOUT` when no
/// data is ready). On `OK`: `*out_len` bytes were written to `buf`, and
/// `*out_eof` = 1 means the body ended (`*out_len` is then 0); a short
/// non-EOF read is normal. A timed-out read consumes nothing. Pull-based
/// backpressure: Rust reads the QUIC stream only when this is called.
///
/// Returns an `aster_http_body_status_t`. Never call it from a tokio runtime
/// thread (`BLOCKING_ON_RUNTIME`); `cap` must be > 0.
#[no_mangle]
pub unsafe extern "C" fn aster_http_body_read(
    runtime: aster_runtime_t,
    bodies: aster_http_bodies_t,
    stream_id: u64,
    buf: *mut u8,
    cap: usize,
    timeout_ms: u32,
    out_len: *mut usize,
    out_eof: *mut u32,
) -> i32 {
    if out_len.is_null() || out_eof.is_null() || buf.is_null() || cap == 0 {
        return body_invalid("aster_http_body_read: NULL out pointer / buffer or zero cap");
    }
    *out_len = 0;
    *out_eof = 0;
    let t = match body_ctx(runtime, bodies) {
        Ok(t) => t,
        Err(c) => return c,
    };
    match t.read_blocking(stream_id, cap, Duration::from_millis(timeout_ms as u64)) {
        Ok(Some(chunk)) => {
            // `read` returns at most `max`, so this always fits.
            let n = chunk.len().min(cap);
            ptr::copy_nonoverlapping(chunk.as_ptr(), buf, n);
            *out_len = n;
            B::ASTER_HTTP_BODY_OK as i32
        }
        Ok(None) => {
            *out_eof = 1;
            B::ASTER_HTTP_BODY_OK as i32
        }
        Err(e) => body_code(&e),
    }
}

/// Queue one response-body chunk (copied; at most the options' `max_chunk`
/// bytes), waiting up to `timeout_ms` while the buffer is full. A timed-out
/// write queued nothing. `CANCELLED` means the peer is gone: stop producing.
/// Respond with the `C5` head (body mode `Stream`) first; writes made before
/// it are queued up to `response_buffer` chunks. Blocking: see
/// `aster_http_body_read` for the thread rule.
#[no_mangle]
pub unsafe extern "C" fn aster_http_body_write(
    runtime: aster_runtime_t,
    bodies: aster_http_bodies_t,
    stream_id: u64,
    chunk: *const u8,
    len: usize,
    timeout_ms: u32,
) -> i32 {
    let data = match chunk_in(chunk, len) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let t = match body_ctx(runtime, bodies) {
        Ok(t) => t,
        Err(c) => return c,
    };
    match t.write_blocking(stream_id, data, Duration::from_millis(timeout_ms as u64)) {
        Ok(()) => B::ASTER_HTTP_BODY_OK as i32,
        Err(e) => body_code(&e),
    }
}

/// Queue one chunk if there is room now. On `OK`, `*out_queued` is 1 (queued)
/// or 0 (the buffer is full: nothing queued, try again later). Never blocks.
#[no_mangle]
pub unsafe extern "C" fn aster_http_body_try_write(
    runtime: aster_runtime_t,
    bodies: aster_http_bodies_t,
    stream_id: u64,
    chunk: *const u8,
    len: usize,
    out_queued: *mut u32,
) -> i32 {
    if out_queued.is_null() {
        return body_invalid("aster_http_body_try_write: out_queued is NULL");
    }
    *out_queued = 0;
    let data = match chunk_in(chunk, len) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let t = match body_ctx(runtime, bodies) {
        Ok(t) => t,
        Err(c) => return c,
    };
    match t.try_write(stream_id, data) {
        Ok(queued) => {
            *out_queued = u32::from(queued);
            B::ASTER_HTTP_BODY_OK as i32
        }
        Err(e) => body_code(&e),
    }
}

/// End the response body cleanly. `trailers` / `len` is an optional UCv1 `C6`
/// message (`headers:list<Header>`; NULL / 0 = none); a malformed one is
/// `INVALID_TRAILERS` and leaves the body open. Queued chunks are still
/// delivered. (The relay hop does not carry trailers yet; an in-process
/// consumer sees them.) Never blocks.
#[no_mangle]
pub unsafe extern "C" fn aster_http_body_finish(
    runtime: aster_runtime_t,
    bodies: aster_http_bodies_t,
    stream_id: u64,
    trailers: *const u8,
    len: usize,
) -> i32 {
    let raw = match chunk_in(trailers, len) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let t = match body_ctx(runtime, bodies) {
        Ok(t) => t,
        Err(c) => return c,
    };
    let trailers: Vec<HttpHeader> = if raw.is_empty() {
        Vec::new()
    } else {
        match codec::decode_expose_http_trailers(&raw) {
            Ok(v) => v,
            Err(e) => return body_code(&BodyError::InvalidTrailers(e.to_string())),
        }
    };
    match t.finish(stream_id, trailers) {
        Ok(()) => B::ASTER_HTTP_BODY_OK as i32,
        Err(e) => body_code(&e),
    }
}

/// End the response body with an error: queued chunks are discarded and the
/// relay resets the stream, so the consumer sees a failed response, never a
/// silently truncated one. `message` (UTF-8, may be NULL / 0) is logged, not
/// sent. Never blocks.
#[no_mangle]
pub unsafe extern "C" fn aster_http_body_abort(
    runtime: aster_runtime_t,
    bodies: aster_http_bodies_t,
    stream_id: u64,
    message: *const u8,
    len: usize,
) -> i32 {
    let raw = match chunk_in(message, len) {
        Ok(d) => d,
        Err(c) => return c,
    };
    let t = match body_ctx(runtime, bodies) {
        Ok(t) => t,
        Err(c) => return c,
    };
    match t.abort(stream_id, &String::from_utf8_lossy(&raw)) {
        Ok(()) => B::ASTER_HTTP_BODY_OK as i32,
        Err(e) => body_code(&e),
    }
}

// ============================================================================
// Consumer side
// ============================================================================

fn http_version(code: u8) -> http::Version {
    match code {
        0 => http::Version::HTTP_09,
        1 => http::Version::HTTP_10,
        3 => http::Version::HTTP_2,
        4 => http::Version::HTTP_3,
        _ => http::Version::HTTP_11,
    }
}

/// Relay one buffered HTTP request to a service the peer exposed on
/// `connection`, and collect the whole response (the consumer needs no
/// `aster_expose_t`).
///
/// `request` is a UCv1 `C4` message (`ExposeHttpRequest`) with body mode
/// `Inline`: `service_id` names the peer's exposed service, `version` /
/// `method` / `uri` / `headers` / `body` are the request; `peer_id` (leave
/// absent) and `stream_id` (leave 0) are ignored. Completes with
/// `ASTER_EVENT_BYTES_RESULT`, payload = UCv1 `C5` (`ExposeHttpResponse`,
/// body `Inline`, copy out then `aster_buffer_release`) for **any** HTTP
/// status, including the `5xx` a failing handler maps to. A transport
/// failure, a refused request or a response that failed mid-body (the peer
/// aborted it) is `ASTER_EVENT_ERROR`. Streaming relay is not in the C ABI.
#[no_mangle]
pub unsafe extern "C" fn aster_expose_relay_request(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    request: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_expose_relay_request: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let Some(core) = bridge.connections.get(connection) else {
        return not_found(format!("unknown connection handle {connection}"));
    };
    let raw = try_status!(in_slice(&request, "request"));
    let req = match codec::decode_expose_http_request(raw) {
        Ok(r) => r,
        Err(e) => return invalid(format!("request is not a valid UCv1 C4 message: {e:#}")),
    };
    let HttpBodyMode::Inline(body) = req.body else {
        return invalid("request body must be Inline (streaming relay is not in the C ABI)");
    };
    if req.service_id.is_empty() {
        return invalid("service_id must be non-empty");
    }
    let mut builder = http::Request::builder()
        .method(req.method.as_str())
        .uri(req.uri.as_str())
        .version(http_version(req.version));
    for (name, value) in &req.headers {
        builder = builder.header(name.as_str(), value.as_slice());
    }
    let http_req = match builder.body(body) {
        Ok(r) => r,
        Err(e) => return invalid(format!("invalid HTTP request: {e}")),
    };
    let service_id = req.service_id;
    let conn = Connection::from_core_connection((*core).clone());
    spawn_op(&bridge, user_data, out_operation, async move {
        let resp = relay_request(&conn, &service_id, http_req)
            .await
            .map_err(|e| format!("{e:#}"))?;
        let (parts, body) = resp.into_parts();
        let payload = codec::encode_expose_http_response(&ExposeHttpResponse {
            status: parts.status.as_u16(),
            headers: parts
                .headers
                .iter()
                .map(|(n, v)| (n.as_str().to_owned(), v.as_bytes().to_vec()))
                .collect(),
            body: HttpBodyMode::Inline(body),
        });
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT, 0).data(payload))
    })
}

/// Ask `connection`'s peer (an edge serving `aster_expose_route_policy_foreign`)
/// to route `specs` to us, attaching `metadata` for its policy. Completes with
/// `ASTER_EVENT_BYTES_RESULT`: payload = UCv1 `C8` (indexes into `specs` the
/// peer granted, ascending), `handle` = an `aster_route_registration_t` that
/// keeps the routes live until `aster_route_registration_free`. A refusal of
/// the whole request is `ASTER_EVENT_ERROR`.
#[no_mangle]
pub unsafe extern "C" fn aster_expose_request_route(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    specs: *const aster_route_spec_t,
    spec_count: usize,
    metadata: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_expose_request_route: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let Some(core) = bridge.connections.get(connection) else {
        return not_found(format!("unknown connection handle {connection}"));
    };
    if specs.is_null() && spec_count != 0 {
        return invalid("specs: NULL with non-zero count");
    }
    let mut requested = Vec::with_capacity(spec_count);
    if spec_count != 0 {
        for (i, s) in std::slice::from_raw_parts(specs, spec_count)
            .iter()
            .enumerate()
        {
            let protocol = match s.protocol {
                0 => RouteProtocol::Http,
                1 => RouteProtocol::Tcp,
                p => {
                    return invalid(format!(
                        "specs[{i}].protocol {p} is not 0 (http) or 1 (tcp)"
                    ))
                }
            };
            let port = match u16::try_from(s.port) {
                Ok(p) => p,
                Err(_) => return invalid(format!("specs[{i}].port {} exceeds 65535", s.port)),
            };
            requested.push(RouteSpec {
                host: try_status!(in_string(&s.host, "specs[].host")),
                port,
                protocol,
                service_id: try_status!(in_string(&s.service_id, "specs[].service_id")),
            });
        }
    }
    let metadata = try_status!(in_slice(&metadata, "metadata")).to_vec();
    let conn = Connection::from_core_connection((*core).clone());
    spawn_op(&bridge, user_data, out_operation, async move {
        let reg = request_route(&conn, requested.clone(), metadata)
            .await
            .map_err(|e| format!("{e:#}"))?;
        // Map the granted subset back to indexes (the edge returns a subset
        // of the request, in request order).
        let mut indexes = Vec::new();
        let mut next = 0;
        for (i, spec) in requested.iter().enumerate() {
            if reg.granted().get(next) == Some(spec) {
                indexes.push(i as u32);
                next += 1;
            }
        }
        let handle = REGISTRATIONS.insert(Mutex::new(Some(reg)));
        Ok(
            Done::ok(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT, handle)
                .data(codec::encode_expose_route_grant(&indexes)),
        )
    })
}

/// End a route registration: the edge evicts its routes. `NOT_FOUND` for an
/// unknown or already freed handle.
#[no_mangle]
pub unsafe extern "C" fn aster_route_registration_free(
    runtime: aster_runtime_t,
    registration: aster_route_registration_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    match REGISTRATIONS.remove(registration) {
        Some(slot) => {
            drop(slot.lock().unwrap().take());
            OK
        }
        None => not_found(format!("unknown route registration {registration}")),
    }
}
