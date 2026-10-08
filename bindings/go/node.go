//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"encoding/hex"
	"fmt"
	"sync"
	"time"
	"unsafe"
)

type nodeHealthSettings struct {
	host    string
	port    uint16
	enabled bool
}

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

	// ownsRuntime: started by StartNode; Close releases the runtime reference.
	ownsRuntime bool
	// Immutable health-only snapshot; nil identifies a node without source config.
	healthSettings *nodeHealthSettings
	admissions     chan *AdmissionRequest
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

	r := C.aster_node_id(
		C.uint64_t(n.runtime.handle),
		C.uint64_t(n.handle),
		(*C.uint8_t)(unsafe.Pointer(&buf[0])),
		64,
		&outLen,
	)
	if r != 0 {
		return "", fmt.Errorf("aster_node_id: %w", Error(r))
	}
	if outLen == 0 {
		return "", nil
	}
	return string(buf[:outLen]), nil
}

// NodeAddr returns the structured address of this node.
func (n *Node) NodeAddr() (*NodeAddr, error) {
	var addrBuf [4096]byte
	var addr C.aster_node_addr_t

	r := C.aster_node_addr_info(
		C.uint64_t(n.runtime.handle),
		C.uint64_t(n.handle),
		(*C.uint8_t)(unsafe.Pointer(&addrBuf[0])),
		4096,
		&addr,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_node_addr_info: %w", Error(r))
	}

	nodeAddr := &NodeAddr{
		DirectAddresses: []string{},
	}

	// Fields of addr point into addrBuf (borrowed); read via the cgo struct.
	nodeAddr.EndpointID = string(viewBytes(addr.endpoint_id))
	nodeAddr.RelayURL = string(viewBytes(addr.relay_url))
	if addr.direct_addresses.items != nil && addr.direct_addresses.len > 0 {
		items := unsafe.Slice(addr.direct_addresses.items, int(addr.direct_addresses.len))
		for _, item := range items {
			if b := viewBytes(item); len(b) > 0 {
				nodeAddr.DirectAddresses = append(nodeAddr.DirectAddresses, string(b))
			}
		}
	}

	return nodeAddr, nil
}

// ExportSecretKey exports this node's 32-byte secret key seed.
func (n *Node) ExportSecretKey() ([]byte, error) {
	var buf [32]byte
	var outLen C.uintptr_t

	r := C.aster_node_export_secret_key(
		C.uint64_t(n.runtime.handle),
		C.uint64_t(n.handle),
		(*C.uint8_t)(unsafe.Pointer(&buf[0])),
		32,
		&outLen,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_node_export_secret_key: %w", Error(r))
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
	if n.admissions != nil {
		n.runtime.unregisterInbound(n.handle)
		n.mu.Lock()
		close(n.admissions)
		n.mu.Unlock()
	}

	if !n.ownsRuntime {
		// Legacy factories: free the handle; the runtime is left running.
		r := C.aster_node_free(C.uint64_t(n.runtime.handle), C.uint64_t(n.handle))
		if r != 0 {
			return fmt.Errorf("aster_node_free: %w", Error(r))
		}
		return nil
	}

	// Graceful close, bounded; then drop our runtime reference.
	var op C.aster_operation_t
	if C.aster_node_close(C.uint64_t(n.runtime.handle), C.uint64_t(n.handle), 0, &op) == 0 {
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		if ev, err := n.runtime.await(ctx, uint64(op)); err == nil {
			n.runtime.releaseEventBuffer(ev)
		}
		cancel()
	} else {
		C.aster_node_free(C.uint64_t(n.runtime.handle), C.uint64_t(n.handle))
	}
	return n.runtime.release()
}

// ─── Factory methods ─────────────────────────────────────────────────────────

// Memory creates an in-memory node with all protocols enabled, on a runtime
// of its own.
//
// A convenience wrapper over StartNode (ABI 4.0 removed aster_node_memory*
// and aster_node_persistent); prefer StartNode, which also takes the full
// AsterConfig and a Gate. Close frees the node and leaves its runtime
// running (Node.Runtime().Close() stops it).
func Memory(ctx context.Context) (*Node, error) {
	return MemoryWithAlpns(ctx, nil)
}

// MemoryWithAlpns creates an in-memory node that accepts connections on the
// given ALPNs. See Memory.
func MemoryWithAlpns(ctx context.Context, alpns []string) (*Node, error) {
	return createNode(ctx, alpns, "")
}

// Persistent creates a persistent node at the given path with all protocols.
// See Memory.
func Persistent(ctx context.Context, path string) (*Node, error) {
	return PersistentWithAlpns(ctx, path, nil)
}

// PersistentWithAlpns creates a persistent node at path that accepts
// connections on the given ALPNs. See Memory.
func PersistentWithAlpns(ctx context.Context, path string, alpns []string) (*Node, error) {
	if path == "" {
		return nil, fmt.Errorf("persistent node: empty path: %w", ErrInvalidArgument)
	}
	return createNode(ctx, alpns, path)
}

func createNode(ctx context.Context, alpns []string, path string) (*Node, error) {
	runtime, err := NewRuntime(ctx, DefaultRuntimeConfig())
	if err != nil {
		return nil, fmt.Errorf("create runtime: %w", err)
	}
	cfg := DefaultConfig()
	cfg.ALPNs = alpns
	cfg.StoragePath = path
	node, err := startNodeOn(ctx, runtime, cfg, NodeOptions{})
	if err != nil {
		runtime.Close()
		return nil, fmt.Errorf("node create: %w", err)
	}
	// Legacy shape: Close frees the handle and leaves the runtime running.
	node.ownsRuntime = false
	return node, nil
}

// onAcceptEvent is called by the runtime when an ASTER_EVENT_ASTER_ACCEPTED event arrives.
func (n *Node) onAcceptEvent(ev *Event) {
	if ev.Kind != ASTER_EVENT_ASTER_ACCEPTED {
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
