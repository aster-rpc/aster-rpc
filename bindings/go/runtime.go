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
	"bytes"
	"context"
	"fmt"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"
)

// EventKind is the type of an event kind, mirroring iroh_event_kind_t.
type EventKind uint32

// Event represents an event emitted by the Rust completion queue.
type Event struct {
	Kind       uint32
	Status     uint32
	Operation  uint64
	Handle     uint64
	Related    uint64
	UserData   uint64
	DataPtr    unsafe.Pointer
	DataLen    uintptr
	Buffer     uint64
	ErrorCode  int32
	Flags      uint32
}

// String returns a human-readable description of the event.
func (e *Event) String() string {
	return fmt.Sprintf("Event{kind=%d status=%d op=%d handle=%d err=%d}",
		e.Kind, e.Status, e.Operation, e.Handle, e.ErrorCode)
}

// cDataToGoBytes copies C event data (ptr + len) to a new Go []byte.
func cDataToGoBytes(ptr unsafe.Pointer, n uintptr) []byte {
	if ptr == nil || n == 0 {
		return nil
	}
	// Create a Go slice that views the C data.
	cslice := unsafe.Slice((*byte)(ptr), n)
	return bytes.Clone(cslice)
}

// RuntimeConfig controls how the runtime is created.
type RuntimeConfig struct {
	WorkerThreads      uint32
	EventQueueCapacity uint32
}

// DefaultRuntimeConfig returns a sensible default configuration.
func DefaultRuntimeConfig() RuntimeConfig {
	return RuntimeConfig{
		WorkerThreads:      1,
		EventQueueCapacity: 256,
	}
}

// Runtime owns a tokio runtime and drives the completion queue.
// It starts a background poller goroutine that dispatches events
// to registered channels.
type Runtime struct {
	handle uint64

	// closed signals the poller to exit.
	closed atomic.Bool

	// ops holds the mapping from operation ID to event channel.
	// Only accessed by the poller goroutine.
	ops map[uint64]chan<- *Event

	// mu protects ops during cancel/close.
	mu sync.Mutex

	// acceptHandlers maps node handle to the Node that owns its accept loop.
	// Only accessed by the poller goroutine.
	acceptHandlers map[uint64]*Node

	pollCh chan struct{} // wakes the poller

	wg sync.WaitGroup // tracks the poller goroutine
}

// NewRuntime creates a new Runtime with the given config.
// The background poller goroutine starts immediately.
func NewRuntime(ctx context.Context, cfg RuntimeConfig) (*Runtime, error) {
	config := C.iroh_runtime_config_t{
		struct_size:          16,
		worker_threads:       C.uint32_t(cfg.WorkerThreads),
		event_queue_capacity: C.uint32_t(cfg.EventQueueCapacity),
		reserved:            0,
	}

	var handle C.uint64_t
	r := C.iroh_runtime_new(&config, &handle)
	if r != 0 {
		return nil, fmt.Errorf("iroh_runtime_new: %w", Error(r))
	}

	runtime := &Runtime{
		handle:          uint64(handle),
		ops:             make(map[uint64]chan<- *Event),
		acceptHandlers:  make(map[uint64]*Node),
		pollCh:          make(chan struct{}, 1),
	}

	runtime.wg.Add(1)
	go runtime.pollLoop()

	return runtime, nil
}

// Close shuts down the runtime, signals the poller to exit,
// and waits for it to finish. All pending operations will have
// their channels closed.
func (r *Runtime) Close() error {
	if r.closed.Swap(true) {
		return nil // already closed
	}

	// Wake the poller so it exits promptly.
	select {
	case r.pollCh <- struct{}{}:
	default:
	}

	r.wg.Wait()

	r.mu.Lock()
	for op, ch := range r.ops {
		close(ch)
		delete(r.ops, op)
	}
	r.mu.Unlock()

	err := C.iroh_runtime_close(C.uint64_t(r.handle))
	if err != 0 {
		return fmt.Errorf("iroh_runtime_close: %w", Error(err))
	}

	return nil
}

// Register adds ch as the event sink for op. The channel is closed
// when the operation reaches a terminal state.
func (r *Runtime) Register(op uint64, ch chan<- *Event) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.ops[op] = ch
}

// Unregister removes the channel for op. Returns the channel if it was registered.
func (r *Runtime) Unregister(op uint64) {
	r.mu.Lock()
	defer r.mu.Unlock()
	delete(r.ops, op)
}

// pollLoop runs the background event poller. It exits when r.closed is true.
func (r *Runtime) pollLoop() {
	defer r.wg.Done()

	for !r.closed.Load() {
		var events [64]C.iroh_event_t
		n := C.iroh_poll_events(C.uint64_t(r.handle), &events[0], 64, 10)
		if n < 0 {
			// Negative return indicates an error; back off.
			time.Sleep(10 * time.Millisecond)
			continue
		}

		r.dispatchBatch(events[:int(n)])

		// If we got fewer than the batch size, there may be more events
		// shortly. Don't sleep before the next poll.
	}

	// Drain any remaining events before exiting.
	var events [64]C.iroh_event_t
	for {
		n := C.iroh_poll_events(C.uint64_t(r.handle), &events[0], 64, 0)
		if n == 0 {
			break
		}
		r.dispatchBatch(events[:int(n)])
	}
}

// dispatchBatch dispatches each event to its registered channel.
func (r *Runtime) dispatchBatch(events []C.iroh_event_t) {
	for i := range events {
		ev := &events[i]

		// Marshal into the Go event type.
		goEv := &Event{
			Kind:     uint32(ev.kind),
			Status:   uint32(ev.status),
			Operation: uint64(ev.operation),
			Handle:   uint64(ev.handle),
			Related:  uint64(ev.related),
			UserData:  uint64(ev.user_data),
			DataPtr:   unsafe.Pointer(ev.data_ptr),
			DataLen:   uintptr(ev.data_len),
			Buffer:    uint64(ev.buffer),
			ErrorCode: int32(ev.error_code),
			Flags:     uint32(ev.flags),
		}

		// Check if this is an inbound accept event routed to a registered Node.
		if goEv.Kind == IROH_EVENT_ASTER_ACCEPTED {
			r.mu.Lock()
			node, ok := r.acceptHandlers[goEv.Handle]
			r.mu.Unlock()
			if ok {
				node.onAcceptEvent(goEv)
			}
			continue
		}

		// Normal op-based dispatch.
		r.mu.Lock()
		ch, ok := r.ops[uint64(ev.operation)]
		if ok {
			delete(r.ops, uint64(ev.operation))
		}
		r.mu.Unlock()

		if !ok {
			continue
		}

		select {
		case ch <- goEv:
		default:
			// Channel full or closed; drop the event.
		}
	}
}

// registerAcceptHandler registers node as the handler for IROH_EVENT_ASTER_ACCEPTED
// events whose handle matches node.handle.
func (r *Runtime) registerAcceptHandler(nodeHandle uint64, node *Node) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.acceptHandlers[nodeHandle] = node
}

// removeAcceptHandler removes the accept handler for the given node handle.
func (r *Runtime) removeAcceptHandler(nodeHandle uint64) {
	r.mu.Lock()
	defer r.mu.Unlock()
	delete(r.acceptHandlers, nodeHandle)
}

// Poll attempts to receive a single event for op, with a context timeout.
// This is an alternative to channel-based dispatch for synchronous use.
func (r *Runtime) Poll(ctx context.Context, op uint64) (*Event, error) {
	ch := make(chan *Event, 1)
	r.Register(op, ch)
	defer r.Unregister(op)

	select {
	case ev := <-ch:
		return ev, nil
	case <-ctx.Done():
		return nil, ctx.Err()
	}
}

// Cancel submits a cancellation request for op.
func (r *Runtime) Cancel(op uint64) error {
	r.Unregister(op) // Remove channel so cancelled event doesn't get dispatched.
	r.mu.Lock()
	delete(r.ops, op)
	r.mu.Unlock()

	err := C.iroh_operation_cancel(C.uint64_t(r.handle), C.uint64_t(op))
	if err != 0 {
		return fmt.Errorf("iroh_operation_cancel: %w", Error(err))
	}
	return nil
}

// ReleaseBuffer releases a buffer allocated by Rust.
func (r *Runtime) ReleaseBuffer(buffer uint64) error {
	err := C.iroh_buffer_release(C.uint64_t(r.handle), C.uint64_t(buffer))
	if err != 0 {
		return fmt.Errorf("iroh_buffer_release: %w", Error(err))
	}
	return nil
}

// AddNodeAddr adds a peer's address to the given endpoint's address book,
// enabling connection without prior address exchange.
func (r *Runtime) AddNodeAddr(endpointHandle uint64, addr *NodeAddr) error {
	if addr == nil {
		return fmt.Errorf("addr is nil")
	}

	// Encode endpoint_id
	var endpointIDPtr *C.uint8_t
	var endpointIDLen C.uintptr_t
	if len(addr.EndpointID) > 0 {
		idBytes := []byte(addr.EndpointID)
		cBuf := C.malloc(C.size_t(len(idBytes)))
		C.memcpy(cBuf, unsafe.Pointer(&idBytes[0]), C.size_t(len(idBytes)))
		endpointIDPtr = (*C.uint8_t)(cBuf)
		endpointIDLen = C.uintptr_t(len(idBytes))
	}

	// Encode relay_url
	var relayURLPtr *C.uint8_t
	var relayURLLen C.uintptr_t
	if addr.RelayURL != "" {
		urlBytes := []byte(addr.RelayURL)
		cBuf := C.malloc(C.size_t(len(urlBytes)))
		C.memcpy(cBuf, unsafe.Pointer(&urlBytes[0]), C.size_t(len(urlBytes)))
		relayURLPtr = (*C.uint8_t)(cBuf)
		relayURLLen = C.uintptr_t(len(urlBytes))
	}

	// Encode direct_addresses
	daItems := make([]C.struct_iroh_bytes_t, len(addr.DirectAddresses))
	for i, da := range addr.DirectAddresses {
		b := []byte(da)
		cBuf := C.malloc(C.size_t(len(b)))
		C.memcpy(cBuf, unsafe.Pointer(&b[0]), C.size_t(len(b)))
		daItems[i].ptr = (*C.uint8_t)(cBuf)
		daItems[i].len = C.uintptr_t(len(b))
	}

	cAddr := C.struct_iroh_node_addr_t{
		endpoint_id: C.struct_iroh_bytes_t{
			ptr: endpointIDPtr,
			len: endpointIDLen,
		},
		relay_url: C.struct_iroh_bytes_t{
			ptr: relayURLPtr,
			len: relayURLLen,
		},
		direct_addresses: C.struct_iroh_bytes_list_t{
			items: &daItems[0],
			len:   C.uintptr_t(len(addr.DirectAddresses)),
		},
	}

	err := C.iroh_add_node_addr(C.uint64_t(r.handle), C.uint64_t(endpointHandle), cAddr)
	if err != 0 {
		return fmt.Errorf("iroh_add_node_addr: %w", Error(err))
	}
	return nil
}

// ReleaseString releases a string allocated by Rust.
// data must be the pointer returned from Rust, and len must be its byte length.
func ReleaseString(data unsafe.Pointer, len uintptr) error {
	err := C.iroh_string_release((*C.uint8_t)(data), C.uintptr_t(len))
	if err != 0 {
		return fmt.Errorf("iroh_string_release: %w", Error(err))
	}
	return nil
}

// ─── Sync FFI helpers ─────────────────────────────────────────────────────

// Version returns the ABI version of the native library.
func Version() (major, minor, patch uint32) {
	major = uint32(C.iroh_abi_version_major())
	minor = uint32(C.iroh_abi_version_minor())
	patch = uint32(C.iroh_abi_version_patch())
	return
}

// Error converts an iroh status code to a Go error.
// It returns nil for IROH_STATUS_OK.
// The returned error supports errors.Is and errors.As for status code matching.
func Error(code C.int) error {
	return wrapError(code)
}

// StatusName returns the string name of a status code.
func StatusName(code uint32) string {
	name := C.iroh_status_name(code)
	if name == nil {
		return "UNKNOWN"
	}
	return C.GoString(name)
}

// LastErrorMessage reads the most recent error message from the runtime.
// Returns an empty string if there is no current error.
func (r *Runtime) LastErrorMessage() string {
	var buf [512]byte
	n := C.iroh_last_error_message((*C.uint8_t)(unsafe.Pointer(&buf[0])), 512)
	if n == 0 {
		return ""
	}
	return C.GoString((*C.char)(unsafe.Pointer(&buf[0])))
}
