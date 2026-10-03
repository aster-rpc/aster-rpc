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
		"aster_reactor_call_t":       unsafe.Sizeof(C.aster_reactor_call_t{}),
	}
}

// reactorCallEventKindOffset is offsetof(aster_reactor_call_t, event_kind).
func reactorCallEventKindOffset() uintptr {
	return unsafe.Offsetof(C.aster_reactor_call_t{}.event_kind)
}

// headerABIVersion is ASTER_ABI_VERSION from the header this binding was built against.
const headerABIVersion = uint32(C.ASTER_ABI_VERSION)
