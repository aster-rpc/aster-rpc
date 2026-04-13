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

// EventKind constants — mirrors iroh_event_kind_t in iroh_ffi.h.
// These are available in all build configurations.
const (
	IROH_EVENT_NONE                   EventKind = 0
	IROH_EVENT_NODE_CREATED           EventKind = 1
	IROH_EVENT_NODE_CREATE_FAILED     EventKind = 2
	IROH_EVENT_ENDPOINT_CREATED       EventKind = 3
	IROH_EVENT_ENDPOINT_CREATE_FAILED EventKind = 4
	IROH_EVENT_CLOSED                 EventKind = 5
	IROH_EVENT_CONNECTED              EventKind = 10
	IROH_EVENT_CONNECT_FAILED         EventKind = 11
	IROH_EVENT_CONNECTION_ACCEPTED    EventKind = 12
	IROH_EVENT_CONNECTION_CLOSED      EventKind = 13
	IROH_EVENT_STREAM_OPENED          EventKind = 20
	IROH_EVENT_STREAM_ACCEPTED        EventKind = 21
	IROH_EVENT_FRAME_RECEIVED         EventKind = 22
	IROH_EVENT_SEND_COMPLETED         EventKind = 23
	IROH_EVENT_STREAM_FINISHED        EventKind = 24
	IROH_EVENT_STREAM_RESET           EventKind = 25
	// Blobs
	IROH_EVENT_BLOB_ADDED            EventKind = 30
	IROH_EVENT_BLOB_READ             EventKind = 31
	IROH_EVENT_BLOB_DOWNLOADED       EventKind = 32
	IROH_EVENT_BLOB_TICKET_CREATED   EventKind = 33
	IROH_EVENT_BLOB_COLLECTION_ADDED EventKind = 34
	// Tags
	IROH_EVENT_TAG_SET             EventKind = 36
	IROH_EVENT_TAG_GET             EventKind = 37
	IROH_EVENT_TAG_DELETED         EventKind = 38
	IROH_EVENT_TAG_LIST            EventKind = 39
	// Docs
	IROH_EVENT_DOC_CREATED             EventKind = 40
	IROH_EVENT_DOC_JOINED             EventKind = 41
	IROH_EVENT_DOC_SET                EventKind = 42
	IROH_EVENT_DOC_GET                EventKind = 43
	IROH_EVENT_DOC_SHARED             EventKind = 44
	IROH_EVENT_AUTHOR_CREATED         EventKind = 45
	IROH_EVENT_DOC_QUERY              EventKind = 46
	IROH_EVENT_DOC_SUBSCRIBED         EventKind = 47
	IROH_EVENT_DOC_EVENT              EventKind = 48
	IROH_EVENT_DOC_JOINED_AND_SUBSCRIBED EventKind = 49
	// Gossip
	IROH_EVENT_GOSSIP_SUBSCRIBED      EventKind = 50
	IROH_EVENT_GOSSIP_BROADCAST_DONE  EventKind = 51
	IROH_EVENT_GOSSIP_RECEIVED        EventKind = 52
	IROH_EVENT_GOSSIP_NEIGHBOR_UP     EventKind = 53
	IROH_EVENT_GOSSIP_NEIGHBOR_DOWN   EventKind = 54
	IROH_EVENT_GOSSIP_LAGGED          EventKind = 55
	// Datagrams
	// TODO(iroh): IROH_EVENT_DATAGRAM_RECEIVED (60) is never emitted by any FFI function.
	// iroh_connection_read_datagram emits IROH_EVENT_BYTES_RESULT (91) instead.
	// Consider removing DATAGRAM_RECEIVED from this enum or clarifying its intended use.
	IROH_EVENT_DATAGRAM_RECEIVED      EventKind = 60
	// Aster custom-ALPN
	IROH_EVENT_ASTER_ACCEPTED         EventKind = 65
	// Hooks
	IROH_EVENT_HOOK_BEFORE_CONNECT   EventKind = 70
	IROH_EVENT_HOOK_AFTER_CONNECT    EventKind = 71
	IROH_EVENT_HOOK_INVOCATION_RELEASED EventKind = 72
	// Registry async ops (§11.9)
	IROH_EVENT_REGISTRY_RESOLVED     EventKind = 80
	IROH_EVENT_REGISTRY_PUBLISHED    EventKind = 81
	IROH_EVENT_REGISTRY_RENEWED      EventKind = 82
	IROH_EVENT_REGISTRY_ACL_UPDATED  EventKind = 83
	IROH_EVENT_REGISTRY_ACL_LISTED   EventKind = 84
	// Generic
	IROH_EVENT_STRING_RESULT       EventKind = 90
	IROH_EVENT_BYTES_RESULT        EventKind = 91
	IROH_EVENT_UNIT_RESULT         EventKind = 92
	IROH_EVENT_OPERATION_CANCELLED EventKind = 98
	IROH_EVENT_ERROR               EventKind = 99
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
