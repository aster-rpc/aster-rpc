//! Upcalls (ABI 4.1, plan F5 / R6 / D6): Rust asks foreign code a question,
//! the binding answers by id.
//!
//! Design: `docs/_internal/bindings/upcall-design.md` (§4 "C ABI"). Payloads
//! are UCv1 messages (design §3, vectors `conformance/vectors/upcalls.json`);
//! bindings decode and encode them themselves.
//!
//! ```text
//!   aster_upcalls_new(rt, capacity) → upcalls     one table + bounded queue
//!   aster_upcall_set_policy(rt, u, kind, ms, fm)  per-kind timeout / failure mode
//!   aster_upcall_poll(rt, u, events[], max, ms)   drain events (batched)
//!   aster_upcall_respond(rt, u, id, status, ..)   answer by invocation id
//!   aster_upcall_buffer_release(rt, u, buffer)    free an event's payload
//!   aster_upcalls_close / aster_upcalls_free
//! ```
//!
//! Adapters that turn the table into the Rust extension points:
//!
//! - **Leases.** `aster_lease_serializer_foreign_new` (kinds 1/2: the binding
//!   is the serialization point) or `aster_lease_serializer_memory_new` (the
//!   reference in-memory serializer, optionally guarded by a foreign
//!   authorizer, kind 3). `aster_lease_acquire` on either returns a lease
//!   handle that renews in the background; `aster_lease_fence`,
//!   `aster_lease_status`, `aster_lease_watch`, `aster_lease_release`.
//!   `aster_lease_snapshot` / `aster_lease_revoke` go to the serializer.
//! - **Fenced resource.** `aster_fenced_resource_foreign_new` (kind 4) and
//!   `aster_fenced_mutate`.
//! - **Topology.** `aster_topology_admitted_new` (kind 16, cached and
//!   batched; see `aster::binding_support::upcall::topology`) and
//!   `aster_topology_enable_shared`, which installs it as the swarm's
//!   read-time `admitted` filter.
//!
//! Not in the C ABI (yet): placement providers (kinds 17/32/33 -- the C ABI
//! has no coordination client), expose authorize (kind 48 -- no expose
//! surface) and Gate 0 over upcalls (kinds 64/65 -- the 3.2 hook iterators
//! serve admission).
//!
//! # Threading
//!
//! `aster_upcall_poll` is single-consumer and may block up to `timeout_ms`
//! (run it off the runtime's threads). `aster_upcall_respond` never blocks
//! and may be called from any thread, in any order. Rust never waits for a
//! binding forever: each kind's timeout (default 5 s) ends the wait, the
//! binding then sees an `ASTER_UPCALL_KIND_CANCEL` event for the id (or the
//! queued event is withdrawn), and a late respond returns
//! `ASTER_STATUS_NOT_FOUND`, which bindings ignore.

use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aster::binding_support::upcall::lease::{
    AuthorizedSerializer, ForeignFencedResource, ForeignLeaseAuthorizer, ForeignLeaseSerializer,
};
use aster::binding_support::upcall::topology::{AdmittedCacheOptions, ForeignAdmittedFn};
use aster::binding_support::upcall::{
    codec, HookFailureMode, UpcallEvent, UpcallKind, UpcallPolicy, UpcallQueue, UpcallStatus,
    Upcalls,
};
use aster::lease::{
    AcquireOutcome, Fence, FencedResource, LeaseHandle, LeaseOptions, LeaseSerializer, LeaseStatus,
    MemorySerializer, TransitionOutcome,
};
use aster_transport_core::{CoreTopoSwarmConfig, TopoConfig};

use crate::data::{
    bridge_for, in_hex32, in_node_list, in_string, invalid, not_found, spawn_op, Done,
};
use crate::pure::{in_key, try_status};
use crate::{
    aster_buffer_t, aster_bytes_list_t, aster_bytes_t, aster_event_kind_t,
    aster_hook_failure_mode_t, aster_node_t, aster_operation_t, aster_runtime_t, aster_status_t,
    set_last_error, BufferRegistry, HandleRegistry,
};

const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;

// ============================================================================
// Types
// ============================================================================

/// An upcall table plus its bounded event queue (`aster_upcalls_new`).
pub type aster_upcalls_t = u64;
/// A lease serializer (`aster_lease_serializer_*_new`).
pub type aster_lease_serializer_t = u64;
/// A held lease (`ASTER_EVENT_LEASE_ACQUIRED`).
pub type aster_lease_t = u64;
/// A foreign fenced resource (`aster_fenced_resource_foreign_new`).
pub type aster_fenced_resource_t = u64;
/// A cached, batched foreign topology admission filter
/// (`aster_topology_admitted_new`).
pub type aster_topology_admitted_t = u64;

/// `aster_upcall_event_t.kind` (= `aster::binding_support::upcall::UpcallKind`).
/// Request → response UCv1 tags per kind are in the design doc §3.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_upcall_kind_t {
    /// Rust stopped waiting for `invocation_id` (`cancel_reason`). Never
    /// answered; drop the work for that id.
    ASTER_UPCALL_KIND_CANCEL = 0,
    /// `LeaseSerializer::transition`: `B1` → `B2`.
    ASTER_UPCALL_KIND_LEASE_TRANSITION = 1,
    /// `LeaseSerializer::snapshot`: `B3` → `B4`.
    ASTER_UPCALL_KIND_LEASE_SNAPSHOT = 2,
    /// Lease authorizer (decision): `B5` → `CF`.
    ASTER_UPCALL_KIND_LEASE_AUTHORIZE = 3,
    /// `FencedResource::mutate`: `B6` → `B7`.
    ASTER_UPCALL_KIND_FENCED_MUTATE = 4,
    /// Topology `admitted` filter, batched (decision): `B8` → `B9`.
    ASTER_UPCALL_KIND_TOPOLOGY_ADMITTED = 16,
    /// Placement topology source: `BA` → `BB` (not issued by the C ABI yet).
    ASTER_UPCALL_KIND_TOPOLOGY_SNAPSHOT = 17,
    /// Placement policy source: `BC` → `BD` (not issued by the C ABI yet).
    ASTER_UPCALL_KIND_PLACEMENT_POLICY = 32,
    /// Placement provisioner: `BE` → `BF` (not issued by the C ABI yet).
    ASTER_UPCALL_KIND_PLACEMENT_PROVISION = 33,
    /// expose authorize (decision): `C0` → `CF` (not issued by the C ABI yet).
    ASTER_UPCALL_KIND_EXPOSE_AUTHORIZE = 48,
    /// expose HTTP handler (R9): `C4` → `C5` (not issued by the C ABI yet).
    ASTER_UPCALL_KIND_EXPOSE_HTTP = 49,
    /// expose route policy (R9, decision): `C7` → `C8` (not issued by the
    /// C ABI yet).
    ASTER_UPCALL_KIND_EXPOSE_ROUTE_POLICY = 50,
    /// Gate 0 connect hook (decision): `C1` → `CF` (not issued by the C ABI yet).
    ASTER_UPCALL_KIND_ADMISSION_CONNECT = 64,
    /// Gate 0 handshake hook (decision): `C2` → `C3` (not issued by the C ABI yet).
    ASTER_UPCALL_KIND_ADMISSION_HANDSHAKE = 65,
}

/// `status` of `aster_upcall_respond`. An unknown value reads as `ERROR`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_upcall_status_t {
    /// `payload` is the kind's UCv1 response.
    ASTER_UPCALL_STATUS_OK = 0,
    /// The handler failed; `payload` is a UTF-8 message.
    ASTER_UPCALL_STATUS_ERROR = 1,
    /// No handler for this kind (`payload` ignored).
    ASTER_UPCALL_STATUS_UNIMPLEMENTED = 2,
}

/// `aster_upcall_event_t.cancel_reason` (0 on every other kind).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_upcall_cancel_reason_t {
    ASTER_UPCALL_CANCEL_NONE = 0,
    /// The kind's timeout passed.
    ASTER_UPCALL_CANCEL_TIMEOUT = 1,
    /// The waiting Rust caller gave up (e.g. a dropped acquire).
    ASTER_UPCALL_CANCEL_ABANDONED = 2,
    /// `aster_upcalls_close` / `aster_upcalls_free` ran.
    ASTER_UPCALL_CANCEL_SHUTDOWN = 3,
}

/// Holder-side lease status (`aster_lease_status`, `ASTER_EVENT_LEASE_STATUS`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum aster_lease_status_t {
    ASTER_LEASE_STATUS_HELD = 0,
    /// Released by this holder.
    ASTER_LEASE_STATUS_RELEASED = 1,
    /// A renewal was refused (someone holds a newer epoch, or revoked).
    ASTER_LEASE_STATUS_FENCED = 2,
    /// The self-limit passed without a successful renewal.
    ASTER_LEASE_STATUS_LOST = 3,
}

/// Default `aster_upcalls_new` capacity (queued upcalls before refusal).
pub const ASTER_UPCALL_DEFAULT_CAPACITY: u32 = 1024;

/// One upcall event, filled by `aster_upcall_poll`. 40 bytes (LP64).
///
/// `payload_ptr` / `payload_len` is the kind's UCv1 request, valid until
/// `aster_upcall_buffer_release(rt, upcalls, buffer)`; an empty payload is
/// `{NULL, 0}` with `buffer` 0. `CANCEL` events carry no payload: the reason
/// is in `cancel_reason` and `invocation_id` names the cancelled upcall.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct aster_upcall_event_t {
    /// The id to pass to `aster_upcall_respond`. Unique per table, from 1.
    pub invocation_id: u64,
    /// `aster_upcall_kind_t`.
    pub kind: u16,
    /// `CANCEL`: `aster_upcall_cancel_reason_t`; otherwise 0.
    pub cancel_reason: u8,
    pub _p0: u8,
    pub payload_len: u32,
    /// When Rust stops waiting (Unix ms; 0 on `CANCEL`). Informational.
    pub deadline_unix_ms: u64,
    pub payload_ptr: *const u8,
    /// Lease for `payload_ptr` (0 = nothing to release).
    pub buffer: aster_buffer_t,
}

/// Counters of a topology admission filter (`aster_topology_admitted_stats`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct aster_topology_admitted_stats_t {
    /// Lookups answered from a fresh entry.
    pub hits: u64,
    /// Lookups answered from a stale entry (refresh queued).
    pub stale: u64,
    /// Lookups with no entry (failure-mode answer, resolution queued).
    pub misses: u64,
    /// Batched upcalls issued.
    pub upcalls: u64,
}

// ============================================================================
// Registries
// ============================================================================

pub(crate) struct UpcallsState {
    upcalls: Upcalls,
    queue: Arc<UpcallQueue>,
    buffers: BufferRegistry,
    /// Serializes consumers (`aster_upcall_poll` is single-consumer).
    poll: Mutex<()>,
}

impl UpcallsState {
    fn close(&self) {
        // Pending upcalls fail now (decisions fall back) and their Cancel
        // events are queued; closing the queue afterwards wakes the poller
        // but keeps those events drainable.
        self.upcalls.close();
        self.queue.close();
    }
}

static UPCALLS: once_cell::sync::Lazy<HandleRegistry<UpcallsState>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);
static SERIALIZERS: once_cell::sync::Lazy<HandleRegistry<Arc<dyn LeaseSerializer>>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);
static LEASES: once_cell::sync::Lazy<HandleRegistry<Mutex<Option<LeaseHandle>>>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);
static FENCED: once_cell::sync::Lazy<HandleRegistry<ForeignFencedResource>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);
static ADMITTED: once_cell::sync::Lazy<HandleRegistry<ForeignAdmittedFn>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);

fn upcalls_for(upcalls: aster_upcalls_t) -> Result<Arc<UpcallsState>, i32> {
    UPCALLS
        .get(upcalls)
        .ok_or_else(|| not_found(format!("unknown upcalls handle {upcalls}")))
}

fn failure_mode(v: u32) -> Result<HookFailureMode, i32> {
    match v {
        x if x == aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_OPEN as u32 => {
            Ok(HookFailureMode::FailOpen)
        }
        x if x == aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_CLOSED as u32 => {
            Ok(HookFailureMode::FailClosed)
        }
        _ => Err(invalid(format!("unknown aster_hook_failure_mode_t {v}"))),
    }
}

fn policy(timeout_ms: u32, mode: u32) -> Result<UpcallPolicy, i32> {
    if timeout_ms == 0 {
        return Err(invalid("timeout_ms must be > 0"));
    }
    Ok(UpcallPolicy::new(
        Duration::from_millis(timeout_ms as u64),
        failure_mode(mode)?,
    ))
}

fn lease_status_code(s: LeaseStatus) -> u32 {
    (match s {
        LeaseStatus::Held => aster_lease_status_t::ASTER_LEASE_STATUS_HELD,
        LeaseStatus::Released => aster_lease_status_t::ASTER_LEASE_STATUS_RELEASED,
        LeaseStatus::Fenced => aster_lease_status_t::ASTER_LEASE_STATUS_FENCED,
        LeaseStatus::Lost => aster_lease_status_t::ASTER_LEASE_STATUS_LOST,
    }) as u32
}

// ============================================================================
// The upcall table
// ============================================================================

/// Create an upcall table with a bounded queue of `capacity` upcalls (0 =
/// `ASTER_UPCALL_DEFAULT_CAPACITY`). When the queue is full, further
/// upcalls fail at once (`Overloaded`: decisions take the failure mode,
/// other kinds error); `CANCEL` events are always queued. Every kind starts
/// with the default policy: 5 s, fail closed.
#[no_mangle]
pub unsafe extern "C" fn aster_upcalls_new(
    runtime: aster_runtime_t,
    capacity: u32,
    out_upcalls: *mut aster_upcalls_t,
) -> i32 {
    if out_upcalls.is_null() {
        return invalid("aster_upcalls_new: out_upcalls is NULL");
    }
    *out_upcalls = 0;
    try_status!(bridge_for(runtime));
    let cap = if capacity == 0 {
        ASTER_UPCALL_DEFAULT_CAPACITY
    } else {
        capacity
    };
    let queue = UpcallQueue::new(cap as usize);
    let upcalls = Upcalls::new(queue.clone());
    *out_upcalls = UPCALLS.insert(UpcallsState {
        upcalls,
        queue,
        buffers: BufferRegistry::new(),
        poll: Mutex::new(()),
    });
    OK
}

/// Close the table: pending upcalls fail now (decision kinds take their
/// failure mode, others error), each gets a `CANCEL` event with reason
/// `SHUTDOWN`, and new upcalls fail with `Closed`. Queued events stay
/// drainable; `aster_upcall_respond` keeps returning `NOT_FOUND`. Adapters
/// built on the table keep their handles but can no longer reach the
/// binding. Idempotent.
#[no_mangle]
pub unsafe extern "C" fn aster_upcalls_close(
    runtime: aster_runtime_t,
    upcalls: aster_upcalls_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    try_status!(upcalls_for(upcalls)).close();
    OK
}

/// Close (as `aster_upcalls_close`) and free the handle. Payload buffers not
/// yet released are freed with it.
#[no_mangle]
pub unsafe extern "C" fn aster_upcalls_free(
    runtime: aster_runtime_t,
    upcalls: aster_upcalls_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    match UPCALLS.remove(upcalls) {
        Some(state) => {
            state.close();
            OK
        }
        None => not_found(format!("unknown upcalls handle {upcalls}")),
    }
}

/// Set the timeout and failure mode (`aster_hook_failure_mode_t`) of one
/// kind. The failure mode applies to decision kinds (lease authorize,
/// topology admitted, expose authorize, admission): it is the answer used
/// when the binding gives none (error, timeout, overload, shutdown,
/// undecodable answer). `timeout_ms` must be > 0; `CANCEL` is not a kind
/// that can have a policy.
#[no_mangle]
pub unsafe extern "C" fn aster_upcall_set_policy(
    runtime: aster_runtime_t,
    upcalls: aster_upcalls_t,
    kind: u32,
    timeout_ms: u32,
    failure_mode: u32,
) -> i32 {
    try_status!(bridge_for(runtime));
    let state = try_status!(upcalls_for(upcalls));
    let kind = match u16::try_from(kind).ok().and_then(UpcallKind::from_code) {
        Some(k) if k != UpcallKind::Cancel => k,
        _ => return invalid(format!("unknown or invalid upcall kind {kind}")),
    };
    let p = try_status!(policy(timeout_ms, failure_mode));
    state.upcalls.set_policy(kind, p);
    OK
}

/// Set the policy of every kind without its own (`aster_upcall_set_policy`).
#[no_mangle]
pub unsafe extern "C" fn aster_upcall_set_default_policy(
    runtime: aster_runtime_t,
    upcalls: aster_upcalls_t,
    timeout_ms: u32,
    failure_mode: u32,
) -> i32 {
    try_status!(bridge_for(runtime));
    let state = try_status!(upcalls_for(upcalls));
    let p = try_status!(policy(timeout_ms, failure_mode));
    state.upcalls.set_default_policy(p);
    OK
}

/// Drain up to `max` events into `out`. `timeout_ms` 0 = return at once;
/// otherwise wait up to that long for the first event (or until the table
/// is closed). Returns the count (0 on timeout, unknown handle or bad
/// arguments). Single consumer; blocks the calling thread, so run it off
/// the runtime's threads (and off virtual-thread carriers when waiting).
#[no_mangle]
pub unsafe extern "C" fn aster_upcall_poll(
    runtime: aster_runtime_t,
    upcalls: aster_upcalls_t,
    out: *mut aster_upcall_event_t,
    max: u32,
    timeout_ms: u32,
) -> u32 {
    if out.is_null() || max == 0 || load_ok(runtime).is_none() {
        return 0;
    }
    let Some(state) = UPCALLS.get(upcalls) else {
        return 0;
    };
    let _consumer = state.poll.lock().unwrap();
    let mut written = drain_into(&state, out, max);
    if written == 0 && timeout_ms > 0 {
        state
            .queue
            .wait_blocking(Duration::from_millis(timeout_ms as u64));
        written = drain_into(&state, out, max);
    }
    written
}

fn load_ok(runtime: aster_runtime_t) -> Option<()> {
    crate::load_runtime(runtime).ok().map(|_| ())
}

fn drain_into(state: &UpcallsState, out: *mut aster_upcall_event_t, max: u32) -> u32 {
    let mut written = 0u32;
    state.queue.drain(max as usize, |ev| {
        let lowered = lower(&state.buffers, ev);
        // SAFETY: the caller provides room for `max` events; `drain` yields
        // at most `max`.
        unsafe { ptr::write(out.add(written as usize), lowered) };
        written += 1;
    });
    written
}

fn lower(buffers: &BufferRegistry, ev: UpcallEvent) -> aster_upcall_event_t {
    let mut out = aster_upcall_event_t {
        invocation_id: ev.invocation_id,
        kind: ev.kind.code(),
        cancel_reason: 0,
        _p0: 0,
        payload_len: 0,
        deadline_unix_ms: ev.deadline_unix_ms,
        payload_ptr: ptr::null(),
        buffer: 0,
    };
    if ev.kind == UpcallKind::Cancel {
        out.cancel_reason = ev.payload.first().copied().unwrap_or(0);
        return out;
    }
    if !ev.payload.is_empty() {
        let (id, data) = buffers.insert(ev.payload);
        out.payload_len = data.len() as u32;
        out.payload_ptr = data.as_ptr();
        out.buffer = id;
    }
    out
}

/// Release an event's payload. 0 is a no-op; an unknown buffer returns
/// `NOT_FOUND`.
#[no_mangle]
pub unsafe extern "C" fn aster_upcall_buffer_release(
    runtime: aster_runtime_t,
    upcalls: aster_upcalls_t,
    buffer: aster_buffer_t,
) -> i32 {
    if buffer == 0 {
        return OK;
    }
    try_status!(bridge_for(runtime));
    let state = try_status!(upcalls_for(upcalls));
    if state.buffers.release(buffer) {
        OK
    } else {
        not_found(format!("unknown upcall buffer {buffer}"))
    }
}

/// Answer upcall `invocation_id` with `status` (`aster_upcall_status_t`) and
/// `payload` (the kind's UCv1 response for `OK`, a UTF-8 message for
/// `ERROR`, ignored for `UNIMPLEMENTED`). Copies the payload; never blocks;
/// any thread, any order. `NOT_FOUND` when the id is not pending (never
/// issued, answered, cancelled or timed out) -- ignore it. A malformed `OK`
/// payload is not detected here: the waiting Rust caller sees a decode
/// error (decision kinds take their failure mode).
#[no_mangle]
pub unsafe extern "C" fn aster_upcall_respond(
    runtime: aster_runtime_t,
    upcalls: aster_upcalls_t,
    invocation_id: u64,
    status: u32,
    payload: *const u8,
    payload_len: u32,
) -> i32 {
    try_status!(bridge_for(runtime));
    let state = try_status!(upcalls_for(upcalls));
    if payload.is_null() && payload_len != 0 {
        return invalid("aster_upcall_respond: NULL payload with non-zero length");
    }
    let bytes = if payload_len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(payload, payload_len as usize).to_vec()
    };
    match state
        .upcalls
        .respond(invocation_id, UpcallStatus::from_code(status), bytes)
    {
        Ok(()) => OK,
        Err(e) => {
            set_last_error(e);
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        }
    }
}

// ============================================================================
// Leases
// ============================================================================

/// A lease serializer whose serialization point is the binding: every
/// transition is a `LEASE_TRANSITION` upcall and every snapshot a
/// `LEASE_SNAPSHOT` upcall on `upcalls`. A failed upcall is a backend error
/// (the acquire / renew / release fails; it is never read as a denial).
#[no_mangle]
pub unsafe extern "C" fn aster_lease_serializer_foreign_new(
    runtime: aster_runtime_t,
    upcalls: aster_upcalls_t,
    out_serializer: *mut aster_lease_serializer_t,
) -> i32 {
    if out_serializer.is_null() {
        return invalid("aster_lease_serializer_foreign_new: out_serializer is NULL");
    }
    *out_serializer = 0;
    try_status!(bridge_for(runtime));
    let state = try_status!(upcalls_for(upcalls));
    let s: Arc<dyn LeaseSerializer> = Arc::new(ForeignLeaseSerializer::new(state.upcalls.clone()));
    *out_serializer = SERIALIZERS.insert(s);
    OK
}

/// The reference in-memory serializer (`aster::lease::MemorySerializer`:
/// rows under one lock, expiry on this process's clock). With
/// `authorizer_upcalls` != 0 every transition is first put to the binding as
/// a `LEASE_AUTHORIZE` decision; a refusal (or no answer under fail-closed)
/// is `Denied(Unauthorized)`. 0 = everyone is authorized.
#[no_mangle]
pub unsafe extern "C" fn aster_lease_serializer_memory_new(
    runtime: aster_runtime_t,
    authorizer_upcalls: aster_upcalls_t,
    out_serializer: *mut aster_lease_serializer_t,
) -> i32 {
    if out_serializer.is_null() {
        return invalid("aster_lease_serializer_memory_new: out_serializer is NULL");
    }
    *out_serializer = 0;
    try_status!(bridge_for(runtime));
    let s: Arc<dyn LeaseSerializer> = if authorizer_upcalls == 0 {
        Arc::new(MemorySerializer::new())
    } else {
        let state = try_status!(upcalls_for(authorizer_upcalls));
        Arc::new(AuthorizedSerializer::new(
            MemorySerializer::new(),
            ForeignLeaseAuthorizer::new(state.upcalls.clone()),
        ))
    };
    *out_serializer = SERIALIZERS.insert(s);
    OK
}

/// Free a serializer handle. Leases acquired from it keep their own
/// reference and keep renewing.
#[no_mangle]
pub unsafe extern "C" fn aster_lease_serializer_free(
    runtime: aster_runtime_t,
    serializer: aster_lease_serializer_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    match SERIALIZERS.remove(serializer) {
        Some(_) => OK,
        None => not_found(format!("unknown lease serializer {serializer}")),
    }
}

fn serializer_for(serializer: aster_lease_serializer_t) -> Result<Arc<dyn LeaseSerializer>, i32> {
    SERIALIZERS
        .get(serializer)
        .map(|s| s.as_ref().clone())
        .ok_or_else(|| not_found(format!("unknown lease serializer {serializer}")))
}

fn lease_options(ttl_ms: u64, renew_ms: u64, margin_ms: u64) -> Result<LeaseOptions, i32> {
    if ttl_ms == 0 {
        if renew_ms != 0 || margin_ms != 0 {
            return Err(invalid(
                "renew_interval_ms / holder_margin_ms need an explicit ttl_ms",
            ));
        }
        return Ok(LeaseOptions::default());
    }
    let ttl = Duration::from_millis(ttl_ms);
    let margin = if margin_ms == 0 {
        Duration::from_millis((ttl_ms / 10).clamp(1, 1000))
    } else {
        Duration::from_millis(margin_ms)
    };
    let renew = if renew_ms == 0 {
        ttl / 3
    } else {
        Duration::from_millis(renew_ms)
    };
    if margin >= ttl {
        return Err(invalid("holder_margin_ms must be smaller than ttl_ms"));
    }
    if renew.is_zero() || renew >= ttl - margin {
        return Err(invalid(
            "renew_interval_ms must be > 0 and fit inside ttl_ms - holder_margin_ms",
        ));
    }
    Ok(LeaseOptions {
        ttl,
        renew_interval: renew,
        holder_margin: margin,
    })
}

/// Try to acquire `resource` for `holder` (a hex node id, or any stable
/// actor name the serializer understands) -- one ACQUIRE at the serializer.
///
/// `ttl_ms` 0 = the defaults (30 s ttl, 10 s renew, 1 s margin); with a ttl,
/// 0 for `renew_interval_ms` = ttl/3 and 0 for `holder_margin_ms` =
/// ttl/10 capped at 1 s. Invalid timing is `INVALID_ARGUMENT`.
///
/// Completes with `ASTER_EVENT_LEASE_ACQUIRED`; the payload is the UCv1
/// `TransitionOutcome` (`B2`): granted (`handle` = a new `aster_lease_t`
/// that renews in the background until released or freed; the payload
/// carries the fence) or denied (`handle` = 0; reason + snapshot, the
/// snapshot's `remaining_ms` is a back-off hint). A serializer failure is
/// `ASTER_EVENT_ERROR`.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_lease_acquire(
    runtime: aster_runtime_t,
    serializer: aster_lease_serializer_t,
    resource: aster_bytes_t,
    holder: aster_bytes_t,
    ttl_ms: u64,
    renew_interval_ms: u64,
    holder_margin_ms: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_lease_acquire: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let ser = try_status!(serializer_for(serializer));
    let resource = try_status!(in_string(&resource, "resource"));
    let holder = try_status!(in_string(&holder, "holder"));
    if resource.is_empty() || holder.is_empty() {
        return invalid("resource and holder must be non-empty");
    }
    let opts = try_status!(lease_options(ttl_ms, renew_interval_ms, holder_margin_ms));
    spawn_op(&bridge, user_data, out_operation, async move {
        let outcome = LeaseHandle::acquire(ser, &resource, &holder, opts)
            .await
            .map_err(|e| format!("{e:#}"))?;
        let kind = aster_event_kind_t::ASTER_EVENT_LEASE_ACQUIRED;
        Ok(match outcome {
            AcquireOutcome::Granted(lease) => {
                let payload =
                    codec::encode_transition_outcome(&TransitionOutcome::Granted(Fence {
                        resource: lease.resource().to_string(),
                        epoch: lease.epoch(),
                        holder: holder.clone(),
                    }));
                let id = LEASES.insert(Mutex::new(Some(lease)));
                Done::ok(kind, id).data(payload)
            }
            AcquireOutcome::Denied(reason, snap) => Done::ok(kind, 0).data(
                codec::encode_transition_outcome(&TransitionOutcome::Denied(reason, snap)),
            ),
        })
    })
}

fn lease_for(lease: aster_lease_t) -> Result<Arc<Mutex<Option<LeaseHandle>>>, i32> {
    LEASES
        .get(lease)
        .ok_or_else(|| not_found(format!("unknown lease {lease}")))
}

/// The current fence epoch of a held lease, for stamping on a protected
/// action **at send time** (do not cache it). `ALREADY_CLOSED` once the
/// holder must stop acting: the lease was fenced, lost, released, or its
/// self-limit passed.
#[no_mangle]
pub unsafe extern "C" fn aster_lease_fence(
    runtime: aster_runtime_t,
    lease: aster_lease_t,
    out_epoch: *mut u64,
) -> i32 {
    if out_epoch.is_null() {
        return invalid("aster_lease_fence: out_epoch is NULL");
    }
    *out_epoch = 0;
    try_status!(bridge_for(runtime));
    let slot = try_status!(lease_for(lease));
    let guard = slot.lock().unwrap();
    match guard.as_ref().and_then(LeaseHandle::fence) {
        Some(f) => {
            *out_epoch = f.epoch;
            OK
        }
        None => {
            set_last_error("lease no longer usable (fenced, lost, released or self-limit passed)");
            aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32
        }
    }
}

/// The holder-side status (`aster_lease_status_t`).
#[no_mangle]
pub unsafe extern "C" fn aster_lease_status(
    runtime: aster_runtime_t,
    lease: aster_lease_t,
    out_status: *mut u32,
) -> i32 {
    if out_status.is_null() {
        return invalid("aster_lease_status: out_status is NULL");
    }
    try_status!(bridge_for(runtime));
    let slot = try_status!(lease_for(lease));
    let guard = slot.lock().unwrap();
    *out_status = guard.as_ref().map_or(
        aster_lease_status_t::ASTER_LEASE_STATUS_RELEASED as u32,
        |h| lease_status_code(h.status()),
    );
    OK
}

/// Complete when the lease leaves `HELD` (fenced, lost or released) with
/// `ASTER_EVENT_LEASE_STATUS`, `flags` = the new `aster_lease_status_t`.
/// Completes at once if it already has. If the lease is freed first the
/// event carries status `ALREADY_CLOSED`. Cancel with
/// `aster_operation_cancel`.
#[no_mangle]
pub unsafe extern "C" fn aster_lease_watch(
    runtime: aster_runtime_t,
    lease: aster_lease_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_lease_watch: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let slot = try_status!(lease_for(lease));
    let rx = slot.lock().unwrap().as_ref().map(LeaseHandle::watch);
    let kind = aster_event_kind_t::ASTER_EVENT_LEASE_STATUS;
    spawn_op(&bridge, user_data, out_operation, async move {
        let Some(mut rx) = rx else {
            return Ok(Done::ok(kind, lease)
                .flags(aster_lease_status_t::ASTER_LEASE_STATUS_RELEASED as u32));
        };
        let res = rx.wait_for(|s| *s != LeaseStatus::Held).await.map(|s| *s);
        Ok(match res {
            Ok(s) => Done::ok(kind, lease).flags(lease_status_code(s)),
            Err(_) => Done::ok(kind, lease).status(aster_status_t::ASTER_STATUS_ALREADY_CLOSED),
        })
    })
}

/// Graceful release: stop renewing, then RELEASE at the serializer, which
/// advances the fence (this holder's in-flight writes are rejected from
/// here on). Consumes the handle (later calls return `NOT_FOUND`).
/// Completes with `ASTER_EVENT_UNIT_RESULT`, or `ASTER_EVENT_ERROR` if the
/// serializer failed (the lease then lapses by TTL).
#[no_mangle]
pub unsafe extern "C" fn aster_lease_release(
    runtime: aster_runtime_t,
    lease: aster_lease_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_lease_release: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let Some(slot) = LEASES.remove(lease) else {
        return not_found(format!("unknown lease {lease}"));
    };
    let Some(handle) = slot.lock().unwrap().take() else {
        return not_found(format!("lease {lease} already released"));
    };
    spawn_op(&bridge, user_data, out_operation, async move {
        handle.release().await.map_err(|e| format!("{e:#}"))?;
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_UNIT_RESULT, lease))
    })
}

/// Free a lease handle **without** releasing: renewals stop and the lease
/// lapses at the serializer by TTL. Prefer `aster_lease_release`.
#[no_mangle]
pub unsafe extern "C" fn aster_lease_free(runtime: aster_runtime_t, lease: aster_lease_t) -> i32 {
    try_status!(bridge_for(runtime));
    match LEASES.remove(lease) {
        Some(slot) => {
            drop(slot.lock().unwrap().take());
            OK
        }
        None => not_found(format!("unknown lease {lease}")),
    }
}

/// Advisory snapshot of `resource` at the serializer (stale the moment it
/// returns; hints only). Completes with `ASTER_EVENT_BYTES_RESULT`, payload
/// = UCv1 `LeaseSnapshot` (`B4`).
#[no_mangle]
pub unsafe extern "C" fn aster_lease_snapshot(
    runtime: aster_runtime_t,
    serializer: aster_lease_serializer_t,
    resource: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_lease_snapshot: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let ser = try_status!(serializer_for(serializer));
    let resource = try_status!(in_string(&resource, "resource"));
    spawn_op(&bridge, user_data, out_operation, async move {
        let snap = ser
            .snapshot(&resource)
            .await
            .map_err(|e| format!("{e:#}"))?;
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT, 0)
            .data(codec::encode_lease_snapshot(&snap)))
    })
}

/// REVOKE `resource` at the serializer on behalf of `authority` (advances
/// the fence). Completes with `ASTER_EVENT_BYTES_RESULT`, payload = UCv1
/// `TransitionOutcome` (`B2`: revoked, or denied with a reason).
#[no_mangle]
pub unsafe extern "C" fn aster_lease_revoke(
    runtime: aster_runtime_t,
    serializer: aster_lease_serializer_t,
    resource: aster_bytes_t,
    authority: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_lease_revoke: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let ser = try_status!(serializer_for(serializer));
    let resource = try_status!(in_string(&resource, "resource"));
    let authority = try_status!(in_string(&authority, "authority"));
    spawn_op(&bridge, user_data, out_operation, async move {
        let outcome = aster::lease::revoke(ser.as_ref(), &resource, &authority)
            .await
            .map_err(|e| format!("{e:#}"))?;
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT, 0)
            .data(codec::encode_transition_outcome(&outcome)))
    })
}

// ============================================================================
// Fenced resource
// ============================================================================

/// A fenced resource implemented by the binding: each mutation is a
/// `FENCED_MUTATE` upcall that must evaluate the fence rule inside the
/// resource's own serialization point (the adapter cannot enforce that).
#[no_mangle]
pub unsafe extern "C" fn aster_fenced_resource_foreign_new(
    runtime: aster_runtime_t,
    upcalls: aster_upcalls_t,
    out_resource: *mut aster_fenced_resource_t,
) -> i32 {
    if out_resource.is_null() {
        return invalid("aster_fenced_resource_foreign_new: out_resource is NULL");
    }
    *out_resource = 0;
    try_status!(bridge_for(runtime));
    let state = try_status!(upcalls_for(upcalls));
    *out_resource = FENCED.insert(ForeignFencedResource::new(state.upcalls.clone()));
    OK
}

/// Free a fenced resource handle.
#[no_mangle]
pub unsafe extern "C" fn aster_fenced_resource_free(
    runtime: aster_runtime_t,
    resource: aster_fenced_resource_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    match FENCED.remove(resource) {
        Some(_) => OK,
        None => not_found(format!("unknown fenced resource {resource}")),
    }
}

/// Apply `op` (opaque bytes) under the fence `(fence_resource, epoch,
/// fence_holder)` as authenticated `writer`. Completes with
/// `ASTER_EVENT_BYTES_RESULT`, payload = UCv1 `FencedMutateResponse`
/// (`B7`: applied with the output bytes, or the fence rejection). Any other
/// failure (timeout, binding error, closed table) is `ASTER_EVENT_ERROR`
/// and means the **outcome is unknown** -- a write cannot be failed open.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_fenced_mutate(
    runtime: aster_runtime_t,
    resource: aster_fenced_resource_t,
    fence_resource: aster_bytes_t,
    epoch: u64,
    fence_holder: aster_bytes_t,
    writer: aster_bytes_t,
    op: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_fenced_mutate: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let Some(res) = FENCED.get(resource) else {
        return not_found(format!("unknown fenced resource {resource}"));
    };
    let fence = Fence {
        resource: try_status!(in_string(&fence_resource, "fence_resource")),
        epoch,
        holder: try_status!(in_string(&fence_holder, "fence_holder")),
    };
    let writer = try_status!(in_string(&writer, "writer"));
    let op = try_status!(crate::data::in_slice(&op, "op")).to_vec();
    spawn_op(&bridge, user_data, out_operation, async move {
        let result = match res.mutate(&fence, &writer, op).await {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(e)) => return Err(format!("fenced mutation outcome unknown: {e}")),
            Err(rejection) => Err(rejection),
        };
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT, 0)
            .data(codec::encode_fenced_mutate_response(&result)))
    })
}

// ============================================================================
// Topology
// ============================================================================

/// A topology read-time admission filter answered by the binding
/// (`TOPOLOGY_ADMITTED`, batched). Lookups never block: a fresh cached
/// answer is served; a stale one is served and refreshed; a miss gets the
/// kind's failure-mode answer (default: not admitted) and is resolved in
/// the background. `ttl_ms` (0 = 30 s) is how long an answer is served,
/// `failure_retry_ms` (0 = 1 s) how long a failed batch's fallback is
/// cached, `max_batch` (0 = 256) the most ids per upcall.
#[no_mangle]
pub unsafe extern "C" fn aster_topology_admitted_new(
    runtime: aster_runtime_t,
    upcalls: aster_upcalls_t,
    ttl_ms: u32,
    failure_retry_ms: u32,
    max_batch: u32,
    out_admitted: *mut aster_topology_admitted_t,
) -> i32 {
    if out_admitted.is_null() {
        return invalid("aster_topology_admitted_new: out_admitted is NULL");
    }
    *out_admitted = 0;
    let bridge = try_status!(bridge_for(runtime));
    let state = try_status!(upcalls_for(upcalls));
    let d = AdmittedCacheOptions::default();
    let opts = AdmittedCacheOptions {
        ttl: if ttl_ms == 0 {
            d.ttl
        } else {
            Duration::from_millis(ttl_ms as u64)
        },
        failure_retry: if failure_retry_ms == 0 {
            d.failure_retry
        } else {
            Duration::from_millis(failure_retry_ms as u64)
        },
        max_batch: if max_batch == 0 {
            d.max_batch
        } else {
            max_batch as usize
        },
    };
    // The refresher task is spawned on the bridge runtime.
    let _enter = bridge.runtime.enter();
    *out_admitted = ADMITTED.insert(ForeignAdmittedFn::new(state.upcalls.clone(), opts));
    OK
}

/// Free a filter handle. A swarm it was installed in keeps using it.
#[no_mangle]
pub unsafe extern "C" fn aster_topology_admitted_free(
    runtime: aster_runtime_t,
    admitted: aster_topology_admitted_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    match ADMITTED.remove(admitted) {
        Some(_) => OK,
        None => not_found(format!("unknown topology admitted filter {admitted}")),
    }
}

fn admitted_for(admitted: aster_topology_admitted_t) -> Result<Arc<ForeignAdmittedFn>, i32> {
    ADMITTED
        .get(admitted)
        .ok_or_else(|| not_found(format!("unknown topology admitted filter {admitted}")))
}

/// Look `node_id` (64 hex) up exactly as the swarm's filter does (never
/// blocks; a miss queues a resolution). `*out_admitted` = 1 or 0.
#[no_mangle]
pub unsafe extern "C" fn aster_topology_admitted_check(
    runtime: aster_runtime_t,
    admitted: aster_topology_admitted_t,
    node_id: aster_bytes_t,
    out_admitted: *mut u32,
) -> i32 {
    if out_admitted.is_null() {
        return invalid("aster_topology_admitted_check: out_admitted is NULL");
    }
    try_status!(bridge_for(runtime));
    let f = try_status!(admitted_for(admitted));
    let id = try_status!(in_hex32(&node_id, "node_id"));
    *out_admitted = u32::from(f.is_admitted(&id));
    OK
}

/// Push an answer for `node_id` (e.g. on revocation); served for `ttl`.
#[no_mangle]
pub unsafe extern "C" fn aster_topology_admitted_set(
    runtime: aster_runtime_t,
    admitted: aster_topology_admitted_t,
    node_id: aster_bytes_t,
    is_admitted: u32,
) -> i32 {
    try_status!(bridge_for(runtime));
    let f = try_status!(admitted_for(admitted));
    let id = try_status!(in_hex32(&node_id, "node_id"));
    f.set(&id, is_admitted != 0);
    OK
}

/// Forget `node_id`'s answer (the next lookup re-resolves); an empty
/// `node_id` forgets every answer.
#[no_mangle]
pub unsafe extern "C" fn aster_topology_admitted_invalidate(
    runtime: aster_runtime_t,
    admitted: aster_topology_admitted_t,
    node_id: aster_bytes_t,
) -> i32 {
    try_status!(bridge_for(runtime));
    let f = try_status!(admitted_for(admitted));
    if node_id.len == 0 {
        f.clear();
    } else {
        f.invalidate(&try_status!(in_hex32(&node_id, "node_id")));
    }
    OK
}

/// Resolve `node_ids` (64 hex each) now with one upcall and cache the
/// answers (warm the cache before `aster_topology_enable_shared`).
/// Completes with `ASTER_EVENT_UNIT_RESULT`, or `ASTER_EVENT_ERROR` if the
/// upcall failed (the fallback is then cached for `failure_retry`).
#[no_mangle]
pub unsafe extern "C" fn aster_topology_admitted_prefetch(
    runtime: aster_runtime_t,
    admitted: aster_topology_admitted_t,
    node_ids: aster_bytes_list_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_topology_admitted_prefetch: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let f = try_status!(admitted_for(admitted));
    let ids: Vec<String> = try_status!(in_hex_list(&node_ids, "node_ids"));
    spawn_op(&bridge, user_data, out_operation, async move {
        f.prefetch(&ids).await.map_err(|e| e.to_string())?;
        Ok(Done::ok(
            aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
            admitted,
        ))
    })
}

unsafe fn in_hex_list(l: &aster_bytes_list_t, what: &str) -> Result<Vec<String>, i32> {
    if l.len == 0 {
        return Ok(Vec::new());
    }
    if l.items.is_null() {
        return Err(invalid(format!("{what}: NULL items with length {}", l.len)));
    }
    std::slice::from_raw_parts(l.items, l.len)
        .iter()
        .enumerate()
        .map(|(i, b)| in_hex32(b, &format!("{what}[{i}]")))
        .collect()
}

/// Copy the filter's counters into `*out`.
#[no_mangle]
pub unsafe extern "C" fn aster_topology_admitted_stats(
    runtime: aster_runtime_t,
    admitted: aster_topology_admitted_t,
    out: *mut aster_topology_admitted_stats_t,
) -> i32 {
    if out.is_null() {
        return invalid("aster_topology_admitted_stats: out is NULL");
    }
    try_status!(bridge_for(runtime));
    let f = try_status!(admitted_for(admitted));
    let s = f.stats();
    *out = aster_topology_admitted_stats_t {
        hits: s.hits,
        stale: s.stale,
        misses: s.misses,
        upcalls: s.upcalls,
    };
    OK
}

/// Join a topology swarm (topology v2, `aster::Topology::enable_shared`):
/// import the shared namespace from its 32-byte `secret`, publish this
/// node's measurements and derive the swarm-wide cluster view. The node must
/// run with `enable_monitoring`. `admitted` (0 = none: every holder of the
/// secret counts) installs a foreign read-time admission filter.
/// `bootstrap` are extra peers (64-hex node ids) to sync with;
/// `position_refresh_ms` 0 = 60 s. Completes with
/// `ASTER_EVENT_UNIT_RESULT` (`handle` = node).
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn aster_topology_enable_shared(
    runtime: aster_runtime_t,
    node: aster_node_t,
    secret: *const u8,
    admitted: aster_topology_admitted_t,
    bootstrap: aster_bytes_list_t,
    position_refresh_ms: u32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return invalid("aster_topology_enable_shared: out_operation is NULL");
    }
    let bridge = try_status!(bridge_for(runtime));
    let Some(core) = bridge.nodes.get(node) else {
        return not_found(format!("unknown node handle {node}"));
    };
    let secret = try_status!(in_key(secret, "secret"));
    let filter = if admitted == 0 {
        None
    } else {
        Some(try_status!(admitted_for(admitted)).admitted_fn())
    };
    let bootstrap: Vec<String> = try_status!(in_node_list(&bootstrap, "bootstrap"))
        .iter()
        .map(|n| n.to_string())
        .collect();
    let position_refresh = if position_refresh_ms == 0 {
        Duration::from_secs(60)
    } else {
        Duration::from_millis(position_refresh_ms as u64)
    };
    let config = CoreTopoSwarmConfig {
        bootstrap,
        admitted: filter,
        position_refresh,
        topo: TopoConfig::default(),
    };
    spawn_op(&bridge, user_data, out_operation, async move {
        core.topology_join_swarm(&secret, config)
            .await
            .map_err(|e| format!("{e:#}"))?;
        Ok(Done::ok(aster_event_kind_t::ASTER_EVENT_UNIT_RESULT, node))
    })
}
