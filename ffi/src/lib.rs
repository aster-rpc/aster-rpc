//! Iroh Transport FFI - C ABI Bridge
//!
//! This module provides a C-compatible FFI layer for iroh transport capabilities.
//! The design follows the completion queue architecture for async operations.

#![allow(clippy::missing_safety_doc)]
#![allow(non_camel_case_types)] // FFI types intentionally use snake_case
#![allow(nonstandard_style)] // FFI types intentionally use snake_case

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, CString};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;

use aster_transport_core::*;

pub mod audit_acknowledgements;
pub mod audit_outbox;
#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod audit_test_common;
#[cfg(test)]
extern crate self as aster_transport_ffi;
pub mod call;
pub mod coordination;
pub mod coordination_codec;
pub mod custody;
pub mod data;
pub mod enrollment;
pub mod envelope;
pub mod expose;
pub mod grants;
pub mod health;
pub mod http_server;
mod layout;
pub mod node;
pub mod observability;
pub mod probe;
pub mod provisioning;
pub mod pure;
pub mod raw_webtransport;
pub mod rpc;
pub mod server;
pub mod server_drain;
pub mod tls;
pub mod topology;
pub mod upcall;

// ============================================================================
// ABI Version
// ============================================================================

// History:
// - 2.0.0: `aster_reactor_submit`, `..._submit_frame` and `..._submit_trailer`
//   can return `ASTER_STATUS_QUEUE_FULL` (behavioural break, see the retry
//   contract in `aster_ffi.h` and `docs/_internal/reactor-ffi-guide.md`).
// - 3.0.0: whole C ABI renamed to the single `aster_` / `ASTER_` prefix;
//   `aster_abi_version()` replaces the three per-component getters;
//   `aster_runtime_new` takes `expected_abi_major` and refuses a mismatch
//   with `ASTER_STATUS_ABI_MISMATCH`; reactor event-kind constants renamed
//   `ASTER_REACTOR_EVENT_*`; every enum is exported to the header.
// - 3.1.0 (additive): AEv1 RPC envelope codec (`aster_{stream_header,
//   call_header,rpc_status}_{encode,decode,release}` + their structs) and
//   zstd payload compression (`aster_zstd_maybe_compress`,
//   `aster_zstd_decompress`); see `envelope.rs`.
// - 3.2.0 (additive): nodes from `aster_config_t` on `aster::Node`, Gate 0,
//   admission respond, Prometheus metrics; see `node.rs`.
// - 3.3.0 (additive): data-plane parity -- multi-provider blob fetch with a
//   fetch report, ranged reads, tag prefix delete, one-shot GC; docs
//   authority/retire/open/import/durable/entry_exists/content status;
//   connection paths and send-stream priority; see `data.rs`.
// - 3.4.0 (additive): pure functions -- typed id parse/format/validate,
//   HPKE seal/open + identity-derived X25519 keys, Ed25519 attestation
//   chains (`pure.rs`); sealed grants, namespace grants, grant records and
//   reconcile (`grants.rs`); new status `ASTER_STATUS_VERIFICATION_FAILED`.
// - 3.5.0 (additive): dynamic dispatch -- `aster_server_*` serves
//   JSON-registered services through `aster::rpc::Server` (Gate 3, sessions,
//   deadlines, frame limits, baseline services in Rust) and the binding
//   answers by call id with `aster_dispatch_*` (`server.rs`); payload-level
//   client `aster_rpc_unary` / `aster_rpc_open` / `aster_call_*_payload`
//   over the pooled `RpcConnection` (`rpc.rs`).
// - 4.0.0: removals only -- the reactor (`aster_reactor_*`,
//   `aster_reactor_call_t`, `ASTER_REACTOR_EVENT_*`, `ASTER_RECV_FRAME_*`),
//   the framed client calls (`aster_call_{acquire,acquire_streaming,
//   send_frame,recv_frame,unary}`, `ASTER_CALL_RECV_*`, `ASTER_CALL_ERR_*`),
//   the 3.2-deprecated node factories (`aster_node_memory`,
//   `aster_node_memory_with_alpns`, `aster_node_persistent`) and bare
//   endpoints (`aster_endpoint_create`, `_close`, `_free`, `_id`,
//   `_export_secret_key`, `_addr_info`, `aster_endpoint_config_t`).
//   `aster_accept` on a node now reads the node's aster-ALPN accept queue.
//   See `ffi/RENAME_MAP.md` "4.0".
// - 4.1.0 (additive): upcalls (R6/D6) -- `aster_upcalls_*` /
//   `aster_upcall_{poll,respond,set_policy,set_default_policy,
//   buffer_release}` carry Rust's questions to foreign code as UCv1
//   payloads; upcall-backed lease serializer / authorizer / fenced resource
//   and lease handles (`aster_lease_*`, `aster_fenced_*`), the topology
//   `admitted` filter (`aster_topology_admitted_*`) and
//   `aster_topology_enable_shared`; see `upcall.rs`.
// - 4.2.0 (additive): expose (`expose.rs`) -- `aster_expose_*` handle,
//   socket and foreign HTTP targets, foreign route policy, the
//   `aster_http_body_*` body I/O over upcall kind 49, and the buffered
//   consumer `aster_expose_relay_request`; upcall kinds 48-50 are now
//   issued by the C ABI.
// - 4.3.0 (additive): streaming consumer relay (`expose.rs`) --
//   `aster_expose_relay_stream` with `aster_http_upload_*` /
//   `aster_http_response_*` handles, readiness on the completion queue
//   (`ASTER_EVENT_HTTP_RELAY_READY`), relay codes 10-17 in
//   `aster_http_body_status_t`; `aster_status_t` reserves
//   `UNAUTHENTICATED` / `PERMISSION_DENIED` / `CREDENTIAL_EXPIRED` for the
//   R4 auth model. Semantic refinements: an `ASTER_EVENT_ERROR` of
//   `aster_expose_relay_request` now carries a specific `status` (e.g.
//   `PERMISSION_DENIED` for a denied relay; was always `INTERNAL`) and the
//   relay code in `error_code` (was -1); a refused http-relay stream is now
//   reset with a refusal code (wire), no longer dropped silently.
// - 4.4.0 (additive): coordination client + placement (`coordination.rs`,
//   design `docs/_internal/bindings/coordination-abi-design.md`) --
//   `aster_coordination_client_*` (durable journaled client or read-only),
//   `aster_coordination_{mutate,read}` with UCv1 requests `D0` / `D1` and
//   results `D2`-`DE`, resolved-mutation and coordination-lease handles,
//   `aster_coordination_placement_manager_*` over upcall kinds 17/32/33
//   (now issued by the C ABI); events 110-113.
// - 4.5.0 (additive): native unary cancel on the pooled fast path (`rpc.rs`)
//   -- `aster_cancel_token_{new,cancel,release}` and
//   `aster_rpc_unary_cancellable`: cancelling the token aborts the in-flight
//   call like dropping the Rust future (FLAG_CANCEL + reset of the pooled
//   stream; the server sees the cancel; the call returns CANCELLED). See
//   `ffi/RENAME_MAP.md` "New in 4.5".
// - 4.6.0 (observability and health, design
//   `docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md` §7):
//   `aster_observability_{init,flush,shutdown}` over the Rust-owned OTel
//   pipeline; the log bridge poll (`aster_log_{poll,close,dropped,
//   set_filter,emit}`); binding instruments and attribute sets
//   (`aster_instrument_*`, `aster_attrset_*`, `aster_metric_*`) and
//   `aster_metrics_prometheus`; binding spans (`aster_span_*`,
//   `aster_span_record`), `aster_dispatch_trace_context` with
//   `ASTER_DISPATCH_FLAG_HAS_TRACE`, and the `traceparent` codec
//   (`aster_trace_context_{format,parse}`); the ops listener
//   (`aster_health_*`, `aster_server_attach_health`); new status
//   `ASTER_STATUS_ALREADY_EXISTS` (17). Built-in metrics and spans now record
//   on the C ABI paths (server dispatch, `aster_rpc_unary` client calls).
//   Removed outright (pre-release, decision Q12):
//   `aster_node_metrics_prometheus`, replaced by `aster_metrics_prometheus`.
//   Behaviour on misuse: `aster_upcall_poll` refuses a tokio runtime thread.
//   See `ffi/RENAME_MAP.md` "New in 4.6".
// - 4.7.0 (additive): `aster_runtime_config_t.max_inflight_operations`
//   separates operation admission from `event_queue_capacity`; saturation
//   returns `ASTER_STATUS_OPERATION_LIMIT` (18).
// - 4.8.0 (additive): `ASTER_SERVER_OPTION_MAX_FOREIGN_INFLIGHT` (5) and
//   bounded FIFO foreign-dispatch staging behind the existing event ring.
// - 4.9.0 (additive): durable audit outbox, versioned bounded JSON and
//   asynchronous lifecycle/operator completions (`audit_outbox.rs`).
// - 4.10.0 (additive): Docs event receiver release and cancellation-safe
//   receives (`aster_doc_event_receiver_free`).
// - 4.11.0 (additive): authenticated HTTPS/WebTransport listener ownership
//   and bounded RPC authentication upcalls (`http_server.rs`).
// - 4.12.0 (additive): explicit public HTTP/WT listeners and certificate helpers.
// - 4.13.0 (additive): temporary blob custody and verified retained imports.
// - 4.14.0 (additive): locally complete batch document content reads.
// - 4.15.0 (additive): stateless coordination authority provisioning helpers.
// - 4.16.0 (additive): topology reads, owned snapshots, reader options and RPC gating.
// - 4.17.0 (additive): standalone raw WebTransport owners and cancellable operations.
// - 5.0.0: removals only -- the Fory ownership-attestation functions
//   (`aster_attest_root_node`, `aster_attestation_chain_{to,from}_text`,
//   `aster_attestation_verify_chain`, `aster_verified_chain_{role_of,release}`)
//   and their types (`aster_attest_options_t`, `aster_attestation_role_t`,
//   `aster_verified_chain_t`). Delegation chains replace them and have no C
//   ABI yet (`docs/_internal/ownership-attestations.md`). See
//   `ffi/RENAME_MAP.md` "5.0".
// - 5.1.0 (additive): Enrollment driven by the resolved `AsterConfig`
//   (`aster_config_enrollment_from_*`, `aster_node_start_enrolled`,
//   `aster_enrollment_*`, `ASTER_EVENT_ENROLLMENT_RESULT`) and the lease
//   grantor (`aster_server_add_lease_grantor`,
//   `aster_lease_serializer_grantor_new`), and runtime role restriction for
//   enrolled peers (`aster_server_restrict_peer_roles`,
//   `aster_server_clear_peer_restriction`). Root operations stay in the Rust
//   CLI. See `ffi/RENAME_MAP.md` "5.1".
// - 6.0.0 (breaking): `aster_config_t` grows from 280 to 288 bytes:
//   `blob_zstd_level` (`ASTER_BLOB_ZSTD_LEVEL_DEFAULT` 0 / `_OFF` -1 /
//   1..=19) and `reuse_last_port` (`aster_reuse_last_port_t`) are appended
//   after `storage`; existing offsets are unchanged. The library writes the
//   whole struct in `aster_config_init` / `_from_env` / `_from_toml`, so a
//   5.x caller's 280-byte struct would be overrun: those three functions now
//   return `ASTER_STATUS_ABI_MISMATCH` without writing until the caller has
//   proven it was built for 6.x, through the new `aster_abi_accept(6)` or a
//   successful `aster_runtime_new(6, ...)`. Readers of an `aster_config_t`
//   check `struct_size` before reading past it. See `ffi/RENAME_MAP.md`
//   "6.0".
// - 7.0.0 (breaking): `aster_hook_failure_mode_t` values swap so zero fails
//   closed: `ASTER_HOOK_FAILURE_FAIL_CLOSED` = 0, `ASTER_HOOK_FAILURE_FAIL_OPEN`
//   = 1. A zero-filled `aster_config_t` (`hook_failure_mode`) or a 0
//   `failure_mode` passed to `aster_upcall_set_policy` /
//   `_set_default_policy` now rejects instead of accepting. Layouts are unchanged; a 6.x
//   caller's literal 0/1 would mean the opposite mode, so it is refused at
//   the handshake (`aster_runtime_new(6, ...)` / `aster_abi_accept(6)`). See
//   `ffi/RENAME_MAP.md` "7.0".
// - 7.1.0 (additive): the audit outbox acknowledgement log
//   (`aster_audit_outbox_acknowledgements`,
//   `ASTER_EVENT_AUDIT_OUTBOX_ACKNOWLEDGEMENTS` = 128,
//   `audit_acknowledgements.rs`). See `ffi/RENAME_MAP.md` "7.1".
// - 7.2.0 (additive): `aster_server_drain` and `aster_drain_report_t`
//   (graceful server drain), `ASTER_DRAIN_CLOSE_CODE`. See
//   `ffi/RENAME_MAP.md` "7.2".
pub const ASTER_ABI_VERSION_MAJOR: u32 = 7;
pub const ASTER_ABI_VERSION_MINOR: u32 = 2;
pub const ASTER_ABI_VERSION_PATCH: u32 = 0;
/// Packed ABI version: `(major << 16) | (minor << 8) | patch`.
/// This is the value `aster_abi_version()` returns.
pub const ASTER_ABI_VERSION: u32 =
    (ASTER_ABI_VERSION_MAJOR << 16) | (ASTER_ABI_VERSION_MINOR << 8) | ASTER_ABI_VERSION_PATCH;

// ============================================================================
// ABI Types (FFI-safe C-compatible structs)
// ============================================================================

/// Opaque handle types
pub type aster_runtime_t = u64;
/// Since ABI 4.0 there are no bare endpoints: functions taking an
/// `aster_endpoint_t` take a node handle (`aster_node_t`).
pub type aster_endpoint_t = u64;
pub type aster_connection_t = u64;
pub type aster_send_stream_t = u64;
pub type aster_recv_stream_t = u64;
pub type aster_node_t = u64;
pub type aster_operation_t = u64;
pub type aster_buffer_t = u64;
pub type aster_hook_invocation_t = u64; // Phase 1b: identifies a pending hook invocation

/// Status codes
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_status_t {
    ASTER_STATUS_OK = 0,
    ASTER_STATUS_INVALID_ARGUMENT = 1,
    ASTER_STATUS_NOT_FOUND = 2,
    ASTER_STATUS_ALREADY_CLOSED = 3,
    ASTER_STATUS_QUEUE_FULL = 4,
    ASTER_STATUS_BUFFER_TOO_SMALL = 5,
    ASTER_STATUS_UNSUPPORTED = 6,
    ASTER_STATUS_INTERNAL = 7,
    ASTER_STATUS_TIMEOUT = 8,
    ASTER_STATUS_CANCELLED = 9,
    ASTER_STATUS_CONNECTION_REFUSED = 10,
    ASTER_STATUS_STREAM_RESET = 11,
    /// `aster_runtime_new` was called with an `expected_abi_major` that does
    /// not match the loaded library's `ASTER_ABI_VERSION_MAJOR`.
    ASTER_STATUS_ABI_MISMATCH = 12,
    /// A well-formed input failed a security check (ABI 3.4): an HPKE / grant
    /// envelope did not open (wrong key, context or AAD, corrupt bytes), or a
    /// grant role did not match its capability. `aster_last_error_message` says which check failed.
    ASTER_STATUS_VERIFICATION_FAILED = 13,
    /// An auth outcome (ABI 4.3, reserved for auth results): the caller was
    /// not authenticated. Since 4.4: coordination admission answered RPC 16
    /// (coordination error events).
    ASTER_STATUS_UNAUTHENTICATED = 14,
    /// An auth outcome (ABI 4.3): the peer's (or local) policy refused the
    /// caller. Reported by an expose relay the serving node's authorizer
    /// denied (`aster_expose_relay_request` error events,
    /// `ASTER_EVENT_HTTP_RELAY_READY`) and, since 4.4, by a coordination ACL /
    /// admin refusal (RPC 7, coordination error events).
    ASTER_STATUS_PERMISSION_DENIED = 15,
    /// Reserved for the R4 auth model (ABI 4.3): the presented credential
    /// has expired. Not returned by any 4.3 function.
    ASTER_STATUS_CREDENTIAL_EXPIRED = 16,
    /// ABI 4.6: the thing already exists in a form that cannot be joined --
    /// `aster_observability_init` with a configuration that differs from the
    /// running pipeline's, or a second concurrent `aster_log_poll`.
    ASTER_STATUS_ALREADY_EXISTS = 17,
    /// ABI 4.7: the independent asynchronous-operation admission bound is
    /// exhausted. The refused operation did not start and may be retried only
    /// when the caller knows an existing operation can finish independently.
    ASTER_STATUS_OPERATION_LIMIT = 18,
    /// ABI 5.1: the operation could not complete now and may be retried --
    /// an Enrollment admission whose policy could not be evaluated, a clock
    /// outside the activation leeway (`ASTER_ENROLLMENT_ERROR_UNAVAILABLE` /
    /// `_CLOCK_SKEW`), or a server that could not be reached to enroll.
    ASTER_STATUS_UNAVAILABLE = 19,
}

/// Relay mode
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_relay_mode_t {
    ASTER_RELAY_MODE_DEFAULT = 0,
    ASTER_RELAY_MODE_CUSTOM = 1,
    ASTER_RELAY_MODE_DISABLED = 2,
    ASTER_RELAY_MODE_STAGING = 3,
}

/// Hook fallback behavior when callback processing is unavailable.
///
/// `ASTER_HOOK_FAILURE_FAIL_CLOSED` (0) is the default, so a zero-filled
/// `aster_config_t` (one that skipped `aster_config_init`) fails closed too;
/// `ASTER_HOOK_FAILURE_FAIL_OPEN` (1) is an explicit opt-in. Before ABI 7.0
/// the values were the other way round.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_hook_failure_mode_t {
    ASTER_HOOK_FAILURE_FAIL_CLOSED = 0,
    ASTER_HOOK_FAILURE_FAIL_OPEN = 1,
}

/// Event kinds
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_event_kind_t {
    ASTER_EVENT_NONE = 0,

    // Lifecycle
    ASTER_EVENT_NODE_CREATED = 1,
    ASTER_EVENT_NODE_CREATE_FAILED = 2,
    /// Not emitted since ABI 4.0 (`aster_endpoint_create` removed); value reserved.
    ASTER_EVENT_ENDPOINT_CREATED = 3,
    /// Not emitted since ABI 4.0; value reserved.
    ASTER_EVENT_ENDPOINT_CREATE_FAILED = 4,
    ASTER_EVENT_CLOSED = 5,

    // Connections
    ASTER_EVENT_CONNECTED = 10,
    ASTER_EVENT_CONNECT_FAILED = 11,
    ASTER_EVENT_CONNECTION_ACCEPTED = 12,
    ASTER_EVENT_CONNECTION_CLOSED = 13,

    // Streams
    ASTER_EVENT_STREAM_OPENED = 20,
    ASTER_EVENT_STREAM_ACCEPTED = 21,
    ASTER_EVENT_FRAME_RECEIVED = 22,
    ASTER_EVENT_SEND_COMPLETED = 23,
    ASTER_EVENT_STREAM_FINISHED = 24,
    ASTER_EVENT_STREAM_RESET = 25,

    // Blobs
    ASTER_EVENT_BLOB_ADDED = 30,
    ASTER_EVENT_BLOB_READ = 31,
    ASTER_EVENT_BLOB_DOWNLOADED = 32,
    ASTER_EVENT_BLOB_TICKET_CREATED = 33,
    ASTER_EVENT_BLOB_COLLECTION_ADDED = 34,
    ASTER_EVENT_BLOB_COLLECTION_TICKET_CREATED = 35,

    /// Emitted by aster_blobs_observe_complete when the blob is fully available locally.
    ASTER_EVENT_BLOB_OBSERVE_COMPLETE = 56,

    // Tags (Phase 1c)
    ASTER_EVENT_TAG_SET = 36,
    ASTER_EVENT_TAG_GET = 37,
    ASTER_EVENT_TAG_DELETED = 38,
    ASTER_EVENT_TAG_LIST = 39,

    // Docs
    ASTER_EVENT_DOC_CREATED = 40,
    ASTER_EVENT_DOC_JOINED = 41,
    ASTER_EVENT_DOC_SET = 42,
    ASTER_EVENT_DOC_GET = 43,
    ASTER_EVENT_DOC_SHARED = 44,
    ASTER_EVENT_DOC_QUERY = 46,
    ASTER_EVENT_AUTHOR_CREATED = 45,
    ASTER_EVENT_DOC_SUBSCRIBED = 47,
    ASTER_EVENT_DOC_EVENT = 48,
    /// join_and_subscribe: event.handle = doc_handle, event.related = receiver_handle
    ASTER_EVENT_DOC_JOINED_AND_SUBSCRIBED = 49,
    /// event.payload = removed count as little-endian u64.
    ASTER_EVENT_DOC_DEL = 57,

    // Gossip
    ASTER_EVENT_GOSSIP_SUBSCRIBED = 50,
    ASTER_EVENT_GOSSIP_BROADCAST_DONE = 51,
    ASTER_EVENT_GOSSIP_RECEIVED = 52,
    ASTER_EVENT_GOSSIP_NEIGHBOR_UP = 53,
    ASTER_EVENT_GOSSIP_NEIGHBOR_DOWN = 54,
    ASTER_EVENT_GOSSIP_LAGGED = 55,

    // Datagrams (Phase 1b)
    ASTER_EVENT_DATAGRAM_RECEIVED = 60,

    // Hooks (Phase 1b)
    ASTER_EVENT_HOOK_BEFORE_CONNECT = 70,
    ASTER_EVENT_HOOK_AFTER_CONNECT = 71,
    ASTER_EVENT_HOOK_INVOCATION_RELEASED = 72,

    // Aster custom-ALPN (Phase 1e)
    ASTER_EVENT_ASTER_ACCEPTED = 65,

    // Registry (§11.9 — async doc-backed ops)
    ASTER_EVENT_REGISTRY_RESOLVED = 80,
    ASTER_EVENT_REGISTRY_PUBLISHED = 81,
    ASTER_EVENT_REGISTRY_RENEWED = 82,
    ASTER_EVENT_REGISTRY_ACL_UPDATED = 83,
    ASTER_EVENT_REGISTRY_ACL_LISTED = 84,

    // Generic
    ASTER_EVENT_STRING_RESULT = 90,
    ASTER_EVENT_BYTES_RESULT = 91,
    ASTER_EVENT_UNIT_RESULT = 92,
    ASTER_EVENT_OPERATION_CANCELLED = 98,
    ASTER_EVENT_ERROR = 99,

    // Data plane (ABI 3.3, data.rs)
    /// `aster_blobs_download_hash_from`: payload = fetch report v1, then the
    /// blob bytes unless `ASTER_FETCH_TO_STORE`; `flags` = provider count.
    ASTER_EVENT_BLOB_FETCHED = 100,
    /// `flags` = `aster_local_access_t`; payload = namespace id (32) +
    /// authority fingerprint (32).
    ASTER_EVENT_DOC_AUTHORITY_REPORT = 101,
    /// `flags` = `aster_retire_outcome_t`; payload = u32 access, u32 reserved,
    /// u64 open references.
    ASTER_EVENT_DOC_RETIRE_OUTCOME = 102,
    /// `handle` = doc, `related` = node; status NOT_FOUND (handle 0) when
    /// `aster_docs_open` finds no such namespace.
    ASTER_EVENT_DOC_OPENED = 103,
    /// `flags` = 1 if the entry (or its tombstone) exists, else 0.
    ASTER_EVENT_DOC_ENTRY_EXISTS = 104,
    /// `flags` = `aster_blob_status_t`; payload = size (u64 LE).
    ASTER_EVENT_DOC_CONTENT_STATUS = 105,
    /// payload = priority (i32 LE); `flags` = its bit pattern.
    ASTER_EVENT_STREAM_PRIORITY = 106,

    // Upcall-backed leases (ABI 4.1, upcall.rs)
    /// `aster_lease_acquire`: payload = UCv1 `TransitionOutcome` (tag `B2`):
    /// granted (`handle` = the new `aster_lease_t`, the payload carries the
    /// fence) or denied (`handle` = 0, reason + snapshot).
    ASTER_EVENT_LEASE_ACQUIRED = 107,
    /// `aster_lease_watch`: the lease left `HELD`; `flags` =
    /// `aster_lease_status_t`.
    ASTER_EVENT_LEASE_STATUS = 108,

    // Streaming consumer relay (ABI 4.3, expose.rs)
    /// `aster_http_upload_ready` / `aster_http_response_ready`: the armed
    /// handle (`handle`) is ready. `error_code` = the
    /// `aster_http_body_status_t` its next non-blocking call returns (0:
    /// it succeeds -- room to write, a head, data or the end), `status` =
    /// that code as an `aster_status_t` (`PERMISSION_DENIED` for a denied
    /// relay; see `aster_http_body_status_to_status`). One-shot: re-arm
    /// after handling.
    ASTER_EVENT_HTTP_RELAY_READY = 109,

    // Coordination client (ABI 4.4, coordination.rs)
    /// `aster_coordination_client_open`: `handle` = the new
    /// `aster_coordination_client_t`.
    ASTER_EVENT_COORDINATION_CLIENT_OPENED = 110,
    /// `aster_coordination_mutate` / `_unacknowledged` / `_resume`:
    /// `handle` = a new `aster_coordination_resolved_t` (0 = nothing to
    /// deliver; then no payload), payload = UCv1 `MutationReply` (`D2`).
    ASTER_EVENT_COORDINATION_RESOLVED = 111,
    /// `aster_coordination_read`, `_retire_other_client`,
    /// `_recovery_export_to_file`, `_lease_compare_and_set`,
    /// `_lease_release_with_value`, `_placement_reconcile`: payload = the
    /// UCv1 result (`D2`-`DE`, see the call).
    ASTER_EVENT_COORDINATION_RESULT = 112,
    /// `aster_coordination_lease_acquire`: granted -- `handle` = a new
    /// `aster_coordination_lease_t`, payload = `DD` (the fence); denied --
    /// `handle` = 0, payload = UCv1 `B2` (denied, reason + snapshot).
    ASTER_EVENT_COORDINATION_LEASE_ACQUIRED = 113,
    /// `aster_audit_outbox_open`: `handle` = the new outbox handle.
    ASTER_EVENT_AUDIT_OUTBOX_OPENED = 114,
    /// `aster_audit_outbox_submit`: payload = the full UTF-8 event ID.
    /// Success means durable local acceptance, not remote delivery.
    ASTER_EVENT_AUDIT_OUTBOX_SUBMITTED = 115,
    /// `aster_audit_outbox_status`: versioned JSON status payload.
    ASTER_EVENT_AUDIT_OUTBOX_STATUS = 116,
    /// `aster_audit_outbox_export`: versioned JSON evidence payload.
    ASTER_EVENT_AUDIT_OUTBOX_EXPORTED = 117,
    /// A custody protect/retain operation completed; handle = custody scope.
    ASTER_EVENT_BLOB_CUSTODY_DONE = 118,
    /// Ordered optional content values; see aster_doc_read_entry_contents_if_complete.
    ASTER_EVENT_DOC_CONTENTS_BATCH = 119,
    /// One immutable shared topology view; handle = owned topology snapshot.
    ASTER_EVENT_TOPOLOGY_SNAPSHOT = 120,
    ASTER_EVENT_RAW_WT_SESSION = 121,
    ASTER_EVENT_RAW_WT_STREAM = 122,
    ASTER_EVENT_RAW_WT_READ = 123,
    ASTER_EVENT_RAW_WT_DATAGRAM = 124,
    ASTER_EVENT_RAW_WT_CLOSED = 125,
    ASTER_EVENT_RAW_WT_ACKED = 126,
    /// ABI 5.1: an Enrollment operation's JSON result (payload); `handle` =
    /// the enrollment it ran on.
    ASTER_EVENT_ENROLLMENT_RESULT = 127,
    /// ABI 7.1: `aster_audit_outbox_acknowledgements`: versioned JSON page of
    /// acknowledgement-log entries; `handle` = the outbox.
    ASTER_EVENT_AUDIT_OUTBOX_ACKNOWLEDGEMENTS = 128,
}

/// Value of `connection_type` in `aster_remote_info_t` and
/// `aster_connection_info_t` (carried as `uint32_t`).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_connection_type_t {
    ASTER_CONNECTION_TYPE_NOT_CONNECTED = 0,
    ASTER_CONNECTION_TYPE_CONNECTING = 1,
    ASTER_CONNECTION_TYPE_UDP_DIRECT = 2,
    ASTER_CONNECTION_TYPE_UDP_RELAY = 3,
    ASTER_CONNECTION_TYPE_OTHER = 4,
}

/// Value of `path_kind` in `aster_transport_snapshot_t` (carried as `uint32_t`).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_path_kind_t {
    /// No path selected (connection dropped or not yet negotiated).
    ASTER_PATH_KIND_NONE = 0,
    /// Direct UDP: `peer_host` and `peer_port` are populated.
    ASTER_PATH_KIND_DIRECT = 1,
    /// Relay: `relay_url` is populated.
    ASTER_PATH_KIND_RELAY = 2,
    /// A custom or future transport (ABI 3.3, `aster_path_info_t.other`).
    ASTER_PATH_KIND_OTHER = 3,
}

/// `mode` argument of `aster_doc_share` / `aster_doc_share_with_addr`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_doc_share_mode_t {
    ASTER_DOC_SHARE_READ = 0,
    ASTER_DOC_SHARE_WRITE = 1,
}

/// `kind` argument of `aster_registry_key`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_registry_key_kind_t {
    /// `contract_key(arg1)`
    ASTER_REGISTRY_KEY_CONTRACT = 0,
    /// `version_key(arg1, arg2 as int)`
    ASTER_REGISTRY_KEY_VERSION = 1,
    /// `channel_key(arg1, arg2)`
    ASTER_REGISTRY_KEY_CHANNEL = 2,
    /// `lease_key(arg1, arg2, arg3)`
    ASTER_REGISTRY_KEY_LEASE = 3,
    /// `lease_prefix(arg1, arg2)`
    ASTER_REGISTRY_KEY_LEASE_PREFIX = 4,
    /// `acl_key(arg1)`
    ASTER_REGISTRY_KEY_ACL = 5,
}

/// Hook decision
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_hook_decision_t {
    ASTER_HOOK_DECISION_ALLOW = 0,
    ASTER_HOOK_DECISION_DENY = 1,
}

// ============================================================================
// C Structs (#[repr(C)])
// ============================================================================

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_runtime_config_t {
    pub struct_size: u32,
    pub worker_threads: u32,
    /// Maximum events waiting for `aster_poll_events`. Zero selects 4096.
    pub event_queue_capacity: u32,
    /// Maximum admitted asynchronous operations, including terminal events
    /// waiting for queue capacity. Zero selects 4096. Admission above this
    /// independent bound returns `ASTER_STATUS_OPERATION_LIMIT` before work starts.
    pub max_inflight_operations: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_connect_config_t {
    pub struct_size: u32,
    pub flags: u32,
    pub node_id: aster_bytes_t, // hex string
    pub alpn: aster_bytes_t,
    pub addr: *const aster_node_addr_t,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_node_addr_t {
    pub endpoint_id: aster_bytes_t,
    pub relay_url: aster_bytes_t,
    pub direct_addresses: aster_bytes_list_t,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_bytes_t {
    pub ptr: *const u8,
    pub len: usize,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_bytes_list_t {
    pub items: *const aster_bytes_t,
    pub len: usize,
}

/// C-compatible event structure for FFI output
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_event_t {
    /// `sizeof(aster_event_t)`; written by the library.
    pub struct_size: u32,
    /// One of `aster_event_kind_t`.
    pub kind: u32,
    /// One of `aster_status_t`.
    pub status: u32,
    pub operation: u64,
    pub handle: u64,
    pub related: u64,
    pub user_data: u64,
    pub data_ptr: *const u8,
    pub data_len: usize,
    pub buffer: u64,
    pub error_code: i32,
    pub flags: u32,
}

// ============================================================================
// Thread-local Error Storage
// ============================================================================

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error(msg: impl ToString) -> i32 {
    let mut s = msg.to_string();
    if s.contains('\0') {
        s = s.replace('\0', " ");
    }
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new(s).ok();
    });
    aster_status_t::ASTER_STATUS_INTERNAL as i32
}

#[allow(dead_code)]
fn get_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(ptr::null())
    })
}

// ============================================================================
// Handle Registry - Arc-backed safe handle storage
// ============================================================================

pub(crate) struct HandleRegistry<T> {
    items: RwLock<HashMap<u64, Arc<T>>>,
}

/// One id space for every handle registry, so a node handle can never equal an
/// endpoint (or any other) handle. Functions that accept "endpoint or node"
/// (`aster_connect`, `aster_accept`, remote info, transport metrics) rely on it.
static NEXT_HANDLE_ID: AtomicU64 = AtomicU64::new(1);

/// A fresh id from the shared handle id space (for tables that are not a
/// `HandleRegistry`, e.g. the sharded cancel-token table).
pub(crate) fn next_handle_id() -> u64 {
    NEXT_HANDLE_ID.fetch_add(1, Ordering::Relaxed)
}

impl<T> HandleRegistry<T> {
    pub(crate) fn new() -> Self {
        Self {
            items: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) fn insert(&self, value: T) -> u64 {
        let id = NEXT_HANDLE_ID.fetch_add(1, Ordering::Relaxed);
        let arc = Arc::new(value);
        self.items.write().unwrap().insert(id, arc);
        id
    }

    pub(crate) fn get(&self, id: u64) -> Option<Arc<T>> {
        self.items.read().unwrap().get(&id).cloned()
    }

    pub(crate) fn remove(&self, id: u64) -> Option<Arc<T>> {
        self.items.write().unwrap().remove(&id)
    }

    #[allow(dead_code)]
    pub(crate) fn count(&self) -> usize {
        self.items.read().unwrap().len()
    }

    /// Drop every entry (outside the lock).
    pub(crate) fn clear(&self) {
        let items = std::mem::take(&mut *self.items.write().unwrap_or_else(|e| e.into_inner()));
        drop(items);
    }
}

// ============================================================================
// Event System - Send-safe internal events
// ============================================================================

/// Internal event for crossing thread boundary (no raw pointers - Send-safe)
#[derive(Clone)]
struct EventInternal {
    struct_size: u32,
    kind: u32,
    status: u32,
    operation: u64,
    handle: u64,
    related: u64,
    user_data: u64,
    data_len: usize,
    buffer_id: u64,
    error_code: i32,
    flags: u32,
}

impl Default for EventInternal {
    fn default() -> Self {
        Self {
            struct_size: std::mem::size_of::<aster_event_t>() as u32,
            kind: aster_event_kind_t::ASTER_EVENT_NONE as u32,
            status: aster_status_t::ASTER_STATUS_OK as u32,
            operation: 0,
            handle: 0,
            related: 0,
            user_data: 0,
            data_len: 0,
            buffer_id: 0,
            error_code: 0,
            flags: 0,
        }
    }
}

impl EventInternal {
    fn new(
        kind: aster_event_kind_t,
        status: aster_status_t,
        operation: u64,
        handle: u64,
        related: u64,
        user_data: u64,
        error_code: i32,
    ) -> Self {
        Self {
            struct_size: std::mem::size_of::<aster_event_t>() as u32,
            kind: kind as u32,
            status: status as u32,
            operation,
            handle,
            related,
            user_data,
            data_len: 0,
            buffer_id: 0,
            error_code,
            flags: 0,
        }
    }

    #[allow(dead_code)]
    fn with_data(mut self, data: Vec<u8>, buffers: &BufferRegistry) -> Self {
        let (buf_id, arc) = buffers.insert(data);
        self.data_len = arc.len();
        self.buffer_id = buf_id;
        self
    }

    fn with_buffer(mut self, buf_id: u64, data_len: usize) -> Self {
        self.buffer_id = buf_id;
        self.data_len = data_len;
        self
    }
}

/// An event queued for `aster_poll_events`. Send-safe: `EventInternal` has
/// no raw pointers. A payload lives only in the `BufferRegistry` under
/// `event.buffer_id`, which keeps `aster_event_t.data_ptr` valid until
/// `aster_buffer_release`. (The queue used to carry a second reference that
/// polling `mem::forget`-ed, leaking every payload.)
struct EventOwned {
    event: EventInternal,
    data: Option<Vec<u8>>,
    /// Keeps the operation-admission slot occupied until its terminal event
    /// has entered and then left the polled-event queue.
    _operation_slot: Option<tokio::sync::OwnedSemaphorePermit>,
}

// ============================================================================
// Buffer Registry - Track allocated buffers
// ============================================================================

pub(crate) struct BufferRegistry {
    next_id: AtomicU64,
    buffers: RwLock<HashMap<u64, Arc<[u8]>>>,
}

impl BufferRegistry {
    pub(crate) fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            buffers: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) fn insert(&self, data: Vec<u8>) -> (u64, Arc<[u8]>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let arc: Arc<[u8]> = data.into();
        self.buffers.write().unwrap().insert(id, arc.clone());
        (id, arc)
    }

    #[allow(dead_code)]
    pub(crate) fn get(&self, id: u64) -> Option<Arc<[u8]>> {
        self.buffers.read().unwrap().get(&id).cloned()
    }

    #[cfg(test)]
    fn count(&self) -> usize {
        self.buffers.read().unwrap().len()
    }

    pub(crate) fn release(&self, id: u64) -> bool {
        self.buffers.write().unwrap().remove(&id).is_some()
    }

    fn clear(&self) {
        let buffers = std::mem::take(&mut *self.buffers.write().unwrap_or_else(|e| e.into_inner()));
        drop(buffers);
    }
}

// ============================================================================
// Hook Invocation State - stores pending hook reply senders (Phase 1b)
// ============================================================================

/// Wraps the oneshot reply sender for a pending hook invocation.
enum HookSender {
    BeforeConnect(tokio::sync::oneshot::Sender<bool>),
    AfterConnect(tokio::sync::oneshot::Sender<CoreAfterHandshakeDecision>),
}

struct HookInvocationState {
    /// Consumed exactly once when the caller responds.
    sender: std::sync::Mutex<Option<HookSender>>,
}

// ============================================================================
// Bridge Runtime - Main FFI runtime
// ============================================================================

pub(crate) struct BridgeRuntime {
    // Dropped by hand in `Drop for BridgeRuntime`: spawned tasks hold
    // `Arc<BridgeRuntime>` clones, so the last reference can go away on one of
    // this runtime's own worker threads, where dropping a Runtime panics.
    pub(crate) runtime: std::mem::ManuallyDrop<tokio::runtime::Runtime>,
    events_tx: tokio::sync::mpsc::Sender<EventOwned>,
    events_rx: Mutex<tokio::sync::mpsc::Receiver<EventOwned>>,
    operation_slots: Arc<tokio::sync::Semaphore>,
    closing: Arc<AtomicBool>,
    close_notify: Arc<tokio::sync::Notify>,

    // Handle registries
    pub(crate) nodes: HandleRegistry<CoreNode>,
    pub(crate) connections: HandleRegistry<CoreConnection>,
    pub(crate) audit_outboxes: HandleRegistry<audit_outbox::OutboxState>,
    pub(crate) http_listeners: HandleRegistry<http_server::HttpListenerState>,
    pub(crate) raw_wt_listeners:
        HandleRegistry<aster_transport_salvo::raw_webtransport::RawWtListener>,
    pub(crate) raw_wt_sessions:
        HandleRegistry<aster_transport_salvo::raw_webtransport::RawWtSession>,
    pub(crate) raw_wt_streams: HandleRegistry<raw_webtransport::StreamState>,
    pub(crate) blob_custody: HandleRegistry<custody::CustodyState>,
    pub(crate) topology_snapshots: HandleRegistry<aster::topology::TopologySnapshot>,
    // ABI 5.1: Enrollments installed from configuration, and the node each
    // was started with (node handle -> enrollment handle).
    pub(crate) enrollments: HandleRegistry<enrollment::EnrollmentState>,
    pub(crate) node_enrollments: Mutex<HashMap<u64, u64>>,
    pub(crate) topology_admitted:
        HandleRegistry<aster::binding_support::upcall::topology::ForeignAdmittedFn>,
    pub(crate) server_handles: Mutex<Vec<u64>>,
    pub(crate) upcall_handles: Mutex<Vec<u64>>,
    // Includes engines retained only by an in-flight operation after handle free.
    pub(crate) audit_outbox_lifetimes: Mutex<Vec<std::sync::Weak<audit_outbox::OutboxState>>>,
    send_streams: HandleRegistry<CoreSendStream>,
    recv_streams: HandleRegistry<CoreRecvStream>,
    #[allow(dead_code)]
    blobs_clients: HandleRegistry<CoreBlobsClient>,
    #[allow(dead_code)]
    docs_clients: HandleRegistry<CoreDocsClient>,
    docs: HandleRegistry<CoreDoc>,
    #[allow(dead_code)]
    gossip_clients: HandleRegistry<CoreGossipClient>,
    gossip_topics: HandleRegistry<CoreGossipTopic>,

    // Operation registry (for cancellation)
    operations: HandleRegistry<OperationState>,

    // Hook invocation registry (Phase 1b)
    hook_invocations: HandleRegistry<HookInvocationState>,

    // ABI 3.2: Gate-0 policies and the gate slot of each hooked node.
    gates: HandleRegistry<node::GateEntry>,
    node_gates: node::NodeGates,

    // Doc event receiver registry (Phase 1c.4)
    doc_event_receivers: HandleRegistry<DocEventReceiver>,

    // Buffer registry
    buffers: BufferRegistry,

    // Registry resolution state (§11.9): persistent round-robin counters + monotonic
    // lease_seq cache, shared across every aster_registry_resolve call so rotation
    // and stale rejection survive call boundaries.
    registry_state: aster_transport_core::registry::ResolveState,

    // Per-doc RegistryAcl, keyed by doc handle. Lazily created in open mode on first
    // touch via `registry_acl_for_doc`.
    registry_acls: Mutex<HashMap<u64, Arc<aster_transport_core::registry::RegistryAcl>>>,

    // Signalled when `runtime` has finished shutting down.
    teardown: Arc<RuntimeTeardown>,
}

impl Drop for BridgeRuntime {
    fn drop(&mut self) {
        // SAFETY: `runtime` is never touched again after this.
        let runtime = unsafe { std::mem::ManuallyDrop::take(&mut self.runtime) };
        let teardown = self.teardown.clone();
        if tokio::runtime::Handle::try_current().is_ok() {
            // Inside an async context -- possibly a worker of this very
            // runtime, which a blocking drop would wait on forever. Hand the
            // drop to a detached OS thread; it completes once the current
            // task returns to the scheduler.
            std::thread::spawn(move || {
                drop(runtime);
                teardown.finish();
            });
        } else {
            drop(runtime);
            teardown.finish();
        }
    }
}

/// Set once a `BridgeRuntime`'s tokio runtime has fully shut down (worker
/// threads joined), which can be after `aster_runtime_close` returned: tasks
/// still holding the bridge keep it alive, and the final drop may run on a
/// detached thread (see `Drop for BridgeRuntime`).
#[derive(Default)]
struct RuntimeTeardown {
    done: Mutex<bool>,
    cv: std::sync::Condvar,
}

impl RuntimeTeardown {
    fn finish(&self) {
        if let Ok(mut done) = self.done.lock() {
            *done = true;
        }
        self.cv.notify_all();
    }

    fn wait(&self, timeout: Duration) -> bool {
        let Ok(done) = self.done.lock() else {
            return false;
        };
        match self.cv.wait_timeout_while(done, timeout, |done| !*done) {
            Ok((done, _)) => *done,
            Err(_) => false,
        }
    }
}

struct OperationState {
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
    cooperative_cancel: bool,
    user_data: u64,
    /// Independent operation-admission budget. It travels with the terminal
    /// event and is released only when that event is polled or shutdown drops it.
    completion: Mutex<Option<tokio::sync::OwnedSemaphorePermit>>,
}

impl BridgeRuntime {
    fn new(worker_threads: u32, queue_capacity: u32, max_inflight_operations: u32) -> Result<Self> {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all();

        if worker_threads > 0 {
            builder.worker_threads(worker_threads as usize);
        }

        builder.thread_name("iroh-ffi");

        let runtime = builder.build()?;

        let capacity = if queue_capacity > 0 {
            queue_capacity
        } else {
            4096
        } as usize;
        let operation_capacity = if max_inflight_operations > 0 {
            max_inflight_operations
        } else {
            4096
        } as usize;
        let (events_tx, events_rx) = tokio::sync::mpsc::channel(capacity);

        Ok(Self {
            runtime: std::mem::ManuallyDrop::new(runtime),
            events_tx,
            events_rx: Mutex::new(events_rx),
            operation_slots: Arc::new(tokio::sync::Semaphore::new(operation_capacity)),
            closing: Arc::new(AtomicBool::new(false)),
            close_notify: Arc::new(tokio::sync::Notify::new()),
            nodes: HandleRegistry::new(),
            connections: HandleRegistry::new(),
            audit_outboxes: HandleRegistry::new(),
            http_listeners: HandleRegistry::new(),
            raw_wt_listeners: HandleRegistry::new(),
            raw_wt_sessions: HandleRegistry::new(),
            raw_wt_streams: HandleRegistry::new(),
            blob_custody: HandleRegistry::new(),
            topology_snapshots: HandleRegistry::new(),
            enrollments: HandleRegistry::new(),
            node_enrollments: Mutex::new(HashMap::new()),
            topology_admitted: HandleRegistry::new(),
            server_handles: Mutex::new(Vec::new()),
            upcall_handles: Mutex::new(Vec::new()),
            audit_outbox_lifetimes: Mutex::new(Vec::new()),
            send_streams: HandleRegistry::new(),
            recv_streams: HandleRegistry::new(),
            blobs_clients: HandleRegistry::new(),
            docs_clients: HandleRegistry::new(),
            docs: HandleRegistry::new(),
            gossip_clients: HandleRegistry::new(),
            gossip_topics: HandleRegistry::new(),
            operations: HandleRegistry::new(),
            hook_invocations: HandleRegistry::new(),
            gates: HandleRegistry::new(),
            node_gates: Mutex::new(HashMap::new()),
            doc_event_receivers: HandleRegistry::new(),
            buffers: BufferRegistry::new(),
            registry_state: aster_transport_core::registry::ResolveState::new(),
            registry_acls: Mutex::new(HashMap::new()),
            teardown: Arc::new(RuntimeTeardown::default()),
        })
    }

    /// Drop every handle the host still holds, for `aster_runtime_close`.
    /// Once the runtime handle is gone no call can reach them, and they can
    /// keep the bridge alive forever through a cycle: an unfreed connection
    /// keeps its endpoint, whose hook senders keep the hook drainer tasks,
    /// which hold the bridge (and with it this runtime and its threads).
    fn release_handles(&self) {
        // Dropping endpoints and streams may need the runtime's context.
        let _context = self.runtime.enter();
        // Pending hook replies first: dropping a sender settles the hook.
        upcall::shutdown_runtime(self);
        server::shutdown_runtime(self);
        self.http_listeners.clear();
        raw_webtransport::shutdown(self);
        self.topology_snapshots.clear();
        self.topology_admitted.clear();
        std::mem::take(
            &mut *self
                .node_enrollments
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        self.enrollments.clear();
        custody::shutdown(self);
        self.hook_invocations.clear();
        self.operations.clear();
        self.doc_event_receivers.clear();
        self.gossip_topics.clear();
        self.gossip_clients.clear();
        self.docs.clear();
        self.docs_clients.clear();
        self.blobs_clients.clear();
        self.send_streams.clear();
        self.recv_streams.clear();
        self.connections.clear();
        std::mem::take(&mut *self.node_gates.lock().unwrap_or_else(|e| e.into_inner()));
        self.gates.clear();
        self.nodes.clear();
        std::mem::take(&mut *self.registry_acls.lock().unwrap_or_else(|e| e.into_inner()));
        self.buffers.clear();
    }

    fn begin_close(&self) {
        self.closing.store(true, Ordering::Release);
        self.close_notify.notify_waiters();
    }

    /// Queue one operation's terminal event. The independent operation permit
    /// bounds terminal senders waiting behind a full event queue without
    /// consuming the queue capacity needed by pushed dependencies.
    fn enqueue_terminal(
        &self,
        event: EventInternal,
        data: Option<Vec<u8>>,
        operation_slot: tokio::sync::OwnedSemaphorePermit,
    ) {
        let owned = EventOwned {
            event,
            data,
            _operation_slot: Some(operation_slot),
        };
        match self.events_tx.try_send(owned) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(owned)) => {
                let sender = self.events_tx.clone();
                let closing = self.closing.clone();
                let close_notify = self.close_notify.clone();
                self.runtime.spawn(async move {
                    let notified = close_notify.notified();
                    tokio::pin!(notified);
                    if closing.load(Ordering::Acquire) {
                        return;
                    }
                    tokio::select! {
                        biased;
                        _ = &mut notified => {}
                        _ = sender.send(owned) => {}
                    }
                });
            }
        }
    }

    /// The first terminal path wins; cancellation and normal completion can
    /// race without duplicating a result.
    fn complete(&self, event: EventInternal, data: Option<Vec<u8>>) -> bool {
        debug_assert_ne!(event.operation, 0);
        let Some(operation) = self.operations.remove(event.operation) else {
            return false;
        };
        let Some(permit) = operation
            .completion
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return false;
        };
        self.enqueue_terminal(event, data, permit);
        true
    }

    fn emit_error(&self, operation: u64, user_data: u64, message: &str) {
        self.emit_error_coded(
            operation,
            user_data,
            aster_status_t::ASTER_STATUS_INTERNAL,
            -1,
            message,
        );
    }

    /// An `ASTER_EVENT_ERROR` with a specific `status` and `error_code`.
    pub(crate) fn emit_error_coded(
        &self,
        operation: u64,
        user_data: u64,
        status: aster_status_t,
        error_code: i32,
        message: &str,
    ) {
        self.emit_error_flagged(operation, user_data, status, error_code, 0, message);
    }

    /// An `ASTER_EVENT_ERROR` with `status`, `error_code` and `flags`.
    pub(crate) fn emit_error_flagged(
        &self,
        operation: u64,
        user_data: u64,
        status: aster_status_t,
        error_code: i32,
        flags: u32,
        message: &str,
    ) {
        let mut event = EventInternal::new(
            aster_event_kind_t::ASTER_EVENT_ERROR,
            status,
            operation,
            0,
            0,
            user_data,
            error_code,
        );
        event.flags = flags;
        self.complete(event, Some(message.as_bytes().to_vec()));
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_simple(
        &self,
        kind: aster_event_kind_t,
        status: aster_status_t,
        operation: u64,
        handle: u64,
        related: u64,
        user_data: u64,
        error_code: i32,
    ) -> bool {
        let event = EventInternal::new(
            kind, status, operation, handle, related, user_data, error_code,
        );
        self.complete(event, None)
    }

    fn emit_with_data(&self, event: EventInternal, data: Vec<u8>) -> bool {
        self.complete(event, Some(data))
    }

    /// Complete an operation that has already inserted a returned resource
    /// handle. If cancellation won the completion race, remove the handle so
    /// it cannot become an unreachable registry entry.
    fn complete_with_handle<T>(
        &self,
        event: EventInternal,
        data: Option<Vec<u8>>,
        registry: &HandleRegistry<T>,
        handle: u64,
    ) -> bool {
        let completed = self.complete(event, data);
        if !completed {
            registry.remove(handle);
        }
        completed
    }

    /// Paired form for APIs that return send/receive or document/subscription
    /// handles in one terminal event.
    fn complete_with_handles<T, U>(
        &self,
        event: EventInternal,
        data: Option<Vec<u8>>,
        first_registry: &HandleRegistry<T>,
        first: u64,
        second_registry: &HandleRegistry<U>,
        second: u64,
    ) -> bool {
        let completed = self.complete(event, data);
        if !completed {
            first_registry.remove(first);
            second_registry.remove(second);
        }
        completed
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_simple_with_handle<T>(
        &self,
        kind: aster_event_kind_t,
        status: aster_status_t,
        operation: u64,
        handle: u64,
        related: u64,
        user_data: u64,
        error_code: i32,
        registry: &HandleRegistry<T>,
    ) -> bool {
        let event = EventInternal::new(
            kind, status, operation, handle, related, user_data, error_code,
        );
        self.complete_with_handle(event, None, registry, handle)
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_simple_with_handles<T, U>(
        &self,
        kind: aster_event_kind_t,
        status: aster_status_t,
        operation: u64,
        first: u64,
        second: u64,
        user_data: u64,
        error_code: i32,
        first_registry: &HandleRegistry<T>,
        second_registry: &HandleRegistry<U>,
    ) -> bool {
        let event = EventInternal::new(
            kind, status, operation, first, second, user_data, error_code,
        );
        self.complete_with_handles(event, None, first_registry, first, second_registry, second)
    }

    /// Backpressure a pushed event that has no operation admission slot. The
    /// producer awaits queue capacity on the runtime instead of blocking a
    /// worker thread or allocating an unbounded staging queue.
    async fn emit_pushed_with_data(&self, event: EventInternal, data: Vec<u8>) -> bool {
        debug_assert_eq!(event.operation, 0);
        let notified = self.close_notify.notified();
        tokio::pin!(notified);
        if self.closing.load(Ordering::Acquire) {
            return false;
        }
        let permit = tokio::select! {
            biased;
            _ = &mut notified => return false,
            result = self.events_tx.clone().reserve_owned() => match result {
                Ok(permit) => permit,
                Err(_) => return false,
            },
        };
        permit.send(EventOwned {
            event,
            data: Some(data),
            _operation_slot: None,
        });
        true
    }

    fn new_operation(
        &self,
        user_data: u64,
    ) -> Result<(u64, Arc<std::sync::atomic::AtomicBool>), aster_status_t> {
        self.new_operation_with_cancel_mode(user_data, false)
    }

    fn new_operation_with_cancel_mode(
        &self,
        user_data: u64,
        cooperative_cancel: bool,
    ) -> Result<(u64, Arc<AtomicBool>), aster_status_t> {
        if self.closing.load(Ordering::Acquire) {
            return Err(aster_status_t::ASTER_STATUS_ALREADY_CLOSED);
        }
        let completion = self
            .operation_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| aster_status_t::ASTER_STATUS_OPERATION_LIMIT)?;
        if self.closing.load(Ordering::Acquire) {
            return Err(aster_status_t::ASTER_STATUS_ALREADY_CLOSED);
        }
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let op = OperationState {
            cancelled: cancelled.clone(),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            cooperative_cancel,
            user_data,
            completion: Mutex::new(Some(completion)),
        };
        let id = self.operations.insert(op);
        Ok((id, cancelled))
    }

    /// Return the per-doc RegistryAcl, lazily creating one in open mode on first
    /// touch. Returns the same Arc on subsequent calls so add/remove writer state
    /// persists for the life of the bridge.
    fn registry_acl_for_doc(&self, doc: u64) -> Arc<aster_transport_core::registry::RegistryAcl> {
        let mut guard = self.registry_acls.lock().unwrap();
        guard
            .entry(doc)
            .or_insert_with(|| Arc::new(aster_transport_core::registry::RegistryAcl::new()))
            .clone()
    }

    /// Return the per-doc RegistryAcl if one has been created, without creating one.
    fn registry_acl_lookup(
        &self,
        doc: u64,
    ) -> Option<Arc<aster_transport_core::registry::RegistryAcl>> {
        self.registry_acls.lock().unwrap().get(&doc).cloned()
    }

    fn cancel_operation(&self, op_id: u64) -> bool {
        if let Some(op) = self.operations.get(op_id) {
            if op.cooperative_cancel {
                // Custody operations retain their terminal slot until cleanup
                // or an irrevocable named-tag commit has actually completed.
                op.cancelled.store(true, Ordering::SeqCst);
                op.cancel_notify.notify_waiters();
                return true;
            }
        }
        let Some(op) = self.operations.remove(op_id) else {
            return false;
        };
        op.cancelled.store(true, Ordering::SeqCst);
        op.cancel_notify.notify_waiters();
        let Some(permit) = op
            .completion
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return false;
        };
        self.enqueue_terminal(
            EventInternal::new(
                aster_event_kind_t::ASTER_EVENT_OPERATION_CANCELLED,
                aster_status_t::ASTER_STATUS_CANCELLED,
                op_id,
                0,
                0,
                op.user_data,
                aster_status_t::ASTER_STATUS_CANCELLED as i32,
            ),
            None,
            permit,
        );
        true
    }
}

// ============================================================================
// Global Runtime Registry
// ============================================================================

static RUNTIMES: std::sync::OnceLock<Mutex<HashMap<aster_runtime_t, Arc<BridgeRuntime>>>> =
    std::sync::OnceLock::new();

fn runtimes() -> &'static Mutex<HashMap<aster_runtime_t, Arc<BridgeRuntime>>> {
    RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn load_runtime(handle: aster_runtime_t) -> Result<Arc<BridgeRuntime>, aster_status_t> {
    let guard = runtimes()
        .lock()
        .map_err(|_| aster_status_t::ASTER_STATUS_INTERNAL)?;
    guard
        .get(&handle)
        .cloned()
        .ok_or(aster_status_t::ASTER_STATUS_NOT_FOUND)
}

// ============================================================================
// Utility Functions
// ============================================================================

fn new_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

unsafe fn read_bytes(b: &aster_bytes_t) -> Vec<u8> {
    if b.ptr.is_null() || b.len == 0 {
        Vec::new()
    } else {
        slice::from_raw_parts(b.ptr, b.len).to_vec()
    }
}

unsafe fn read_bytes_opt(b: &aster_bytes_t) -> Option<Vec<u8>> {
    if b.ptr.is_null() || b.len == 0 {
        None
    } else {
        Some(slice::from_raw_parts(b.ptr, b.len).to_vec())
    }
}

unsafe fn read_string(b: &aster_bytes_t) -> Result<String, i32> {
    let bytes = read_bytes(b);
    String::from_utf8(bytes).map_err(set_last_error)
}

unsafe fn read_string_list(list: &aster_bytes_list_t) -> Vec<String> {
    if list.items.is_null() || list.len == 0 {
        Vec::new()
    } else {
        let items = slice::from_raw_parts(list.items, list.len);
        items.iter().filter_map(|b| read_string(b).ok()).collect()
    }
}

/// Hand a Rust-owned byte buffer to the caller as an `aster_bytes_t` that the
/// caller frees with `aster_string_release(ptr, len)`.
///
/// Allocation contract (must stay in lock-step with `aster_string_release`):
/// - empty input -> `{ ptr: NULL, len: 0 }`, nothing is allocated;
/// - otherwise exactly `len + 1` bytes are allocated as a `Box<[u8]>` whose
///   last byte is a NUL terminator (so C callers may treat text as a C
///   string), and `len` excludes that terminator.
///
/// `into_boxed_slice` shrinks the allocation to exactly `len + 1`, so the
/// release side can rebuild the identical `Box<[u8]>` from `(ptr, len + 1)`.
fn alloc_owned_bytes(mut bytes: Vec<u8>) -> aster_bytes_t {
    if bytes.is_empty() {
        return aster_bytes_t {
            ptr: std::ptr::null(),
            len: 0,
        };
    }
    let len = bytes.len();
    bytes.reserve_exact(1);
    bytes.push(0);
    let boxed: Box<[u8]> = bytes.into_boxed_slice();
    debug_assert_eq!(boxed.len(), len + 1);
    let ptr = Box::into_raw(boxed) as *mut u8 as *const u8;
    aster_bytes_t { ptr, len }
}

fn alloc_string(s: String) -> aster_bytes_t {
    alloc_owned_bytes(s.into_bytes())
}

fn alloc_bytes(bytes: Vec<u8>) -> aster_bytes_t {
    alloc_owned_bytes(bytes)
}

/// Encode a before_connect hook payload as:
/// [4 bytes LE: remote_id_len][remote_id bytes][4 bytes LE: alpn_len][alpn bytes]
fn encode_hook_connect_payload(info: &CoreHookConnectInfo) -> Vec<u8> {
    let id = info.remote_endpoint_id.as_bytes();
    let alpn = &info.alpn;
    let mut buf = Vec::with_capacity(8 + id.len() + alpn.len());
    buf.extend_from_slice(&(id.len() as u32).to_le_bytes());
    buf.extend_from_slice(id);
    buf.extend_from_slice(&(alpn.len() as u32).to_le_bytes());
    buf.extend_from_slice(alpn);
    buf
}

/// Encode an after_handshake hook payload as:
/// [4 bytes LE: remote_id_len][remote_id bytes][4 bytes LE: alpn_len][alpn bytes][1 byte: is_alive]
fn encode_hook_handshake_payload(info: &CoreHookHandshakeInfo) -> Vec<u8> {
    let id = info.remote_endpoint_id.as_bytes();
    let alpn = &info.alpn;
    let mut buf = Vec::with_capacity(9 + id.len() + alpn.len());
    buf.extend_from_slice(&(id.len() as u32).to_le_bytes());
    buf.extend_from_slice(id);
    buf.extend_from_slice(&(alpn.len() as u32).to_le_bytes());
    buf.extend_from_slice(alpn);
    buf.push(info.is_alive as u8);
    buf
}

// Helper to check if operation was cancelled
fn check_cancelled(
    cancelled: &Arc<std::sync::atomic::AtomicBool>,
    bridge: &Arc<BridgeRuntime>,
    op_id: u64,
    user_data: u64,
) -> bool {
    if cancelled.load(Ordering::SeqCst) {
        bridge.emit_simple(
            aster_event_kind_t::ASTER_EVENT_OPERATION_CANCELLED,
            aster_status_t::ASTER_STATUS_CANCELLED,
            op_id,
            0,
            0,
            user_data,
            aster_status_t::ASTER_STATUS_CANCELLED as i32,
        );
        true
    } else {
        false
    }
}

// ============================================================================
// C FFI Functions - Versioning
// ============================================================================

/// Packed ABI version of the loaded library:
/// `(major << 16) | (minor << 8) | patch` (see `ASTER_ABI_VERSION`).
/// Unpack with `v >> 16`, `(v >> 8) & 0xff`, `v & 0xff`. A binding must
/// refuse to run when the major differs from the one it was written
/// against; `aster_runtime_new` enforces this as well.
#[no_mangle]
pub extern "C" fn aster_abi_version() -> u32 {
    ASTER_ABI_VERSION
}

/// Set once a caller proved it was built for this ABI major
/// (`aster_abi_accept` or a successful `aster_runtime_new`).
static ABI_ACCEPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn abi_major_matches(expected_abi_major: u32) -> bool {
    if expected_abi_major == ASTER_ABI_VERSION_MAJOR {
        ABI_ACCEPTED.store(true, std::sync::atomic::Ordering::Release);
        return true;
    }
    set_last_error(format!(
        "ABI major mismatch: caller expects {expected_abi_major}, library provides {ASTER_ABI_VERSION_MAJOR}.{ASTER_ABI_VERSION_MINOR}.{ASTER_ABI_VERSION_PATCH}"
    ));
    false
}

/// `ASTER_STATUS_OK` when the process has passed the ABI handshake, else
/// `ASTER_STATUS_ABI_MISMATCH` with a last-error naming `what`. Functions that
/// write a struct whose size changed across majors call this first, so a
/// caller built for an older major is refused before anything is written.
pub(crate) fn require_abi_accepted(what: &str) -> i32 {
    if ABI_ACCEPTED.load(std::sync::atomic::Ordering::Acquire) {
        return aster_status_t::ASTER_STATUS_OK as i32;
    }
    set_last_error(format!(
        "{what}: call aster_abi_accept({ASTER_ABI_VERSION_MAJOR}) or aster_runtime_new first; \
         this library is ABI {ASTER_ABI_VERSION_MAJOR}.{ASTER_ABI_VERSION_MINOR}.{ASTER_ABI_VERSION_PATCH}"
    ));
    aster_status_t::ASTER_STATUS_ABI_MISMATCH as i32
}

/// The ABI handshake without a runtime (ABI 6.0). `expected_abi_major` is the
/// `ASTER_ABI_VERSION_MAJOR` the caller was built against. On a match this
/// returns `ASTER_STATUS_OK` and unlocks the functions that write an
/// `aster_config_t` (`aster_config_init`, `_from_env`, `_from_toml`); on a
/// mismatch it returns `ASTER_STATUS_ABI_MISMATCH` and sets the last error.
/// A successful `aster_runtime_new` does the same. Idempotent and thread-safe.
#[no_mangle]
pub extern "C" fn aster_abi_accept(expected_abi_major: u32) -> i32 {
    if abi_major_matches(expected_abi_major) {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_ABI_MISMATCH as i32
    }
}

/// Exact Aster build version as a process-lifetime, NUL-terminated UTF-8 string.
/// The returned pointer is borrowed and must not be freed.
#[no_mangle]
pub extern "C" fn aster_version() -> *const c_char {
    static VERSION: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| {
            CString::new(aster_transport_core::VERSION)
                .expect("ASTER_BUILD_VERSION cannot contain a NUL byte")
        })
        .as_ptr()
}

// ============================================================================
// C FFI Functions - Runtime
// ============================================================================

/// Create a runtime.
///
/// `expected_abi_major` is the `ASTER_ABI_VERSION_MAJOR` the caller was
/// written against. A mismatch returns `ASTER_STATUS_ABI_MISMATCH` (and sets
/// the last-error message) without creating anything.
///
/// `config` may be NULL for defaults. When non-NULL, `config->struct_size`
/// must equal `sizeof(aster_runtime_config_t)`, otherwise
/// `ASTER_STATUS_INVALID_ARGUMENT` is returned.
#[no_mangle]
pub unsafe extern "C" fn aster_runtime_new(
    expected_abi_major: u32,
    config: *const aster_runtime_config_t,
    out_runtime: *mut aster_runtime_t,
) -> i32 {
    // Opt-in Rust log sink (`ASTER_LOG_STDERR=1`, filter `ASTER_LOG`);
    // idempotent, so every runtime after the first is a no-op.
    aster_otel::init_stderr_from_env();
    if !abi_major_matches(expected_abi_major) {
        return aster_status_t::ASTER_STATUS_ABI_MISMATCH as i32;
    }
    if out_runtime.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    if let Some(c) = config.as_ref() {
        let expected = std::mem::size_of::<aster_runtime_config_t>() as u32;
        if c.struct_size != expected {
            set_last_error(format!(
                "aster_runtime_config_t.struct_size is {}, expected {expected}",
                c.struct_size
            ));
            return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
        }
    }

    let worker_threads = config.as_ref().map(|c| c.worker_threads).unwrap_or(0);
    let queue_capacity = config
        .as_ref()
        .map(|c| c.event_queue_capacity)
        .unwrap_or(4096);
    let max_inflight_operations = config
        .as_ref()
        .map(|c| c.max_inflight_operations)
        .unwrap_or(4096);

    let bridge = match BridgeRuntime::new(worker_threads, queue_capacity, max_inflight_operations) {
        Ok(b) => b,
        Err(e) => return set_last_error(e),
    };

    let runtime_id = new_handle();

    if let Ok(mut guard) = runtimes().lock() {
        guard.insert(runtime_id, Arc::new(bridge));
        *out_runtime = runtime_id;
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_INTERNAL as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn aster_runtime_close(runtime: aster_runtime_t) -> i32 {
    match close_runtime(runtime) {
        Ok(_) => aster_status_t::ASTER_STATUS_OK as i32,
        Err(status) => status as i32,
    }
}

/// Unregister `runtime` and drop the registry's reference. In-flight tasks
/// may still hold clones; whichever reference is last shuts the runtime down
/// safely (see `Drop for BridgeRuntime`). Returns its teardown signal.
fn close_runtime(runtime: aster_runtime_t) -> Result<Arc<RuntimeTeardown>, aster_status_t> {
    let arc = match runtimes().lock() {
        Ok(mut guard) => match guard.remove(&runtime) {
            Some(arc) => arc,
            None => return Err(aster_status_t::ASTER_STATUS_NOT_FOUND),
        },
        Err(_) => return Err(aster_status_t::ASTER_STATUS_INTERNAL),
    };
    arc.begin_close();
    audit_outbox::shutdown_runtime(&arc);
    arc.release_handles();
    let teardown = arc.teardown.clone();
    drop(arc);
    Ok(teardown)
}

/// Rust-only, not part of the C ABI: `aster_runtime_close`, then wait up to
/// `timeout` for the runtime's worker threads to finish shutting down.
/// Returns `ASTER_STATUS_TIMEOUT` if they have not by then (the shutdown
/// still completes in the background).
///
/// For tests: a test process that exits while a closed runtime is still
/// tearing down on its detached drop thread trips LeakSanitizer.
#[doc(hidden)]
pub fn aster_runtime_close_and_wait(runtime: aster_runtime_t, timeout: Duration) -> i32 {
    match close_runtime(runtime) {
        Ok(teardown) if teardown.wait(timeout) => aster_status_t::ASTER_STATUS_OK as i32,
        Ok(_) => aster_status_t::ASTER_STATUS_TIMEOUT as i32,
        Err(status) => status as i32,
    }
}

// ============================================================================
// C FFI Functions - Secret Key
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_secret_key_generate(
    out_key_ptr: *mut u8,
    out_key_capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let mut key = [0u8; 32];
    if getrandom::getrandom(&mut key).is_err() {
        return aster_status_t::ASTER_STATUS_INTERNAL as i32;
    }
    let len = key.len();

    *out_len = len;

    if out_key_ptr.is_null() || out_key_capacity < len {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }

    unsafe {
        ptr::copy_nonoverlapping(key.as_ptr(), out_key_ptr, len);
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Event Polling
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_poll_events(
    runtime: aster_runtime_t,
    out_events: *mut aster_event_t,
    max_events: usize,
    timeout_ms: u32,
) -> usize {
    if out_events.is_null() || max_events == 0 {
        return 0;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(_) => return 0,
    };

    let mut guard = match bridge.events_rx.lock() {
        Ok(g) => g,
        Err(_) => return 0,
    };

    let first = if timeout_ms == 0 {
        guard.try_recv().ok()
    } else {
        bridge.runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(timeout_ms as u64), guard.recv())
                .await
                .ok()
                .flatten()
        })
    };

    let mut written = 0usize;

    if let Some(event_owned) = first {
        let ev = to_iroh_event(event_owned, &bridge.buffers);
        unsafe {
            ptr::write(out_events.add(written), ev);
        }
        written += 1;
    } else {
        return 0;
    }

    while written < max_events {
        match guard.try_recv() {
            Ok(event_owned) => {
                let ev = to_iroh_event(event_owned, &bridge.buffers);
                unsafe {
                    ptr::write(out_events.add(written), ev);
                }
                written += 1;
            }
            Err(_) => break,
        }
    }

    written
}

/// Convert internal EventInternal to C-compatible aster_event_t
fn to_iroh_event(mut owned: EventOwned, buffers: &BufferRegistry) -> aster_event_t {
    if let Some(data) = owned.data.take() {
        let (buffer, bytes) = buffers.insert(data);
        owned.event = owned.event.with_buffer(buffer, bytes.len());
    }
    let event = &owned.event;
    let (data_ptr, data_len) = if event.buffer_id != 0 {
        if let Some(buf) = buffers.get(event.buffer_id) {
            (buf.as_ptr(), buf.len())
        } else {
            (ptr::null(), 0)
        }
    } else {
        (ptr::null(), 0)
    };

    aster_event_t {
        struct_size: event.struct_size,
        kind: event.kind,
        status: event.status,
        operation: event.operation,
        handle: event.handle,
        related: event.related,
        user_data: event.user_data,
        data_ptr,
        data_len,
        buffer: event.buffer_id,
        error_code: event.error_code,
        flags: event.flags,
    }
}

#[no_mangle]
pub unsafe extern "C" fn aster_buffer_release(runtime: aster_runtime_t, buffer: u64) -> i32 {
    if buffer == 0 {
        return aster_status_t::ASTER_STATUS_OK as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(_) => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    if bridge.buffers.release(buffer) {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

/// Release a Rust-owned buffer returned in an `aster_bytes_t` field of an
/// out-struct: `aster_remote_info_t.node_id` / `.relay_url` (from
/// `aster_endpoint_remote_info` and each entry of
/// `aster_endpoint_remote_info_list`), `aster_connection_info_t.alpn`, and
/// `aster_transport_snapshot_t.peer_host` / `.relay_url`, and the `out`
/// buffer of `aster_{stream_header,call_header,rpc_status}_encode`,
/// `aster_zstd_maybe_compress`, `aster_zstd_decompress`, and both PEM fields
/// of `aster_generated_cert_t`.
///
/// Pass back exactly the `ptr` and `len` the library returned. Non-empty
/// buffers are additionally NUL-terminated (the terminator is not counted in
/// `len`). Empty buffers are returned as `{NULL, 0}`; releasing a NULL `ptr`
/// or a `len` of 0 is a no-op that returns `ASTER_STATUS_OK`.
///
/// # Safety
/// - A non-NULL `ptr` with `len > 0` must be a buffer returned by this
///   library through one of the fields above, with its original `len`.
/// - Must be called at most once for each buffer.
#[no_mangle]
pub unsafe extern "C" fn aster_string_release(ptr: *const u8, len: usize) -> i32 {
    if ptr.is_null() || len == 0 {
        return aster_status_t::ASTER_STATUS_OK as i32;
    }
    let Some(alloc_len) = len.checked_add(1) else {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    };
    // SAFETY: the caller guarantees (ptr, len) came from `alloc_owned_bytes`,
    // which allocated exactly `len + 1` bytes as a `Box<[u8]>`. Rebuild that
    // identical boxed slice so the deallocation layout matches.
    let _boxed: Box<[u8]> = unsafe {
        Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            ptr as *mut u8,
            alloc_len,
        ))
    };
    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Operation Cancellation
// ============================================================================

/// Request cancellation. Continue polling and collect the operation's terminal
/// event. ABI 4.13 custody and verified imports acknowledge cancellation only
/// after native cleanup; a named-tag commit already started reports its actual
/// result, which may be success. Fetch cleanup can wait for downloader drain.
/// # Safety
/// This function does not dereference caller pointers.
#[no_mangle]
pub unsafe extern "C" fn aster_operation_cancel(
    runtime: aster_runtime_t,
    operation: aster_operation_t,
) -> i32 {
    if operation == 0 {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.cancel_operation(operation) {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

// ============================================================================
// C FFI Functions - Error
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_last_error_message(buffer: *mut u8, capacity: usize) -> usize {
    let msg = LAST_ERROR.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|s| s.to_bytes().to_vec())
            .unwrap_or_default()
    });

    let len = msg.len().min(capacity);
    if !buffer.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(msg.as_ptr(), buffer, len);
        }
    }

    msg.len()
}

/// Static, NUL-terminated name for an `aster_status_t` value (passed as the
/// raw `int32_t` that every function returns). Unknown values (including the
/// negative ones) yield `"ASTER_STATUS_UNKNOWN"`.
#[no_mangle]
pub extern "C" fn aster_status_name(status: i32) -> *const c_char {
    let s = match status {
        x if x == aster_status_t::ASTER_STATUS_OK as i32 => "ASTER_STATUS_OK\0",
        x if x == aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32 => {
            "ASTER_STATUS_INVALID_ARGUMENT\0"
        }
        x if x == aster_status_t::ASTER_STATUS_NOT_FOUND as i32 => "ASTER_STATUS_NOT_FOUND\0",
        x if x == aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32 => {
            "ASTER_STATUS_ALREADY_CLOSED\0"
        }
        x if x == aster_status_t::ASTER_STATUS_QUEUE_FULL as i32 => "ASTER_STATUS_QUEUE_FULL\0",
        x if x == aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32 => {
            "ASTER_STATUS_BUFFER_TOO_SMALL\0"
        }
        x if x == aster_status_t::ASTER_STATUS_UNSUPPORTED as i32 => "ASTER_STATUS_UNSUPPORTED\0",
        x if x == aster_status_t::ASTER_STATUS_INTERNAL as i32 => "ASTER_STATUS_INTERNAL\0",
        x if x == aster_status_t::ASTER_STATUS_TIMEOUT as i32 => "ASTER_STATUS_TIMEOUT\0",
        x if x == aster_status_t::ASTER_STATUS_CANCELLED as i32 => "ASTER_STATUS_CANCELLED\0",
        x if x == aster_status_t::ASTER_STATUS_CONNECTION_REFUSED as i32 => {
            "ASTER_STATUS_CONNECTION_REFUSED\0"
        }
        x if x == aster_status_t::ASTER_STATUS_STREAM_RESET as i32 => "ASTER_STATUS_STREAM_RESET\0",
        x if x == aster_status_t::ASTER_STATUS_ABI_MISMATCH as i32 => "ASTER_STATUS_ABI_MISMATCH\0",
        x if x == aster_status_t::ASTER_STATUS_VERIFICATION_FAILED as i32 => {
            "ASTER_STATUS_VERIFICATION_FAILED\0"
        }
        x if x == aster_status_t::ASTER_STATUS_UNAUTHENTICATED as i32 => {
            "ASTER_STATUS_UNAUTHENTICATED\0"
        }
        x if x == aster_status_t::ASTER_STATUS_PERMISSION_DENIED as i32 => {
            "ASTER_STATUS_PERMISSION_DENIED\0"
        }
        x if x == aster_status_t::ASTER_STATUS_CREDENTIAL_EXPIRED as i32 => {
            "ASTER_STATUS_CREDENTIAL_EXPIRED\0"
        }
        x if x == aster_status_t::ASTER_STATUS_ALREADY_EXISTS as i32 => {
            "ASTER_STATUS_ALREADY_EXISTS\0"
        }
        x if x == aster_status_t::ASTER_STATUS_OPERATION_LIMIT as i32 => {
            "ASTER_STATUS_OPERATION_LIMIT\0"
        }
        x if x == aster_status_t::ASTER_STATUS_UNAVAILABLE as i32 => "ASTER_STATUS_UNAVAILABLE\0",
        _ => "ASTER_STATUS_UNKNOWN\0",
    };
    s.as_ptr() as *const c_char
}

// ============================================================================
// C FFI Functions - Node
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_node_close(
    runtime: aster_runtime_t,
    node: aster_node_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        node_arc.close().await;
        node::forget_node(&bridge2, node);
        bridge2.nodes.remove(node);
        bridge2.emit_simple(
            aster_event_kind_t::ASTER_EVENT_CLOSED,
            aster_status_t::ASTER_STATUS_OK,
            op_id,
            node,
            0,
            user_data,
            0,
        );
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_node_id(
    runtime: aster_runtime_t,
    node: aster_node_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let id = node_arc.node_id();
    let len = id.len();

    *out_len = len;

    if capacity < len {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(id.as_ptr(), out_buf, len);
        }
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_node_addr_info(
    runtime: aster_runtime_t,
    node: aster_node_t,
    out_buf: *mut u8,
    buf_capacity: usize,
    out_addr: *mut aster_node_addr_t,
) -> i32 {
    if out_addr.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let addr = node_arc.node_addr_info();
    pack_node_addr(&addr, out_buf, buf_capacity, out_addr)
}

#[no_mangle]
pub unsafe extern "C" fn aster_node_export_secret_key(
    runtime: aster_runtime_t,
    node: aster_node_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let key = node_arc.export_secret_key();
    let len = key.len();

    *out_len = len;

    if capacity < len {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(key.as_ptr(), out_buf, len);
        }
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Pack `addr` into the caller's scratch buffer and point `*out_addr` into it:
/// NUL-terminated endpoint id, relay URL and direct addresses, then an aligned
/// `aster_bytes_t` array for the direct addresses. Shared by
/// `aster_node_addr_info` and `aster_endpoint_addr_info`.
unsafe fn pack_node_addr(
    addr: &CoreNodeAddr,
    out_buf: *mut u8,
    buf_capacity: usize,
    out_addr: *mut aster_node_addr_t,
) -> i32 {
    if out_buf.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    // Pack into scratch buffer: endpoint_id, relay_url, direct_addresses
    let mut offset = 0;

    // endpoint_id
    if offset + addr.endpoint_id.len() + 1 > buf_capacity {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }
    let ep_id_offset = offset;
    unsafe {
        ptr::copy_nonoverlapping(
            addr.endpoint_id.as_ptr(),
            out_buf.add(offset),
            addr.endpoint_id.len(),
        );
    }
    offset += addr.endpoint_id.len();
    unsafe {
        *out_buf.add(offset) = 0;
    }
    offset += 1;

    // relay_url
    let relay_offset = if let Some(ref url) = addr.relay_url {
        if offset + url.len() + 1 > buf_capacity {
            return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
        }
        let rel_off = offset;
        unsafe {
            ptr::copy_nonoverlapping(url.as_ptr(), out_buf.add(offset), url.len());
        }
        offset += url.len();
        unsafe {
            *out_buf.add(offset) = 0;
        }
        offset += 1;
        Some(rel_off)
    } else {
        unsafe {
            *out_buf.add(offset) = 0;
        }
        offset += 1;
        None
    };

    // direct_addresses - pack as additional null-terminated strings
    let mut addr_offsets = Vec::new();
    for direct_addr in &addr.direct_addresses {
        if offset + direct_addr.len() + 1 > buf_capacity {
            return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
        }
        let addr_off = offset;
        unsafe {
            ptr::copy_nonoverlapping(direct_addr.as_ptr(), out_buf.add(offset), direct_addr.len());
        }
        offset += direct_addr.len();
        unsafe {
            *out_buf.add(offset) = 0;
        }
        offset += 1;
        addr_offsets.push(addr_off);
    }

    // Create the bytes list for direct addresses
    let direct_addrs_ptr: *const aster_bytes_t = if addr_offsets.is_empty() {
        ptr::null()
    } else {
        // Allocate space for the list at the end, aligned for `aster_bytes_t`
        // (the packed strings before it leave `offset` at an arbitrary byte
        // boundary; writing the structs there unaligned is UB).
        let pad = out_buf
            .wrapping_add(offset)
            .align_offset(std::mem::align_of::<aster_bytes_t>());
        let list_offset = offset.saturating_add(pad);
        offset =
            list_offset.saturating_add(addr_offsets.len() * std::mem::size_of::<aster_bytes_t>());
        if offset > buf_capacity {
            return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
        }

        for (i, &addr_off) in addr_offsets.iter().enumerate() {
            let item_ptr = unsafe {
                out_buf
                    .add(list_offset)
                    .add(i * std::mem::size_of::<aster_bytes_t>())
                    as *mut aster_bytes_t
            };
            let addr_len = addr.direct_addresses[i].len();
            unsafe {
                *item_ptr = aster_bytes_t {
                    ptr: out_buf.add(addr_off),
                    len: addr_len,
                };
            }
        }
        unsafe { out_buf.add(list_offset) as *const aster_bytes_t }
    };

    *out_addr = aster_node_addr_t {
        endpoint_id: aster_bytes_t {
            ptr: unsafe { out_buf.add(ep_id_offset) },
            len: addr.endpoint_id.len(),
        },
        relay_url: aster_bytes_t {
            ptr: relay_offset
                .map(|o| unsafe { out_buf.add(o) })
                .unwrap_or(ptr::null_mut()),
            len: addr.relay_url.as_ref().map(|s| s.len()).unwrap_or(0),
        },
        direct_addresses: aster_bytes_list_t {
            items: direct_addrs_ptr,
            len: addr.direct_addresses.len(),
        },
    };

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Endpoint
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_stream_stop(
    runtime: aster_runtime_t,
    recv_stream: aster_recv_stream_t,
    error_code: u32,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.recv_streams.get(recv_stream) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    if let Err(e) = stream_arc.stop(error_code as u64) {
        return set_last_error(e);
    }

    bridge.recv_streams.remove(recv_stream);
    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Connections
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_connect(
    runtime: aster_runtime_t,
    endpoint_or_node: u64,
    config: *const aster_connect_config_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if config.is_null() || out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let ep_arc = bridge
        .nodes
        .get(endpoint_or_node)
        .map(|n| Arc::new(n.net_client()));

    let ep = match ep_arc {
        Some(e) => e,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let cfg = unsafe { *config };
    let node_id = match unsafe { read_string(&cfg.node_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let alpn = unsafe { read_bytes(&cfg.alpn) };

    // If addr is provided, copy it out before the async block to avoid lifetime issues
    let connect_addr = if !cfg.addr.is_null() {
        let addr_ref = unsafe { &*cfg.addr };
        let addr_node_id = match unsafe { read_string(&addr_ref.endpoint_id) } {
            Ok(s) => s,
            Err(e) => return e,
        };
        let relay_url = if addr_ref.relay_url.ptr.is_null() || addr_ref.relay_url.len == 0 {
            None
        } else {
            match unsafe { read_string(&addr_ref.relay_url) } {
                Ok(s) => Some(s),
                Err(e) => return e,
            }
        };
        let mut direct_addresses = Vec::new();
        if !addr_ref.direct_addresses.items.is_null() && addr_ref.direct_addresses.len > 0 {
            let items = unsafe {
                slice::from_raw_parts(
                    addr_ref.direct_addresses.items,
                    addr_ref.direct_addresses.len,
                )
            };
            for item in items {
                match unsafe { read_string(item) } {
                    Ok(s) => direct_addresses.push(s),
                    Err(e) => return e,
                }
            }
        }
        Some(CoreNodeAddr {
            endpoint_id: addr_node_id,
            relay_url,
            direct_addresses,
        })
    } else {
        None
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        let connect_result = if let Some(addr) = connect_addr {
            ep.connect_node_addr(addr, alpn).await
        } else {
            ep.connect(node_id, alpn).await
        };

        match connect_result {
            Ok(conn) => {
                let handle = bridge2.connections.insert(conn);
                bridge2.emit_simple_with_handle(
                    aster_event_kind_t::ASTER_EVENT_CONNECTED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    handle,
                    endpoint_or_node,
                    user_data,
                    0,
                    &bridge2.connections,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Accept the next inbound connection of a node (`aster_node_start`).
///
/// Since ABI 4.0 this takes the connection from the node's aster-ALPN accept
/// queue (the same queue `aster_node_accept_aster` reads: connections on the
/// node's configured ALPNs that the router hands over) and emits
/// `ASTER_EVENT_CONNECTION_ACCEPTED` with `handle` = connection, `related` =
/// node. Before 4.0 it called the endpoint's accept directly, racing the
/// node's router. Unknown handle -> `NOT_FOUND`.
#[no_mangle]
pub unsafe extern "C" fn aster_accept(
    runtime: aster_runtime_t,
    node: aster_node_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match node_arc.accept_aster().await {
            Ok((_alpn, conn)) => {
                let handle = bridge2.connections.insert(conn);
                bridge2.emit_simple_with_handle(
                    aster_event_kind_t::ASTER_EVENT_CONNECTION_ACCEPTED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    handle,
                    node,
                    user_data,
                    0,
                    &bridge2.connections,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// Phase 1e: Unified Aster Node — custom ALPNs on the shared iroh Router
// ============================================================================

/// Pull the next incoming aster-ALPN connection from the node's queue.
/// Long-poll: spawns a tokio task, emits ASTER_EVENT_ASTER_ACCEPTED when a
/// connection arrives. event.handle = connection_handle, event.data_ptr/len
/// = ALPN bytes, event.buffer = lease to release via aster_buffer_release.
#[no_mangle]
pub unsafe extern "C" fn aster_node_accept_aster(
    runtime: aster_runtime_t,
    node: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match node_arc.accept_aster().await {
            Ok((alpn, conn)) => {
                let conn_handle = bridge2.connections.insert(conn);
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_ASTER_ACCEPTED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    conn_handle,
                    0,
                    user_data,
                    0,
                );
                // ALPN bytes go into the event payload (data_ptr/data_len).
                bridge2.complete_with_handle(event, Some(alpn), &bridge2.connections, conn_handle);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_connection_remote_id(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let id = conn_arc.remote_id();
    let len = id.len();

    *out_len = len;

    if capacity < len {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(id.as_ptr(), out_buf, len);
        }
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_connection_close(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    error_code: u32,
    reason: aster_bytes_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let reason_bytes = unsafe { read_bytes(&reason) };

    if let Err(e) = conn_arc.close(error_code as u64, reason_bytes) {
        return set_last_error(e);
    }

    bridge.connections.remove(connection);
    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_connection_closed(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        let closed_info = conn_arc.closed().await;
        // Serialize closed info: kind + error_code + reason
        let mut payload = Vec::new();
        payload.extend_from_slice(closed_info.kind.as_bytes());
        payload.push(0); // null separator
        if let Some(reason) = &closed_info.reason {
            payload.extend_from_slice(reason);
        }

        let error_code = closed_info.code.map(|c| c as i32).unwrap_or(-1);

        let event = EventInternal::new(
            aster_event_kind_t::ASTER_EVENT_CONNECTION_CLOSED,
            aster_status_t::ASTER_STATUS_OK,
            op_id,
            connection,
            0,
            user_data,
            error_code,
        );
        bridge2.emit_with_data(event, payload);
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_connection_send_datagram(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    data: aster_bytes_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let data_bytes = unsafe { read_bytes(&data) };

    if let Err(e) = conn_arc.send_datagram(data_bytes) {
        return set_last_error(e);
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_connection_read_datagram(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match conn_arc.read_datagram().await {
            Ok(data) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_DATAGRAM_RECEIVED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    connection,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Streams
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_open_bi(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match conn_arc.open_bi().await {
            Ok((send, recv)) => {
                let send_handle = bridge2.send_streams.insert(send);
                let recv_handle = bridge2.recv_streams.insert(recv);
                bridge2.emit_simple_with_handles(
                    aster_event_kind_t::ASTER_EVENT_STREAM_OPENED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    send_handle,
                    recv_handle,
                    user_data,
                    0,
                    &bridge2.send_streams,
                    &bridge2.recv_streams,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_accept_bi(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match conn_arc.accept_bi().await {
            Ok((send, recv)) => {
                let send_handle = bridge2.send_streams.insert(send);
                let recv_handle = bridge2.recv_streams.insert(recv);
                bridge2.emit_simple_with_handles(
                    aster_event_kind_t::ASTER_EVENT_STREAM_ACCEPTED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    send_handle,
                    recv_handle,
                    user_data,
                    0,
                    &bridge2.send_streams,
                    &bridge2.recv_streams,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// Tunneling — see ffi_spec/Aster-tunneling.md
// ============================================================================

/// Synchronous: authorize a TCP tunnel on `connection` to `host:port`.
/// On success, writes the 32-byte ticket bytes to `out_ticket` (must
/// point to ≥32 bytes). The bytes go into the RPC response payload
/// the handler returns to the peer.
///
/// `host` is read as a UTF-8 string from `aster_bytes_t`.
/// `ttl_secs == 0` uses the node default (30s).
#[no_mangle]
pub unsafe extern "C" fn aster_connection_authorize_tunnel_tcp(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    host: aster_bytes_t,
    port: u16,
    ttl_secs: u32,
    out_ticket: *mut u8,
) -> i32 {
    if out_ticket.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };
    let host_str = match unsafe { read_string(&host) } {
        Ok(s) => s,
        Err(s) => return s,
    };
    let addr_str = if host_str.contains(':') && !host_str.starts_with('[') {
        format!("[{host_str}]:{port}")
    } else {
        format!("{host_str}:{port}")
    };
    let addr: std::net::SocketAddr = match addr_str.parse() {
        Ok(a) => a,
        Err(e) => return set_last_error(format!("bad address: {e}")),
    };
    let ticket = match conn_arc.authorize_tunnel(
        vec![aster_transport_core::tunnel::TunnelTarget::Tcp { addr }],
        Duration::from_secs(ttl_secs as u64),
    ) {
        Ok(t) => t,
        Err(e) => return set_last_error(e),
    };
    unsafe {
        ptr::copy_nonoverlapping(ticket.as_bytes().as_ptr(), out_ticket, 32);
    }
    aster_status_t::ASTER_STATUS_OK as i32
}

/// Async: redeem a 32-byte tunnel ticket on `connection`. Emits
/// ASTER_EVENT_STREAM_OPENED with the (send, recv) pair on success.
/// The streams are raw — bytes after this carry application data.
///
/// `ticket` must point to exactly 32 readable bytes.
#[no_mangle]
pub unsafe extern "C" fn aster_connection_open_tunnel(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    ticket: *const u8,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() || ticket.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let mut ticket_bytes = [0u8; 32];
    unsafe { ptr::copy_nonoverlapping(ticket, ticket_bytes.as_mut_ptr(), 32) };
    let ticket = aster_transport_core::tunnel::TunnelTicket(ticket_bytes);

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }
        match conn_arc.open_tunnel(ticket).await {
            Ok((send, recv)) => {
                let send_handle = bridge2.send_streams.insert(send);
                let recv_handle = bridge2.recv_streams.insert(recv);
                bridge2.emit_simple_with_handles(
                    aster_event_kind_t::ASTER_EVENT_STREAM_OPENED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    send_handle,
                    recv_handle,
                    user_data,
                    0,
                    &bridge2.send_streams,
                    &bridge2.recv_streams,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_open_uni(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match conn_arc.open_uni().await {
            Ok(send) => {
                let handle = bridge2.send_streams.insert(send);
                bridge2.emit_simple_with_handle(
                    aster_event_kind_t::ASTER_EVENT_STREAM_OPENED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    handle,
                    0,
                    user_data,
                    0,
                    &bridge2.send_streams,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_accept_uni(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match conn_arc.accept_uni().await {
            Ok(recv) => {
                let handle = bridge2.recv_streams.insert(recv);
                bridge2.emit_simple_with_handle(
                    aster_event_kind_t::ASTER_EVENT_STREAM_ACCEPTED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    handle,
                    0,
                    user_data,
                    0,
                    &bridge2.recv_streams,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_stream_write(
    runtime: aster_runtime_t,
    send_stream: aster_send_stream_t,
    data: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.send_streams.get(send_stream) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let data_bytes = unsafe { read_bytes(&data) };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match stream_arc.write_all(data_bytes).await {
            Ok(()) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_SEND_COMPLETED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    send_stream,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_stream_finish(
    runtime: aster_runtime_t,
    send_stream: aster_send_stream_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.send_streams.get(send_stream) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match stream_arc.finish().await {
            Ok(()) => {
                bridge2.send_streams.remove(send_stream);
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_STREAM_FINISHED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    send_stream,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_stream_read(
    runtime: aster_runtime_t,
    recv_stream: aster_recv_stream_t,
    max_len: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.recv_streams.get(recv_stream) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match stream_arc.read(max_len).await {
            Ok(Some(data)) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_FRAME_RECEIVED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    recv_stream,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Ok(None) => {
                bridge2.recv_streams.remove(recv_stream);
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_STREAM_FINISHED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    recv_stream,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_stream_read_to_end(
    runtime: aster_runtime_t,
    recv_stream: aster_recv_stream_t,
    max_size: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.recv_streams.get(recv_stream) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match stream_arc.read_to_end(max_size).await {
            Ok(data) => {
                bridge2.recv_streams.remove(recv_stream);
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BYTES_RESULT,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    recv_stream,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_stream_read_exact(
    runtime: aster_runtime_t,
    recv_stream: aster_recv_stream_t,
    exact_len: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.recv_streams.get(recv_stream) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match stream_arc.read_exact(exact_len).await {
            Ok(data) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BYTES_RESULT,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    recv_stream,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_stream_stopped(
    runtime: aster_runtime_t,
    send_stream: aster_send_stream_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let stream_arc = match bridge.send_streams.get(send_stream) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match stream_arc.stopped().await {
            Ok(Some(error_code)) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_STREAM_RESET,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    send_stream,
                    0,
                    user_data,
                    error_code as i32,
                );
            }
            Ok(None) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_STREAM_FINISHED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    send_stream,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Add a peer address to the address lookup of a node, so later dials and
/// blob/doc fetches by node id can reach it.
#[no_mangle]
pub unsafe extern "C" fn aster_add_node_addr(
    runtime: aster_runtime_t,
    endpoint: aster_endpoint_t,
    addr: aster_node_addr_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let ep_arc = match bridge.nodes.get(endpoint).map(|n| Arc::new(n.net_client())) {
        Some(e) => e,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    // Build CoreNodeAddr from FFI struct
    let endpoint_id = match unsafe { read_string(&addr.endpoint_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let relay_url = if addr.relay_url.ptr.is_null() || addr.relay_url.len == 0 {
        None
    } else {
        match unsafe { read_string(&addr.relay_url) } {
            Ok(s) => Some(s),
            Err(e) => return e,
        }
    };

    let mut direct_addresses = Vec::new();
    if !addr.direct_addresses.items.is_null() && addr.direct_addresses.len > 0 {
        let items = unsafe {
            slice::from_raw_parts(addr.direct_addresses.items, addr.direct_addresses.len)
        };
        for item in items {
            match unsafe { read_string(item) } {
                Ok(s) => direct_addresses.push(s),
                Err(e) => return e,
            }
        }
    }

    let core_addr = aster_transport_core::CoreNodeAddr {
        endpoint_id,
        relay_url,
        direct_addresses,
    };

    // Convert to EndpointAddr and add to the endpoint's address lookup
    match aster_transport_core::core_to_endpoint_addr(&core_addr) {
        Ok(endpoint_addr) => {
            if let Ok(lookup) = ep_arc.endpoint.address_lookup() {
                let mem = iroh::address_lookup::memory::MemoryLookup::new();
                mem.add_endpoint_info(endpoint_addr);
                lookup.add(mem);
            }
        }
        Err(e) => return set_last_error(e),
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Handle Free (typed, one per handle kind)
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_node_free(runtime: aster_runtime_t, node: aster_node_t) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    node::forget_node(&bridge, node);
    if bridge.nodes.remove(node).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn aster_connection_free(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.connections.remove(connection).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn aster_send_stream_free(
    runtime: aster_runtime_t,
    stream: aster_send_stream_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.send_streams.remove(stream).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn aster_recv_stream_free(
    runtime: aster_runtime_t,
    stream: aster_recv_stream_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.recv_streams.remove(stream).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

#[no_mangle]
pub unsafe extern "C" fn aster_doc_free(runtime: aster_runtime_t, doc: u64) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.docs.remove(doc).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

/// Synchronous: write the 64-char hex namespace id of the doc to `out_buf`.
///
/// Mirrors `CoreDoc::doc_id()` — used to share a registry namespace so a
/// client can `join_and_subscribe_namespace` the doc and fetch published
/// contract manifests.
///
/// On BUFFER_TOO_SMALL, sets `*out_len` to required size.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_id(
    runtime: aster_runtime_t,
    doc: u64,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };
    let id_hex = doc_arc.doc_id();
    write_to_caller_buf(id_hex.as_bytes(), out_buf, out_len) as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_gossip_topic_free(runtime: aster_runtime_t, topic: u64) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.gossip_topics.remove(topic).is_some() {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

// ============================================================================
// C FFI Functions - Blobs (simplified for Phase 1)
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_blobs_add_bytes(
    runtime: aster_runtime_t,
    node: aster_node_t,
    data: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let data_bytes = unsafe { read_bytes(&data) };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match blobs.add_bytes(data_bytes).await {
            Ok(hash) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BLOB_ADDED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, hash.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_blobs_read(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let hash = match unsafe { read_string(&hash_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match blobs.read_to_bytes(hash).await {
            Ok(data) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BLOB_READ,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Import a file by path into the blob store (Copy mode, never TryReference).
/// On success emits `ASTER_EVENT_BLOB_ADDED` carrying the hash hex as the event
/// payload. `path` is the UTF-8 filesystem path.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_add_path(
    runtime: aster_runtime_t,
    node: aster_node_t,
    path: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let path_str = match unsafe { read_string(&path) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match blobs.add_path(path_str).await {
            Ok(hash) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BLOB_ADDED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, hash.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Import a file by path and set a persistent named tag in one step. On success
/// emits `ASTER_EVENT_BLOB_ADDED` carrying the hash hex as the event payload.
/// `path` and `tag_name` are UTF-8.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_add_path_with_named_tag(
    runtime: aster_runtime_t,
    node: aster_node_t,
    path: aster_bytes_t,
    tag_name: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let path_str = match unsafe { read_string(&path) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let tag_str = match unsafe { read_string(&tag_name) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match blobs.add_path_with_named_tag(path_str, tag_str).await {
            Ok(hash) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BLOB_ADDED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, hash.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Blobs: Collection (sendme-compatible)
// ============================================================================

/// Store bytes as a single-file Collection (HashSeq), compatible with sendme.
/// Emits ASTER_EVENT_BLOB_ADDED with the collection hash in the payload.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_add_bytes_as_collection(
    runtime: aster_runtime_t,
    node: aster_node_t,
    name: aster_bytes_t,
    data: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let name_str = match unsafe { read_string(&name) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let data_bytes = unsafe { read_bytes(&data) };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match blobs.add_bytes_as_collection(name_str, data_bytes).await {
            Ok(hash) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BLOB_ADDED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, hash.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Store a multi-file collection (HashSeq).
///
/// `entries_json` is a UTF-8 JSON string: `[["name1","base64data1"],["name2","base64data2"]]`
/// Each entry's data is base64-encoded. The result (collection hash hex) is emitted as
/// `ASTER_EVENT_BLOB_COLLECTION_ADDED` with the hash string in the event data buffer.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_add_collection(
    runtime: aster_runtime_t,
    node: aster_node_t,
    entries_json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let json_str = match unsafe { read_string(&entries_json) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    // Parse JSON array of [name, base64_data] pairs
    let parsed: Vec<(String, String)> = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(_) => return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
    };

    use base64::Engine;
    let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(parsed.len());
    for (name, b64_data) in parsed {
        match base64::engine::general_purpose::STANDARD.decode(&b64_data) {
            Ok(data) => entries.push((name, data)),
            Err(_) => return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
        }
    }

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match blobs.add_collection(entries).await {
            Ok(hash) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BLOB_COLLECTION_ADDED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, hash.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// List entries from a stored collection.
///
/// `hash_hex` is the collection hash. The result is emitted as
/// `ASTER_EVENT_BLOB_READ` with a JSON string:
/// `[["name1","hash_hex1",size1],["name2","hash_hex2",size2]]`
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_list_collection(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let hash_str = match unsafe { read_string(&hash_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match blobs.list_collection(hash_str).await {
            Ok(entries) => {
                let json = serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string());
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BLOB_READ,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, json.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Create a ticket for a Collection (HashSeq format), compatible with sendme.
/// Writes the ticket string into the caller-provided buffer.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_create_collection_ticket(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex: aster_bytes_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let hash = match unsafe { read_string(&hash_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let ticket = match blobs.create_collection_ticket(hash) {
        Ok(t) => t,
        Err(e) => return set_last_error(e),
    };

    let len = ticket.len();
    *out_len = len;

    if capacity < len {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(ticket.as_ptr(), out_buf, len);
        }
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Blobs: Ticket & Download
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_blobs_create_ticket(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex: aster_bytes_t,
    out_buf: *mut u8,
    capacity: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let hash = match unsafe { read_string(&hash_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let ticket = match blobs.create_ticket(hash) {
        Ok(t) => t,
        Err(e) => return set_last_error(e),
    };

    let len = ticket.len();
    *out_len = len;

    if capacity < len {
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
    }

    if !out_buf.is_null() && len > 0 {
        unsafe {
            ptr::copy_nonoverlapping(ticket.as_ptr(), out_buf, len);
        }
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_blobs_download(
    runtime: aster_runtime_t,
    node: aster_node_t,
    ticket: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let blobs = node_arc.blobs_client();
    let ticket_str = match unsafe { read_string(&ticket) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match blobs.download_blob(ticket_str).await {
            Ok(data) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BLOB_DOWNLOADED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Blob Status / Has (Phase 1c.3)
// ============================================================================

/// Check the status of a blob in the local store. Synchronous.
///
/// Writes to `out_status`: 0 = not_found, 1 = partial, 2 = complete.
/// Writes to `out_size`: byte size (0 if not_found).
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_status(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    out_status: *mut u32,
    out_size: *mut u64,
) -> i32 {
    if hash_hex_ptr.is_null() || out_status.is_null() || out_size.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let hash_hex =
        String::from_utf8(unsafe { slice::from_raw_parts(hash_hex_ptr, hash_hex_len).to_vec() })
            .unwrap_or_default();

    let blobs = node_arc.blobs_client();
    let result = bridge
        .runtime
        .block_on(async move { blobs.blob_status(hash_hex).await });

    match result {
        Ok(status) => {
            let (code, size) = match status {
                aster_transport_core::CoreBlobStatus::NotFound => (0u32, 0u64),
                aster_transport_core::CoreBlobStatus::Partial { size } => (1u32, size),
                aster_transport_core::CoreBlobStatus::Complete { size } => (2u32, size),
            };
            unsafe {
                *out_status = code;
                *out_size = size;
            }
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(_) => aster_status_t::ASTER_STATUS_INTERNAL as i32,
    }
}

/// Check if a blob is fully stored locally. Synchronous.
///
/// Writes to `out_has`: 1 = complete/present, 0 = not complete.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_has(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    out_has: *mut u32,
) -> i32 {
    if hash_hex_ptr.is_null() || out_has.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let hash_hex =
        String::from_utf8(unsafe { slice::from_raw_parts(hash_hex_ptr, hash_hex_len).to_vec() })
            .unwrap_or_default();

    let blobs = node_arc.blobs_client();
    let result = bridge
        .runtime
        .block_on(async move { blobs.blob_has(hash_hex).await });

    match result {
        Ok(has) => {
            unsafe {
                *out_has = if has { 1 } else { 0 };
            }
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(_) => aster_status_t::ASTER_STATUS_INTERNAL as i32,
    }
}

// ============================================================================
// C FFI Functions - Blob Transfer Observability (Phase 1d)
// ============================================================================

/// Snapshot of the current bitfield for a blob.
/// Fills `out_is_complete` (1=complete, 0=partial/not-found) and `out_size` (total bytes, 0 if unknown).
/// Returns ASTER_STATUS_NOT_FOUND if the node handle is unknown.
/// Returns ASTER_STATUS_INVALID_ARGUMENT if any pointer is null.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_observe_snapshot(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    out_is_complete: *mut u32,
    out_size: *mut u64,
) -> i32 {
    if hash_hex_ptr.is_null() || out_is_complete.is_null() || out_size.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let hash_hex =
        String::from_utf8(unsafe { slice::from_raw_parts(hash_hex_ptr, hash_hex_len).to_vec() })
            .unwrap_or_default();
    let blobs = node_arc.blobs_client();

    let result = bridge
        .runtime
        .block_on(async move { blobs.blob_observe_snapshot(hash_hex).await });

    match result {
        Ok(r) => {
            unsafe {
                *out_is_complete = if r.is_complete { 1 } else { 0 };
                *out_size = r.size;
            }
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(_) => aster_status_t::ASTER_STATUS_INTERNAL as i32,
    }
}

/// Wait until a blob is fully downloaded locally.
/// Emits ASTER_EVENT_BLOB_OBSERVE_COMPLETE (via ASTER_EVENT_UNIT_RESULT) when complete,
/// or an error event if the observation stream ends without completion.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_observe_complete(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if hash_hex_ptr.is_null() || out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let hash_hex =
        String::from_utf8(unsafe { slice::from_raw_parts(hash_hex_ptr, hash_hex_len).to_vec() })
            .unwrap_or_default();

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        let blobs = node_arc.blobs_client();
        match blobs.blob_observe_complete(hash_hex).await {
            Ok(()) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_BLOB_OBSERVE_COMPLETE,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Check local availability of a blob using the Remote API.
/// Fills `out_is_complete` (1=complete, 0=partial) and `out_local_bytes` (bytes we have locally).
/// Returns ASTER_STATUS_NOT_FOUND if the node handle is unknown.
/// Returns ASTER_STATUS_INVALID_ARGUMENT if any pointer is null.
#[no_mangle]
pub unsafe extern "C" fn aster_blobs_local_info(
    runtime: aster_runtime_t,
    node: aster_node_t,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    out_is_complete: *mut u32,
    out_local_bytes: *mut u64,
) -> i32 {
    if hash_hex_ptr.is_null() || out_is_complete.is_null() || out_local_bytes.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let hash_hex =
        String::from_utf8(unsafe { slice::from_raw_parts(hash_hex_ptr, hash_hex_len).to_vec() })
            .unwrap_or_default();
    let blobs = node_arc.blobs_client();

    let result = bridge
        .runtime
        .block_on(async move { blobs.blob_local_info(hash_hex).await });

    match result {
        Ok(r) => {
            unsafe {
                *out_is_complete = if r.is_complete { 1 } else { 0 };
                *out_local_bytes = r.local_bytes;
            }
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Err(_) => aster_status_t::ASTER_STATUS_INTERNAL as i32,
    }
}

// ============================================================================
// C FFI Functions - Tags (Phase 1c)
// ============================================================================

/// Encode a CoreTagInfo as a null-separated UTF-8 payload: name\0hash\0format\0
fn encode_tag_payload(t: &aster_transport_core::CoreTagInfo) -> Vec<u8> {
    let mut buf = Vec::with_capacity(t.name.len() + t.hash.len() + t.format.len() + 3);
    buf.extend_from_slice(t.name.as_bytes());
    buf.push(0);
    buf.extend_from_slice(t.hash.as_bytes());
    buf.push(0);
    buf.extend_from_slice(t.format.as_bytes());
    buf.push(0);
    buf
}

/// Encode a list of CoreTagInfo as concatenated null-separated records.
/// `flags` in the event holds the count.
fn encode_tag_list_payload(tags: &[aster_transport_core::CoreTagInfo]) -> Vec<u8> {
    tags.iter().flat_map(encode_tag_payload).collect()
}

/// Set a named tag. format: 0 = raw, 1 = hash_seq. Emits ASTER_EVENT_TAG_SET.
#[no_mangle]
pub unsafe extern "C" fn aster_tags_set(
    runtime: aster_runtime_t,
    node: aster_node_t,
    name_ptr: *const u8,
    name_len: usize,
    hash_hex_ptr: *const u8,
    hash_hex_len: usize,
    format: u32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() || name_ptr.is_null() || hash_hex_ptr.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let name = String::from_utf8(unsafe { slice::from_raw_parts(name_ptr, name_len).to_vec() })
        .unwrap_or_default();
    let hash_hex =
        String::from_utf8(unsafe { slice::from_raw_parts(hash_hex_ptr, hash_hex_len).to_vec() })
            .unwrap_or_default();
    let format_str = if format == 1 {
        "hash_seq".to_string()
    } else {
        "raw".to_string()
    };

    let blobs = node_arc.blobs_client();
    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }
        match blobs.tag_set(name, hash_hex, format_str).await {
            Ok(()) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_TAG_SET,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Get a tag by name. Emits ASTER_EVENT_TAG_GET with payload on found, NOT_FOUND status if absent.
#[no_mangle]
pub unsafe extern "C" fn aster_tags_get(
    runtime: aster_runtime_t,
    node: aster_node_t,
    name_ptr: *const u8,
    name_len: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() || name_ptr.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let name = String::from_utf8(unsafe { slice::from_raw_parts(name_ptr, name_len).to_vec() })
        .unwrap_or_default();

    let blobs = node_arc.blobs_client();
    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }
        match blobs.tag_get(name).await {
            Ok(Some(tag)) => {
                let payload = encode_tag_payload(&tag);
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_TAG_GET,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, payload);
            }
            Ok(None) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_TAG_GET,
                    aster_status_t::ASTER_STATUS_NOT_FOUND,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Delete a tag by name. Emits ASTER_EVENT_TAG_DELETED with count in event.flags.
#[no_mangle]
pub unsafe extern "C" fn aster_tags_delete(
    runtime: aster_runtime_t,
    node: aster_node_t,
    name_ptr: *const u8,
    name_len: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() || name_ptr.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let name = String::from_utf8(unsafe { slice::from_raw_parts(name_ptr, name_len).to_vec() })
        .unwrap_or_default();

    let blobs = node_arc.blobs_client();
    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }
        match blobs.tag_delete(name).await {
            Ok(count) => {
                let mut event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_TAG_DELETED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                event.flags = count as u32;
                bridge2.complete(event, None);
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// List tags matching a prefix (empty prefix = all tags).
/// Emits ASTER_EVENT_TAG_LIST with packed tag records in payload; event.flags = count.
#[no_mangle]
pub unsafe extern "C" fn aster_tags_list_prefix(
    runtime: aster_runtime_t,
    node: aster_node_t,
    prefix_ptr: *const u8,
    prefix_len: usize,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };
    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let prefix = if prefix_ptr.is_null() || prefix_len == 0 {
        String::new()
    } else {
        String::from_utf8(unsafe { slice::from_raw_parts(prefix_ptr, prefix_len).to_vec() })
            .unwrap_or_default()
    };

    let blobs = node_arc.blobs_client();
    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }
        let result = if prefix.is_empty() {
            blobs.tag_list().await
        } else {
            blobs.tag_list_prefix(prefix).await
        };
        match result {
            Ok(tags) => {
                let count = tags.len() as u32;
                let payload = encode_tag_list_payload(&tags);
                let mut event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_TAG_LIST,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                event.flags = count;
                bridge2.emit_with_data(event, payload);
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Docs (simplified for Phase 1)
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_docs_create(
    runtime: aster_runtime_t,
    node: aster_node_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let docs = node_arc.docs_client();

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match docs.create().await {
            Ok(doc) => {
                let handle = bridge2.docs.insert(doc);
                bridge2.emit_simple_with_handle(
                    aster_event_kind_t::ASTER_EVENT_DOC_CREATED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    handle,
                    node,
                    user_data,
                    0,
                    &bridge2.docs,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_docs_create_author(
    runtime: aster_runtime_t,
    node: aster_node_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let docs = node_arc.docs_client();

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match docs.create_author().await {
            Ok(author_id) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_AUTHOR_CREATED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    node,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, author_id.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_docs_join(
    runtime: aster_runtime_t,
    node: aster_node_t,
    ticket: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let docs = node_arc.docs_client();
    let ticket_str = match unsafe { read_string(&ticket) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match docs.join(ticket_str).await {
            Ok(doc) => {
                let handle = bridge2.docs.insert(doc);
                bridge2.emit_simple_with_handle(
                    aster_event_kind_t::ASTER_EVENT_DOC_JOINED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    handle,
                    node,
                    user_data,
                    0,
                    &bridge2.docs,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_doc_set_bytes(
    runtime: aster_runtime_t,
    doc: u64,
    author_hex: aster_bytes_t,
    key: aster_bytes_t,
    value: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let author = match unsafe { read_string(&author_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let key_bytes = unsafe { read_bytes(&key) };
    let value_bytes = unsafe { read_bytes(&value) };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.set_bytes(author, key_bytes, value_bytes).await {
            Ok(hash) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_DOC_SET,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, hash.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Delete all entries for an author whose key starts with `prefix`.
///
/// Emits ASTER_EVENT_DOC_DEL with an 8-byte little-endian u64 payload containing
/// the number of removed entries.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_del(
    runtime: aster_runtime_t,
    doc: u64,
    author_hex: aster_bytes_t,
    prefix: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let author = match unsafe { read_string(&author_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let prefix_bytes = unsafe { read_bytes(&prefix) };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.del(author, prefix_bytes).await {
            Ok(removed) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_DOC_DEL,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, removed.to_le_bytes().to_vec());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_doc_get_exact(
    runtime: aster_runtime_t,
    doc: u64,
    author_hex: aster_bytes_t,
    key: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let author = match unsafe { read_string(&author_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let key_bytes = unsafe { read_bytes(&key) };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.get_exact(author, key_bytes).await {
            Ok(Some(data)) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_DOC_GET,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Ok(None) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_DOC_GET,
                    aster_status_t::ASTER_STATUS_NOT_FOUND,
                    op_id,
                    doc,
                    0,
                    user_data,
                    aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Share a document. `mode`: one of `aster_doc_share_mode_t`.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_share(
    runtime: aster_runtime_t,
    doc: u64,
    mode: u32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    // `mode` is one of `aster_doc_share_mode_t`.
    let mode_str = match mode {
        x if x == aster_doc_share_mode_t::ASTER_DOC_SHARE_READ as u32 => "read".to_string(),
        x if x == aster_doc_share_mode_t::ASTER_DOC_SHARE_WRITE as u32 => "write".to_string(),
        _ => return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.share(mode_str).await {
            Ok(ticket) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_DOC_SHARED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, ticket.into_bytes());
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Docs: Query (key_exact / key_prefix without author)
// ============================================================================

/// Query mode for doc queries
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_doc_query_mode_t {
    ASTER_DOC_QUERY_KEY_EXACT = 0,
    ASTER_DOC_QUERY_KEY_PREFIX = 1,
    /// Return only the latest entry for the key (across all authors), or nothing if the
    /// latest write was a deletion. Always emits 0 or 1 entries; `limit` is ignored.
    ASTER_DOC_QUERY_LATEST_EXACT = 2,
    /// List the latest entry for each distinct key under the prefix (across all authors),
    /// omitting keys whose latest write was a deletion. `limit` caps the number of keys.
    ASTER_DOC_QUERY_LATEST_PREFIX = 3,
}

/// Query a doc by key.
///
/// `limit` caps the number of entries returned by a prefix (`ASTER_DOC_QUERY_KEY_PREFIX`)
/// or exact (`ASTER_DOC_QUERY_KEY_EXACT`) query; pass `0` for an unbounded result. It is
/// ignored by `ASTER_DOC_QUERY_LATEST_EXACT`, which always returns at most one entry (the
/// latest writer for the key, or none if the latest write was a deletion).
///
/// `mode`: one of `aster_doc_query_mode_t`.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_query(
    runtime: aster_runtime_t,
    doc: u64,
    mode: u32,
    key: aster_bytes_t,
    limit: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let key_bytes = unsafe { read_bytes(&key) };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        let limit = if limit == 0 { None } else { Some(limit) };
        let result = if mode == aster_doc_query_mode_t::ASTER_DOC_QUERY_KEY_PREFIX as u32 {
            doc_arc.query_key_prefix(key_bytes, limit).await
        } else if mode == aster_doc_query_mode_t::ASTER_DOC_QUERY_LATEST_PREFIX as u32 {
            doc_arc.query_latest_prefix(key_bytes, limit).await
        } else if mode == aster_doc_query_mode_t::ASTER_DOC_QUERY_LATEST_EXACT as u32 {
            doc_arc
                .query_latest_exact(key_bytes)
                .await
                .map(|opt| opt.into_iter().collect::<Vec<_>>())
        } else {
            doc_arc.query_key_exact(key_bytes, limit).await
        };

        match result {
            Ok(entries) => {
                // Serialize entries into a packed binary payload
                let mut payload = Vec::new();
                let entry_count = entries.len() as u32;

                for entry in &entries {
                    // author_id
                    let author_bytes = entry.author_id.as_bytes();
                    payload.extend_from_slice(&(author_bytes.len() as u32).to_le_bytes());
                    payload.extend_from_slice(author_bytes);
                    // key
                    payload.extend_from_slice(&(entry.key.len() as u32).to_le_bytes());
                    payload.extend_from_slice(&entry.key);
                    // content_hash
                    let hash_bytes = entry.content_hash.as_bytes();
                    payload.extend_from_slice(&(hash_bytes.len() as u32).to_le_bytes());
                    payload.extend_from_slice(hash_bytes);
                    // content_len
                    payload.extend_from_slice(&entry.content_len.to_le_bytes());
                    // timestamp
                    payload.extend_from_slice(&entry.timestamp.to_le_bytes());
                }

                let mut event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_DOC_QUERY,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                event.flags = entry_count;
                bridge2.emit_with_data(event, payload);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Read the content bytes for a doc entry given its content hash.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_read_entry_content(
    runtime: aster_runtime_t,
    doc: u64,
    content_hash_hex: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let hash_hex = match unsafe { read_string(&content_hash_hex) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.read_entry_content(hash_hex).await {
            Ok(data) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_BLOB_READ,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, data);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Doc Sync Lifecycle (Phase 1c.5)
// ============================================================================

/// Start syncing a document with the specified peers (endpoint-ID hex strings).
/// Emits ASTER_EVENT_UNIT_RESULT on completion.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_start_sync(
    runtime: aster_runtime_t,
    doc: u64,
    peers: aster_bytes_list_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let peer_strs = unsafe { read_string_list(&peers) };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.start_sync(peer_strs).await {
            Ok(()) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Leave (stop syncing) a document.
/// Emits ASTER_EVENT_UNIT_RESULT on completion.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_leave(
    runtime: aster_runtime_t,
    doc: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.leave().await {
            Ok(()) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Doc Subscribe (Phase 1c.4)
// ============================================================================

/// Encode a CoreDocEntry into the packed binary format used by DOC_QUERY payloads.
fn encode_entry_payload(entry: &aster_transport_core::CoreDocEntry) -> Vec<u8> {
    let mut payload = Vec::new();
    let author_bytes = entry.author_id.as_bytes();
    payload.extend_from_slice(&(author_bytes.len() as u32).to_le_bytes());
    payload.extend_from_slice(author_bytes);
    payload.extend_from_slice(&(entry.key.len() as u32).to_le_bytes());
    payload.extend_from_slice(&entry.key);
    let hash_bytes = entry.content_hash.as_bytes();
    payload.extend_from_slice(&(hash_bytes.len() as u32).to_le_bytes());
    payload.extend_from_slice(hash_bytes);
    payload.extend_from_slice(&entry.content_len.to_le_bytes());
    payload.extend_from_slice(&entry.timestamp.to_le_bytes());
    payload
}

/// Encode a CoreDocEvent into (subtype: u32, payload: Vec<u8>).
///
/// Subtype: 0=InsertLocal, 1=InsertRemote, 2=ContentReady, 3=PendingContentReady,
///          4=NeighborUp, 5=NeighborDown, 6=SyncFinished
fn encode_doc_event(ev: &aster_transport_core::CoreDocEvent) -> (u32, Vec<u8>) {
    use aster_transport_core::CoreDocEvent::*;
    match ev {
        InsertLocal { entry } => (0, encode_entry_payload(entry)),
        InsertRemote { from, entry } => {
            let mut payload = encode_entry_payload(entry);
            let from_bytes = from.as_bytes();
            payload.extend_from_slice(&(from_bytes.len() as u32).to_le_bytes());
            payload.extend_from_slice(from_bytes);
            (1, payload)
        }
        ContentReady { hash } => (2, hash.as_bytes().to_vec()),
        PendingContentReady => (3, Vec::new()),
        NeighborUp { peer } => (4, peer.as_bytes().to_vec()),
        NeighborDown { peer } => (5, peer.as_bytes().to_vec()),
        SyncFinished {
            peer,
            origin,
            result,
        } => {
            let peer = peer.as_bytes();
            let error = result.as_ref().err().map_or(&[][..], |e| e.as_bytes());
            let mut payload = Vec::with_capacity(10 + peer.len() + error.len());
            payload.extend_from_slice(&(peer.len() as u32).to_le_bytes());
            payload.extend_from_slice(peer);
            payload.push(match origin {
                aster_transport_core::CoreSyncOrigin::Accept => 0,
                aster_transport_core::CoreSyncOrigin::Connect => 1,
            });
            payload.push(u8::from(result.is_ok()));
            payload.extend_from_slice(&(error.len() as u32).to_le_bytes());
            payload.extend_from_slice(error);
            (6, payload)
        }
    }
}

/// Subscribe to live document events.
/// Emits ASTER_EVENT_DOC_SUBSCRIBED (status=OK, handle=receiver_handle) on success.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_subscribe(
    runtime: aster_runtime_t,
    doc: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.subscribe().await {
            Ok(receiver) => {
                let handle = bridge2
                    .doc_event_receivers
                    .insert(DocEventReceiver::new(receiver));
                bridge2.emit_simple_with_handle(
                    aster_event_kind_t::ASTER_EVENT_DOC_SUBSCRIBED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    handle,
                    doc,
                    user_data,
                    0,
                    &bridge2.doc_event_receivers,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// A receiver owns its stream independently from the Doc handle. The lock both
/// serializes reads and keeps an event consumed just before cancellation wins
/// the completion race, so the next receive observes that same event.
struct DocEventReceiver {
    inner: CoreDocEventReceiver,
    pending: tokio::sync::Mutex<Option<CoreDocEvent>>,
    closed: AtomicBool,
    close_notify: tokio::sync::Notify,
}

impl DocEventReceiver {
    fn new(inner: CoreDocEventReceiver) -> Self {
        Self {
            inner,
            pending: tokio::sync::Mutex::new(None),
            closed: AtomicBool::new(false),
            close_notify: tokio::sync::Notify::new(),
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.close_notify.notify_waiters();
    }

    /// Called while holding `pending`, so a later receive cannot overtake an
    /// event being returned to the stream after cancellation won.
    fn deliver(
        &self,
        bridge: &BridgeRuntime,
        operation: u64,
        receiver: u64,
        user_data: u64,
        value: CoreDocEvent,
        pending: &mut Option<CoreDocEvent>,
    ) {
        let (subtype, payload) = encode_doc_event(&value);
        let mut event = EventInternal::new(
            aster_event_kind_t::ASTER_EVENT_DOC_EVENT,
            aster_status_t::ASTER_STATUS_OK,
            operation,
            receiver,
            0,
            user_data,
            subtype as i32,
        );
        event.flags = subtype;
        if !bridge.complete(event, Some(payload)) && !self.closed.load(Ordering::Acquire) {
            *pending = Some(value);
        }
    }
}

fn close_doc_event_receiver(bridge: &BridgeRuntime, receiver: u64) -> bool {
    if let Some(receiver) = bridge.doc_event_receivers.remove(receiver) {
        receiver.close();
        true
    } else {
        false
    }
}

/// Release a document event receiver independently from its document handle.
/// Removes the handle and wakes pending receives, which complete with
/// ASTER_EVENT_DOC_EVENT / ASTER_STATUS_NOT_FOUND (end of stream). An in-flight
/// receive racing close may complete successfully. Repeated
/// release, or receiving through a released handle, returns NOT_FOUND.
/// Runtime shutdown also releases receivers and wakes pending receives.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_event_receiver_free(
    runtime: aster_runtime_t,
    receiver: u64,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(bridge) => bridge,
        Err(status) => return status as i32,
    };
    if close_doc_event_receiver(&bridge, receiver) {
        aster_status_t::ASTER_STATUS_OK as i32
    } else {
        aster_status_t::ASTER_STATUS_NOT_FOUND as i32
    }
}

/// Receive the next live document event. Subtype is carried in event.flags:
/// 0=InsertLocal, 1=InsertRemote, 2=ContentReady, 3=PendingContentReady,
/// 4=NeighborUp, 5=NeighborDown, 6=SyncFinished. Entry events use the DOC_QUERY
/// metadata layout; InsertRemote appends a length-prefixed peer ID. ContentReady
/// carries the hash string; neighbor events carry the peer string.
/// SyncFinished carries a u32-length-prefixed peer ID, a u8 origin (0=Accept,
/// 1=Connect), a u8 ok flag (1=completed, 0=failed) and a u32-length-prefixed
/// UTF-8 error message (empty when ok). PendingContentReady has an empty payload.
///
/// Receives on one receiver are serialized. Cancelling an operation wakes its
/// wait without consuming a later event; if cancellation wins after a stream
/// event was consumed, that event is retained for the next receive. EOF and
/// errors release the receiver handle automatically. EOF or receiver close
/// emits ASTER_EVENT_DOC_EVENT / NOT_FOUND; cancellation emits the usual
/// ASTER_EVENT_OPERATION_CANCELLED. Release every returned payload buffer.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_event_recv(
    runtime: aster_runtime_t,
    receiver: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let bridge = match load_runtime(runtime) {
        Ok(bridge) => bridge,
        Err(status) => return status as i32,
    };
    let receiver_arc = match bridge.doc_event_receivers.get(receiver) {
        Some(receiver) => receiver,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };
    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    // Fetch this before publishing the operation ID to the caller.
    let cancel_notify = match bridge.operations.get(op_id) {
        Some(operation) => operation.cancel_notify.clone(),
        // Runtime shutdown can clear the operation registry immediately
        // after admission, before its ID has been published to the caller.
        None => return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32,
    };
    unsafe {
        *out_operation = op_id;
    }
    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        let cancelled_wait = cancel_notify.notified();
        let closed_wait = receiver_arc.close_notify.notified();
        let runtime_closed = bridge2.close_notify.notified();
        tokio::pin!(cancelled_wait, closed_wait, runtime_closed);
        // Notify::notify_waiters does not retain permits. Register before
        // testing the corresponding atomics to cover close/cancel-before-poll.
        cancelled_wait.as_mut().enable();
        closed_wait.as_mut().enable();
        runtime_closed.as_mut().enable();
        if cancelled.load(Ordering::Acquire) || bridge2.closing.load(Ordering::Acquire) {
            return;
        }
        let emit_eof = || {
            close_doc_event_receiver(&bridge2, receiver);
            bridge2.emit_simple(
                aster_event_kind_t::ASTER_EVENT_DOC_EVENT,
                aster_status_t::ASTER_STATUS_NOT_FOUND,
                op_id,
                receiver,
                0,
                user_data,
                0,
            );
        };
        if receiver_arc.closed.load(Ordering::Acquire) {
            emit_eof();
            return;
        }
        let mut pending = tokio::select! {
            biased;
            _ = &mut cancelled_wait => return,
            _ = &mut runtime_closed => return,
            _ = &mut closed_wait => { emit_eof(); return; }
            pending = receiver_arc.pending.lock() => pending,
        };
        if cancelled.load(Ordering::Acquire) || bridge2.closing.load(Ordering::Acquire) {
            return;
        }
        if receiver_arc.closed.load(Ordering::Acquire) {
            emit_eof();
            return;
        }
        let result = if let Some(value) = pending.take() {
            Ok(Some(value))
        } else {
            tokio::select! {
                biased;
                _ = &mut cancelled_wait => return,
                _ = &mut runtime_closed => return,
                _ = &mut closed_wait => { emit_eof(); return; }
                result = receiver_arc.inner.recv() => result,
            }
        };
        match result {
            Ok(Some(value)) => {
                receiver_arc.deliver(&bridge2, op_id, receiver, user_data, value, &mut pending)
            }
            Ok(None) => emit_eof(),
            Err(error) => {
                close_doc_event_receiver(&bridge2, receiver);
                bridge2.emit_error(op_id, user_data, &error.to_string());
            }
        }
    });
    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Docs: Download Policy, Share with Addr, Join+Subscribe (Phase 1c.6-1c.8)
// ============================================================================

/// Download policy mode for aster_doc_set_download_policy.
/// 0 = everything (download all entries, no prefixes needed)
/// 1 = nothing_except (only download entries matching the given prefixes)
/// 2 = everything_except (download all except entries matching the given prefixes)
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum aster_download_policy_mode_t {
    ASTER_DOWNLOAD_POLICY_EVERYTHING = 0,
    ASTER_DOWNLOAD_POLICY_NOTHING_EXCEPT = 1,
    ASTER_DOWNLOAD_POLICY_EVERYTHING_EXCEPT = 2,
}

/// Set the download policy for a document.
/// `mode`: one of `aster_download_policy_mode_t`.
/// prefixes: list of byte-string prefixes (used for NOTHING_EXCEPT / EVERYTHING_EXCEPT modes).
/// Emits ASTER_EVENT_UNIT_RESULT on success.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_set_download_policy(
    runtime: aster_runtime_t,
    doc: u64,
    mode: u32,
    prefixes: aster_bytes_list_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let prefix_list: Vec<Vec<u8>> = {
        let n = prefixes.len;
        if n == 0 || prefixes.items.is_null() {
            vec![]
        } else {
            let slice = unsafe { std::slice::from_raw_parts(prefixes.items, n) };
            slice.iter().map(|b| unsafe { read_bytes(b) }).collect()
        }
    };

    let policy = match mode {
        0 => aster_transport_core::CoreDownloadPolicy::Everything,
        1 => aster_transport_core::CoreDownloadPolicy::NothingExcept {
            prefixes: prefix_list,
        },
        2 => aster_transport_core::CoreDownloadPolicy::EverythingExcept {
            prefixes: prefix_list,
        },
        _ => return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.set_download_policy(policy).await {
            Ok(()) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_UNIT_RESULT,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Share a document with full relay+address info (AddrInfoOptions::RelayAndAddresses).
/// `mode`: one of `aster_doc_share_mode_t`.
/// Emits ASTER_EVENT_DOC_SHARED with the ticket string in the payload.
#[no_mangle]
pub unsafe extern "C" fn aster_doc_share_with_addr(
    runtime: aster_runtime_t,
    doc: u64,
    mode: u32,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    // `mode` is one of `aster_doc_share_mode_t`.
    let mode_str = match mode {
        x if x == aster_doc_share_mode_t::ASTER_DOC_SHARE_READ as u32 => "read".to_string(),
        x if x == aster_doc_share_mode_t::ASTER_DOC_SHARE_WRITE as u32 => "write".to_string(),
        _ => return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match doc_arc.share_with_addr(mode_str).await {
            Ok(ticket) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_DOC_SHARED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, ticket.into_bytes());
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Join a document and subscribe to live events atomically.
/// Emits ASTER_EVENT_DOC_JOINED_AND_SUBSCRIBED:
///   event.handle = doc handle
///   event.related = event-receiver handle (64-bit; not event.flags)
#[no_mangle]
pub unsafe extern "C" fn aster_docs_join_and_subscribe(
    runtime: aster_runtime_t,
    node: aster_node_t,
    ticket: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let docs = node_arc.docs_client();
    let ticket_str = match unsafe { read_string(&ticket) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match docs.join_and_subscribe(ticket_str).await {
            Ok((doc, receiver)) => {
                let doc_handle = bridge2.docs.insert(doc);
                let receiver_handle = bridge2
                    .doc_event_receivers
                    .insert(DocEventReceiver::new(receiver));
                // event.handle = doc_handle, event.related = receiver_handle
                bridge2.emit_simple_with_handles(
                    aster_event_kind_t::ASTER_EVENT_DOC_JOINED_AND_SUBSCRIBED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc_handle,
                    receiver_handle,
                    user_data,
                    0,
                    &bridge2.docs,
                    &bridge2.doc_event_receivers,
                );
            }
            Err(e) => bridge2.emit_error(op_id, user_data, &e.to_string()),
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Gossip (simplified for Phase 1)
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_gossip_subscribe(
    runtime: aster_runtime_t,
    node: aster_node_t,
    topic: aster_bytes_t,
    peers: aster_bytes_list_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let node_arc = match bridge.nodes.get(node) {
        Some(n) => n,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let gossip = node_arc.gossip_client();
    let topic_bytes = unsafe { read_bytes(&topic) };
    let peers_strs = unsafe { read_string_list(&peers) };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match gossip.subscribe(topic_bytes, peers_strs).await {
            Ok(topic_handle) => {
                let handle = bridge2.gossip_topics.insert(topic_handle);
                bridge2.emit_simple_with_handle(
                    aster_event_kind_t::ASTER_EVENT_GOSSIP_SUBSCRIBED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    handle,
                    node,
                    user_data,
                    0,
                    &bridge2.gossip_topics,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_gossip_broadcast(
    runtime: aster_runtime_t,
    topic: u64,
    data: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let topic_arc = match bridge.gossip_topics.get(topic) {
        Some(t) => t,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let data_bytes = unsafe { read_bytes(&data) };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match topic_arc.broadcast(data_bytes).await {
            Ok(()) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_GOSSIP_BROADCAST_DONE,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    topic,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_gossip_recv(
    runtime: aster_runtime_t,
    topic: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let topic_arc = match bridge.gossip_topics.get(topic) {
        Some(t) => t,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match topic_arc.recv().await {
            Ok(event) => {
                let event_bytes = event.data.unwrap_or_default();
                let internal_event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_GOSSIP_RECEIVED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    topic,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(internal_event, event_bytes);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// Phase 1b: Datagram Completion FFI Functions
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn aster_connection_max_datagram_size(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    out_size: *mut u64,
    out_is_some: *mut u32,
) -> i32 {
    if out_size.is_null() || out_is_some.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    match conn_arc.max_datagram_size() {
        Some(size) => unsafe {
            *out_size = size as u64;
            *out_is_some = 1;
        },
        None => unsafe {
            *out_size = 0;
            *out_is_some = 0;
        },
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

#[no_mangle]
pub unsafe extern "C" fn aster_connection_datagram_send_buffer_space(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    out_bytes: *mut u64,
) -> i32 {
    if out_bytes.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let space = conn_arc.datagram_send_buffer_space();
    unsafe {
        *out_bytes = space as u64;
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// Phase 1b: Remote-Info FFI Functions
// ============================================================================

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_remote_info_t {
    pub struct_size: u32,
    pub node_id: aster_bytes_t,
    pub is_connected: u32,
    /// One of `aster_connection_type_t`.
    pub connection_type: u32,
    pub relay_url: aster_bytes_t,
    pub last_handshake_ns: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_connection_info_t {
    pub struct_size: u32,
    /// One of `aster_connection_type_t`.
    pub connection_type: u32,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub rtt_ns: u64,
    pub alpn: aster_bytes_t,
    pub is_connected: u32,
}

/// Snapshot of the currently selected QUIC path for a connection.
/// `path_kind` discriminates the populated fields:
///   0 = no path selected (connection dropped or not yet negotiated)
///   1 = direct UDP — `peer_host` and `peer_port` are populated
///   2 = relay     — `relay_url` is populated
/// `rtt_micros` is `UINT64_MAX` when RTT has not yet been measured.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_transport_snapshot_t {
    pub struct_size: u32,
    /// One of `aster_path_kind_t`.
    pub path_kind: u32,
    pub peer_host: aster_bytes_t,
    pub peer_port: u16,
    pub _pad: u16,
    pub relay_url: aster_bytes_t,
    pub rtt_micros: u64,
}

/// On success the caller owns `out_info.node_id` and `out_info.relay_url`
/// and must free each with `aster_string_release(ptr, len)`.
#[no_mangle]
pub unsafe extern "C" fn aster_endpoint_remote_info(
    runtime: aster_runtime_t,
    endpoint_or_node: u64,
    node_id: aster_bytes_t,
    out_info: *mut aster_remote_info_t,
) -> i32 {
    if out_info.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let ep_arc = bridge
        .nodes
        .get(endpoint_or_node)
        .map(|n| Arc::new(n.net_client()));

    let ep = match ep_arc {
        Some(e) => e,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let node_id_str = match unsafe { read_string(&node_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let info = match ep.remote_info(&node_id_str) {
        Some(info) => info,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    // Map connection type
    let conn_type = match info.connection_type {
        aster_transport_core::ConnectionType::NotConnected => {
            aster_connection_type_t::ASTER_CONNECTION_TYPE_NOT_CONNECTED as u32
        }
        aster_transport_core::ConnectionType::Connecting => {
            aster_connection_type_t::ASTER_CONNECTION_TYPE_CONNECTING as u32
        }
        aster_transport_core::ConnectionType::Connected(detail) => match detail {
            aster_transport_core::ConnectionTypeDetail::UdpDirect => {
                aster_connection_type_t::ASTER_CONNECTION_TYPE_UDP_DIRECT as u32
            }
            aster_transport_core::ConnectionTypeDetail::UdpRelay => {
                aster_connection_type_t::ASTER_CONNECTION_TYPE_UDP_RELAY as u32
            }
            aster_transport_core::ConnectionTypeDetail::Other(_) => {
                aster_connection_type_t::ASTER_CONNECTION_TYPE_OTHER as u32
            }
        },
    };

    // Get relay_url offset (we'll use placeholder since this is a single call)
    let relay_url_str = info.relay_url.unwrap_or_default();
    let relay_url_bytes = alloc_string(relay_url_str);

    // Pack node_id into buffer (simplified - caller should use separate buffer)
    let node_id_bytes = alloc_string(info.node_id);

    unsafe {
        *out_info = aster_remote_info_t {
            struct_size: std::mem::size_of::<aster_remote_info_t>() as u32,
            node_id: node_id_bytes,
            is_connected: if info.is_connected { 1 } else { 0 },
            connection_type: conn_type,
            relay_url: relay_url_bytes,
            last_handshake_ns: info.last_handshake_ns.unwrap_or(0),
            bytes_sent: info.bytes_sent,
            bytes_received: info.bytes_received,
        };
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

/// On success the caller owns `node_id` and `relay_url` of each of the first
/// `*out_count` entries and must free each with `aster_string_release(ptr, len)`.
#[no_mangle]
pub unsafe extern "C" fn aster_endpoint_remote_info_list(
    runtime: aster_runtime_t,
    endpoint_or_node: u64,
    out_infos: *mut aster_remote_info_t,
    max_infos: usize,
    out_count: *mut usize,
) -> i32 {
    if out_infos.is_null() || out_count.is_null() || max_infos == 0 {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let ep_arc = bridge
        .nodes
        .get(endpoint_or_node)
        .map(|n| Arc::new(n.net_client()));

    let ep = match ep_arc {
        Some(e) => e,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let infos = ep.remote_info_iter();
    let count = infos.len().min(max_infos);

    // Note: In a full implementation, we'd need to handle buffer allocation properly
    // For now, we return the count and let the caller allocate buffers
    unsafe {
        *out_count = count;
    }

    // Pack first 'count' items
    for (i, info) in infos.iter().take(count).enumerate() {
        let conn_type = match &info.connection_type {
            aster_transport_core::ConnectionType::NotConnected => {
                aster_connection_type_t::ASTER_CONNECTION_TYPE_NOT_CONNECTED as u32
            }
            aster_transport_core::ConnectionType::Connecting => {
                aster_connection_type_t::ASTER_CONNECTION_TYPE_CONNECTING as u32
            }
            aster_transport_core::ConnectionType::Connected(detail) => match detail {
                aster_transport_core::ConnectionTypeDetail::UdpDirect => {
                    aster_connection_type_t::ASTER_CONNECTION_TYPE_UDP_DIRECT as u32
                }
                aster_transport_core::ConnectionTypeDetail::UdpRelay => {
                    aster_connection_type_t::ASTER_CONNECTION_TYPE_UDP_RELAY as u32
                }
                aster_transport_core::ConnectionTypeDetail::Other(_) => {
                    aster_connection_type_t::ASTER_CONNECTION_TYPE_OTHER as u32
                }
            },
        };

        let relay_url_str = info.relay_url.clone().unwrap_or_default();
        let node_id_str = info.node_id.clone();

        unsafe {
            *out_infos.add(i) = aster_remote_info_t {
                struct_size: std::mem::size_of::<aster_remote_info_t>() as u32,
                node_id: alloc_string(node_id_str),
                is_connected: if info.is_connected { 1 } else { 0 },
                connection_type: conn_type,
                relay_url: alloc_string(relay_url_str),
                last_handshake_ns: info.last_handshake_ns.unwrap_or(0),
                bytes_sent: info.bytes_sent,
                bytes_received: info.bytes_received,
            };
        }
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

/// On success the caller owns `out_info.alpn` and must free it with
/// `aster_string_release(ptr, len)`.
#[no_mangle]
pub unsafe extern "C" fn aster_connection_info(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    out_info: *mut aster_connection_info_t,
) -> i32 {
    if out_info.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let info = conn_arc.connection_info();

    let conn_type = match info.connection_type {
        aster_transport_core::ConnectionTypeDetail::UdpDirect => {
            aster_connection_type_t::ASTER_CONNECTION_TYPE_UDP_DIRECT as u32
        }
        aster_transport_core::ConnectionTypeDetail::UdpRelay => {
            aster_connection_type_t::ASTER_CONNECTION_TYPE_UDP_RELAY as u32
        }
        aster_transport_core::ConnectionTypeDetail::Other(_) => {
            aster_connection_type_t::ASTER_CONNECTION_TYPE_OTHER as u32
        }
    };

    let alpn_bytes = alloc_bytes(info.alpn);

    unsafe {
        *out_info = aster_connection_info_t {
            struct_size: std::mem::size_of::<aster_connection_info_t>() as u32,
            connection_type: conn_type,
            bytes_sent: info.bytes_sent,
            bytes_received: info.bytes_received,
            rtt_ns: info.rtt_ns.unwrap_or(0),
            alpn: alpn_bytes,
            is_connected: if info.is_connected { 1 } else { 0 },
        };
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Snapshot the currently selected QUIC path for `connection`. See
/// `aster_transport_snapshot_t` for the discriminator semantics. The
/// caller must free `peer_host` and `relay_url` via `aster_string_release(ptr, len)`
/// (empty buffers — `len == 0` — are safe no-ops).
#[no_mangle]
pub unsafe extern "C" fn aster_connection_transport_snapshot(
    runtime: aster_runtime_t,
    connection: aster_connection_t,
    out_snapshot: *mut aster_transport_snapshot_t,
) -> i32 {
    if out_snapshot.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let conn_arc = match bridge.connections.get(connection) {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let snap = conn_arc.transport_snapshot();

    let empty = aster_bytes_t {
        ptr: std::ptr::null(),
        len: 0,
    };
    let (path_kind, peer_host, peer_port, relay_url) = match (snap.peer_addr, snap.relay_url) {
        (Some(addr), _) => (
            aster_path_kind_t::ASTER_PATH_KIND_DIRECT as u32,
            alloc_string(addr.ip().to_string()),
            addr.port(),
            empty,
        ),
        (None, Some(url)) => (
            aster_path_kind_t::ASTER_PATH_KIND_RELAY as u32,
            empty,
            0u16,
            alloc_string(url),
        ),
        (None, None) => (
            aster_path_kind_t::ASTER_PATH_KIND_NONE as u32,
            empty,
            0u16,
            empty,
        ),
    };

    unsafe {
        *out_snapshot = aster_transport_snapshot_t {
            struct_size: std::mem::size_of::<aster_transport_snapshot_t>() as u32,
            path_kind,
            peer_host,
            peer_port,
            _pad: 0,
            relay_url,
            rtt_micros: snap.rtt_micros.unwrap_or(u64::MAX),
        };
    }

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// C FFI Functions - Hook Replies (Phase 1b)
// ============================================================================

/// Respond to a pending before_connect hook invocation.
///
/// `decision` is ASTER_HOOK_DECISION_ALLOW (0) or ASTER_HOOK_DECISION_DENY (1).
/// Calling this consumes the invocation; a second call returns NOT_FOUND.
#[no_mangle]
pub unsafe extern "C" fn aster_hook_before_connect_respond(
    runtime: aster_runtime_t,
    invocation: aster_hook_invocation_t,
    decision: u32,
) -> i32 {
    // `decision` is one of `aster_hook_decision_t`; taken as a raw integer so
    // an out-of-range value from C is rejected instead of being UB.
    let allow = match decision {
        x if x == aster_hook_decision_t::ASTER_HOOK_DECISION_ALLOW as u32 => true,
        x if x == aster_hook_decision_t::ASTER_HOOK_DECISION_DENY as u32 => false,
        _ => return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32,
    };
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let state = match bridge.hook_invocations.remove(invocation) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let sender = match state.sender.lock().ok().and_then(|mut g| g.take()) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    match sender {
        HookSender::BeforeConnect(tx) => {
            // Ignore send error — the hook may have timed out already.
            let _ = tx.send(allow);
            aster_status_t::ASTER_STATUS_OK as i32
        }
        HookSender::AfterConnect(_) => {
            // Wrong invocation type.
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
        }
    }
}

/// Respond to a pending after_connect hook invocation (always accepts).
/// `aster_admission_respond` can also reject with a code and reason.
///
/// Calling this consumes the invocation; a second call returns NOT_FOUND.
#[no_mangle]
pub unsafe extern "C" fn aster_hook_after_connect_respond(
    runtime: aster_runtime_t,
    invocation: aster_hook_invocation_t,
) -> i32 {
    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let state = match bridge.hook_invocations.remove(invocation) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let sender = match state.sender.lock().ok().and_then(|mut g| g.take()) {
        Some(s) => s,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    match sender {
        HookSender::AfterConnect(tx) => {
            let _ = tx.send(CoreAfterHandshakeDecision::Accept);
            aster_status_t::ASTER_STATUS_OK as i32
        }
        HookSender::BeforeConnect(_) => {
            // Wrong invocation type.
            aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
        }
    }
}

// ============================================================================
// Cross-language contract identity, framing & signing
// ============================================================================
//
// Synchronous functions. No runtime handle required. No event queue.
// Pure computation: data in → result out → status code returned.

/// Helper: write `src` into caller-provided buffer `(out_buf, out_len)`.
///
/// On success sets `*out_len` to the number of bytes written and returns OK.
/// If the buffer is too small, sets `*out_len` to the required size and
/// returns BUFFER_TOO_SMALL.
///
/// Returns INVALID_ARGUMENT when any pointer is null.
unsafe fn write_to_caller_buf(src: &[u8], out_buf: *mut u8, out_len: *mut usize) -> aster_status_t {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT;
    }
    let capacity = *out_len;
    if src.len() > capacity || out_buf.is_null() {
        *out_len = src.len();
        return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(src.as_ptr(), out_buf, src.len());
    *out_len = src.len();
    aster_status_t::ASTER_STATUS_OK
}

// ============================================================================
// Phase 1g: Transport Metrics FFI
// ============================================================================

/// Transport-layer metrics snapshot from the iroh endpoint.
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct aster_transport_metrics_t {
    pub struct_size: u32,
    pub send_ipv4: u64,
    pub send_ipv6: u64,
    pub send_relay: u64,
    pub recv_data_ipv4: u64,
    pub recv_data_ipv6: u64,
    pub recv_data_relay: u64,
    pub recv_datagrams: u64,
    pub num_conns_direct: u64,
    pub num_conns_opened: u64,
    pub num_conns_closed: u64,
    pub paths_direct: u64,
    pub paths_relay: u64,
    pub holepunch_attempts: u64,
    pub relay_home_change: u64,
    pub net_reports: u64,
    pub net_reports_full: u64,
}

/// Snapshot current transport metrics from the iroh endpoint.
///
/// `endpoint_or_node` may be either an endpoint handle or a node handle.
/// Writes the metrics into `out_metrics`.
#[no_mangle]
pub unsafe extern "C" fn aster_endpoint_transport_metrics(
    runtime: aster_runtime_t,
    endpoint_or_node: u64,
    out_metrics: *mut aster_transport_metrics_t,
) -> i32 {
    if out_metrics.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let ep_arc = bridge
        .nodes
        .get(endpoint_or_node)
        .map(|n| Arc::new(n.net_client()));

    let client = match ep_arc {
        Some(c) => c,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let m = client.transport_metrics();
    let out = &mut *out_metrics;
    out.struct_size = std::mem::size_of::<aster_transport_metrics_t>() as u32;
    out.send_ipv4 = m.send_ipv4;
    out.send_ipv6 = m.send_ipv6;
    out.send_relay = m.send_relay;
    out.recv_data_ipv4 = m.recv_data_ipv4;
    out.recv_data_ipv6 = m.recv_data_ipv6;
    out.recv_data_relay = m.recv_data_relay;
    out.recv_datagrams = m.recv_datagrams;
    out.num_conns_direct = m.num_conns_direct;
    out.num_conns_opened = m.num_conns_opened;
    out.num_conns_closed = m.num_conns_closed;
    out.paths_direct = m.paths_direct;
    out.paths_relay = m.paths_relay;
    out.holepunch_attempts = m.holepunch_attempts;
    out.relay_home_change = m.relay_home_change;
    out.net_reports = m.net_reports;
    out.net_reports_full = m.net_reports_full;

    aster_status_t::ASTER_STATUS_OK as i32
}

// ============================================================================
// Phase 1f: Cross-Language Contract Identity, Framing & Signing
// ============================================================================

/// Compute contract_id from a ServiceContract JSON string.
/// Writes 64-byte hex string (no null terminator) to out_buf.
/// On BUFFER_TOO_SMALL, sets `*out_len` to required size.
#[no_mangle]
pub unsafe extern "C" fn aster_contract_id(
    json_ptr: *const u8,
    json_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if json_ptr.is_null() || out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let json_bytes = slice::from_raw_parts(json_ptr, json_len);
    let json_str = match std::str::from_utf8(json_bytes) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("invalid UTF-8 in JSON input: {e}")),
    };
    match aster_transport_core::contract::compute_contract_id_from_json(json_str) {
        Ok(hex_id) => write_to_caller_buf(hex_id.as_bytes(), out_buf, out_len) as i32,
        Err(e) => set_last_error(e),
    }
}

/// Compute canonical bytes for a named type from JSON.
/// `type_name`: `"ServiceContract"`, `"TypeDef"`, or `"MethodDef"`.
#[no_mangle]
pub unsafe extern "C" fn aster_canonical_bytes(
    type_name_ptr: *const u8,
    type_name_len: usize,
    json_ptr: *const u8,
    json_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if type_name_ptr.is_null() || json_ptr.is_null() || out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let type_name_bytes = slice::from_raw_parts(type_name_ptr, type_name_len);
    let type_name = match std::str::from_utf8(type_name_bytes) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("invalid UTF-8 in type_name: {e}")),
    };
    let json_bytes = slice::from_raw_parts(json_ptr, json_len);
    let json_str = match std::str::from_utf8(json_bytes) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("invalid UTF-8 in JSON input: {e}")),
    };
    match aster_transport_core::contract::canonical_bytes_from_json(type_name, json_str) {
        Ok(bytes) => write_to_caller_buf(&bytes, out_buf, out_len) as i32,
        Err(e) => set_last_error(e),
    }
}

/// BLAKE3 hash of arbitrary bytes → 64-char lowercase hex.
///
/// General-purpose primitive for bindings that need to hash canonical output from
/// [`aster_canonical_bytes`] (e.g. per-TypeDef hashing when assembling a
/// [`ServiceContract`](aster_transport_core::contract::ServiceContract) for
/// [`aster_contract_id`]). Mirrors what PyO3 already exposes to Python as
/// `_native.contract.compute_type_hash`.
///
/// Writes 64 bytes of hex to `out_buf`. On `BUFFER_TOO_SMALL`, sets `*out_len` to
/// the required size (64). `bytes_len == 0` is valid and hashes the empty input.
#[no_mangle]
pub unsafe extern "C" fn aster_blake3_hex(
    bytes_ptr: *const u8,
    bytes_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    if bytes_ptr.is_null() && bytes_len != 0 {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let input = if bytes_len == 0 {
        &[][..]
    } else {
        slice::from_raw_parts(bytes_ptr, bytes_len)
    };
    let hex = hex::encode(aster_transport_core::contract::compute_type_hash(input));
    write_to_caller_buf(hex.as_bytes(), out_buf, out_len) as i32
}

/// Compute canonical signing bytes from credential JSON.
///
/// The JSON must contain a `"kind"` field (`"producer"` or `"consumer"`)
/// to dispatch to the correct signing bytes format.
#[no_mangle]
pub unsafe extern "C" fn aster_signing_bytes(
    json_ptr: *const u8,
    json_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if json_ptr.is_null() || out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let json_bytes = slice::from_raw_parts(json_ptr, json_len);
    let json_str = match std::str::from_utf8(json_bytes) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("invalid UTF-8 in JSON input: {e}")),
    };
    match aster_transport_core::signing::canonical_signing_bytes_from_json(json_str) {
        Ok(bytes) => write_to_caller_buf(&bytes, out_buf, out_len) as i32,
        Err(e) => set_last_error(e),
    }
}

/// Canonical JSON normalization: parse, sort all keys recursively, re-serialize compact.
///
/// This is a general-purpose canonical form — not specific to credentials or contracts.
/// Useful for computing deterministic hashes of arbitrary JSON payloads.
#[no_mangle]
pub unsafe extern "C" fn aster_canonical_json(
    json_ptr: *const u8,
    json_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if json_ptr.is_null() || out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let json_bytes = slice::from_raw_parts(json_ptr, json_len);
    let json_str = match std::str::from_utf8(json_bytes) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("invalid UTF-8 in JSON input: {e}")),
    };
    // Parse into serde_json::Value, which uses BTreeMap for objects (sorted keys).
    // Then re-serialize compact.
    let value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => return set_last_error(format!("invalid JSON: {e}")),
    };
    let canonical = sort_json_value(&value);
    let output = match serde_json::to_string(&canonical) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("JSON re-serialization failed: {e}")),
    };
    write_to_caller_buf(output.as_bytes(), out_buf, out_len) as i32
}

// ============================================================================
// AsterTicket — compact ticket encode/decode
// ============================================================================

/// Encode an AsterTicket to a base58 string (``aster1<base58>``).
///
/// # Parameters
/// - `endpoint_id_hex`: 64-char hex endpoint ID
/// - `relay_addr_ptr/len`: relay "ip:port" string (NULL/0 for none)
/// - `direct_addrs_json_ptr/len`: JSON array of "ip:port" strings (NULL/0 for none)
/// - `credential_type_ptr/len`: credential type string: "open", "consumer_rcan",
///   "enrollment", "registry_read" (NULL/0 for none)
/// - `credential_data_ptr/len`: credential payload bytes (NULL/0 for none)
/// - `out_buf/out_len`: output buffer for the aster1... string
#[no_mangle]
pub unsafe extern "C" fn aster_ticket_encode(
    endpoint_id_hex_ptr: *const u8,
    endpoint_id_hex_len: usize,
    relay_addr_ptr: *const u8,
    relay_addr_len: usize,
    direct_addrs_json_ptr: *const u8,
    direct_addrs_json_len: usize,
    credential_type_ptr: *const u8,
    credential_type_len: usize,
    credential_data_ptr: *const u8,
    credential_data_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if endpoint_id_hex_ptr.is_null() || out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let hex_str = match std::str::from_utf8(slice::from_raw_parts(
        endpoint_id_hex_ptr,
        endpoint_id_hex_len,
    )) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("invalid endpoint_id UTF-8: {e}")),
    };
    let id_bytes = match hex::decode(hex_str) {
        Ok(b) => b,
        Err(e) => return set_last_error(format!("invalid endpoint_id hex: {e}")),
    };
    if id_bytes.len() != 32 {
        return set_last_error("endpoint_id must be 32 bytes (64 hex chars)");
    }
    let mut endpoint_id = [0u8; 32];
    endpoint_id.copy_from_slice(&id_bytes);

    let relay: Option<std::net::SocketAddr> = if !relay_addr_ptr.is_null() && relay_addr_len > 0 {
        let s = match std::str::from_utf8(slice::from_raw_parts(relay_addr_ptr, relay_addr_len)) {
            Ok(s) => s,
            Err(e) => return set_last_error(format!("invalid relay addr UTF-8: {e}")),
        };
        match s.parse() {
            Ok(a) => Some(a),
            Err(e) => return set_last_error(format!("invalid relay addr: {e}")),
        }
    } else {
        None
    };

    let direct_addrs: Vec<std::net::SocketAddr> =
        if !direct_addrs_json_ptr.is_null() && direct_addrs_json_len > 0 {
            let s = match std::str::from_utf8(slice::from_raw_parts(
                direct_addrs_json_ptr,
                direct_addrs_json_len,
            )) {
                Ok(s) => s,
                Err(e) => return set_last_error(format!("invalid direct_addrs UTF-8: {e}")),
            };
            let arr: Vec<String> = match serde_json::from_str(s) {
                Ok(a) => a,
                Err(e) => return set_last_error(format!("invalid direct_addrs JSON: {e}")),
            };
            let mut addrs = Vec::with_capacity(arr.len());
            for a in &arr {
                match a.parse() {
                    Ok(sa) => addrs.push(sa),
                    Err(e) => return set_last_error(format!("bad direct addr '{}': {e}", a)),
                }
            }
            addrs
        } else {
            vec![]
        };

    let credential = if !credential_type_ptr.is_null() && credential_type_len > 0 {
        let ctype = match std::str::from_utf8(slice::from_raw_parts(
            credential_type_ptr,
            credential_type_len,
        )) {
            Ok(s) => s,
            Err(e) => return set_last_error(format!("invalid credential_type UTF-8: {e}")),
        };
        let cdata = if !credential_data_ptr.is_null() && credential_data_len > 0 {
            slice::from_raw_parts(credential_data_ptr, credential_data_len).to_vec()
        } else {
            vec![]
        };
        use aster_transport_core::ticket::TicketCredential;
        match ctype {
            "open" => Some(TicketCredential::Open),
            "consumer_rcan" => Some(TicketCredential::ConsumerRcan(cdata)),
            "enrollment" => Some(TicketCredential::Enrollment(cdata)),
            "registry_read" => {
                if cdata.len() != 32 {
                    return set_last_error("registry_read credential requires exactly 32 bytes");
                }
                let mut ns = [0u8; 32];
                ns.copy_from_slice(&cdata[..32]);
                Some(TicketCredential::RegistryRead(ns))
            }
            _ => return set_last_error(format!("unknown credential type '{ctype}'")),
        }
    } else {
        None
    };

    let ticket = aster_transport_core::ticket::AsterTicket {
        endpoint_id,
        relay,
        direct_addrs,
        credential,
    };

    match ticket.to_base58_string() {
        Ok(s) => write_to_caller_buf(s.as_bytes(), out_buf, out_len) as i32,
        Err(e) => set_last_error(e),
    }
}

/// Decode an AsterTicket from a base58 string (``aster1<base58>``).
///
/// Writes a JSON object to `out_buf`:
/// ```json
/// {
///   "endpoint_id": "hex...",
///   "relay_addr": "ip:port" | null,
///   "direct_addrs": ["ip:port", ...],
///   "credential_type": "open" | "consumer_rcan" | "enrollment" | "registry_read" | null,
///   "credential_data_hex": "hex..." | null
/// }
/// ```
#[no_mangle]
pub unsafe extern "C" fn aster_ticket_decode(
    ticket_ptr: *const u8,
    ticket_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if ticket_ptr.is_null() || out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let ticket_str = match std::str::from_utf8(slice::from_raw_parts(ticket_ptr, ticket_len)) {
        Ok(s) => s,
        Err(e) => return set_last_error(format!("invalid ticket UTF-8: {e}")),
    };
    let ticket = match aster_transport_core::ticket::AsterTicket::from_base58_str(ticket_str) {
        Ok(t) => t,
        Err(e) => return set_last_error(format!("ticket decode failed: {e}")),
    };

    use aster_transport_core::ticket::TicketCredential;
    let (cred_type, cred_hex): (Option<&str>, Option<String>) = match &ticket.credential {
        None => (None, None),
        Some(TicketCredential::Open) => (Some("open"), None),
        Some(TicketCredential::ConsumerRcan(v)) => (Some("consumer_rcan"), Some(hex::encode(v))),
        Some(TicketCredential::Enrollment(v)) => (Some("enrollment"), Some(hex::encode(v))),
        Some(TicketCredential::RegistryRead(namespace_id)) => {
            (Some("registry_read"), Some(hex::encode(namespace_id)))
        }
    };

    let json = serde_json::json!({
        "endpoint_id": hex::encode(ticket.endpoint_id),
        "relay_addr": ticket.relay.map(|a| a.to_string()),
        "direct_addrs": ticket.direct_addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
        "credential_type": cred_type,
        "credential_data_hex": cred_hex,
    });

    let output = json.to_string();
    write_to_caller_buf(output.as_bytes(), out_buf, out_len) as i32
}

/// Recursively sort JSON object keys for canonical form.
fn sort_json_value(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let sorted: serde_json::Map<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), sort_json_value(v)))
                .collect::<std::collections::BTreeMap<_, _>>()
                .into_iter()
                .collect();
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sort_json_value).collect()),
        other => other.clone(),
    }
}

// ─── Public Soak Test API ───────────────────────────────────────────────

/// Run the soak test for the specified duration in seconds.
///
/// This is exposed publicly so the `soak_test_runner` binary can invoke it
/// without duplicating the test logic.
pub fn run_soak_test(duration_secs: u64) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    // ─── Metrics ────────────────────────────────────────────────────────────
    struct SoakMetrics {
        cycles_completed: AtomicUsize,
        ops_submitted: AtomicUsize,
        ops_completed: AtomicUsize,
        ops_cancelled: AtomicUsize,
        ops_errored: AtomicUsize,
        max_pending_ops: AtomicUsize,
        current_pending_ops: AtomicUsize,
    }

    impl SoakMetrics {
        fn new() -> Self {
            Self {
                cycles_completed: AtomicUsize::new(0),
                ops_submitted: AtomicUsize::new(0),
                ops_completed: AtomicUsize::new(0),
                ops_cancelled: AtomicUsize::new(0),
                ops_errored: AtomicUsize::new(0),
                max_pending_ops: AtomicUsize::new(0),
                current_pending_ops: AtomicUsize::new(0),
            }
        }

        fn record_submit(&self) {
            self.ops_submitted.fetch_add(1, Ordering::Relaxed);
            let pending = self.current_pending_ops.fetch_add(1, Ordering::Relaxed) + 1;
            loop {
                let current_max = self.max_pending_ops.load(Ordering::Relaxed);
                if pending <= current_max {
                    break;
                }
                if self
                    .max_pending_ops
                    .compare_exchange(current_max, pending, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
            }
        }

        fn record_complete(&self) {
            self.ops_completed.fetch_add(1, Ordering::Relaxed);
            self.current_pending_ops.fetch_sub(1, Ordering::Relaxed);
        }

        fn record_error(&self) {
            self.ops_errored.fetch_add(1, Ordering::Relaxed);
            self.current_pending_ops.fetch_sub(1, Ordering::Relaxed);
        }

        fn record_cycle(&self) {
            self.cycles_completed.fetch_add(1, Ordering::Relaxed);
        }
    }

    // ─── Helpers ─────────────────────────────────────────────────────────────

    fn rand_u8() -> u8 {
        let nanos = std::time::Instant::now().elapsed().as_nanos();
        nanos as u8
    }

    fn drain_all_events(runtime: aster_runtime_t) {
        loop {
            let mut events = unsafe { [std::mem::zeroed::<aster_event_t>(); 16] };
            let count = unsafe { aster_poll_events(runtime, events.as_mut_ptr(), 16, 0) };
            if count == 0 {
                break;
            }
        }
    }

    fn poll_for_event(runtime: aster_runtime_t, kind: aster_event_kind_t, timeout_ms: u32) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        loop {
            if Instant::now() >= deadline {
                return false;
            }
            let mut events = unsafe { [std::mem::zeroed::<aster_event_t>(); 4] };
            let count = unsafe { aster_poll_events(runtime, events.as_mut_ptr(), 4, 50) };
            for ev in events.iter().take(count) {
                if ev.kind == kind as u32 {
                    return true;
                }
            }
        }
    }

    fn poll_for_handle(
        runtime: aster_runtime_t,
        kind: aster_event_kind_t,
        timeout_ms: u32,
    ) -> Option<u64> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        while Instant::now() < deadline {
            let mut events = unsafe { [std::mem::zeroed::<aster_event_t>(); 4] };
            let count = unsafe { aster_poll_events(runtime, events.as_mut_ptr(), 4, 50) };
            if let Some(ev) = events.iter().take(count).find(|ev| ev.kind == kind as u32) {
                return Some(ev.handle);
            }
        }
        None
    }

    fn run_soak_cycle(runtime: aster_runtime_t, metrics: &Arc<SoakMetrics>) -> bool {
        drain_all_events(runtime);

        let alpns = [aster_bytes_t {
            ptr: b"aster".as_ptr(),
            len: 5,
        }];
        let mut cfg: node::aster_config_t = unsafe { std::mem::zeroed() };
        if unsafe { node::aster_config_init(&mut cfg) } != aster_status_t::ASTER_STATUS_OK as i32 {
            return false;
        }
        cfg.alpns = aster_bytes_list_t {
            items: alpns.as_ptr(),
            len: alpns.len(),
        };

        let mut node_op: aster_operation_t = 0;
        let status = unsafe { node::aster_node_start(runtime, &cfg, 0, 0, &mut node_op) };
        if status != aster_status_t::ASTER_STATUS_OK as i32 {
            return false;
        }

        let Some(node) =
            poll_for_handle(runtime, aster_event_kind_t::ASTER_EVENT_NODE_CREATED, 2000)
        else {
            return false;
        };

        let mut accept_op: aster_operation_t = 0;
        let status = unsafe { aster_node_accept_aster(runtime, node, 0, &mut accept_op) };
        if status != aster_status_t::ASTER_STATUS_OK as i32 {
            return false;
        }
        metrics.record_submit();

        let should_cancel = rand_u8() < 25;
        if should_cancel {
            std::thread::sleep(Duration::from_millis(5));
            let _ = unsafe { aster_operation_cancel(runtime, accept_op) };
        }

        loop {
            let mut events = unsafe { [std::mem::zeroed::<aster_event_t>(); 8] };
            let count = unsafe { aster_poll_events(runtime, events.as_mut_ptr(), 8, 100) };
            if count == 0 {
                break;
            }
            for ev in events.iter().take(count).copied() {
                if ev.operation == accept_op {
                    if ev.status == aster_status_t::ASTER_STATUS_OK as u32 {
                        metrics.record_complete();
                    } else {
                        metrics.record_error();
                    }
                }
            }
        }

        let mut close_op: aster_operation_t = 0;
        let status = unsafe { aster_node_close(runtime, node, 0, &mut close_op) };
        if status == aster_status_t::ASTER_STATUS_OK as i32 {
            let _closed = poll_for_event(runtime, aster_event_kind_t::ASTER_EVENT_CLOSED, 2000);
        }

        metrics.record_cycle();
        true
    }

    // ─── Main ────────────────────────────────────────────────────────────────

    println!(
        "Starting soak test for {} seconds ({} hours)",
        duration_secs,
        duration_secs / 3600
    );
    println!("Churn pattern: node_create → accept → cancel/close → node_close (10% cancel rate)");
    println!();

    let start = Instant::now();
    let deadline = start + Duration::from_secs(duration_secs);

    let mut runtime: aster_runtime_t = 0;
    let status =
        unsafe { aster_runtime_new(ASTER_ABI_VERSION_MAJOR, std::ptr::null(), &mut runtime) };
    assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

    let metrics = Arc::new(SoakMetrics::new());
    let mut cycle_count = 0usize;
    let print_interval = 100;

    while Instant::now() < deadline {
        let cycle_start = Instant::now();

        let success = run_soak_cycle(runtime, &metrics);

        let _cycle_duration = cycle_start.elapsed();
        cycle_count += 1;

        if cycle_count.is_multiple_of(print_interval) || !success {
            let elapsed = start.elapsed();
            let ops_sub = metrics.ops_submitted.load(Ordering::Relaxed);
            let ops_comp = metrics.ops_completed.load(Ordering::Relaxed);
            let ops_cancel = metrics.ops_cancelled.load(Ordering::Relaxed);
            let ops_err = metrics.ops_errored.load(Ordering::Relaxed);
            let max_pending = metrics.max_pending_ops.load(Ordering::Relaxed);
            let current_pending = metrics.current_pending_ops.load(Ordering::Relaxed);

            println!(
                "[{:?}] cycle {} — submitted={}, completed={}, cancelled={}, errored={}, max_pending={}, current_pending={}",
                elapsed,
                cycle_count,
                ops_sub,
                ops_comp,
                ops_cancel,
                ops_err,
                max_pending,
                current_pending
            );
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    let elapsed = start.elapsed();

    let ops_sub = metrics.ops_submitted.load(Ordering::Relaxed);
    let ops_comp = metrics.ops_completed.load(Ordering::Relaxed);
    let ops_cancel = metrics.ops_cancelled.load(Ordering::Relaxed);
    let ops_err = metrics.ops_errored.load(Ordering::Relaxed);
    let max_pending = metrics.max_pending_ops.load(Ordering::Relaxed);
    let final_pending = metrics.current_pending_ops.load(Ordering::Relaxed);

    println!();
    println!("=== Soak Test Results ===");
    println!("Duration: {:?}", elapsed);
    println!("Cycles: {}", cycle_count);
    println!("Ops submitted: {}", ops_sub);
    println!("Ops completed: {}", ops_comp);
    println!("Ops cancelled: {}", ops_cancel);
    println!("Ops errored: {}", ops_err);
    println!("Max pending ops: {}", max_pending);
    println!("Final pending ops: {}", final_pending);
    println!();

    assert_eq!(
        final_pending, 0,
        "Final pending ops should be 0 (leaked ops detected)"
    );
    assert!(
        max_pending < 100,
        "Max pending ops should be bounded (was {})",
        max_pending
    );

    let status = unsafe { aster_runtime_close(runtime) };
    assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

    println!("Soak test PASSED — no leaks detected");
}

// ============================================================================
// Registry FFI — centralized filter + rank logic (§11.9)
//
// These are synchronous pure-function FFI entry points that take JSON input
// and return JSON output. All language bindings use their existing doc-read
// FFI to fetch lease entries, then call into this layer to apply the
// mandatory filters and ranking strategy, so there is exactly one copy of the
// resolution logic across all five bindings.
//
// Async doc-read + publish integrations are available on the Rust core
// (`core::registry::resolve`, `publish_lease`, `publish_artifact`,
// `renew_lease`) and will be exposed through the event-based FFI model in a
// follow-up pass.
// ============================================================================

/// Return the current wall-clock epoch-millis as seen by the Rust runtime.
/// Single shared clock across all bindings prevents time-skew in lease freshness checks.
#[no_mangle]
pub unsafe extern "C" fn aster_registry_now_epoch_ms() -> i64 {
    aster_transport_core::registry::now_epoch_ms()
}

/// Report whether a lease is still fresh given the lease duration window.
/// `lease_json` is a single EndpointLease JSON object.
/// Returns 1 = fresh, 0 = expired, negative = error (see set_last_error).
#[no_mangle]
pub unsafe extern "C" fn aster_registry_is_fresh(
    lease_json_ptr: *const u8,
    lease_json_len: usize,
    lease_duration_s: i32,
) -> i32 {
    if lease_json_ptr.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let bytes = slice::from_raw_parts(lease_json_ptr, lease_json_len);
    let lease: aster_transport_core::registry::EndpointLease = match serde_json::from_slice(bytes) {
        Ok(l) => l,
        Err(e) => return set_last_error(format!("invalid EndpointLease JSON: {e}")),
    };
    if lease.is_fresh(lease_duration_s) {
        1
    } else {
        0
    }
}

/// Report whether a health status string is routable (READY or DEGRADED).
/// `status_json` is a UTF-8 string (not JSON-quoted).
#[no_mangle]
pub unsafe extern "C" fn aster_registry_is_routable(
    status_ptr: *const u8,
    status_len: usize,
) -> i32 {
    if status_ptr.is_null() {
        return 0;
    }
    let bytes = slice::from_raw_parts(status_ptr, status_len);
    match std::str::from_utf8(bytes) {
        Ok(s) => {
            if aster_transport_core::registry::is_routable(s) {
                1
            } else {
                0
            }
        }
        Err(_) => 0,
    }
}

/// Apply mandatory filters + ranking to a list of EndpointLease JSON objects.
///
/// Input:
/// - `leases_json`: a JSON array of EndpointLease objects.
/// - `opts_json`: a ResolveOptions JSON object with fields:
///   service (string), version (int|null), channel (string|null),
///   contract_id (string|null), strategy (string), caller_alpn (string),
///   caller_serialization_modes (array), caller_policy_realm (string|null),
///   lease_duration_s (int).
///
/// Output (written to `out_buf`): a JSON array of EndpointLease objects in
/// ranked order. The top element is the resolved winner. If the output buffer
/// is too small, returns `BUFFER_TOO_SMALL` and sets `*out_len` to the
/// required size.
///
/// Note: the round-robin rotation state is reset on every call (stateless).
/// Stateful multi-call round-robin will be added when the async resolve FFI
/// lands; for now, bindings should maintain their own rotation counter or
/// accept per-call randomization.
#[no_mangle]
pub unsafe extern "C" fn aster_registry_filter_and_rank(
    leases_json_ptr: *const u8,
    leases_json_len: usize,
    opts_json_ptr: *const u8,
    opts_json_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if leases_json_ptr.is_null() || opts_json_ptr.is_null() || out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let leases_bytes = slice::from_raw_parts(leases_json_ptr, leases_json_len);
    let opts_bytes = slice::from_raw_parts(opts_json_ptr, opts_json_len);

    let leases: Vec<aster_transport_core::registry::EndpointLease> =
        match serde_json::from_slice(leases_bytes) {
            Ok(v) => v,
            Err(e) => return set_last_error(format!("invalid leases JSON: {e}")),
        };

    let opts_val: serde_json::Value = match serde_json::from_slice(opts_bytes) {
        Ok(v) => v,
        Err(e) => return set_last_error(format!("invalid ResolveOptions JSON: {e}")),
    };
    let obj = match opts_val.as_object() {
        Some(o) => o,
        None => return set_last_error("ResolveOptions must be a JSON object"),
    };
    let get_str = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(String::from);
    let get_i32 = |k: &str| obj.get(k).and_then(|v| v.as_i64()).map(|v| v as i32);
    let get_str_list = |k: &str| {
        obj.get(k)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.as_str().map(String::from))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default()
    };
    let opts = aster_transport_core::registry::ResolveOptions {
        service: get_str("service").unwrap_or_default(),
        version: get_i32("version"),
        channel: get_str("channel"),
        contract_id: get_str("contract_id"),
        strategy: get_str("strategy").unwrap_or_else(|| "round_robin".to_string()),
        caller_alpn: get_str("caller_alpn").unwrap_or_else(|| "aster/1".to_string()),
        caller_serialization_modes: {
            let v = get_str_list("caller_serialization_modes");
            if v.is_empty() {
                vec!["fory-xlang".to_string()]
            } else {
                v
            }
        },
        caller_policy_realm: get_str("caller_policy_realm"),
        lease_duration_s: get_i32("lease_duration_s").unwrap_or(45),
    };

    let filtered = aster_transport_core::registry::apply_mandatory_filters(leases, &opts);
    let state = aster_transport_core::registry::ResolveState::new();
    let cid = opts.contract_id.clone().unwrap_or_else(|| "_".to_string());
    let ranked = state.rank(filtered, &opts.strategy, &cid);

    match serde_json::to_vec(&ranked) {
        Ok(bytes) => write_to_caller_buf(&bytes, out_buf, out_len) as i32,
        Err(e) => set_last_error(format!("failed to encode ranked leases: {e}")),
    }
}

/// Return one of the registry key-schema strings by kind, for all bindings to share.
///
/// `kind`: one of `aster_registry_key_kind_t`:
///   0 = contract_key(arg1)
///   1 = version_key(arg1, arg2 as int)
///   2 = channel_key(arg1, arg2)
///   3 = lease_key(arg1, arg2, arg3)
///   4 = lease_prefix(arg1, arg2)
///   5 = acl_key(arg1)
///
/// `arg1/arg2/arg3` are UTF-8 strings. For version_key, arg2 must parse as i32.
#[no_mangle]
pub unsafe extern "C" fn aster_registry_key(
    kind: i32,
    arg1_ptr: *const u8,
    arg1_len: usize,
    arg2_ptr: *const u8,
    arg2_len: usize,
    arg3_ptr: *const u8,
    arg3_len: usize,
    out_buf: *mut u8,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    let read = |ptr: *const u8, len: usize| -> Result<&str, i32> {
        if ptr.is_null() && len != 0 {
            return Err(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);
        }
        let s = if len == 0 {
            ""
        } else {
            match std::str::from_utf8(slice::from_raw_parts(ptr, len)) {
                Ok(s) => s,
                Err(e) => return Err(set_last_error(format!("invalid UTF-8: {e}"))),
            }
        };
        Ok(s)
    };
    let a1 = match read(arg1_ptr, arg1_len) {
        Ok(s) => s,
        Err(s) => return s,
    };
    let a2 = match read(arg2_ptr, arg2_len) {
        Ok(s) => s,
        Err(s) => return s,
    };
    let a3 = match read(arg3_ptr, arg3_len) {
        Ok(s) => s,
        Err(s) => return s,
    };

    let key_bytes: Vec<u8> = match kind {
        0 => aster_transport_core::registry::contract_key(a1),
        1 => {
            let v: i32 = match a2.parse() {
                Ok(n) => n,
                Err(e) => return set_last_error(format!("version must be int: {e}")),
            };
            aster_transport_core::registry::version_key(a1, v)
        }
        2 => aster_transport_core::registry::channel_key(a1, a2),
        3 => aster_transport_core::registry::lease_key(a1, a2, a3),
        4 => aster_transport_core::registry::lease_prefix(a1, a2),
        5 => aster_transport_core::registry::acl_key(a1),
        _ => return set_last_error(format!("unknown key kind: {kind}")),
    };

    write_to_caller_buf(&key_bytes, out_buf, out_len) as i32
}

// ============================================================================
// Registry FFI — async doc-backed operations (§11.8 / §11.9)
//
// These operations follow the standard event-based FFI model:
//   1. Caller invokes the function, gets back an operation handle.
//   2. The op runs on the bridge tokio runtime.
//   3. On completion the bridge emits an event which the caller drains via
//      `aster_event_recv`. The event payload (when present) is JSON.
//
// Round-robin rotation and stale-seq rejection are persistent across calls
// because all ops share `bridge.registry_state`. Per-doc ACLs are stored on
// the bridge keyed by doc handle and survive until the doc is freed.
// ============================================================================

fn parse_resolve_options_json(
    bytes: &[u8],
) -> Result<aster_transport_core::registry::ResolveOptions, String> {
    let val: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid ResolveOptions JSON: {e}"))?;
    let obj = val
        .as_object()
        .ok_or_else(|| "ResolveOptions must be a JSON object".to_string())?;
    let get_str = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(String::from);
    let get_i32 = |k: &str| obj.get(k).and_then(|v| v.as_i64()).map(|v| v as i32);
    let get_str_list = |k: &str| {
        obj.get(k)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.as_str().map(String::from))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default()
    };
    Ok(aster_transport_core::registry::ResolveOptions {
        service: get_str("service").unwrap_or_default(),
        version: get_i32("version"),
        channel: get_str("channel"),
        contract_id: get_str("contract_id"),
        strategy: get_str("strategy").unwrap_or_else(|| "round_robin".to_string()),
        caller_alpn: get_str("caller_alpn").unwrap_or_else(|| "aster/1".to_string()),
        caller_serialization_modes: {
            let v = get_str_list("caller_serialization_modes");
            if v.is_empty() {
                vec!["fory-xlang".to_string()]
            } else {
                v
            }
        },
        caller_policy_realm: get_str("caller_policy_realm"),
        lease_duration_s: get_i32("lease_duration_s").unwrap_or(45),
    })
}

/// Async resolve: pointer lookup → list_leases → seq filter → mandatory filters → rank.
///
/// Inputs:
/// - `doc`: a doc handle previously returned by `aster_docs_create` / `aster_docs_join`.
/// - `opts_json`: ResolveOptions JSON (same shape as `aster_registry_filter_and_rank`).
///
/// On success emits `ASTER_EVENT_REGISTRY_RESOLVED`. The event payload is the JSON
/// of the winning EndpointLease, or empty with `ASTER_STATUS_NOT_FOUND` if no
/// candidate survived the filters.
#[no_mangle]
pub unsafe extern "C" fn aster_registry_resolve(
    runtime: aster_runtime_t,
    doc: u64,
    opts_json: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let opts_bytes = unsafe { read_bytes(&opts_json) };
    let opts = match parse_resolve_options_json(&opts_bytes) {
        Ok(o) => o,
        Err(e) => return set_last_error(e),
    };
    let acl = bridge.registry_acl_lookup(doc);

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        let result = aster_transport_core::registry::resolve(
            &doc_arc,
            &bridge2.registry_state,
            &opts,
            acl.as_deref(),
        )
        .await;

        match result {
            Ok(Some(lease)) => match serde_json::to_vec(&lease) {
                Ok(bytes) => {
                    let event = EventInternal::new(
                        aster_event_kind_t::ASTER_EVENT_REGISTRY_RESOLVED,
                        aster_status_t::ASTER_STATUS_OK,
                        op_id,
                        doc,
                        0,
                        user_data,
                        0,
                    );
                    bridge2.emit_with_data(event, bytes);
                }
                Err(e) => {
                    bridge2.emit_error(op_id, user_data, &format!("encode lease: {e}"));
                }
            },
            Ok(None) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_REGISTRY_RESOLVED,
                    aster_status_t::ASTER_STATUS_NOT_FOUND,
                    op_id,
                    doc,
                    0,
                    user_data,
                    aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Publish a lease and/or an artifact in a single op.
///
/// Either or both of `lease_json` / `artifact_json` may be provided (pass an
/// empty `aster_bytes_t` to skip). For artifact publication, `service` and
/// `version` must be set; `channel` is optional. `gossip_topic` is a topic
/// handle to broadcast the corresponding GossipEvent on, or 0 to skip.
///
/// Emits `ASTER_EVENT_REGISTRY_PUBLISHED` on success.
#[allow(clippy::too_many_arguments)]
#[no_mangle]
pub unsafe extern "C" fn aster_registry_publish(
    runtime: aster_runtime_t,
    doc: u64,
    author_id: aster_bytes_t,
    lease_json: aster_bytes_t,
    artifact_json: aster_bytes_t,
    service: aster_bytes_t,
    version: i32,
    channel: aster_bytes_t,
    gossip_topic: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let author = match unsafe { read_string(&author_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let lease_bytes = unsafe { read_bytes(&lease_json) };
    let artifact_bytes = unsafe { read_bytes(&artifact_json) };

    let lease: Option<aster_transport_core::registry::EndpointLease> = if lease_bytes.is_empty() {
        None
    } else {
        match serde_json::from_slice(&lease_bytes) {
            Ok(l) => Some(l),
            Err(e) => return set_last_error(format!("invalid EndpointLease JSON: {e}")),
        }
    };

    let artifact: Option<aster_transport_core::registry::ArtifactRef> = if artifact_bytes.is_empty()
    {
        None
    } else {
        match serde_json::from_slice(&artifact_bytes) {
            Ok(a) => Some(a),
            Err(e) => return set_last_error(format!("invalid ArtifactRef JSON: {e}")),
        }
    };

    if lease.is_none() && artifact.is_none() {
        return set_last_error("aster_registry_publish: both lease and artifact are empty");
    }

    let service_str = match unsafe { read_string(&service) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let channel_str = match unsafe { read_bytes_opt(&channel) } {
        Some(b) => match String::from_utf8(b) {
            Ok(s) => Some(s),
            Err(e) => return set_last_error(format!("invalid channel UTF-8: {e}")),
        },
        None => None,
    };

    let topic = if gossip_topic == 0 {
        None
    } else {
        bridge.gossip_topics.get(gossip_topic)
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        if let Some(ref lease) = lease {
            if let Err(e) = aster_transport_core::registry::publish_lease(
                &doc_arc,
                &author,
                lease,
                topic.as_deref(),
            )
            .await
            {
                bridge2.emit_error(op_id, user_data, &e.to_string());
                return;
            }
        }

        if let Some(ref artifact) = artifact {
            if let Err(e) = aster_transport_core::registry::publish_artifact(
                &doc_arc,
                &author,
                artifact,
                &service_str,
                version,
                channel_str.as_deref(),
                topic.as_deref(),
            )
            .await
            {
                bridge2.emit_error(op_id, user_data, &e.to_string());
                return;
            }
        }

        bridge2.emit_simple(
            aster_event_kind_t::ASTER_EVENT_REGISTRY_PUBLISHED,
            aster_status_t::ASTER_STATUS_OK,
            op_id,
            doc,
            gossip_topic,
            user_data,
            0,
        );
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Renew an existing lease in place. Reads the current row, bumps lease_seq +
/// timestamps, updates health/load, rewrites it.
///
/// `load` uses NaN as a sentinel for "no load reported".
/// Emits `ASTER_EVENT_REGISTRY_RENEWED` on success.
#[allow(clippy::too_many_arguments)]
#[no_mangle]
pub unsafe extern "C" fn aster_registry_renew_lease(
    runtime: aster_runtime_t,
    doc: u64,
    author_id: aster_bytes_t,
    service: aster_bytes_t,
    contract_id: aster_bytes_t,
    endpoint_id: aster_bytes_t,
    health: aster_bytes_t,
    load: f32,
    lease_duration_s: i32,
    gossip_topic: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let author = match unsafe { read_string(&author_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let service_str = match unsafe { read_string(&service) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let cid = match unsafe { read_string(&contract_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let eid = match unsafe { read_string(&endpoint_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let health_str = match unsafe { read_string(&health) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let load_opt = if load.is_nan() { None } else { Some(load) };

    let topic = if gossip_topic == 0 {
        None
    } else {
        bridge.gossip_topics.get(gossip_topic)
    };

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        match aster_transport_core::registry::renew_lease(
            &doc_arc,
            &author,
            &service_str,
            &cid,
            &eid,
            &health_str,
            load_opt,
            lease_duration_s,
            topic.as_deref(),
        )
        .await
        {
            Ok(()) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_REGISTRY_RENEWED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    gossip_topic,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// Add an author to the per-doc registry ACL writer set, persisting it to the
/// doc under `_aster/acl/writers/`. Switches the ACL out of open mode if it was
/// in open mode. Emits `ASTER_EVENT_REGISTRY_ACL_UPDATED` on success.
#[no_mangle]
pub unsafe extern "C" fn aster_registry_acl_add_writer(
    runtime: aster_runtime_t,
    doc: u64,
    author_id: aster_bytes_t,
    writer_id: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    aster_registry_acl_mutate_writer(
        runtime,
        doc,
        author_id,
        writer_id,
        user_data,
        out_operation,
        true,
    )
}

/// Remove an author from the per-doc registry ACL writer set and persist the
/// updated list. Emits `ASTER_EVENT_REGISTRY_ACL_UPDATED` on success.
#[no_mangle]
pub unsafe extern "C" fn aster_registry_acl_remove_writer(
    runtime: aster_runtime_t,
    doc: u64,
    author_id: aster_bytes_t,
    writer_id: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    aster_registry_acl_mutate_writer(
        runtime,
        doc,
        author_id,
        writer_id,
        user_data,
        out_operation,
        false,
    )
}

unsafe fn aster_registry_acl_mutate_writer(
    runtime: aster_runtime_t,
    doc: u64,
    author_id: aster_bytes_t,
    writer_id: aster_bytes_t,
    user_data: u64,
    out_operation: *mut aster_operation_t,
    add: bool,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    let doc_arc = match bridge.docs.get(doc) {
        Some(d) => d,
        None => return aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    };

    let author = match unsafe { read_string(&author_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let writer = match unsafe { read_string(&writer_id) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let acl = bridge.registry_acl_for_doc(doc);

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        let result = if add {
            acl.add_writer(&doc_arc, &author, &writer).await
        } else {
            acl.remove_writer(&doc_arc, &author, &writer).await
        };

        match result {
            Ok(()) => {
                bridge2.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_REGISTRY_ACL_UPDATED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &e.to_string());
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

/// List the current writer set for the per-doc registry ACL.
///
/// Emits `ASTER_EVENT_REGISTRY_ACL_LISTED`. The event payload is a JSON array
/// of AuthorId strings. If the ACL is in open mode (no writers added yet),
/// the array is empty.
#[no_mangle]
pub unsafe extern "C" fn aster_registry_acl_list_writers(
    runtime: aster_runtime_t,
    doc: u64,
    user_data: u64,
    out_operation: *mut aster_operation_t,
) -> i32 {
    if out_operation.is_null() {
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }

    let bridge = match load_runtime(runtime) {
        Ok(b) => b,
        Err(s) => return s as i32,
    };

    if bridge.docs.get(doc).is_none() {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    }

    let acl = bridge.registry_acl_for_doc(doc);

    let (op_id, cancelled) = match bridge.new_operation(user_data) {
        Ok(operation) => operation,
        Err(status) => return status as i32,
    };
    unsafe {
        *out_operation = op_id;
    }

    let bridge2 = bridge.clone();
    bridge.runtime.spawn(async move {
        if check_cancelled(&cancelled, &bridge2, op_id, user_data) {
            return;
        }

        let writers = acl.writers();
        match serde_json::to_vec(&writers) {
            Ok(bytes) => {
                let event = EventInternal::new(
                    aster_event_kind_t::ASTER_EVENT_REGISTRY_ACL_LISTED,
                    aster_status_t::ASTER_STATUS_OK,
                    op_id,
                    doc,
                    0,
                    user_data,
                    0,
                );
                bridge2.emit_with_data(event, bytes);
            }
            Err(e) => {
                bridge2.emit_error(op_id, user_data, &format!("encode writers: {e}"));
            }
        }
    });

    aster_status_t::ASTER_STATUS_OK as i32
}

#[cfg(test)]
mod string_release_tests {
    use super::*;

    unsafe fn view(b: &aster_bytes_t) -> &[u8] {
        std::slice::from_raw_parts(b.ptr, b.len)
    }

    #[test]
    fn alloc_string_round_trips_and_is_nul_terminated() {
        // Lengths chosen so that a naive `Vec::push(0)` would have grown the
        // capacity past `len + 1` (powers of two), which the old
        // `Vec::from_raw_parts(ptr, len, len)` release mis-sized.
        for n in [1usize, 7, 8, 15, 16, 31, 32, 64, 1000, 4096] {
            let s = "x".repeat(n);
            let b = alloc_string(s.clone());
            assert!(!b.ptr.is_null());
            assert_eq!(b.len, n);
            unsafe {
                assert_eq!(view(&b), s.as_bytes());
                assert_eq!(*b.ptr.add(n), 0, "missing NUL terminator");
                assert_eq!(
                    aster_string_release(b.ptr, b.len),
                    aster_status_t::ASTER_STATUS_OK as i32
                );
            }
        }
    }

    #[test]
    fn alloc_bytes_from_over_capacity_vec_round_trips() {
        let mut v = Vec::with_capacity(4096);
        v.extend_from_slice(b"h3");
        let b = alloc_bytes(v);
        assert_eq!(b.len, 2);
        unsafe {
            assert_eq!(view(&b), b"h3");
            assert_eq!(
                aster_string_release(b.ptr, b.len),
                aster_status_t::ASTER_STATUS_OK as i32
            );
        }
    }

    #[test]
    fn empty_buffers_do_not_allocate_and_release_is_noop() {
        let s = alloc_string(String::new());
        assert!(s.ptr.is_null());
        assert_eq!(s.len, 0);
        let b = alloc_bytes(Vec::new());
        assert!(b.ptr.is_null());
        assert_eq!(b.len, 0);
        unsafe {
            assert_eq!(
                aster_string_release(s.ptr, s.len),
                aster_status_t::ASTER_STATUS_OK as i32
            );
            assert_eq!(
                aster_string_release(std::ptr::null(), 0),
                aster_status_t::ASTER_STATUS_OK as i32
            );
        }
    }
}

#[cfg(test)]
mod runtime_drop_tests {
    use super::*;

    /// The last `Arc<BridgeRuntime>` released inside one of the runtime's own
    /// tasks must not panic ("Cannot drop a runtime in a context where
    /// blocking is not allowed") -- the .NET soak test hit exactly this when
    /// a connection-close task outlived `aster_runtime_close`.
    #[test]
    fn last_reference_dropped_on_own_worker_does_not_panic() {
        let mut handle: aster_runtime_t = 0;
        let status =
            unsafe { aster_runtime_new(ASTER_ABI_VERSION_MAJOR, std::ptr::null(), &mut handle) };
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);
        let bridge = load_runtime(handle).expect("runtime");
        let teardown = bridge.teardown.clone();

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let held = bridge.clone();
        bridge.runtime.spawn(async move {
            started_tx.send(()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            // By now close() has run and this is the last reference.
            drop(held);
            // Unreachable if the drop above panicked.
            let _ = done_tx.send(());
        });
        started_rx.recv().unwrap();
        drop(bridge);
        assert_eq!(
            unsafe { aster_runtime_close(handle) },
            aster_status_t::ASTER_STATUS_OK as i32
        );

        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("last runtime reference dropped on its own worker panicked");
        // The detached drop thread finishes the shutdown (and the test
        // process does not exit with it still running under LSan).
        assert!(
            teardown.wait(std::time::Duration::from_secs(10)),
            "runtime dropped on its own worker never finished shutting down"
        );
    }
}

#[cfg(test)]
mod event_buffer_tests {
    use super::*;

    /// `aster_runtime_config_t.max_inflight_operations` independently bounds
    /// admitted operations and their pending terminal events.
    #[test]
    fn runtime_max_inflight_operations_bounds_pending_operations() {
        let config = aster_runtime_config_t {
            struct_size: std::mem::size_of::<aster_runtime_config_t>() as u32,
            worker_threads: 1,
            event_queue_capacity: 2,
            max_inflight_operations: 2,
        };
        let mut handle: aster_runtime_t = 0;
        let status = unsafe { aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &config, &mut handle) };
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);
        let bridge = load_runtime(handle).expect("runtime");

        let mut admitted = 0;
        let mut rejected = 0;
        for _ in 0..8 {
            match bridge.new_operation(0) {
                Ok((operation, _)) => {
                    admitted += 1;
                    bridge.emit_simple(
                        aster_event_kind_t::ASTER_EVENT_CLOSED,
                        aster_status_t::ASTER_STATUS_OK,
                        operation,
                        0,
                        0,
                        0,
                        0,
                    );
                }
                Err(aster_status_t::ASTER_STATUS_OPERATION_LIMIT) => rejected += 1,
                Err(status) => panic!("unexpected admission status: {status:?}"),
            }
        }

        let mut out: [std::mem::MaybeUninit<aster_event_t>; 8] =
            std::array::from_fn(|_| std::mem::MaybeUninit::uninit());
        let pending = unsafe {
            aster_poll_events(
                handle,
                out.as_mut_ptr().cast::<aster_event_t>(),
                out.len(),
                0,
            )
        };
        assert_eq!(admitted, config.max_inflight_operations as usize);
        assert_eq!(rejected, 8 - admitted);
        assert_eq!(pending, admitted);

        let (operation, _) = bridge.new_operation(0).expect("slot released by poll");
        bridge.emit_simple(
            aster_event_kind_t::ASTER_EVENT_CLOSED,
            aster_status_t::ASTER_STATUS_OK,
            operation,
            0,
            0,
            0,
            0,
        );
        assert_eq!(
            unsafe {
                aster_poll_events(
                    handle,
                    out.as_mut_ptr().cast::<aster_event_t>(),
                    out.len(),
                    0,
                )
            },
            1
        );

        drop(bridge);
        assert_eq!(
            unsafe { aster_runtime_close(handle) },
            aster_status_t::ASTER_STATUS_OK as i32
        );
    }

    #[test]
    fn event_queue_capacity_bounds_queued_and_staged_terminals() {
        let config = aster_runtime_config_t {
            struct_size: std::mem::size_of::<aster_runtime_config_t>() as u32,
            worker_threads: 1,
            event_queue_capacity: 2,
            max_inflight_operations: 8,
        };
        let mut handle: aster_runtime_t = 0;
        assert_eq!(
            unsafe { aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &config, &mut handle) },
            aster_status_t::ASTER_STATUS_OK as i32
        );
        let bridge = load_runtime(handle).expect("runtime");

        for _ in 0..8 {
            let (operation, _) = bridge.new_operation(0).expect("operation admission");
            bridge.emit_simple(
                aster_event_kind_t::ASTER_EVENT_CLOSED,
                aster_status_t::ASTER_STATUS_OK,
                operation,
                0,
                0,
                0,
                0,
            );
        }
        assert_eq!(bridge.events_rx.lock().unwrap().len(), 2);
        assert_eq!(bridge.operation_slots.available_permits(), 0);

        let mut out = std::mem::MaybeUninit::<aster_event_t>::uninit();
        for _ in 0..8 {
            assert_eq!(
                unsafe { aster_poll_events(handle, out.as_mut_ptr(), 1, 500) },
                1
            );
            assert!(bridge.events_rx.lock().unwrap().len() <= 2);
        }
        assert_eq!(bridge.operation_slots.available_permits(), 8);

        drop(bridge);
        assert_eq!(
            unsafe { aster_runtime_close(handle) },
            aster_status_t::ASTER_STATUS_OK as i32
        );
    }

    #[test]
    fn cancellation_uses_reserved_terminal_slot_once() {
        let config = aster_runtime_config_t {
            struct_size: std::mem::size_of::<aster_runtime_config_t>() as u32,
            worker_threads: 1,
            event_queue_capacity: 1,
            max_inflight_operations: 1,
        };
        let mut handle: aster_runtime_t = 0;
        assert_eq!(
            unsafe { aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &config, &mut handle) },
            aster_status_t::ASTER_STATUS_OK as i32
        );
        let bridge = load_runtime(handle).expect("runtime");
        let (operation, _) = bridge.new_operation(73).expect("event slot");

        assert_eq!(
            unsafe { aster_operation_cancel(handle, operation) },
            aster_status_t::ASTER_STATUS_OK as i32
        );
        assert_eq!(
            unsafe { aster_operation_cancel(handle, operation) },
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(
            bridge.new_operation(73).unwrap_err(),
            aster_status_t::ASTER_STATUS_OPERATION_LIMIT
        );
        assert!(!bridge.complete(
            EventInternal::new(
                aster_event_kind_t::ASTER_EVENT_CLOSED,
                aster_status_t::ASTER_STATUS_OK,
                operation,
                0,
                0,
                0,
                0,
            ),
            None,
        ));

        let mut out = std::mem::MaybeUninit::<aster_event_t>::uninit();
        assert_eq!(
            unsafe { aster_poll_events(handle, out.as_mut_ptr(), 1, 0) },
            1
        );
        let event = unsafe { out.assume_init() };
        assert_eq!(
            event.kind,
            aster_event_kind_t::ASTER_EVENT_OPERATION_CANCELLED as u32
        );
        assert_eq!(event.operation, operation);
        assert_eq!(event.user_data, 73);

        let (next, _) = bridge
            .new_operation(73)
            .expect("cancel event released slot");
        bridge.emit_simple(
            aster_event_kind_t::ASTER_EVENT_CLOSED,
            aster_status_t::ASTER_STATUS_OK,
            next,
            0,
            0,
            0,
            0,
        );
        assert_eq!(
            unsafe { aster_poll_events(handle, out.as_mut_ptr(), 1, 0) },
            1
        );

        drop(bridge);
        assert_eq!(
            unsafe { aster_runtime_close(handle) },
            aster_status_t::ASTER_STATUS_OK as i32
        );
    }

    /// Representative of connection/node/doc/stream creation: if cancel wins
    /// after native work inserted a returned handle, the late success path
    /// must remove that otherwise unreachable resource (and must not register
    /// its payload buffer).
    #[test]
    fn cancelled_handle_completion_removes_late_resource() {
        let mut runtime: aster_runtime_t = 0;
        assert_eq!(
            unsafe { aster_runtime_new(ASTER_ABI_VERSION_MAJOR, std::ptr::null(), &mut runtime) },
            aster_status_t::ASTER_STATUS_OK as i32
        );
        let bridge = load_runtime(runtime).expect("runtime");
        let (operation, _) = bridge.new_operation(91).expect("operation admission");
        assert_eq!(
            unsafe { aster_operation_cancel(runtime, operation) },
            aster_status_t::ASTER_STATUS_OK as i32
        );

        let returned_resources = HandleRegistry::new();
        let late_handle = returned_resources.insert(());
        let completed = bridge.complete_with_handle(
            EventInternal::new(
                aster_event_kind_t::ASTER_EVENT_ASTER_ACCEPTED,
                aster_status_t::ASTER_STATUS_OK,
                operation,
                late_handle,
                0,
                91,
                0,
            ),
            Some(b"late alpn".to_vec()),
            &returned_resources,
            late_handle,
        );

        assert!(!completed, "late success must lose to cancellation");
        assert_eq!(returned_resources.count(), 0, "late handle leaked");
        assert_eq!(bridge.buffers.count(), 0, "late payload was registered");

        let mut out = std::mem::MaybeUninit::<aster_event_t>::uninit();
        assert_eq!(
            unsafe { aster_poll_events(runtime, out.as_mut_ptr(), 1, 0) },
            1
        );
        let event = unsafe { out.assume_init() };
        assert_eq!(
            event.kind,
            aster_event_kind_t::ASTER_EVENT_OPERATION_CANCELLED as u32
        );

        drop(bridge);
        assert_eq!(
            unsafe { aster_runtime_close(runtime) },
            aster_status_t::ASTER_STATUS_OK as i32
        );
    }

    #[test]
    fn runtime_close_wakes_pushed_event_backpressure() {
        let config = aster_runtime_config_t {
            struct_size: std::mem::size_of::<aster_runtime_config_t>() as u32,
            worker_threads: 1,
            event_queue_capacity: 1,
            max_inflight_operations: 0,
        };
        let mut handle: aster_runtime_t = 0;
        assert_eq!(
            unsafe { aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &config, &mut handle) },
            aster_status_t::ASTER_STATUS_OK as i32
        );
        let bridge = load_runtime(handle).expect("runtime");
        let (operation, _) = bridge.new_operation(0).expect("event slot");
        bridge.emit_simple(
            aster_event_kind_t::ASTER_EVENT_CLOSED,
            aster_status_t::ASTER_STATUS_OK,
            operation,
            0,
            0,
            0,
            0,
        );

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let producer = bridge.clone();
        bridge.runtime.spawn(async move {
            started_tx.send(()).unwrap();
            let delivered = producer
                .emit_pushed_with_data(
                    EventInternal::new(
                        aster_event_kind_t::ASTER_EVENT_HOOK_BEFORE_CONNECT,
                        aster_status_t::ASTER_STATUS_OK,
                        0,
                        0,
                        0,
                        0,
                        0,
                    ),
                    b"hook".to_vec(),
                )
                .await;
            done_tx.send(delivered).unwrap();
        });
        started_rx.recv().unwrap();

        drop(bridge);
        let teardown = close_runtime(handle).expect("close runtime");
        assert!(!done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("blocked producer did not wake on close"));
        assert!(
            teardown.wait(Duration::from_secs(10)),
            "runtime did not tear down after waking producer"
        );
    }

    /// An admitted operation may need a pushed host decision before it can
    /// complete. Its terminal-event budget must not consume the queue slot
    /// required to deliver that decision.
    #[test]
    fn operation_reservation_does_not_block_its_required_pushed_event() {
        let config = aster_runtime_config_t {
            struct_size: std::mem::size_of::<aster_runtime_config_t>() as u32,
            worker_threads: 1,
            event_queue_capacity: 1,
            max_inflight_operations: 0,
        };
        let mut handle: aster_runtime_t = 0;
        assert_eq!(
            unsafe { aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &config, &mut handle) },
            aster_status_t::ASTER_STATUS_OK as i32
        );
        let bridge = load_runtime(handle).expect("runtime");
        let (operation, _) = bridge.new_operation(0).expect("operation admission");

        let producer = bridge.clone();
        bridge.runtime.spawn(async move {
            if producer
                .emit_pushed_with_data(
                    EventInternal::new(
                        aster_event_kind_t::ASTER_EVENT_HOOK_BEFORE_CONNECT,
                        aster_status_t::ASTER_STATUS_OK,
                        0,
                        1,
                        2,
                        0,
                        0,
                    ),
                    b"decision".to_vec(),
                )
                .await
            {
                producer.emit_simple(
                    aster_event_kind_t::ASTER_EVENT_CONNECTED,
                    aster_status_t::ASTER_STATUS_OK,
                    operation,
                    3,
                    0,
                    0,
                    0,
                );
            }
        });

        let mut out = std::mem::MaybeUninit::<aster_event_t>::uninit();
        assert_eq!(
            unsafe { aster_poll_events(handle, out.as_mut_ptr(), 1, 500) },
            1,
            "operation reservation blocked the pushed event it depends on"
        );
        let hook = unsafe { out.assume_init() };
        assert_eq!(
            hook.kind,
            aster_event_kind_t::ASTER_EVENT_HOOK_BEFORE_CONNECT as u32
        );
        assert_eq!(unsafe { aster_buffer_release(handle, hook.buffer) }, 0);

        assert_eq!(
            unsafe { aster_poll_events(handle, out.as_mut_ptr(), 1, 500) },
            1,
            "operation completion was not delivered after its dependency"
        );
        let completion = unsafe { out.assume_init() };
        assert_eq!(completion.operation, operation);

        drop(bridge);
        assert_eq!(
            unsafe { aster_runtime_close(handle) },
            aster_status_t::ASTER_STATUS_OK as i32
        );
    }

    /// A polled event's payload is freed by `aster_buffer_release`: polling
    /// must not keep (or leak) a reference of its own. Regression for the
    /// `mem::forget(payload)` in `aster_poll_events` that LSan reported in
    /// every test binary that read an event payload.
    #[test]
    fn released_event_payload_is_freed() {
        let mut handle: aster_runtime_t = 0;
        let status =
            unsafe { aster_runtime_new(ASTER_ABI_VERSION_MAJOR, std::ptr::null(), &mut handle) };
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);
        let bridge = load_runtime(handle).expect("runtime");
        let (operation, _) = bridge.new_operation(7).expect("event slot");

        let event = EventInternal::new(
            aster_event_kind_t::ASTER_EVENT_BYTES_RESULT,
            aster_status_t::ASTER_STATUS_OK,
            operation,
            0,
            0,
            7,
            0,
        );
        bridge.emit_with_data(event, b"payload".to_vec());

        let mut out = std::mem::MaybeUninit::<aster_event_t>::uninit();
        let n = unsafe { aster_poll_events(handle, out.as_mut_ptr(), 1, 1000) };
        assert_eq!(n, 1);
        let ev = unsafe { out.assume_init() };
        assert_eq!(ev.user_data, 7);
        assert_ne!(ev.buffer, 0);
        let data = unsafe { std::slice::from_raw_parts(ev.data_ptr, ev.data_len) };
        assert_eq!(data, b"payload");

        let weak = Arc::downgrade(&bridge.buffers.get(ev.buffer).expect("buffer"));
        assert_eq!(
            unsafe { aster_buffer_release(handle, ev.buffer) },
            aster_status_t::ASTER_STATUS_OK as i32
        );
        assert!(
            weak.upgrade().is_none(),
            "event payload still referenced after aster_buffer_release"
        );

        drop(bridge);
        assert_eq!(
            unsafe { aster_runtime_close(handle) },
            aster_status_t::ASTER_STATUS_OK as i32
        );
    }
}

#[cfg(test)]
mod doc_event_encoding_tests {
    use super::*;

    /// The SyncFinished layout the Go, .NET and Java decoders read; their
    /// tests decode these same bytes.
    #[test]
    fn sync_finished_encodes_peer_origin_and_result() {
        let ok = CoreDocEvent::SyncFinished {
            peer: "ab".into(),
            origin: CoreSyncOrigin::Connect,
            result: Ok(()),
        };
        assert_eq!(
            encode_doc_event(&ok),
            (6, vec![2, 0, 0, 0, b'a', b'b', 1, 1, 0, 0, 0, 0])
        );
        let failed = CoreDocEvent::SyncFinished {
            peer: "ab".into(),
            origin: CoreSyncOrigin::Accept,
            result: Err("no".into()),
        };
        assert_eq!(
            encode_doc_event(&failed),
            (
                6,
                vec![2, 0, 0, 0, b'a', b'b', 0, 0, 2, 0, 0, 0, b'n', b'o']
            )
        );
    }
}

#[cfg(test)]
mod doc_event_receiver_tests {
    use super::*;
    use crate::audit_test_common::{start_node, wait_for_operation, wait_for_successful_operation};
    use std::time::Instant;

    struct Fixture {
        runtime: u64,
        bridge: Arc<BridgeRuntime>,
        doc: u64,
        receiver: u64,
        author: Vec<u8>,
    }

    fn bytes(p: &[u8]) -> aster_bytes_t {
        aster_bytes_t {
            ptr: p.as_ptr(),
            len: p.len(),
        }
    }

    impl Fixture {
        fn new() -> Self {
            unsafe {
                let mut runtime = 0;
                assert_eq!(
                    aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime),
                    0
                );
                let bridge = load_runtime(runtime).unwrap();
                let node = start_node(runtime, &[b"doc-receiver-test"]);
                let mut op = 0;
                assert_eq!(aster_docs_create(runtime, node, 0, &mut op), 0);
                let doc = wait_for_successful_operation(
                    runtime,
                    op,
                    aster_event_kind_t::ASTER_EVENT_DOC_CREATED,
                )
                .handle;
                assert_eq!(aster_docs_create_author(runtime, node, 0, &mut op), 0);
                let event = wait_for_successful_operation(
                    runtime,
                    op,
                    aster_event_kind_t::ASTER_EVENT_AUTHOR_CREATED,
                );
                let author = slice::from_raw_parts(event.data_ptr, event.data_len).to_vec();
                assert_eq!(aster_buffer_release(runtime, event.buffer), 0);
                assert_eq!(aster_doc_subscribe(runtime, doc, 0, &mut op), 0);
                let event = wait_for_successful_operation(
                    runtime,
                    op,
                    aster_event_kind_t::ASTER_EVENT_DOC_SUBSCRIBED,
                );
                assert_eq!(event.related, doc);
                Self {
                    runtime,
                    bridge,
                    doc,
                    receiver: event.handle,
                    author,
                }
            }
        }

        fn recv(&self) -> u64 {
            let mut operation = 0;
            assert_eq!(
                unsafe { aster_doc_event_recv(self.runtime, self.receiver, 73, &mut operation) },
                0
            );
            operation
        }

        fn set(&self, key: &[u8], value: &[u8]) {
            unsafe {
                let mut op = 0;
                assert_eq!(
                    aster_doc_set_bytes(
                        self.runtime,
                        self.doc,
                        bytes(&self.author),
                        bytes(key),
                        bytes(value),
                        0,
                        &mut op
                    ),
                    0
                );
                let event = wait_for_successful_operation(
                    self.runtime,
                    op,
                    aster_event_kind_t::ASTER_EVENT_DOC_SET,
                );
                assert_eq!(aster_buffer_release(self.runtime, event.buffer), 0);
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            unsafe {
                aster_runtime_close(self.runtime);
            }
        }
    }

    fn eventually(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(
                Instant::now() < deadline,
                "receiver task did not release its owner"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn doc_receiver_cancel_wakes_wait_and_preserves_next_native_insert() {
        let f = Fixture::new();
        let owner = f.bridge.doc_event_receivers.get(f.receiver).unwrap();
        let operation = f.recv();
        // The serial lock is held throughout the pending stream read. This
        // proves cancellation interrupts a running receive, not just startup.
        eventually(|| owner.pending.try_lock().is_err());
        assert_eq!(unsafe { aster_operation_cancel(f.runtime, operation) }, 0);
        let cancelled = unsafe { wait_for_operation(f.runtime, operation) };
        assert_eq!(
            cancelled.kind,
            aster_event_kind_t::ASTER_EVENT_OPERATION_CANCELLED as u32
        );
        assert_eq!(cancelled.user_data, 73);
        eventually(|| Arc::strong_count(&owner) == 2); // registry + this test
        f.set(b"policy/key", b"hello");
        let operation = f.recv();
        let event = unsafe {
            wait_for_successful_operation(
                f.runtime,
                operation,
                aster_event_kind_t::ASTER_EVENT_DOC_EVENT,
            )
        };
        assert_eq!(event.flags, 0); // InsertLocal
        let payload = unsafe { slice::from_raw_parts(event.data_ptr, event.data_len) };
        assert_eq!(
            u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize,
            f.author.len()
        );
        assert_eq!(&payload[4..4 + f.author.len()], f.author.as_slice());
        let key_offset = 4 + f.author.len();
        assert_eq!(
            u32::from_le_bytes(payload[key_offset..key_offset + 4].try_into().unwrap()),
            10
        );
        assert_eq!(&payload[key_offset + 4..key_offset + 14], b"policy/key");
        assert_eq!(unsafe { aster_buffer_release(f.runtime, event.buffer) }, 0);
        assert_eq!(f.bridge.operations.count(), 0);
        assert_eq!(f.bridge.buffers.count(), 0);
    }

    #[test]
    fn doc_receiver_cancel_winning_after_consumption_requeues_event() {
        let f = Fixture::new();
        let owner = f.bridge.doc_event_receivers.get(f.receiver).unwrap();
        let (operation, _) = f.bridge.new_operation(73).unwrap();
        assert_eq!(unsafe { aster_operation_cancel(f.runtime, operation) }, 0);
        // Deterministically exercise the narrow race after the upstream read
        // consumed an event but before complete claims the terminal permit.
        f.bridge.runtime.block_on(async {
            let mut pending = owner.pending.lock().await;
            owner.deliver(
                &f.bridge,
                operation,
                f.receiver,
                73,
                CoreDocEvent::ContentReady {
                    hash: "hash-after-cancel".into(),
                },
                &mut pending,
            );
            assert!(pending.is_some());
        });
        let terminal = unsafe { wait_for_operation(f.runtime, operation) };
        assert_eq!(
            terminal.kind,
            aster_event_kind_t::ASTER_EVENT_OPERATION_CANCELLED as u32
        );
        assert_eq!(
            f.bridge.buffers.count(),
            0,
            "lost completion registered a buffer"
        );
        let operation = f.recv();
        let event = unsafe {
            wait_for_successful_operation(
                f.runtime,
                operation,
                aster_event_kind_t::ASTER_EVENT_DOC_EVENT,
            )
        };
        assert_eq!(event.flags, 2);
        assert_eq!(
            unsafe { slice::from_raw_parts(event.data_ptr, event.data_len) },
            b"hash-after-cancel"
        );
        assert_eq!(unsafe { aster_buffer_release(f.runtime, event.buffer) }, 0);
        assert!(owner.pending.try_lock().unwrap().is_none());
        assert_eq!(f.bridge.operations.count(), 0);
    }

    #[test]
    fn doc_receiver_committed_event_wins_late_cancellation() {
        let f = Fixture::new();
        let owner = f.bridge.doc_event_receivers.get(f.receiver).unwrap();
        let (operation, _) = f.bridge.new_operation(73).unwrap();
        f.bridge.runtime.block_on(async {
            let mut pending = owner.pending.lock().await;
            owner.deliver(
                &f.bridge,
                operation,
                f.receiver,
                73,
                CoreDocEvent::ContentReady {
                    hash: "committed-before-cancel".into(),
                },
                &mut pending,
            );
        });
        assert_eq!(
            unsafe { aster_operation_cancel(f.runtime, operation) },
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        let event = unsafe {
            wait_for_successful_operation(
                f.runtime,
                operation,
                aster_event_kind_t::ASTER_EVENT_DOC_EVENT,
            )
        };
        assert_eq!(
            unsafe { slice::from_raw_parts(event.data_ptr, event.data_len) },
            b"committed-before-cancel"
        );
        assert_eq!(unsafe { aster_buffer_release(f.runtime, event.buffer) }, 0);
        assert!(owner.pending.try_lock().unwrap().is_none());
        assert_eq!(f.bridge.operations.count(), 0);
    }

    #[test]
    fn doc_receiver_free_wakes_all_reads_and_drops_owner() {
        let f = Fixture::new();
        let owner = f.bridge.doc_event_receivers.get(f.receiver).unwrap();
        let weak = Arc::downgrade(&owner);
        let first = f.recv();
        eventually(|| owner.pending.try_lock().is_err());
        let second = f.recv();
        assert_eq!(
            unsafe { aster_doc_event_receiver_free(f.runtime, f.receiver) },
            0
        );
        assert_eq!(f.bridge.doc_event_receivers.count(), 0);
        assert_eq!(
            unsafe { aster_doc_event_receiver_free(f.runtime, f.receiver) },
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        let mut rejected = 0;
        assert_eq!(
            unsafe { aster_doc_event_recv(f.runtime, f.receiver, 0, &mut rejected) },
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        for operation in [first, second] {
            let event = unsafe { wait_for_operation(f.runtime, operation) };
            assert_eq!(event.kind, aster_event_kind_t::ASTER_EVENT_DOC_EVENT as u32);
            assert_eq!(event.status, aster_status_t::ASTER_STATUS_NOT_FOUND as u32);
            assert_eq!(event.buffer, 0);
        }
        drop(owner);
        eventually(|| weak.upgrade().is_none());
        assert_eq!(f.bridge.operations.count(), 0);
        assert_eq!(f.bridge.buffers.count(), 0);
    }

    #[test]
    fn doc_receiver_runtime_shutdown_wakes_blocked_read() {
        let f = Fixture::new();
        let owner = f.bridge.doc_event_receivers.get(f.receiver).unwrap();
        let weak = Arc::downgrade(&owner);
        f.recv();
        eventually(|| owner.pending.try_lock().is_err());
        drop(owner);
        assert_eq!(unsafe { aster_runtime_close(f.runtime) }, 0);
        eventually(|| weak.upgrade().is_none());
        assert_eq!(f.bridge.doc_event_receivers.count(), 0);
        assert_eq!(f.bridge.operations.count(), 0);
        let teardown = f.bridge.teardown.clone();
        drop(f);
        assert!(
            teardown.wait(Duration::from_secs(5)),
            "blocked receive retained runtime"
        );
    }

    #[test]
    fn doc_receiver_join_subscribe_uses_related_and_independent_ownership() {
        let f = Fixture::new();
        unsafe {
            let mut op = 0;
            assert_eq!(
                aster_doc_share(
                    f.runtime,
                    f.doc,
                    aster_doc_share_mode_t::ASTER_DOC_SHARE_WRITE as u32,
                    0,
                    &mut op
                ),
                0
            );
            let event = wait_for_successful_operation(
                f.runtime,
                op,
                aster_event_kind_t::ASTER_EVENT_DOC_SHARED,
            );
            let ticket = slice::from_raw_parts(event.data_ptr, event.data_len).to_vec();
            assert_eq!(aster_buffer_release(f.runtime, event.buffer), 0);
            let node = start_node(f.runtime, &[b"doc-join-receiver-test"]);
            assert_eq!(
                aster_docs_join_and_subscribe(f.runtime, node, bytes(&ticket), 0, &mut op),
                0
            );
            let event = wait_for_successful_operation(
                f.runtime,
                op,
                aster_event_kind_t::ASTER_EVENT_DOC_JOINED_AND_SUBSCRIBED,
            );
            assert!(f.bridge.docs.get(event.handle).is_some());
            let owner = f
                .bridge
                .doc_event_receivers
                .get(event.related)
                .expect("receiver in related");
            assert_eq!(event.flags, 0);
            assert_eq!(aster_doc_free(f.runtime, event.handle), 0);
            assert!(f.bridge.doc_event_receivers.get(event.related).is_some());
            let weak = Arc::downgrade(&owner);
            drop(owner);
            assert_eq!(aster_doc_event_receiver_free(f.runtime, event.related), 0);
            assert!(weak.upgrade().is_none());
            assert_eq!(aster_doc_event_receiver_free(f.runtime, f.receiver), 0);
            assert_eq!(f.bridge.doc_event_receivers.count(), 0);
        }
    }
}
