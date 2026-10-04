//go:build !cgo

// Package aster provides Go bindings for the Aster Transport FFI.
//
// This file contains pure-Go types that do not require cgo.
// It is only compiled when cgo is disabled (e.g., for unit tests).
// When cgo is enabled, runtime.go provides the Event type via CGO.

package aster

import "unsafe"

// EventKind type for event kind constants.
type EventKind uint32

// EventKind constants — mirrors aster_event_kind_t in aster_ffi.h.
// These are available in all build configurations.
const (
	ASTER_EVENT_NONE                   EventKind = 0
	ASTER_EVENT_NODE_CREATED           EventKind = 1
	ASTER_EVENT_NODE_CREATE_FAILED     EventKind = 2
	ASTER_EVENT_ENDPOINT_CREATED       EventKind = 3
	ASTER_EVENT_ENDPOINT_CREATE_FAILED EventKind = 4
	ASTER_EVENT_CLOSED                 EventKind = 5
	ASTER_EVENT_CONNECTED              EventKind = 10
	ASTER_EVENT_CONNECT_FAILED         EventKind = 11
	ASTER_EVENT_CONNECTION_ACCEPTED    EventKind = 12
	ASTER_EVENT_CONNECTION_CLOSED      EventKind = 13
	ASTER_EVENT_STREAM_OPENED          EventKind = 20
	ASTER_EVENT_STREAM_ACCEPTED        EventKind = 21
	ASTER_EVENT_FRAME_RECEIVED         EventKind = 22
	ASTER_EVENT_SEND_COMPLETED         EventKind = 23
	ASTER_EVENT_STREAM_FINISHED        EventKind = 24
	ASTER_EVENT_STREAM_RESET           EventKind = 25
	// Blobs
	ASTER_EVENT_BLOB_ADDED                     EventKind = 30
	ASTER_EVENT_BLOB_READ                      EventKind = 31
	ASTER_EVENT_BLOB_DOWNLOADED                EventKind = 32
	ASTER_EVENT_BLOB_TICKET_CREATED            EventKind = 33
	ASTER_EVENT_BLOB_COLLECTION_ADDED          EventKind = 34
	ASTER_EVENT_BLOB_COLLECTION_TICKET_CREATED EventKind = 35
	ASTER_EVENT_BLOB_OBSERVE_COMPLETE          EventKind = 56
	// Tags
	ASTER_EVENT_TAG_SET     EventKind = 36
	ASTER_EVENT_TAG_GET     EventKind = 37
	ASTER_EVENT_TAG_DELETED EventKind = 38
	ASTER_EVENT_TAG_LIST    EventKind = 39
	// Docs
	ASTER_EVENT_DOC_CREATED               EventKind = 40
	ASTER_EVENT_DOC_JOINED                EventKind = 41
	ASTER_EVENT_DOC_SET                   EventKind = 42
	ASTER_EVENT_DOC_GET                   EventKind = 43
	ASTER_EVENT_DOC_SHARED                EventKind = 44
	ASTER_EVENT_AUTHOR_CREATED            EventKind = 45
	ASTER_EVENT_DOC_QUERY                 EventKind = 46
	ASTER_EVENT_DOC_SUBSCRIBED            EventKind = 47
	ASTER_EVENT_DOC_EVENT                 EventKind = 48
	ASTER_EVENT_DOC_JOINED_AND_SUBSCRIBED EventKind = 49
	ASTER_EVENT_DOC_DEL                   EventKind = 57
	// Gossip
	ASTER_EVENT_GOSSIP_SUBSCRIBED     EventKind = 50
	ASTER_EVENT_GOSSIP_BROADCAST_DONE EventKind = 51
	ASTER_EVENT_GOSSIP_RECEIVED       EventKind = 52
	ASTER_EVENT_GOSSIP_NEIGHBOR_UP    EventKind = 53
	ASTER_EVENT_GOSSIP_NEIGHBOR_DOWN  EventKind = 54
	ASTER_EVENT_GOSSIP_LAGGED         EventKind = 55
	// Datagrams
	// aster_connection_read_datagram completes with ASTER_EVENT_DATAGRAM_RECEIVED.
	ASTER_EVENT_DATAGRAM_RECEIVED EventKind = 60
	// Aster custom-ALPN
	ASTER_EVENT_ASTER_ACCEPTED EventKind = 65
	// Hooks
	ASTER_EVENT_HOOK_BEFORE_CONNECT      EventKind = 70
	ASTER_EVENT_HOOK_AFTER_CONNECT       EventKind = 71
	ASTER_EVENT_HOOK_INVOCATION_RELEASED EventKind = 72
	// Registry async ops (§11.9)
	ASTER_EVENT_REGISTRY_RESOLVED    EventKind = 80
	ASTER_EVENT_REGISTRY_PUBLISHED   EventKind = 81
	ASTER_EVENT_REGISTRY_RENEWED     EventKind = 82
	ASTER_EVENT_REGISTRY_ACL_UPDATED EventKind = 83
	ASTER_EVENT_REGISTRY_ACL_LISTED  EventKind = 84
	// Generic
	ASTER_EVENT_STRING_RESULT       EventKind = 90
	ASTER_EVENT_BYTES_RESULT        EventKind = 91
	ASTER_EVENT_UNIT_RESULT         EventKind = 92
	ASTER_EVENT_OPERATION_CANCELLED EventKind = 98
	ASTER_EVENT_ERROR               EventKind = 99
	// ABI 3.3 data plane
	ASTER_EVENT_BLOB_FETCHED         EventKind = 100
	ASTER_EVENT_DOC_AUTHORITY_REPORT EventKind = 101
	ASTER_EVENT_DOC_RETIRE_OUTCOME   EventKind = 102
	ASTER_EVENT_DOC_OPENED           EventKind = 103
	ASTER_EVENT_DOC_ENTRY_EXISTS     EventKind = 104
	ASTER_EVENT_DOC_CONTENT_STATUS   EventKind = 105
	ASTER_EVENT_STREAM_PRIORITY      EventKind = 106
)

// Event represents an event emitted by the Rust completion queue.
// In the cgo build (runtime.go), this is backed by the C struct.
// In the pure-Go build, this is a plain Go struct.
type Event struct {
	Kind      EventKind
	Status    uint32
	Operation uint64
	Handle    uint64
	Related   uint64
	UserData  uint64
	DataPtr   unsafe.Pointer
	DataLen   uintptr
	Buffer    uint64
	ErrorCode int32
	Flags     uint32
}
