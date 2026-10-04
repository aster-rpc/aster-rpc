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
		"aster_endpoint_config_t":    unsafe.Sizeof(C.aster_endpoint_config_t{}),
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
