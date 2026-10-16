//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include "aster_ffi.h"

// This binding is written against C ABI 7.x: building it with another
// major's aster_ffi.h is a compile error, not a runtime surprise.
#if ASTER_ABI_VERSION_MAJOR != 7
#error "sdk.getaster.now/go needs aster_ffi.h from C ABI 7.x (libaster_transport_ffi 7.0 or later 7.x)"
#endif
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

// EventKind is the type of an event kind, mirroring aster_event_kind_t.
type EventKind uint32

// Event represents an event emitted by the Rust completion queue.
type Event struct {
	Kind      uint32
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
	WorkerThreads         uint32
	EventQueueCapacity    uint32
	MaxInflightOperations uint32
}

// DefaultRuntimeConfig returns a sensible default configuration.
func DefaultRuntimeConfig() RuntimeConfig {
	return RuntimeConfig{
		WorkerThreads:         1,
		EventQueueCapacity:    256,
		MaxInflightOperations: 4096,
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
	// Guarded by mu; dispatch and cancellation transfer ownership under the lock.
	ops map[uint64]chan<- *Event

	// mu protects ops during cancel/close.
	mu sync.Mutex

	// acceptHandlers maps node handle to the Node that owns its accept loop.
	acceptHandlers map[uint64]*Node

	// inbound maps a node/endpoint handle to a handler for events that are not
	// tied to an operation (admission hooks). Guarded by mu.
	inbound map[uint64]func(*Event)

	// pending holds completions that arrived before their operation was
	// registered (an op can complete before Register runs). Guarded by mu.
	pending map[uint64]*Event

	// abandoned holds cancelled operations whose late completion is dropped.
	abandoned map[uint64]struct{}

	// refs counts owners (nodes, gates) sharing this runtime; the runtime is
	// closed when the last one releases it.
	refs atomic.Int32

	pollCh chan struct{} // wakes the poller

	wg sync.WaitGroup // tracks the poller goroutine
}

// NewRuntime creates a new Runtime with the given config.
// The background poller goroutine starts immediately.
func NewRuntime(ctx context.Context, cfg RuntimeConfig) (*Runtime, error) {
	config := C.aster_runtime_config_t{
		struct_size:             C.uint32_t(unsafe.Sizeof(C.aster_runtime_config_t{})),
		worker_threads:          C.uint32_t(cfg.WorkerThreads),
		event_queue_capacity:    C.uint32_t(cfg.EventQueueCapacity),
		max_inflight_operations: C.uint32_t(cfg.MaxInflightOperations),
	}

	if err := CheckABI(); err != nil {
		return nil, err
	}

	var handle C.uint64_t
	r := C.aster_runtime_new(C.ASTER_ABI_VERSION_MAJOR, &config, &handle)
	if r != C.ASTER_STATUS_OK {
		return nil, fmt.Errorf("aster_runtime_new: %w", Error(r))
	}

	runtime := &Runtime{
		handle:         uint64(handle),
		ops:            make(map[uint64]chan<- *Event),
		acceptHandlers: make(map[uint64]*Node),
		inbound:        make(map[uint64]func(*Event)),
		pending:        make(map[uint64]*Event),
		abandoned:      make(map[uint64]struct{}),
		pollCh:         make(chan struct{}, 1),
	}
	runtime.refs.Store(1)

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
	for op, ev := range r.pending {
		r.releaseEventBuffer(ev)
		delete(r.pending, op)
	}
	r.mu.Unlock()

	err := C.aster_runtime_close(C.uint64_t(r.handle))
	if err != 0 {
		return fmt.Errorf("aster_runtime_close: %w", Error(err))
	}

	return nil
}

// retain adds an owner to the runtime (see release).
func (r *Runtime) retain() *Runtime {
	r.refs.Add(1)
	return r
}

// release drops an owner; the last release closes the runtime.
func (r *Runtime) release() error {
	if r.refs.Add(-1) == 0 {
		return r.Close()
	}
	return nil
}

// Handle returns the native runtime handle.
func (r *Runtime) Handle() uint64 { return r.handle }

// Register adds ch as the event sink for op. The channel should be buffered:
// the completion is delivered without blocking. A completion that arrived
// before Register is delivered immediately.
func (r *Runtime) Register(op uint64, ch chan<- *Event) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.closed.Load() {
		if ev, ok := r.pending[op]; ok {
			delete(r.pending, op)
			r.releaseEventBuffer(ev)
		}
		close(ch)
		return
	}
	if ev, ok := r.pending[op]; ok {
		delete(r.pending, op)
		select {
		case ch <- ev:
		default:
			r.releaseEventBuffer(ev)
		}
		return
	}
	r.ops[op] = ch
}

// Unregister removes the channel for op and forgets an undelivered completion.
func (r *Runtime) Unregister(op uint64) {
	r.mu.Lock()
	defer r.mu.Unlock()
	delete(r.ops, op)
	if ev, ok := r.pending[op]; ok {
		delete(r.pending, op)
		r.releaseEventBuffer(ev)
	}
}

// registerInbound routes operation-less events (admission hooks) raised for
// handle to fn, which runs on the poller goroutine and must not block.
func (r *Runtime) registerInbound(handle uint64, fn func(*Event)) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.inbound[handle] = fn
}

func (r *Runtime) unregisterInbound(handle uint64) {
	r.mu.Lock()
	defer r.mu.Unlock()
	delete(r.inbound, handle)
}

func (r *Runtime) releaseEventBuffer(ev *Event) {
	if ev != nil && ev.Buffer != 0 {
		C.aster_buffer_release(C.uint64_t(r.handle), C.uint64_t(ev.Buffer))
		ev.Buffer = 0
	}
}

// takePayload copies an event's payload into Go memory and releases its
// native buffer.
func (r *Runtime) takePayload(ev *Event) []byte {
	data := cDataToGoBytes(ev.DataPtr, ev.DataLen)
	r.releaseEventBuffer(ev)
	ev.DataPtr, ev.DataLen = nil, 0
	return data
}

// eventError turns an unexpected completion (usually ASTER_EVENT_ERROR) into
// an error carrying the native message and status, releasing its buffer.
func (r *Runtime) eventError(ev *Event, what string) error {
	detail := string(r.takePayload(ev))
	status := int32(ev.Status)
	if status == 0 {
		status = int32(ev.ErrorCode)
	}
	if status == 0 {
		status = ASTER_STATUS_INTERNAL
	}
	msg := fmt.Sprintf("%s: event %d", what, ev.Kind)
	if detail != "" {
		msg += ": " + detail
	}
	return &irohErrorWithSentinel{errorf(status, "%s", msg), statusToError[status]}
}

// await submits nothing; it waits for the completion of op (already
// started) and cancels the native operation if ctx ends first.
func (r *Runtime) await(ctx context.Context, op uint64) (*Event, error) {
	ch := make(chan *Event, 1)
	r.Register(op, ch)
	select {
	case ev, ok := <-ch:
		if !ok || ev == nil {
			return nil, fmt.Errorf("runtime closed: %w", ErrAlreadyClosed)
		}
		return ev, nil
	case <-ctx.Done():
		_ = r.cancelAndDrain(op, ch, nil)
		return nil, ctx.Err()
	}
}

// pollLoop runs the background event poller. It exits when r.closed is true.
func (r *Runtime) pollLoop() {
	defer r.wg.Done()

	for !r.closed.Load() {
		var events [64]C.aster_event_t
		n := C.aster_poll_events(C.uint64_t(r.handle), &events[0], 64, 10)
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
	var events [64]C.aster_event_t
	for {
		n := C.aster_poll_events(C.uint64_t(r.handle), &events[0], 64, 0)
		if n == 0 {
			break
		}
		r.dispatchBatch(events[:int(n)])
	}
}

// dispatchBatch dispatches each event to its registered channel.
func (r *Runtime) dispatchBatch(events []C.aster_event_t) {
	for i := range events {
		ev := &events[i]

		// Marshal into the Go event type.
		goEv := &Event{
			Kind:      uint32(ev.kind),
			Status:    uint32(ev.status),
			Operation: uint64(ev.operation),
			Handle:    uint64(ev.handle),
			Related:   uint64(ev.related),
			UserData:  uint64(ev.user_data),
			DataPtr:   unsafe.Pointer(ev.data_ptr),
			DataLen:   uintptr(ev.data_len),
			Buffer:    uint64(ev.buffer),
			ErrorCode: int32(ev.error_code),
			Flags:     uint32(ev.flags),
		}

		// Admission hooks are not tied to an operation: route by handle.
		if goEv.Kind == ASTER_EVENT_HOOK_BEFORE_CONNECT || goEv.Kind == ASTER_EVENT_HOOK_AFTER_CONNECT {
			r.mu.Lock()
			fn, ok := r.inbound[goEv.Handle]
			r.mu.Unlock()
			if ok {
				fn(goEv)
			}
			r.releaseEventBuffer(goEv)
			continue
		}

		// Check if this is an inbound accept event routed to a registered Node.
		if goEv.Kind == ASTER_EVENT_ASTER_ACCEPTED {
			r.mu.Lock()
			node, ok := r.acceptHandlers[goEv.Handle]
			r.mu.Unlock()
			if ok {
				node.onAcceptEvent(goEv)
			}
			continue
		}

		r.dispatchCompletion(goEv)
	}
}

// dispatchCompletion transfers ownership while holding mu. Cancellation can
// then detach the channel and drain it without racing an in-flight send.
func (r *Runtime) dispatchCompletion(ev *Event) {
	r.mu.Lock()
	defer r.mu.Unlock()
	op := ev.Operation
	if ch, ok := r.ops[op]; ok {
		delete(r.ops, op)
		select {
		case ch <- ev:
		default:
			r.releaseEventBuffer(ev)
		}
	} else if _, gone := r.abandoned[op]; gone {
		delete(r.abandoned, op)
		r.releaseEventBuffer(ev)
	} else if op != 0 && ev.Kind != ASTER_EVENT_OPERATION_CANCELLED {
		r.pending[op] = ev
	} else {
		r.releaseEventBuffer(ev)
	}
}

// registerAcceptHandler registers node as the handler for ASTER_EVENT_ASTER_ACCEPTED
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
	case ev, ok := <-ch:
		if !ok || ev == nil {
			return nil, fmt.Errorf("runtime closed: %w", ErrAlreadyClosed)
		}
		return ev, nil
	case <-ctx.Done():
		return nil, ctx.Err()
	}
}

// Cancel submits a cancellation request for op and stops future dispatch.
// The caller still owns any event already delivered to its registered channel
// and must release that event's buffer and returned native handles.
func (r *Runtime) Cancel(op uint64) error {
	return r.cancelAndDrain(op, nil, nil)
}

// cancelAndCollect keeps the operation registered until its sole terminal
// event arrives. A native completion may already be committed but not polled;
// callers owning handles must collect it even when cancellation is too late.
func (r *Runtime) cancelAndCollect(op uint64, ch <-chan *Event) *Event {
	C.aster_operation_cancel(C.uint64_t(r.handle), C.uint64_t(op))
	return <-ch
}

// cancelAndDrain takes back a completion already delivered to the caller's
// channel before cancelling. cleanup runs outside mu because it can call FFI.
func (r *Runtime) cancelAndDrain(op uint64, ch <-chan *Event, cleanup func(*Event)) error {
	ev := r.detachCompletion(op, ch)
	if ev != nil {
		if cleanup != nil {
			cleanup(ev)
		}
		r.releaseEventBuffer(ev)
	}
	err := C.aster_operation_cancel(C.uint64_t(r.handle), C.uint64_t(op))
	if err != 0 {
		return fmt.Errorf("aster_operation_cancel: %w", Error(err))
	}
	return nil
}

func (r *Runtime) detachCompletion(op uint64, ch <-chan *Event) *Event {
	r.mu.Lock()
	defer r.mu.Unlock()
	delete(r.ops, op)
	ev := r.pending[op]
	delete(r.pending, op)
	if ev == nil && ch != nil {
		select {
		case ev = <-ch:
		default:
		}
	}
	if ev == nil {
		r.abandoned[op] = struct{}{}
	}
	return ev
}

// ReleaseBuffer releases a buffer allocated by Rust.
func (r *Runtime) ReleaseBuffer(buffer uint64) error {
	err := C.aster_buffer_release(C.uint64_t(r.handle), C.uint64_t(buffer))
	if err != 0 {
		return fmt.Errorf("aster_buffer_release: %w", Error(err))
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
	daItems := make([]C.struct_aster_bytes_t, len(addr.DirectAddresses))
	for i, da := range addr.DirectAddresses {
		b := []byte(da)
		cBuf := C.malloc(C.size_t(len(b)))
		C.memcpy(cBuf, unsafe.Pointer(&b[0]), C.size_t(len(b)))
		daItems[i].ptr = (*C.uint8_t)(cBuf)
		daItems[i].len = C.uintptr_t(len(b))
	}

	cAddr := C.struct_aster_node_addr_t{
		endpoint_id: C.struct_aster_bytes_t{
			ptr: endpointIDPtr,
			len: endpointIDLen,
		},
		relay_url: C.struct_aster_bytes_t{
			ptr: relayURLPtr,
			len: relayURLLen,
		},
		direct_addresses: C.struct_aster_bytes_list_t{
			items: &daItems[0],
			len:   C.uintptr_t(len(addr.DirectAddresses)),
		},
	}

	err := C.aster_add_node_addr(C.uint64_t(r.handle), C.uint64_t(endpointHandle), cAddr)
	if err != 0 {
		return fmt.Errorf("aster_add_node_addr: %w", Error(err))
	}
	return nil
}

// ReleaseString releases a string allocated by Rust.
// data must be the pointer returned from Rust, and len must be its byte length.
func ReleaseString(data unsafe.Pointer, len uintptr) error {
	err := C.aster_string_release((*C.uint8_t)(data), C.uintptr_t(len))
	if err != 0 {
		return fmt.Errorf("aster_string_release: %w", Error(err))
	}
	return nil
}

// ─── Sync FFI helpers ─────────────────────────────────────────────────────

// ABIVersionMajor is the ABI major version this binding was written against
// (ASTER_ABI_VERSION_MAJOR from aster_ffi.h).
const ABIVersionMajor = uint32(C.ASTER_ABI_VERSION_MAJOR)

// Version returns the ABI version of the loaded native library, unpacked from
// aster_abi_version() ((major << 16) | (minor << 8) | patch).
func Version() (major, minor, patch uint32) {
	v := uint32(C.aster_abi_version())
	return v >> 16, (v >> 8) & 0xff, v & 0xff
}

// ABIMinorRequired is the lowest ABI minor version (within ABIVersionMajor)
// this binding runs against. ABI 7.0 swapped aster_hook_failure_mode_t so
// 0 = FAIL_CLOSED (6.0 grew aster_config_t to 288 bytes); 7.1 added the audit
// outbox acknowledgement log (aster_audit_outbox_acknowledgements); 7.2 added
// aster_server_drain (AsterServer.Drain); 7.3 added the cluster network
// settings beside aster_config_t (aster_config_set_network: BindAddrV6,
// AdvertiseAddrs, RelayCACerts, Discovery); 7.4 the transfer metrics
// (Blobs.TransferStats, fetch report v2, topology QuicTraffic / BlobTransfer)
// and the [metrics] peer labels (aster_config_set_metrics); 7.5 the handler
// call identity (aster_dispatch_identity: CallInfo.PeerID is the transport
// peer, CallInfo.Principal the Authenticator's principal). This binding needs
// native ABI 7.5 or a later 7.x.
const ABIMinorRequired = uint32(5)

var (
	abiOnce sync.Once
	abiErr  error
)

// acceptABI runs the ABI handshake once per process: CheckABI, then
// aster_abi_accept, without which the library refuses to fill an
// aster_config_t (ABI 6.0).
func acceptABI() error {
	abiOnce.Do(func() {
		if abiErr = CheckABI(); abiErr != nil {
			return
		}
		if r := C.aster_abi_accept(C.ASTER_ABI_VERSION_MAJOR); r != C.ASTER_STATUS_OK {
			abiErr = fmt.Errorf("aster_abi_accept: %w", Error(C.int(r)))
		}
	})
	return abiErr
}

// CheckABI returns an error if the loaded native library's ABI major version
// differs from the one this binding was compiled against, or its minor
// version is below ABIMinorRequired.
func CheckABI() error {
	major, minor, patch := Version()
	if major != ABIVersionMajor || minor < ABIMinorRequired {
		return fmt.Errorf("aster: native library ABI %d.%d.%d, binding expects %d.%d or newer: %w",
			major, minor, patch, ABIVersionMajor, ABIMinorRequired, Error(C.ASTER_STATUS_ABI_MISMATCH))
	}
	return nil
}

// Error converts an iroh status code to a Go error.
// It returns nil for ASTER_STATUS_OK.
// The returned error supports errors.Is and errors.As for status code matching.
func Error(code C.int) error {
	return wrapError(code)
}

// StatusName returns the string name of a status code.
func StatusName(code int32) string {
	name := C.aster_status_name(C.int32_t(code))
	if name == nil {
		return "UNKNOWN"
	}
	return C.GoString(name)
}

// LastErrorMessage reads the most recent error message from the runtime.
// Returns an empty string if there is no current error.
//
// The message is per OS thread, and a goroutine can move between threads
// between two cgo calls, so this can miss the message of the call before it.
// Prefer the error a failing call returns: it already carries the message,
// read on the calling thread.
func (r *Runtime) LastErrorMessage() string {
	var buf [512]byte
	n := C.aster_last_error_message((*C.uint8_t)(unsafe.Pointer(&buf[0])), 512)
	if n == 0 {
		return ""
	}
	return C.GoString((*C.char)(unsafe.Pointer(&buf[0])))
}
