//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"sync"
	"sync/atomic"
	"unsafe"
)

// Dynamic dispatch (ABI 3.5, aster_server_* / aster_dispatch_*): Rust's
// aster::rpc::Server serves the registered services -- it decodes headers,
// routes, runs Gate 3 (requires), keeps sessions per connection, enforces
// deadlines and per-method frame limits and serves aster.ops.NodeInfo -- and
// hands each call that passed to Go as one CALL event, answered by call id
// with payloads and AEv1 RpcStatus bytes (Rust frames and compresses).

// dispatch event kinds (aster_dispatch_event_kind_t).
const (
	dispatchEventCall             = C.ASTER_DISPATCH_EVENT_CALL
	dispatchEventCancelled        = C.ASTER_DISPATCH_EVENT_CANCELLED
	dispatchEventResponseWritable = C.ASTER_DISPATCH_EVENT_RESPONSE_WRITABLE
	dispatchEventResponseClosed   = C.ASTER_DISPATCH_EVENT_RESPONSE_CLOSED
	dispatchEventSessionClosed    = C.ASTER_DISPATCH_EVENT_SESSION_CLOSED
	dispatchEventConnectionClosed = C.ASTER_DISPATCH_EVENT_CONNECTION_CLOSED
	dispatchEventNonRPC           = C.ASTER_DISPATCH_EVENT_NON_RPC
)

// aster_dispatch_event_t.flags (ASTER_DISPATCH_FLAG_*).
const (
	dispatchFlagFirstIsEnd        = C.ASTER_DISPATCH_FLAG_FIRST_IS_END
	dispatchFlagNewSession        = C.ASTER_DISPATCH_FLAG_NEW_SESSION
	dispatchFlagIrohAuthenticated = C.ASTER_DISPATCH_FLAG_IROH_AUTHENTICATED
	dispatchFlagHasMetadata       = C.ASTER_DISPATCH_FLAG_HAS_METADATA
	dispatchFlagHasAttributes     = C.ASTER_DISPATCH_FLAG_HAS_ATTRIBUTES
)

// aster_server_option_t.
const (
	serverOptionBuiltinNodeInfo          = C.ASTER_SERVER_OPTION_BUILTIN_NODE_INFO
	serverOptionBuiltinTopology          = C.ASTER_SERVER_OPTION_BUILTIN_TOPOLOGY
	serverOptionMaxSessionsPerConnection = C.ASTER_SERVER_OPTION_MAX_SESSIONS_PER_CONNECTION
	serverOptionHandlerTimeoutMs         = C.ASTER_SERVER_OPTION_HANDLER_TIMEOUT_MS
)

const timeoutInfinite = C.ASTER_TIMEOUT_INFINITE

// errCallGone: the call is over (answered, cancelled, deadline, connection
// lost); nothing more can be sent on it.
var errCallGone = &RpcError{Code: StatusCancelled, Message: "call is over"}

// dispatchServer wraps one aster_server_t.
type dispatchServer struct {
	rt     C.uint64_t
	handle C.uint64_t

	mu      sync.Mutex
	waiters map[uint64]chan bool // call id -> RESPONSE_WRITABLE (true) / CLOSED (false)
	closed  bool

	queueFullHits atomic.Uint64
}

type serverSettings struct {
	disableNodeInfo          bool
	topology                 bool
	maxSessionsPerConnection uint64
	handlerTimeoutMs         uint64
	setHandlerTimeout        bool
	ringCapacity             uint32
}

// startDispatchServer registers every service JSON (in order: the index is
// the CALL event's service_idx) and starts serving on node.
func startDispatchServer(node *Node, serviceJSON []string, s serverSettings) (*dispatchServer, error) {
	rt := C.uint64_t(node.runtime.handle)
	var b C.aster_server_builder_t
	if err := statusError(C.aster_server_builder_new(rt, C.uint64_t(node.handle), &b)); err != nil {
		return nil, fmt.Errorf("aster_server_builder_new: %w", err)
	}
	fail := func(what string, err error) (*dispatchServer, error) {
		C.aster_server_builder_free(b)
		return nil, fmt.Errorf("%s: %w", what, err)
	}
	for i, js := range serviceJSON {
		p, n := cBytesArg([]byte(js))
		var idx C.uint32_t
		if err := statusError(C.aster_server_add_service_json(b, p, n, &idx)); err != nil {
			return fail("aster_server_add_service_json", err)
		}
		if int(idx) != i {
			return fail("aster_server_add_service_json", fmt.Errorf("service index %d, want %d", idx, i))
		}
	}
	opt := func(key C.uint32_t, v uint64) error {
		return statusError(C.aster_server_set_option(b, key, C.uint64_t(v)))
	}
	nodeInfo := uint64(1)
	if s.disableNodeInfo {
		nodeInfo = 0
	}
	topology := uint64(0)
	if s.topology {
		topology = 1
	}
	errs := []error{opt(serverOptionBuiltinNodeInfo, nodeInfo), opt(serverOptionBuiltinTopology, topology)}
	if s.maxSessionsPerConnection > 0 {
		errs = append(errs, opt(serverOptionMaxSessionsPerConnection, s.maxSessionsPerConnection))
	}
	if s.setHandlerTimeout {
		errs = append(errs, opt(serverOptionHandlerTimeoutMs, s.handlerTimeoutMs))
	}
	if err := errors.Join(errs...); err != nil {
		return fail("aster_server_set_option", err)
	}
	var h C.aster_server_t
	// aster_server_start consumes the builder whatever the outcome.
	if err := statusError(C.aster_server_start(rt, b, C.uint32_t(s.ringCapacity), &h)); err != nil {
		return nil, fmt.Errorf("aster_server_start: %w", err)
	}
	return &dispatchServer{rt: rt, handle: C.uint64_t(h), waiters: map[uint64]chan bool{}}, nil
}

// destroy stops serving (queued calls are abandoned) and wakes any
// response-capacity waiter.
func (d *dispatchServer) destroy() error {
	d.mu.Lock()
	d.closed = true
	for id, ch := range d.waiters {
		ch <- false
		delete(d.waiters, id)
	}
	d.mu.Unlock()
	return statusError(C.aster_server_destroy(d.rt, d.handle))
}

func (d *dispatchServer) releaseBuffer(id C.uint64_t) {
	if id != 0 {
		C.aster_dispatch_buffer_release(d.rt, d.handle, id)
	}
}

// poll drains up to max events into buf (C memory owned by the single poll
// goroutine).
func (d *dispatchServer) poll(buf *C.aster_dispatch_event_t, max int, timeoutMs uint32) []C.aster_dispatch_event_t {
	n := C.aster_server_poll(d.rt, d.handle, buf, C.uint32_t(max), C.uint32_t(timeoutMs))
	if n == 0 {
		return nil
	}
	return unsafe.Slice(buf, int(n))
}

// capacityEvent wakes the producer waiting for capacity on callID.
func (d *dispatchServer) capacityEvent(callID uint64, writable bool) {
	d.mu.Lock()
	ch, ok := d.waiters[callID]
	if ok {
		delete(d.waiters, callID)
	}
	d.mu.Unlock()
	if ok {
		ch <- writable // buffered(1)
	}
}

// awaitCapacity reserves required bytes for callID and waits for
// RESPONSE_WRITABLE (nil), RESPONSE_CLOSED (errCallGone) or ctx.
func (d *dispatchServer) awaitCapacity(ctx context.Context, callID uint64, required uint64) error {
	ch := make(chan bool, 1)
	d.mu.Lock()
	if d.closed {
		d.mu.Unlock()
		return errCallGone
	}
	if _, busy := d.waiters[callID]; busy {
		d.mu.Unlock()
		return fmt.Errorf("concurrent response-capacity waits for call %d violate the one-producer-per-call contract", callID)
	}
	d.waiters[callID] = ch
	d.mu.Unlock()
	s := C.aster_dispatch_reserve(d.rt, d.handle, C.uint64_t(callID), C.uint64_t(required))
	if s != C.ASTER_STATUS_OK {
		d.mu.Lock()
		delete(d.waiters, callID)
		d.mu.Unlock()
		if s == C.ASTER_STATUS_NOT_FOUND || s == C.ASTER_STATUS_ALREADY_CLOSED {
			return errCallGone
		}
		return fmt.Errorf("aster_dispatch_reserve: %w", statusError(s))
	}
	select {
	case ok := <-ch:
		if ok {
			return nil
		}
		return errCallGone
	case <-ctx.Done():
		d.mu.Lock()
		delete(d.waiters, callID)
		d.mu.Unlock()
		C.aster_dispatch_cancel_reservation(d.rt, d.handle, C.uint64_t(callID))
		return fmt.Errorf("interrupted while the response queue was full: %w", ctx.Err())
	}
}

// submit runs attempt until it is not QUEUE_FULL; the retry after a
// reservation consumes the held capacity. NOT_FOUND / ALREADY_CLOSED (the
// call is over) is errCallGone.
func (d *dispatchServer) submit(ctx context.Context, callID uint64, op string, attempt func(required *C.uint64_t) C.int32_t) error {
	for {
		var required C.uint64_t
		s := attempt(&required)
		switch s {
		case C.ASTER_STATUS_OK:
			return nil
		case C.ASTER_STATUS_NOT_FOUND, C.ASTER_STATUS_ALREADY_CLOSED:
			return errCallGone
		case C.ASTER_STATUS_QUEUE_FULL:
			d.queueFullHits.Add(1)
			if err := d.awaitCapacity(ctx, callID, uint64(required)); err != nil {
				return err
			}
		default:
			return fmt.Errorf("%s: %w", op, statusError(s))
		}
	}
}

func cBytesArg(b []byte) (*C.uint8_t, C.uint32_t) {
	if len(b) == 0 {
		return nil, 0
	}
	return (*C.uint8_t)(unsafe.Pointer(&b[0])), C.uint32_t(len(b))
}

// respond answers a unary / client-stream call: payload plus status (nil = OK).
func (d *dispatchServer) respond(ctx context.Context, callID uint64, payload, status []byte) error {
	p, pn := cBytesArg(payload)
	s, sn := cBytesArg(status)
	return d.submit(ctx, callID, "aster_dispatch_respond", func(req *C.uint64_t) C.int32_t {
		return C.aster_dispatch_respond(d.rt, d.handle, C.uint64_t(callID), p, pn, s, sn, req)
	})
}

// send writes one response data frame; payload must not be empty.
func (d *dispatchServer) send(ctx context.Context, callID uint64, payload []byte) error {
	p, pn := cBytesArg(payload)
	return d.submit(ctx, callID, "aster_dispatch_send", func(req *C.uint64_t) C.int32_t {
		return C.aster_dispatch_send(d.rt, d.handle, C.uint64_t(callID), p, pn, req)
	})
}

// finish writes the trailer (nil status = OK).
func (d *dispatchServer) finish(ctx context.Context, callID uint64, status []byte) error {
	s, sn := cBytesArg(status)
	return d.submit(ctx, callID, "aster_dispatch_finish", func(req *C.uint64_t) C.int32_t {
		return C.aster_dispatch_finish(d.rt, d.handle, C.uint64_t(callID), s, sn, req)
	})
}

// recvStatus is the outcome of dispatchServer.recv.
type recvStatus int

const (
	recvPayload recvStatus = iota
	recvEnd                // end of the request stream
	recvTimeout
	recvGone // the call is over
)

// recv pulls the next request payload after the first (decompressed).
func (d *dispatchServer) recv(callID uint64, timeoutMs uint32) (recvStatus, []byte, error) {
	var ptr *C.uint8_t
	var n C.uint32_t
	var buf C.uint64_t
	s := C.aster_dispatch_recv(d.rt, d.handle, C.uint64_t(callID), C.uint32_t(timeoutMs), &ptr, &n, &buf)
	switch s {
	case C.ASTER_STATUS_OK:
	case C.ASTER_STATUS_TIMEOUT:
		return recvTimeout, nil, nil
	case C.ASTER_STATUS_NOT_FOUND:
		return recvGone, nil, nil
	default:
		return 0, nil, fmt.Errorf("aster_dispatch_recv: %w", statusError(s))
	}
	if buf == 0 {
		return recvEnd, nil, nil
	}
	payload := []byte{}
	if n > 0 {
		payload = C.GoBytes(unsafe.Pointer(ptr), C.int(n))
	}
	d.releaseBuffer(buf)
	return recvPayload, payload, nil
}

// kvOut reads a k/v-list accessor (metadata / attributes) for callID.
func (d *dispatchServer) kvOut(callID uint64, attributes bool) []MetadataEntry {
	var ptr *C.uint8_t
	var n C.uint32_t
	var buf C.uint64_t
	var s C.int32_t
	if attributes {
		s = C.aster_dispatch_attributes(d.rt, d.handle, C.uint64_t(callID), &ptr, &n, &buf)
	} else {
		s = C.aster_dispatch_metadata(d.rt, d.handle, C.uint64_t(callID), &ptr, &n, &buf)
	}
	if s != C.ASTER_STATUS_OK {
		return nil
	}
	var raw []byte
	if n > 0 {
		raw = C.GoBytes(unsafe.Pointer(ptr), C.int(n))
	}
	d.releaseBuffer(buf)
	kv, _ := decodeKV(raw)
	return kv
}

// abandon ends the call with INTERNAL "handler dropped call" unless it ended.
func (d *dispatchServer) abandon(callID uint64) {
	C.aster_dispatch_abandon(d.rt, d.handle, C.uint64_t(callID))
}

// isCancelled: cancelled, finished or unknown.
func (d *dispatchServer) isCancelled(callID uint64) bool {
	return C.aster_dispatch_is_cancelled(d.rt, d.handle, C.uint64_t(callID)) != 0
}

func (d *dispatchServer) setPeerAttributes(peer string, kv []byte) error {
	pp := []byte(peer)
	p, pn := cBytesArg(pp)
	k, kn := cBytesArg(kv)
	return statusError(C.aster_server_set_peer_attributes(d.handle, p, pn, k, kn))
}

func (d *dispatchServer) liveCalls() uint64 {
	return uint64(C.aster_server_live_calls(d.handle))
}

// encodeKV encodes the C ABI k/v list: u32 LE count, then per entry u32 LE
// key length + key, u32 LE value length + value. Empty = nil.
func encodeKV(entries []MetadataEntry) []byte {
	if len(entries) == 0 {
		return nil
	}
	size := 4
	for _, e := range entries {
		size += 8 + len(e.Key) + len(e.Value)
	}
	b := make([]byte, 0, size)
	b = binary.LittleEndian.AppendUint32(b, uint32(len(entries)))
	for _, e := range entries {
		b = binary.LittleEndian.AppendUint32(b, uint32(len(e.Key)))
		b = append(b, e.Key...)
		b = binary.LittleEndian.AppendUint32(b, uint32(len(e.Value)))
		b = append(b, e.Value...)
	}
	return b
}

// decodeKV decodes a k/v list (order kept).
func decodeKV(b []byte) ([]MetadataEntry, error) {
	if len(b) == 0 {
		return nil, nil
	}
	if len(b) < 4 {
		return nil, fmt.Errorf("k/v list: %w", ErrInvalidArgument)
	}
	n := binary.LittleEndian.Uint32(b)
	b = b[4:]
	out := make([]MetadataEntry, 0, min(int(n), 64))
	next := func() (string, bool) {
		if len(b) < 4 {
			return "", false
		}
		l := binary.LittleEndian.Uint32(b)
		if uint64(len(b)-4) < uint64(l) {
			return "", false
		}
		s := string(b[4 : 4+l])
		b = b[4+l:]
		return s, true
	}
	for i := uint32(0); i < n; i++ {
		k, ok1 := next()
		v, ok2 := next()
		if !ok1 || !ok2 {
			return out, fmt.Errorf("k/v list truncated: %w", ErrInvalidArgument)
		}
		out = append(out, MetadataEntry{k, v})
	}
	return out, nil
}

// DynamicServiceContractID returns the contract id (64 hex) Rust computes for
// a service's JSON registration form (aster_dynamic_service_contract_id) --
// the id the server publishes.
func DynamicServiceContractID(serviceJSON string) (string, error) {
	p, n := cBytesArg([]byte(serviceJSON))
	var out C.struct_aster_bytes_t
	if err := statusError(C.aster_dynamic_service_contract_id(p, n, &out)); err != nil {
		return "", fmt.Errorf("aster_dynamic_service_contract_id: %w", err)
	}
	return string(takeRustBytes(out)), nil
}
