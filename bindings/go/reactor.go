//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"fmt"
	"sync"
	"sync/atomic"
	"unsafe"
)

// ReactorEventKind discriminates aster_reactor_call_t entries
// (ASTER_REACTOR_EVENT_*).
type ReactorEventKind uint8

const (
	ReactorEventCall             ReactorEventKind = C.ASTER_REACTOR_EVENT_CALL
	ReactorEventConnectionClosed ReactorEventKind = C.ASTER_REACTOR_EVENT_CONNECTION_CLOSED
	ReactorEventResponseWritable ReactorEventKind = C.ASTER_REACTOR_EVENT_RESPONSE_WRITABLE
	ReactorEventResponseClosed   ReactorEventKind = C.ASTER_REACTOR_EVENT_RESPONSE_CLOSED
)

// ReactorCall is an entry delivered by Reactor.Poll: either an incoming RPC
// call (Kind == ReactorEventCall) or a connection-closed notification
// (Kind == ReactorEventConnectionClosed; only ConnectionID, ConnectionHandle
// and PeerID are set). Response-capacity events are consumed internally by
// Poll and never returned.
type ReactorCall struct {
	Kind         ReactorEventKind
	CallID       uint64
	ConnectionID uint64
	// StreamID identifies the QUIC bi-stream; multiplexed streams carry many
	// calls, so key per-session state on (ConnectionID, header sessionId).
	StreamID     uint64
	Header       []byte
	HeaderFlags  byte
	Request      []byte
	RequestFlags byte
	PeerID       string
	// ConnectionHandle is borrowed from the reactor; never free it.
	ConnectionHandle uint64
}

// ReactorResponse is the response to submit for a call.
type ReactorResponse struct {
	ResponseFrame []byte
	TrailerFrame  []byte
}

// Reactor wraps the aster_reactor_* C API.
// It owns a reactor handle attached to a node and provides
// poll/submit/release operations for RPC call delivery.
//
// Backpressure: submits that hit ASTER_STATUS_QUEUE_FULL reserve response
// capacity (aster_reactor_reserve_response_capacity) and wait for the
// matching RESPONSE_WRITABLE / RESPONSE_CLOSED event, which is delivered via
// Poll. Something must therefore keep calling Poll (Server does) while a
// submit is waiting.
type Reactor struct {
	handle        uint64
	runtimeHandle uint64

	mu      sync.Mutex
	waiters map[uint64]chan ReactorEventKind

	queueFullHits atomic.Uint64
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
	if r != C.ASTER_STATUS_OK {
		return nil, fmt.Errorf("aster_reactor_create: %w", Error(r))
	}
	return &Reactor{
		handle:        uint64(handle),
		runtimeHandle: runtime.handle,
		waiters:       make(map[uint64]chan ReactorEventKind),
	}, nil
}

func (r *Reactor) releaseBuffer(id C.uint64_t) {
	if id != 0 {
		C.aster_reactor_buffer_release(C.uint64_t(r.runtimeHandle), C.uint64_t(r.handle), id)
	}
}

// Poll drains up to maxCalls entries from the reactor ring buffer.
// If timeoutMs is 0, returns immediately (non-blocking).
// If timeoutMs > 0, blocks up to that duration waiting for at least one entry.
// Payloads are copied and native buffers released before returning.
// Response-capacity events wake the matching blocked submit and are not
// returned; unknown event kinds are dropped.
func (r *Reactor) Poll(maxCalls int, timeoutMs uint32) ([]ReactorCall, error) {
	if maxCalls <= 0 {
		maxCalls = 32
	}

	callSize := unsafe.Sizeof(C.aster_reactor_call_t{})
	buf := C.calloc(C.size_t(maxCalls), C.size_t(callSize))
	if buf == nil {
		return nil, fmt.Errorf("calloc failed")
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

	slots := unsafe.Slice((*C.aster_reactor_call_t)(buf), int(n))
	calls := make([]ReactorCall, 0, int(n))
	for i := range slots {
		slot := &slots[i]
		kind := ReactorEventKind(slot.event_kind)
		switch kind {
		case ReactorEventResponseWritable, ReactorEventResponseClosed:
			// No payload buffers on capacity events.
			r.completeCapacityWaiter(uint64(slot.call_id), kind)
			continue
		case ReactorEventConnectionClosed:
			var peerID string
			if slot.peer_len > 0 && slot.peer_ptr != nil {
				peerID = C.GoStringN((*C.char)(unsafe.Pointer(slot.peer_ptr)), C.int(slot.peer_len))
			}
			calls = append(calls, ReactorCall{
				Kind:             kind,
				ConnectionID:     uint64(slot.connection_id),
				PeerID:           peerID,
				ConnectionHandle: uint64(slot.connection_handle),
			})
			r.releaseBuffer(slot.peer_buffer)
			continue
		case ReactorEventCall:
		default:
			// Unknown kind from a newer minor ABI: release whatever it carries.
			r.releaseBuffer(slot.header_buffer)
			r.releaseBuffer(slot.request_buffer)
			r.releaseBuffer(slot.peer_buffer)
			continue
		}

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

		calls = append(calls, ReactorCall{
			Kind:             kind,
			CallID:           uint64(slot.call_id),
			ConnectionID:     uint64(slot.connection_id),
			StreamID:         uint64(slot.stream_id),
			Header:           header,
			HeaderFlags:      byte(slot.header_flags),
			Request:          request,
			RequestFlags:     byte(slot.request_flags),
			PeerID:           peerID,
			ConnectionHandle: uint64(slot.connection_handle),
		})

		// Release native buffers immediately after copy.
		r.releaseBuffer(slot.header_buffer)
		r.releaseBuffer(slot.request_buffer)
		r.releaseBuffer(slot.peer_buffer)
	}

	return calls, nil
}

func (r *Reactor) completeCapacityWaiter(callID uint64, kind ReactorEventKind) {
	r.mu.Lock()
	ch, ok := r.waiters[callID]
	if ok {
		delete(r.waiters, callID)
	}
	r.mu.Unlock()
	if ok {
		ch <- kind // buffered(1)
	}
}

// awaitResponseCapacity reserves encodedLen bytes of response capacity for
// callID and blocks until Poll delivers RESPONSE_WRITABLE (nil) or
// RESPONSE_CLOSED (error), or ctx is done (reservation cancelled).
func (r *Reactor) awaitResponseCapacity(ctx context.Context, callID uint64, encodedLen uint64, op string) error {
	ch := make(chan ReactorEventKind, 1)
	r.mu.Lock()
	if _, busy := r.waiters[callID]; busy {
		r.mu.Unlock()
		return fmt.Errorf("%s: concurrent response-capacity wait for call %d violates the one-producer-per-call contract", op, callID)
	}
	r.waiters[callID] = ch
	r.mu.Unlock()

	s := C.aster_reactor_reserve_response_capacity(
		C.uint64_t(r.runtimeHandle), C.uint64_t(r.handle),
		C.uint64_t(callID), C.uint64_t(encodedLen))
	if s != C.ASTER_STATUS_OK {
		r.mu.Lock()
		delete(r.waiters, callID)
		r.mu.Unlock()
		return fmt.Errorf("aster_reactor_reserve_response_capacity: %w", Error(s))
	}

	select {
	case kind := <-ch:
		if kind == ReactorEventResponseWritable {
			return nil
		}
		return fmt.Errorf("%s: response channel closed for call %d: %w", op, callID, Error(C.ASTER_STATUS_NOT_FOUND))
	case <-ctx.Done():
		r.mu.Lock()
		delete(r.waiters, callID)
		r.mu.Unlock()
		cs := C.aster_reactor_cancel_response_reservation(
			C.uint64_t(r.runtimeHandle), C.uint64_t(r.handle), C.uint64_t(callID))
		if cs != C.ASTER_STATUS_OK && cs != C.ASTER_STATUS_NOT_FOUND {
			return fmt.Errorf("aster_reactor_cancel_response_reservation: %w", Error(cs))
		}
		return fmt.Errorf("%s: interrupted while response channel was full: %w", op, ctx.Err())
	}
}

// retryWhileQueueFull runs attempt; on QUEUE_FULL it reserves capacity and
// retries the exact same submit (which then consumes the held permit).
func (r *Reactor) retryWhileQueueFull(ctx context.Context, callID uint64, encodedLen uint64, op string, attempt func() C.int32_t) error {
	for {
		s := attempt()
		if s == C.ASTER_STATUS_OK {
			return nil
		}
		if s != C.ASTER_STATUS_QUEUE_FULL {
			return fmt.Errorf("%s: %w", op, Error(s))
		}
		r.queueFullHits.Add(1)
		if err := r.awaitResponseCapacity(ctx, callID, encodedLen, op); err != nil {
			return err
		}
	}
}

// QueueFullHits returns how many submits on this reactor hit QUEUE_FULL.
func (r *Reactor) QueueFullHits() uint64 { return r.queueFullHits.Load() }

func cBytesArg(b []byte) (*C.uint8_t, C.uint32_t) {
	if len(b) == 0 {
		return nil, 0
	}
	return (*C.uint8_t)(unsafe.Pointer(&b[0])), C.uint32_t(len(b))
}

// Submit sends a unary response (response frame + trailer) for a call
// previously delivered by Poll. See SubmitContext.
func (r *Reactor) Submit(callID uint64, resp ReactorResponse) error {
	return r.SubmitContext(context.Background(), callID, resp)
}

// SubmitContext is Submit with cancellation of a pending QUEUE_FULL wait.
func (r *Reactor) SubmitContext(ctx context.Context, callID uint64, resp ReactorResponse) error {
	respPtr, respLen := cBytesArg(resp.ResponseFrame)
	trailerPtr, trailerLen := cBytesArg(resp.TrailerFrame)
	return r.retryWhileQueueFull(ctx, callID, uint64(respLen)+uint64(trailerLen), "aster_reactor_submit",
		func() C.int32_t {
			return C.aster_reactor_submit(
				C.uint64_t(r.runtimeHandle), C.uint64_t(r.handle), C.uint64_t(callID),
				respPtr, respLen, trailerPtr, trailerLen)
		})
}

// SubmitFrame sends one already-framed streaming response frame.
func (r *Reactor) SubmitFrame(ctx context.Context, callID uint64, frame []byte) error {
	ptr, n := cBytesArg(frame)
	return r.retryWhileQueueFull(ctx, callID, uint64(n), "aster_reactor_submit_frame",
		func() C.int32_t {
			return C.aster_reactor_submit_frame(
				C.uint64_t(r.runtimeHandle), C.uint64_t(r.handle), C.uint64_t(callID), ptr, n)
		})
}

// SubmitTrailer sends the trailer frame and closes the call.
func (r *Reactor) SubmitTrailer(ctx context.Context, callID uint64, trailer []byte) error {
	ptr, n := cBytesArg(trailer)
	return r.retryWhileQueueFull(ctx, callID, uint64(n), "aster_reactor_submit_trailer",
		func() C.int32_t {
			return C.aster_reactor_submit_trailer(
				C.uint64_t(r.runtimeHandle), C.uint64_t(r.handle), C.uint64_t(callID), ptr, n)
		})
}

// Close destroys the reactor.
func (r *Reactor) Close() error {
	s := C.aster_reactor_destroy(
		C.uint64_t(r.runtimeHandle),
		C.uint64_t(r.handle),
	)
	if s != C.ASTER_STATUS_OK {
		return fmt.Errorf("aster_reactor_destroy: %w", Error(s))
	}
	return nil
}
