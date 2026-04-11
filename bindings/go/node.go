//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include "iroh_ffi.h"
*/
import "C"

import (
	"context"
	"encoding/hex"
	"fmt"
	"sync"
	"unsafe"
)

// NodeAddr represents the address of a node.
type NodeAddr struct {
	EndpointID      string
	RelayURL        string
	DirectAddresses []string
}

// NodeID is a 32-byte node identifier, displayed as hex.
type NodeID [32]byte

// String returns the hex encoding of the node ID.
func (id NodeID) String() string {
	return hex.EncodeToString(id[:])
}

// AcceptedAster represents an incoming Aster connection accepted by a Node.
type AcceptedAster struct {
	ALPN       []byte
	Connection *Connection
}

// Node owns a node handle and dispatches incoming connections.
// It implements AutoCloseable.
type Node struct {
	handle  uint64
	runtime *Runtime

	// connections broadcasts accepted connections to listeners.
	connections chan AcceptedAster
	mu          sync.RWMutex
	closed      bool
}

// Connections returns a channel that emits AcceptedAster for each
// incoming Aster connection accepted by this node.
func (n *Node) Connections() <-chan AcceptedAster {
	return n.connections
}

// Blobs returns the blob storage operations for this node.
func (n *Node) Blobs() *Blobs {
	return &Blobs{node: n, runtime: n.runtime}
}

// Tags returns the tag operations for this node.
func (n *Node) Tags() *Tags {
	return &Tags{node: n, runtime: n.runtime}
}

// Gossip returns the gossip protocol operations for this node.
func (n *Node) Gossip() *Gossip {
	return &Gossip{node: n, runtime: n.runtime}
}

// NodeID returns this node's endpoint identifier as a hex string.
func (n *Node) NodeID() (string, error) {
	var buf [64]byte
	var outLen C.uintptr_t

	r := C.iroh_node_id(
		C.uint64_t(n.runtime.handle),
		C.uint64_t(n.handle),
		(*C.uint8_t)(unsafe.Pointer(&buf[0])),
		64,
		&outLen,
	)
	if r != 0 {
		return "", fmt.Errorf("iroh_node_id: %w", Error(r))
	}
	if outLen == 0 {
		return "", nil
	}
	return string(buf[:outLen]), nil
}

// NodeAddr returns the structured address of this node.
func (n *Node) NodeAddr() (*NodeAddr, error) {
	var addrBuf [4096]byte
	var addr C.iroh_node_addr_t

	r := C.iroh_node_addr_info(
		C.uint64_t(n.runtime.handle),
		C.uint64_t(n.handle),
		(*C.uint8_t)(unsafe.Pointer(&addrBuf[0])),
		4096,
		&addr,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_node_addr_info: %w", Error(r))
	}

	nodeAddr := &NodeAddr{
		DirectAddresses: []string{},
	}

	// Read from addr struct (not addrBuf)
	// iroh_node_addr_t layout: endpoint_id(16) + relay_url(16) + direct_addresses(16)
	// endpoint_id at offset 0: iroh_bytes_t { ptr(8), len(8) }
	idPtr := (*unsafe.Pointer)(unsafe.Pointer(&addr))
	idLen := (*C.uintptr_t)(unsafe.Pointer(uintptr(unsafe.Pointer(&addr)) + 8))
	if idPtr != nil && *idLen > 0 {
		nodeAddr.EndpointID = C.GoStringN((*C.char)(*idPtr), C.int(*idLen))
	}

	// relay_url at offset 16
	relayPtr := (*unsafe.Pointer)(unsafe.Pointer(uintptr(unsafe.Pointer(&addr)) + 16))
	relayLen := (*C.uintptr_t)(unsafe.Pointer(uintptr(unsafe.Pointer(&addr)) + 24))
	if relayPtr != nil && *relayLen > 0 {
		nodeAddr.RelayURL = C.GoStringN((*C.char)(*relayPtr), C.int(*relayLen))
	}

	// direct_addresses at offset 32: iroh_bytes_list_t { items_ptr(8), len(8) }
	daListPtr := (*unsafe.Pointer)(unsafe.Pointer(uintptr(unsafe.Pointer(&addr)) + 32))
	daListLen := (*C.uintptr_t)(unsafe.Pointer(uintptr(unsafe.Pointer(&addr)) + 40))
	if daListPtr != nil && *daListLen > 0 {
		daItems := (*[256]C.struct_iroh_bytes_t)(*daListPtr)
		for i := 0; i < int(*daListLen); i++ {
			item := daItems[i]
			if item.ptr != nil && item.len > 0 {
				nodeAddr.DirectAddresses = append(nodeAddr.DirectAddresses,
					C.GoStringN((*C.char)(unsafe.Pointer(item.ptr)), C.int(item.len)))
			}
		}
	}

	return nodeAddr, nil
}

// ExportSecretKey exports this node's 32-byte secret key seed.
func (n *Node) ExportSecretKey() ([]byte, error) {
	var buf [32]byte
	var outLen C.uintptr_t

	r := C.iroh_node_export_secret_key(
		C.uint64_t(n.runtime.handle),
		C.uint64_t(n.handle),
		(*C.uint8_t)(unsafe.Pointer(&buf[0])),
		32,
		&outLen,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_node_export_secret_key: %w", Error(r))
	}
	if outLen == 0 {
		return nil, nil
	}
	return buf[:outLen], nil
}

// Close shuts down the node and frees its handle.
func (n *Node) Close() error {
	n.mu.Lock()
	if n.closed {
		n.mu.Unlock()
		return nil
	}
	n.closed = true
	n.mu.Unlock()

	// Remove accept handler so no new connections arrive.
	n.runtime.removeAcceptHandler(n.handle)

	r := C.iroh_node_free(C.uint64_t(n.runtime.handle), C.uint64_t(n.handle))
	if r != 0 {
		return fmt.Errorf("iroh_node_free: %w", Error(r))
	}
	return nil
}

// ─── Factory methods ─────────────────────────────────────────────────────────

// Memory creates an in-memory node with all protocols enabled.
func Memory(ctx context.Context) (*Node, error) {
	return MemoryWithAlpns(ctx, nil)
}

// MemoryWithAlpns creates an in-memory node that accepts connections on the given ALPNs.
func MemoryWithAlpns(ctx context.Context, alpns []string) (*Node, error) {
	runtime, err := NewRuntime(ctx, DefaultRuntimeConfig())
	if err != nil {
		return nil, fmt.Errorf("create runtime: %w", err)
	}
	return createNode(ctx, runtime, alpns, "", false)
}

// Persistent creates a persistent node at the given path with all protocols.
func Persistent(ctx context.Context, path string) (*Node, error) {
	return PersistentWithAlpns(ctx, path, nil)
}

// PersistentWithAlpns creates a persistent node at path that accepts connections on the given ALPNs.
func PersistentWithAlpns(ctx context.Context, path string, alpns []string) (*Node, error) {
	runtime, err := NewRuntime(ctx, DefaultRuntimeConfig())
	if err != nil {
		return nil, fmt.Errorf("create runtime: %w", err)
	}
	return createNode(ctx, runtime, alpns, path, true)
}

func createNode(ctx context.Context, runtime *Runtime, alpns []string, path string, persistent bool) (*Node, error) {
	var opID C.iroh_operation_t

	var r C.int32_t
	if persistent {
		pathBytes := []byte(path)
		cBuf := C.malloc(C.size_t(len(pathBytes)))
		C.memcpy(cBuf, unsafe.Pointer(&pathBytes[0]), C.size_t(len(pathBytes)))
		bytes := C.struct_iroh_bytes_t{
			ptr: (*C.uint8_t)(cBuf),
			len: C.uintptr_t(len(pathBytes)),
		}
		r = C.iroh_node_persistent(
			C.uint64_t(runtime.handle),
			bytes,
			0,
			&opID,
		)
	} else if len(alpns) == 0 {
		r = C.iroh_node_memory(
			C.uint64_t(runtime.handle),
			0,
			&opID,
		)
	} else {
		// Build arrays of pointers and lengths for the ALPN strings.
		// Each alpnBuf holds the C-allocated copy of one ALPN string.
		alpnPtrs := make([]*C.uint8_t, len(alpns))
		alpnLens := make([]C.uintptr_t, len(alpns))
		alpnBuf := make([][]byte, len(alpns)) // keep alive until after FFI call
		for i, alpn := range alpns {
			b := []byte(alpn)
			alpnBuf[i] = b
			// Allocate C memory and copy the string bytes.
			// This satisfies Go's cgo pointer rules: we pass C-allocated memory to C.
			cBuf := C.malloc(C.size_t(len(b)))
			C.memcpy(cBuf, unsafe.Pointer(&b[0]), C.size_t(len(b)))
			alpnPtrs[i] = (*C.uint8_t)(cBuf)
			alpnLens[i] = C.uintptr_t(len(b))
		}

		r = C.iroh_node_memory_with_alpns(
			C.uint64_t(runtime.handle),
			(**C.uint8_t)(unsafe.Pointer(&alpnPtrs[0])),
			(*C.uintptr_t)(unsafe.Pointer(&alpnLens[0])),
			C.uintptr_t(len(alpns)),
			0,
			&opID,
		)

		// Free the C-allocated ALPN buffers.
		for i := range alpnPtrs {
			C.free(unsafe.Pointer(alpnPtrs[i]))
		}
	}

	if r != 0 {
		runtime.Close()
		return nil, fmt.Errorf("node create: %w", Error(r))
	}

	ev, err := runtime.Poll(ctx, uint64(opID))
	if err != nil {
		runtime.Close()
		return nil, fmt.Errorf("node create poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_NODE_CREATED {
		runtime.Close()
		return nil, fmt.Errorf("node create: unexpected event %d", ev.Kind)
	}

	node := &Node{
		handle:      ev.Handle,
		runtime:     runtime,
		connections: make(chan AcceptedAster, 64),
	}

	runtime.registerAcceptHandler(node.handle, node)

	return node, nil
}

// onAcceptEvent is called by the runtime when an IROH_EVENT_ASTER_ACCEPTED event arrives.
func (n *Node) onAcceptEvent(ev *Event) {
	if ev.Kind != IROH_EVENT_ASTER_ACCEPTED {
		return
	}

	conn := &Connection{
		handle:  ev.Handle,
		runtime: n.runtime,
	}

	alpn := cDataToGoBytes(ev.DataPtr, ev.DataLen)

	if ev.Buffer != 0 {
		n.runtime.ReleaseBuffer(ev.Buffer)
	}

	select {
	case n.connections <- AcceptedAster{ALPN: alpn, Connection: conn}:
	default:
		// Channel full; drop the event.
	}
}
