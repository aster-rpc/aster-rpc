//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"errors"
	"fmt"
	"io"
	"sync"
	"sync/atomic"
	"unsafe"
)

// ASTER_EVENT_HTTP_RELAY_READY is the one-shot readiness event of the
// streaming relay handles (ABI 4.3).
const ASTER_EVENT_HTTP_RELAY_READY = C.ASTER_EVENT_HTTP_RELAY_READY

// ABI 4.3 streaming consumer relay (upcall-design.md section 6.4).
//
// Connection.RelayStream starts a full-duplex HTTP exchange with a service
// the peer exposed: the request body is pumped from an io.Reader on its own
// goroutine while the response head is awaited. The head can arrive before
// the upload is done (an early 413) and the upload carries on after it.
// Waits use the one-shot readiness events plus zero-timeout calls, so no
// goroutine parks an OS thread in a blocking native call.

// RelayStreamRequest is a streaming HTTP request to a peer's exposed service.
type RelayStreamRequest struct {
	// ServiceID names the peer's exposed service.
	ServiceID string
	// Version is an HTTPVersion* code; the zero value means HTTP/1.1.
	Version uint8
	Method  string
	URI     string
	Headers []HTTPHeader
	// Body is the request body, read until io.EOF on its own goroutine; nil
	// sends an empty body. If it implements io.Closer it is closed when the
	// exchange ends (finished or abandoned), which is also the only way to
	// interrupt a Read blocked in the source.
	Body io.Reader
	// MaxChunk is the largest single upload write and response read (0 =
	// 1 MiB); UploadCapacity is the number of upload chunks queued before a
	// write waits (0 = 8).
	MaxChunk       uint64
	UploadCapacity uint32
	// OnUploadError is called when the request body fails AFTER the response
	// head was returned (before it, the failure is RelayStream's error). The
	// request is reset either way, so the handler sees a failed body, never
	// a truncated one. It is not called when the handler stopped reading
	// (it may already have answered) or the exchange was closed. Nil logs
	// the error.
	OnUploadError func(error)
}

// RelayResponse is the head of a streaming relay response; Body streams the
// rest. Close it: before the body was read to its end, Close abandons the
// exchange and resets an unfinished upload; after a clean end the upload is
// left to finish.
type RelayResponse struct {
	Status  uint16
	Headers []HTTPHeader
	Body    io.ReadCloser
}

// RelayStream starts a streaming HTTP exchange with a service the peer
// exposed on this connection and returns once the response head is in. The
// consumer needs no Expose registry. Failures: ErrRelayUnknownService,
// ErrRelayDenied (also ErrPermissionDenied), ErrRelayOverloaded,
// ErrRelayReset, ErrRelayBadHead, ErrRelayConnectionClosed; if the request
// body source fails before the head, that error (wrapped). Cancelling ctx
// abandons the exchange at any point (reads then return ctx.Err()).
func (c *Connection) RelayStream(ctx context.Context, req RelayStreamRequest) (*RelayResponse, error) {
	version := req.Version
	if version == 0 {
		version = HTTPVersion11
	}
	headers := make([]HTTPHeader, len(req.Headers))
	for i, h := range req.Headers {
		headers[i] = HTTPHeader{Name: lowerASCII(h.Name), Value: h.Value}
	}
	c4 := EncodeExposeHTTPRequest(ExposeHTTPRequest{
		ServiceID: req.ServiceID, Version: version, Method: req.Method, URI: req.URI,
		Headers: headers, Body: HTTPBody{Stream: true},
	})
	var opts C.aster_http_relay_options_t
	if err := wrapCall("aster_http_relay_options_init", func() C.int32_t { return C.aster_http_relay_options_init(&opts) }); err != nil {
		return nil, err
	}
	opts.max_chunk = C.uint64_t(req.MaxChunk)
	opts.upload_capacity = C.uint32_t(req.UploadCapacity)
	maxChunk := req.MaxChunk
	if maxChunk == 0 || maxChunk > 64<<10 {
		maxChunk = 64 << 10 // pump buffer; well under the default 1 MiB
	}

	var a cArena
	defer a.free()
	var up C.aster_http_upload_t
	var resp C.aster_http_response_t
	if err := wrapCall("aster_expose_relay_stream", func() C.int32_t {
		return C.aster_expose_relay_stream(C.uint64_t(c.runtime.handle),
			C.uint64_t(c.handle), a.bytes(c4), &opts, &up, &resp)
	}); err != nil {
		return nil, err
	}
	x := &relayExchange{rt: c.runtime, upload: uint64(up), response: uint64(resp), done: make(chan struct{}),
		pumpDone: make(chan struct{}), onUploadErr: req.OnUploadError}
	x.srcCloser, _ = req.Body.(io.Closer)
	go func() { // ctx ends the exchange
		select {
		case <-ctx.Done():
			x.abandon()
		case <-x.done:
		}
	}()
	go x.pump(req.Body, int(maxChunk))

	head, err := x.awaitHead()
	if err != nil {
		x.abandon()
		x.freeResponse()
		if cerr := ctx.Err(); cerr != nil {
			return nil, fmt.Errorf("relay: %w", cerr)
		}
		if serr := x.sourceErr(); serr != nil && errors.Is(err, ErrRelayUploadFailed) {
			return nil, fmt.Errorf("relay: request body: %w", serr)
		}
		return nil, err
	}
	x.headDone.Store(true)
	// A source failure that raced the head is still the caller's to hear.
	if serr := x.sourceErr(); serr != nil {
		x.reportUploadErr(serr)
	}
	return &RelayResponse{Status: head.Status, Headers: head.Headers, Body: &relayBody{x: x, ctx: ctx}}, nil
}

// relayExchange owns the two native handles of one exchange.
type relayExchange struct {
	rt       *Runtime
	upload   uint64
	response uint64

	done      chan struct{} // closed when the exchange is abandoned
	abandonMu sync.Once
	pumpDone  chan struct{}

	headDone atomic.Bool
	mu       sync.Mutex
	srcErr   error
	reported bool

	onUploadErr func(error)
	respFreed   sync.Once
	srcCloser   io.Closer
	srcClose    sync.Once
}

func (x *relayExchange) rtH() C.uint64_t { return C.uint64_t(x.rt.handle) }

// abandon ends the exchange: the response is closed (an unfinished upload is
// reset, waits wake with CANCELLED) and the pump stops. Idempotent.
func (x *relayExchange) abandon() {
	x.abandonMu.Do(func() {
		close(x.done)
		C.aster_http_response_close(x.rtH(), C.uint64_t(x.response))
		x.closeSource() // interrupts a pump parked in the source's Read
	})
}

func (x *relayExchange) closeSource() {
	x.srcClose.Do(func() {
		if x.srcCloser != nil {
			_ = x.srcCloser.Close()
		}
	})
}

func (x *relayExchange) freeResponse() {
	x.respFreed.Do(func() { C.aster_http_response_free(x.rtH(), C.uint64_t(x.response)) })
}

func (x *relayExchange) sourceErr() error {
	x.mu.Lock()
	defer x.mu.Unlock()
	return x.srcErr
}

func (x *relayExchange) setSourceErr(err error) {
	x.mu.Lock()
	if x.srcErr == nil {
		x.srcErr = err
	}
	x.mu.Unlock()
	if x.headDone.Load() {
		x.reportUploadErr(err)
	}
}

func (x *relayExchange) reportUploadErr(err error) {
	x.mu.Lock()
	if x.reported {
		x.mu.Unlock()
		return
	}
	x.reported = true
	x.mu.Unlock()
	if x.onUploadErr != nil {
		x.onUploadErr(err)
	} else {
		currentLogger().Warn("aster: relay request body failed after the response head", "error", err)
	}
}

// awaitReady arms a one-shot readiness op (arm) and waits for it. When the
// exchange is abandoned meanwhile, onAbort resolves it (closing / freeing the
// armed handle resolves the op). It returns the code the next non-blocking
// call returns.
func (x *relayExchange) awaitReady(what string, arm func(op *C.aster_operation_t) C.int32_t, onAbort func()) (int32, error) {
	var op C.aster_operation_t
	if err := wrapCall(what, func() C.int32_t { return arm(&op) }); err != nil {
		return 0, err
	}
	ch := make(chan *Event, 1)
	x.rt.Register(uint64(op), ch)
	done := (<-chan struct{})(x.done)
	for {
		select {
		case ev, ok := <-ch:
			if !ok || ev == nil {
				return 0, fmt.Errorf("%s: runtime closed: %w", what, ErrAlreadyClosed)
			}
			code := ev.ErrorCode
			x.rt.releaseEventBuffer(ev)
			return code, nil
		case <-done:
			onAbort()
			done = nil // wait for the resolution
		}
	}
}

// awaitHead waits for the response head (sticky failures come back typed).
func (x *relayExchange) awaitHead() (ExposeHTTPResponse, error) {
	buf := make([]byte, 4096)
	for {
		var n C.uintptr_t
		st := C.aster_http_response_wait(x.rtH(), C.uint64_t(x.response), 0, (*C.uint8_t)(unsafe.Pointer(&buf[0])), C.uintptr_t(len(buf)), &n)
		switch {
		case st == 0:
			head, err := DecodeExposeHTTPResponse(buf[:n])
			if err != nil {
				return ExposeHTTPResponse{}, fmt.Errorf("relay head: %w", err)
			}
			if !head.Body.Stream {
				return ExposeHTTPResponse{}, fmt.Errorf("relay head: inline body from a streaming relay: %w", ErrInternal)
			}
			return head, nil
		case st == C.ASTER_HTTP_BODY_BUFFER_TOO_SMALL:
			buf = make([]byte, int(n))
		case st == C.ASTER_HTTP_BODY_TIMEOUT:
			if _, err := x.awaitReady("aster_http_response_ready", func(op *C.aster_operation_t) C.int32_t {
				return C.aster_http_response_ready(x.rtH(), C.uint64_t(x.response), 0, op)
			}, x.abandonResponseOnly); err != nil {
				return ExposeHTTPResponse{}, err
			}
		default:
			return ExposeHTTPResponse{}, bodyStatus("aster_http_response_wait", st)
		}
	}
}

// abandonResponseOnly resolves an armed response readiness op (abandon has
// already closed the response; this is the idempotent re-close).
func (x *relayExchange) abandonResponseOnly() {
	C.aster_http_response_close(x.rtH(), C.uint64_t(x.response))
}

// pump copies the request body into the upload. It owns the upload handle:
// whatever way it ends, the upload is freed (aborting it if unfinished).
func (x *relayExchange) pump(body io.Reader, chunk int) {
	defer close(x.pumpDone)
	defer func() {
		C.aster_http_upload_free(x.rtH(), C.uint64_t(x.upload))
		x.closeSource()
	}()
	if body == nil {
		C.aster_http_upload_finish(x.rtH(), C.uint64_t(x.upload))
		return
	}
	buf := make([]byte, chunk)
	fail := func(err error) {
		x.setSourceErr(err) // before the abort, so the failed head can name it
		msg := err.Error()
		var p *C.uint8_t
		if len(msg) > 0 {
			b := []byte(msg)
			p = (*C.uint8_t)(unsafe.Pointer(&b[0]))
			C.aster_http_upload_abort(x.rtH(), C.uint64_t(x.upload), p, C.uintptr_t(len(b)))
		} else {
			C.aster_http_upload_abort(x.rtH(), C.uint64_t(x.upload), nil, 0)
		}
	}
	for {
		select {
		case <-x.done:
			return // abandoned: the deferred free aborts an unfinished upload
		default:
		}
		n, rerr := body.Read(buf)
		if n > 0 {
			switch err := x.writeChunk(buf[:n]); {
			case err == nil:
			case errors.Is(err, ErrBodyCancelled), errors.Is(err, ErrBodyFinished), errors.Is(err, errExchangeDone):
				return // the handler stopped reading, or the exchange ended: not an upload failure
			default:
				fail(err)
				return
			}
		}
		switch {
		case rerr == io.EOF:
			C.aster_http_upload_finish(x.rtH(), C.uint64_t(x.upload))
			return
		case rerr != nil:
			select {
			case <-x.done:
				return
			default:
			}
			fail(rerr)
			return
		}
	}
}

var errExchangeDone = errors.New("relay: exchange ended")

// writeChunk queues p, waiting for upload room by readiness.
func (x *relayExchange) writeChunk(p []byte) error {
	for {
		var queued C.uint32_t
		st := C.aster_http_upload_try_write(x.rtH(), C.uint64_t(x.upload), (*C.uint8_t)(unsafe.Pointer(&p[0])), C.uintptr_t(len(p)), &queued)
		if err := bodyStatus("aster_http_upload_try_write", st); err != nil {
			return err
		}
		if queued != 0 {
			return nil
		}
		code, err := x.awaitReady("aster_http_upload_ready", func(op *C.aster_operation_t) C.int32_t {
			return C.aster_http_upload_ready(x.rtH(), C.uint64_t(x.upload), 0, op)
		}, func() { C.aster_http_upload_free(x.rtH(), C.uint64_t(x.upload)) })
		if err != nil {
			return err
		}
		select {
		case <-x.done:
			return errExchangeDone
		default:
		}
		if code != 0 && code != C.ASTER_HTTP_BODY_TIMEOUT {
			return bodyStatus("aster_http_upload_ready", C.int32_t(code))
		}
	}
}

// relayBody is the response body: pull-based reads, readiness-driven waits.
type relayBody struct {
	x   *relayExchange
	ctx context.Context

	mu     sync.Mutex
	eof    bool
	err    error
	closed bool
}

func (b *relayBody) Read(p []byte) (int, error) {
	b.mu.Lock()
	if b.err != nil || b.closed {
		err := b.err
		if err == nil {
			err = bodyStatus("relay body", C.ASTER_HTTP_BODY_CANCELLED)
		}
		b.mu.Unlock()
		return 0, err
	}
	b.mu.Unlock()
	if len(p) == 0 {
		return 0, nil
	}
	x := b.x
	for {
		var n C.uintptr_t
		var eof C.uint32_t
		st := C.aster_http_response_read(x.rtH(), C.uint64_t(x.response), (*C.uint8_t)(unsafe.Pointer(&p[0])), C.uintptr_t(len(p)), 0, &n, &eof)
		switch {
		case st == 0 && n > 0:
			return int(n), nil
		case st == 0 && eof != 0:
			b.mu.Lock()
			b.eof, b.err = true, io.EOF
			b.mu.Unlock()
			return 0, io.EOF
		case st == 0:
			continue
		case st == C.ASTER_HTTP_BODY_TIMEOUT:
			if _, err := x.awaitReady("aster_http_response_ready", func(op *C.aster_operation_t) C.int32_t {
				return C.aster_http_response_ready(x.rtH(), C.uint64_t(x.response), 0, op)
			}, x.abandonResponseOnly); err != nil {
				return 0, err
			}
		default:
			err := bodyStatus("aster_http_response_read", st)
			if cerr := b.ctx.Err(); cerr != nil {
				err = cerr
			}
			b.mu.Lock()
			b.err = err
			b.mu.Unlock()
			return 0, err
		}
	}
}

// Close releases the exchange. Before the body was read to its end it
// abandons the exchange (the request is reset, an unfinished upload is
// aborted); after a clean end the upload is left to finish. Idempotent.
func (b *relayBody) Close() error {
	b.mu.Lock()
	if b.closed {
		b.mu.Unlock()
		return nil
	}
	b.closed = true
	eof := b.eof
	b.mu.Unlock()
	x := b.x
	if !eof {
		x.abandon()
	} else {
		// The upload may still be finishing; stop watching ctx only once it is done.
		go func() {
			<-x.pumpDone
			x.abandonMu.Do(func() { close(x.done) })
		}()
	}
	x.freeResponse()
	return nil
}
