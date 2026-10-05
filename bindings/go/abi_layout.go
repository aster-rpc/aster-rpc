//go:build cgo

package aster

/*
#include <stddef.h>
#include "aster_ffi.h"
*/
import "C"

import "unsafe"

// cStructSizes reports sizeof() of the header structs as seen by cgo.
// Used by abi_contract_test.go to pin the layouts the binding relies on.
func cStructSizes() map[string]uintptr {
	return map[string]uintptr{
		"aster_event_t":              unsafe.Sizeof(C.aster_event_t{}),
		"aster_runtime_config_t":     unsafe.Sizeof(C.aster_runtime_config_t{}),
		"aster_connect_config_t":     unsafe.Sizeof(C.aster_connect_config_t{}),
		"aster_node_addr_t":          unsafe.Sizeof(C.aster_node_addr_t{}),
		"aster_bytes_t":              unsafe.Sizeof(C.aster_bytes_t{}),
		"aster_bytes_list_t":         unsafe.Sizeof(C.aster_bytes_list_t{}),
		"aster_remote_info_t":        unsafe.Sizeof(C.aster_remote_info_t{}),
		"aster_connection_info_t":    unsafe.Sizeof(C.aster_connection_info_t{}),
		"aster_transport_snapshot_t": unsafe.Sizeof(C.aster_transport_snapshot_t{}),
		"aster_transport_metrics_t":  unsafe.Sizeof(C.aster_transport_metrics_t{}),
		"aster_dispatch_event_t":     unsafe.Sizeof(C.aster_dispatch_event_t{}),
		"aster_call_options_t":       unsafe.Sizeof(C.aster_call_options_t{}),
		// ABI 4.1 / 4.2
		"aster_upcall_event_t":                unsafe.Sizeof(C.aster_upcall_event_t{}),
		"aster_topology_admitted_stats_t":     unsafe.Sizeof(C.aster_topology_admitted_stats_t{}),
		"aster_http_foreign_options_t":        unsafe.Sizeof(C.aster_http_foreign_options_t{}),
		"aster_route_spec_t":                  unsafe.Sizeof(C.aster_route_spec_t{}),
		"aster_http_relay_options_t":          unsafe.Sizeof(C.aster_http_relay_options_t{}),
		"aster_coordination_client_options_t": unsafe.Sizeof(C.aster_coordination_client_options_t{}),
		// ABI 4.6 (observability, health)
		"aster_observability_config_t": unsafe.Sizeof(C.aster_observability_config_t{}),
		"aster_log_record_t":           unsafe.Sizeof(C.aster_log_record_t{}),
		"aster_log_emit_t":             unsafe.Sizeof(C.aster_log_emit_t{}),
		"aster_instrument_desc_t":      unsafe.Sizeof(C.aster_instrument_desc_t{}),
		"aster_span_context_t":         unsafe.Sizeof(C.aster_span_context_t{}),
		"aster_span_record_t":          unsafe.Sizeof(C.aster_span_record_t{}),
		"aster_health_config_t":        unsafe.Sizeof(C.aster_health_config_t{}),
	}
}

// upcallEventOffsets are the aster_upcall_event_t field offsets the binding
// reads (pinned in ffi/src/layout.rs).
func upcallEventOffsets() map[string]uintptr {
	var e C.aster_upcall_event_t
	return map[string]uintptr{
		"invocation_id": unsafe.Offsetof(e.invocation_id), "kind": unsafe.Offsetof(e.kind),
		"cancel_reason": unsafe.Offsetof(e.cancel_reason), "payload_len": unsafe.Offsetof(e.payload_len),
		"deadline_unix_ms": unsafe.Offsetof(e.deadline_unix_ms), "payload_ptr": unsafe.Offsetof(e.payload_ptr),
		"buffer": unsafe.Offsetof(e.buffer),
	}
}

// httpForeignOptionsOffsets are the aster_http_foreign_options_t offsets.
func httpForeignOptionsOffsets() map[string]uintptr {
	var o C.aster_http_foreign_options_t
	return map[string]uintptr{
		"struct_size": unsafe.Offsetof(o.struct_size), "flags": unsafe.Offsetof(o.flags),
		"inline_request_max": unsafe.Offsetof(o.inline_request_max),
		"max_request_body":   unsafe.Offsetof(o.max_request_body),
		"max_response_body":  unsafe.Offsetof(o.max_response_body),
		"max_chunk":          unsafe.Offsetof(o.max_chunk), "response_buffer": unsafe.Offsetof(o.response_buffer),
		"reserved": unsafe.Offsetof(o.reserved),
	}
}

// relayOptionsOffsets are the aster_http_relay_options_t offsets.
func relayOptionsOffsets() map[string]uintptr {
	var o C.aster_http_relay_options_t
	return map[string]uintptr{
		"struct_size": unsafe.Offsetof(o.struct_size), "flags": unsafe.Offsetof(o.flags),
		"max_chunk": unsafe.Offsetof(o.max_chunk), "upload_capacity": unsafe.Offsetof(o.upload_capacity),
		"reserved": unsafe.Offsetof(o.reserved),
	}
}

// bodyStatusToStatus is aster_http_body_status_to_status.
func bodyStatusToStatus(code int32) int32 {
	return int32(C.aster_http_body_status_to_status(C.int32_t(code)))
}

// coordinationOptionsOffsets are the aster_coordination_client_options_t offsets.
func coordinationOptionsOffsets() map[string]uintptr {
	var o C.aster_coordination_client_options_t
	return map[string]uintptr{
		"struct_size": unsafe.Offsetof(o.struct_size), "flags": unsafe.Offsetof(o.flags),
		"group_id": unsafe.Offsetof(o.group_id), "seed_tickets": unsafe.Offsetof(o.seed_tickets),
		"journal_dir": unsafe.Offsetof(o.journal_dir),
	}
}

// routeSpecOffsets are the aster_route_spec_t offsets.
func routeSpecOffsets() map[string]uintptr {
	var r C.aster_route_spec_t
	return map[string]uintptr{
		"host": unsafe.Offsetof(r.host), "service_id": unsafe.Offsetof(r.service_id),
		"port": unsafe.Offsetof(r.port), "protocol": unsafe.Offsetof(r.protocol),
	}
}

// abiEnums are the 4.1 / 4.2 enum values the binding hard-codes or mirrors
// in plain Go (upcall_types.go), read from the header through cgo.
func abiEnums() map[string]int64 {
	return map[string]int64{
		"UPCALL_KIND_CANCEL":              C.ASTER_UPCALL_KIND_CANCEL,
		"UPCALL_KIND_LEASE_TRANSITION":    C.ASTER_UPCALL_KIND_LEASE_TRANSITION,
		"UPCALL_KIND_LEASE_SNAPSHOT":      C.ASTER_UPCALL_KIND_LEASE_SNAPSHOT,
		"UPCALL_KIND_LEASE_AUTHORIZE":     C.ASTER_UPCALL_KIND_LEASE_AUTHORIZE,
		"UPCALL_KIND_FENCED_MUTATE":       C.ASTER_UPCALL_KIND_FENCED_MUTATE,
		"UPCALL_KIND_TOPOLOGY_ADMITTED":   C.ASTER_UPCALL_KIND_TOPOLOGY_ADMITTED,
		"UPCALL_KIND_EXPOSE_AUTHORIZE":    C.ASTER_UPCALL_KIND_EXPOSE_AUTHORIZE,
		"UPCALL_KIND_EXPOSE_HTTP":         C.ASTER_UPCALL_KIND_EXPOSE_HTTP,
		"UPCALL_KIND_EXPOSE_ROUTE_POLICY": C.ASTER_UPCALL_KIND_EXPOSE_ROUTE_POLICY,
		"UPCALL_KIND_ADMISSION_CONNECT":   C.ASTER_UPCALL_KIND_ADMISSION_CONNECT,
		"UPCALL_KIND_ADMISSION_HANDSHAKE": C.ASTER_UPCALL_KIND_ADMISSION_HANDSHAKE,
		"UPCALL_STATUS_OK":                C.ASTER_UPCALL_STATUS_OK,
		"UPCALL_STATUS_ERROR":             C.ASTER_UPCALL_STATUS_ERROR,
		"UPCALL_STATUS_UNIMPLEMENTED":     C.ASTER_UPCALL_STATUS_UNIMPLEMENTED,
		"UPCALL_CANCEL_NONE":              C.ASTER_UPCALL_CANCEL_NONE,
		"UPCALL_CANCEL_TIMEOUT":           C.ASTER_UPCALL_CANCEL_TIMEOUT,
		"UPCALL_CANCEL_ABANDONED":         C.ASTER_UPCALL_CANCEL_ABANDONED,
		"UPCALL_CANCEL_SHUTDOWN":          C.ASTER_UPCALL_CANCEL_SHUTDOWN,
		"LEASE_STATUS_HELD":               C.ASTER_LEASE_STATUS_HELD,
		"LEASE_STATUS_RELEASED":           C.ASTER_LEASE_STATUS_RELEASED,
		"LEASE_STATUS_FENCED":             C.ASTER_LEASE_STATUS_FENCED,
		"LEASE_STATUS_LOST":               C.ASTER_LEASE_STATUS_LOST,
		"EVENT_LEASE_ACQUIRED":            C.ASTER_EVENT_LEASE_ACQUIRED,
		"EVENT_LEASE_STATUS":              C.ASTER_EVENT_LEASE_STATUS,
		"HTTP_BODY_OK":                    C.ASTER_HTTP_BODY_OK,
		"HTTP_BODY_UNKNOWN_STREAM":        C.ASTER_HTTP_BODY_UNKNOWN_STREAM,
		"HTTP_BODY_CANCELLED":             C.ASTER_HTTP_BODY_CANCELLED,
		"HTTP_BODY_TOO_LARGE":             C.ASTER_HTTP_BODY_TOO_LARGE,
		"HTTP_BODY_CHUNK_TOO_LARGE":       C.ASTER_HTTP_BODY_CHUNK_TOO_LARGE,
		"HTTP_BODY_FINISHED":              C.ASTER_HTTP_BODY_FINISHED,
		"HTTP_BODY_TIMEOUT":               C.ASTER_HTTP_BODY_TIMEOUT,
		"HTTP_BODY_READ":                  C.ASTER_HTTP_BODY_READ,
		"HTTP_BODY_INVALID_TRAILERS":      C.ASTER_HTTP_BODY_INVALID_TRAILERS,
		"HTTP_BODY_BLOCKING_ON_RUNTIME":   C.ASTER_HTTP_BODY_BLOCKING_ON_RUNTIME,
		"HTTP_BODY_INVALID_ARGUMENT":      C.ASTER_HTTP_BODY_INVALID_ARGUMENT,
		"HTTP_BODY_UNKNOWN_HANDLE":        C.ASTER_HTTP_BODY_UNKNOWN_HANDLE,
		"HOOK_FAILURE_FAIL_OPEN":          C.ASTER_HOOK_FAILURE_FAIL_OPEN,
		"HOOK_FAILURE_FAIL_CLOSED":        C.ASTER_HOOK_FAILURE_FAIL_CLOSED,
		"UPCALL_DEFAULT_CAPACITY":         C.ASTER_UPCALL_DEFAULT_CAPACITY,
		// ABI 4.3
		"HTTP_BODY_UNKNOWN_SERVICE":   C.ASTER_HTTP_BODY_UNKNOWN_SERVICE,
		"HTTP_BODY_DENIED":            C.ASTER_HTTP_BODY_DENIED,
		"HTTP_BODY_OVERLOADED":        C.ASTER_HTTP_BODY_OVERLOADED,
		"HTTP_BODY_RESET":             C.ASTER_HTTP_BODY_RESET,
		"HTTP_BODY_BAD_HEAD":          C.ASTER_HTTP_BODY_BAD_HEAD,
		"HTTP_BODY_CONNECTION_CLOSED": C.ASTER_HTTP_BODY_CONNECTION_CLOSED,
		"HTTP_BODY_UPLOAD_FAILED":     C.ASTER_HTTP_BODY_UPLOAD_FAILED,
		"HTTP_BODY_BUFFER_TOO_SMALL":  C.ASTER_HTTP_BODY_BUFFER_TOO_SMALL,
		"EVENT_HTTP_RELAY_READY":      C.ASTER_EVENT_HTTP_RELAY_READY,
		// ABI 4.4
		"EVENT_COORDINATION_CLIENT_OPENED":  C.ASTER_EVENT_COORDINATION_CLIENT_OPENED,
		"EVENT_COORDINATION_RESOLVED":       C.ASTER_EVENT_COORDINATION_RESOLVED,
		"EVENT_COORDINATION_RESULT":         C.ASTER_EVENT_COORDINATION_RESULT,
		"EVENT_COORDINATION_LEASE_ACQUIRED": C.ASTER_EVENT_COORDINATION_LEASE_ACQUIRED,
		"COORDINATION_CLIENT_READ_ONLY":     C.ASTER_COORDINATION_CLIENT_READ_ONLY,
		"COORDINATION_ERROR_NONE":           C.ASTER_COORDINATION_ERROR_NONE,
		"COORDINATION_ERROR_AWAITING_ACK":   C.ASTER_COORDINATION_ERROR_AWAITING_ACK,
		"COORDINATION_ERROR_UNRESOLVED":     C.ASTER_COORDINATION_ERROR_UNRESOLVED,
		"COORDINATION_ERROR_JOURNAL_LOCKED": C.ASTER_COORDINATION_ERROR_JOURNAL_LOCKED,
		"COORDINATION_ERROR_JOURNAL_FATAL":  C.ASTER_COORDINATION_ERROR_JOURNAL_FATAL,
		"COORDINATION_ERROR_RPC":            C.ASTER_COORDINATION_ERROR_RPC,
		"COORDINATION_ERROR_UNAVAILABLE":    C.ASTER_COORDINATION_ERROR_UNAVAILABLE,
		"COORDINATION_ERROR_CLOSED":         C.ASTER_COORDINATION_ERROR_CLOSED,
		"COORDINATION_ERROR_REJECTED":       C.ASTER_COORDINATION_ERROR_REJECTED,
		"COORDINATION_ERROR_INVALID":        C.ASTER_COORDINATION_ERROR_INVALID,
		"COORDINATION_ERROR_OTHER":          C.ASTER_COORDINATION_ERROR_OTHER,
		"MUTATE_ACQUIRE":                    C.ASTER_COORDINATION_MUTATE_ACQUIRE,
		"MUTATE_RENEW":                      C.ASTER_COORDINATION_MUTATE_RENEW,
		"MUTATE_COMPARE_AND_SET":            C.ASTER_COORDINATION_MUTATE_COMPARE_AND_SET,
		"MUTATE_RELEASE":                    C.ASTER_COORDINATION_MUTATE_RELEASE,
		"MUTATE_RELEASE_WITH_VALUE":         C.ASTER_COORDINATION_MUTATE_RELEASE_WITH_VALUE,
		"MUTATE_REVOKE":                     C.ASTER_COORDINATION_MUTATE_REVOKE,
		"MUTATE_SET_GRANT":                  C.ASTER_COORDINATION_MUTATE_SET_GRANT,
		"MUTATE_SET_ADMIN":                  C.ASTER_COORDINATION_MUTATE_SET_ADMIN,
		"MUTATE_CHANGE_MEMBERSHIP":          C.ASTER_COORDINATION_MUTATE_CHANGE_MEMBERSHIP,
		"MUTATE_CANCEL_MEMBERSHIP":          C.ASTER_COORDINATION_MUTATE_CANCEL_MEMBERSHIP,
		"READ_READ_LINEARIZABLE":            C.ASTER_COORDINATION_READ_READ_LINEARIZABLE,
		"READ_INSPECT_ACL":                  C.ASTER_COORDINATION_READ_INSPECT_ACL,
		"READ_INSPECT_ADMINS":               C.ASTER_COORDINATION_READ_INSPECT_ADMINS,
		"READ_MEMBERSHIP_STATUS":            C.ASTER_COORDINATION_READ_MEMBERSHIP_STATUS,
		"READ_GROUP_STATUS":                 C.ASTER_COORDINATION_READ_GROUP_STATUS,
		"READ_PROBE_CLIENT":                 C.ASTER_COORDINATION_READ_PROBE_CLIENT,
		"READ_AUDIT_STREAM":                 C.ASTER_COORDINATION_READ_AUDIT_STREAM,
		"READ_RECOVERY_EXPORT":              C.ASTER_COORDINATION_READ_RECOVERY_EXPORT,
		"TAG_MUTATION_REQUEST":              C.ASTER_COORDINATION_TAG_MUTATION_REQUEST,
		"TAG_PLACEMENT_REPORT":              C.ASTER_COORDINATION_TAG_PLACEMENT_REPORT,
		"UPCALL_KIND_TOPOLOGY_SNAPSHOT":     C.ASTER_UPCALL_KIND_TOPOLOGY_SNAPSHOT,
		"UPCALL_KIND_PLACEMENT_POLICY":      C.ASTER_UPCALL_KIND_PLACEMENT_POLICY,
		"UPCALL_KIND_PLACEMENT_PROVISION":   C.ASTER_UPCALL_KIND_PLACEMENT_PROVISION,
		"STATUS_UNAUTHENTICATED":            C.ASTER_STATUS_UNAUTHENTICATED,
		"STATUS_PERMISSION_DENIED":          C.ASTER_STATUS_PERMISSION_DENIED,
		"STATUS_CREDENTIAL_EXPIRED":         C.ASTER_STATUS_CREDENTIAL_EXPIRED,
	}
}

// dispatchEventOffsets are the aster_dispatch_event_t field offsets the
// binding reads (pinned in ffi/src/layout.rs).
func dispatchEventOffsets() map[string]uintptr {
	var e C.aster_dispatch_event_t
	return map[string]uintptr{
		"kind": unsafe.Offsetof(e.kind), "flags": unsafe.Offsetof(e.flags),
		"call_id": unsafe.Offsetof(e.call_id), "session_key": unsafe.Offsetof(e.session_key),
		"connection_handle": unsafe.Offsetof(e.connection_handle),
		"deadline_unix_ms":  unsafe.Offsetof(e.deadline_unix_ms),
		"session_id":        unsafe.Offsetof(e.session_id), "request_ptr": unsafe.Offsetof(e.request_ptr),
		"peer_ptr": unsafe.Offsetof(e.peer_ptr), "buffer": unsafe.Offsetof(e.buffer),
	}
}

// headerABIVersion is ASTER_ABI_VERSION from the header this binding was built against.
const headerABIVersion = uint32(C.ASTER_ABI_VERSION)
