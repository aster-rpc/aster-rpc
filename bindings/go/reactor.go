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
	"fmt"
	"unsafe"
)

// ReactorCall represents an incoming RPC call delivered by the reactor.
// The header and request payloads are already de-framed.
type ReactorCall struct {
	CallID       uint64
	Header       []byte
	HeaderFlags  byte
	Request      []byte
	RequestFlags byte
	PeerID       string
	IsSession    bool
}

// ReactorResponse is the response to submit for a call.
type ReactorResponse struct {
	ResponseFrame []byte
	TrailerFrame  []byte
}

// Reactor wraps the aster_reactor_* C API.
// It owns a reactor handle attached to a node and provides
// poll/submit/release operations for RPC call delivery.
type Reactor struct {
	handle        uint64
	runtimeHandle uint64
}

// NewReactor creates a reactor attached to the given node.
// The reactor starts accepting connections and delivering calls immediately.
func NewReactor(runtime *Runtime, node *Node, ringCapacity uint32) (*Reactor, error) {
	if ringCapacity == 0 {
		ringCapacity = 256
	}
	var handle C.aster_reactor_t
	r := C.aster_reactor_create(
		C.uint64_t(runtime.handle),
		C.uint64_t(node.handle),
		C.uint32_t(ringCapacity),
		&handle,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_reactor_create: %w", Error(r))
	}
	return &Reactor{
		handle:        uint64(handle),
		runtimeHandle: runtime.handle,
	}, nil
}

// Poll drains up to maxCalls from the reactor ring buffer.
// If timeoutMs is 0, returns immediately (non-blocking).
// If timeoutMs > 0, blocks up to that duration waiting for at least one call.
// Buffers are copied and released before returning.
func (r *Reactor) Poll(maxCalls int, timeoutMs uint32) ([]ReactorCall, error) {
	if maxCalls <= 0 {
		maxCalls = 32
	}

	callSize := C.sizeof_aster_reactor_call_t
	buf := C.malloc(C.size_t(maxCalls) * C.size_t(callSize))
	if buf == nil {
		return nil, fmt.Errorf("malloc failed")
	}
	defer C.free(buf)

	n := C.aster_reactor_poll(
		C.uint64_t(r.runtimeHandle),
		C.uint64_t(r.handle),
		(*C.aster_reactor_call_t)(buf),
		C.uint32_t(maxCalls),
		C.uint32_t(timeoutMs),
	)

	if n == 0 {
		return nil, nil
	}

	calls := make([]ReactorCall, int(n))
	for i := 0; i < int(n); i++ {
		slot := (*C.aster_reactor_call_t)(unsafe.Pointer(uintptr(buf) + uintptr(i)*uintptr(callSize)))

		// Copy data out of native memory
		var header []byte
		if slot.header_len > 0 && slot.header_ptr != nil {
			header = C.GoBytes(unsafe.Pointer(slot.header_ptr), C.int(slot.header_len))
		}
		var request []byte
		if slot.request_len > 0 && slot.request_ptr != nil {
			request = C.GoBytes(unsafe.Pointer(slot.request_ptr), C.int(slot.request_len))
		}
		var peerID string
		if slot.peer_len > 0 && slot.peer_ptr != nil {
			peerID = C.GoStringN((*C.char)(unsafe.Pointer(slot.peer_ptr)), C.int(slot.peer_len))
		}

		calls[i] = ReactorCall{
			CallID:       uint64(slot.call_id),
			Header:       header,
			HeaderFlags:  byte(slot.header_flags),
			Request:      request,
			RequestFlags: byte(slot.request_flags),
			PeerID:       peerID,
			IsSession:    slot.is_session_call != 0,
		}

		// Release native buffers immediately after copy
		C.aster_reactor_buffer_release(C.uint64_t(r.runtimeHandle), C.uint64_t(r.handle), C.uint64_t(slot.header_buffer))
		C.aster_reactor_buffer_release(C.uint64_t(r.runtimeHandle), C.uint64_t(r.handle), C.uint64_t(slot.request_buffer))
		C.aster_reactor_buffer_release(C.uint64_t(r.runtimeHandle), C.uint64_t(r.handle), C.uint64_t(slot.peer_buffer))
	}

	return calls, nil
}

// Submit sends a response for a call previously delivered by Poll.
func (r *Reactor) Submit(callID uint64, resp ReactorResponse) error {
	var respPtr *C.uint8_t
	var respLen C.uint32_t
	if len(resp.ResponseFrame) > 0 {
		respPtr = (*C.uint8_t)(unsafe.Pointer(&resp.ResponseFrame[0]))
		respLen = C.uint32_t(len(resp.ResponseFrame))
	}

	var trailerPtr *C.uint8_t
	var trailerLen C.uint32_t
	if len(resp.TrailerFrame) > 0 {
		trailerPtr = (*C.uint8_t)(unsafe.Pointer(&resp.TrailerFrame[0]))
		trailerLen = C.uint32_t(len(resp.TrailerFrame))
	}

	s := C.aster_reactor_submit(
		C.uint64_t(r.runtimeHandle),
		C.uint64_t(r.handle),
		C.uint64_t(callID),
		respPtr, respLen,
		trailerPtr, trailerLen,
	)
	if s != 0 {
		return fmt.Errorf("aster_reactor_submit: %w", Error(s))
	}
	return nil
}

// Close destroys the reactor.
func (r *Reactor) Close() error {
	s := C.aster_reactor_destroy(
		C.uint64_t(r.runtimeHandle),
		C.uint64_t(r.handle),
	)
	if s != 0 {
		return fmt.Errorf("aster_reactor_destroy: %w", Error(s))
	}
	return nil
}
