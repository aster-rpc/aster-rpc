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
	"errors"
	"fmt"
	"net/http"
	"strconv"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"
)

// C ABI 4.2: expose. A node publishes services to peers over the relay
// protocol: a raw L4 splice to a local socket (ExposeSocket), or an HTTP
// handler written in Go (ExposeHTTP, see expose_http.go) whose request and
// response bodies stream through a body-handle table (HTTPBodies). A peer
// reaches them with Connection.RelayRequest / RelayHTTP, and asks an edge to
// route to it with Connection.RequestRoute.

// ─── body status ─────────────────────────────────────────────────────────

// Sentinels for the aster_http_body_* status codes (aster::...::BodyError);
// match with errors.Is against a *BodyError.
var (
	// ErrBodyUnknownStream: the exchange does not exist, or ended normally.
	ErrBodyUnknownStream = errors.New("http body: unknown stream")
	// ErrBodyCancelled: the peer is gone (or the request was abandoned, or
	// the upcall timed out): stop producing and consuming.
	ErrBodyCancelled = errors.New("http body: cancelled")
	// ErrBodyTooLarge: a body crossed its configured cap.
	ErrBodyTooLarge = errors.New("http body: too large")
	// ErrBodyChunkTooLarge: a single write above the options' max_chunk.
	ErrBodyChunkTooLarge = errors.New("http body: chunk too large")
	// ErrBodyFinished: the response body already ended.
	ErrBodyFinished = errors.New("http body: finished")
	// ErrBodyTimeout: a blocking call timed out; nothing was consumed or
	// queued. Also matches ErrTimeout.
	ErrBodyTimeout = errors.New("http body: timeout")
	// ErrBodyRead: transport error on the request body.
	ErrBodyRead = errors.New("http body: read failed")
	// ErrBodyInvalidTrailers: a malformed trailers message; the body stays open.
	ErrBodyInvalidTrailers = errors.New("http body: invalid trailers")
	// ErrBodyBlockingOnRuntime: a blocking call made on a runtime thread.
	ErrBodyBlockingOnRuntime = errors.New("http body: blocking call on a runtime thread")

	// Relay failures (ABI 4.3, codes 10..16), also reported by a streaming
	// relay's head and body. Each also matches the aster_status_t it maps to
	// (aster_http_body_status_to_status): ErrRelayUnknownService is
	// ErrNotFound, ErrRelayDenied is ErrPermissionDenied.

	// ErrRelayUnknownService: the peer does not expose the service.
	ErrRelayUnknownService = errors.New("relay: unknown service")
	// ErrRelayDenied: the peer's authorizer refused the request.
	ErrRelayDenied = errors.New("relay: denied")
	// ErrRelayOverloaded: the service is at its per-service stream cap.
	ErrRelayOverloaded = errors.New("relay: overloaded")
	// ErrRelayReset: the peer reset the stream.
	ErrRelayReset = errors.New("relay: stream reset")
	// ErrRelayBadHead: the peer's response head was malformed.
	ErrRelayBadHead = errors.New("relay: bad response head")
	// ErrRelayConnectionClosed: the connection ended.
	ErrRelayConnectionClosed = errors.New("relay: connection closed")
	// ErrRelayUploadFailed: the request body failed before the response head.
	ErrRelayUploadFailed = errors.New("relay: upload failed")
)

var bodyErrors = map[int]error{
	1: ErrBodyUnknownStream, 2: ErrBodyCancelled, 3: ErrBodyTooLarge, 4: ErrBodyChunkTooLarge,
	5: ErrBodyFinished, 6: ErrBodyTimeout, 7: ErrBodyRead, 8: ErrBodyInvalidTrailers,
	9: ErrBodyBlockingOnRuntime, 10: ErrRelayUnknownService, 11: ErrRelayDenied, 12: ErrRelayOverloaded,
	13: ErrRelayReset, 14: ErrRelayBadHead, 15: ErrRelayConnectionClosed, 16: ErrRelayUploadFailed,
}

// BodyError is a failed aster_http_body_* (or relay) call; Code is the
// stable aster_http_body_status_t code 1..17.
type BodyError struct {
	Op   string
	Code int
}

func (e *BodyError) Error() string {
	if s, ok := bodyErrors[e.Code]; ok {
		return fmt.Sprintf("%s: %v (code %d)", e.Op, s, e.Code)
	}
	return fmt.Sprintf("%s: body status %d", e.Op, e.Code)
}

// Is matches the sentinels (and ErrTimeout for a timed-out call).
func (e *BodyError) Is(target error) bool {
	if s, ok := bodyErrors[e.Code]; ok && s == target {
		return true
	}
	if e.Code == 6 && target == ErrTimeout {
		return true
	}
	// The aster_status_t the C ABI maps the code to (DENIED -> PERMISSION_DENIED, ...).
	st := int32(C.aster_http_body_status_to_status(C.int32_t(e.Code)))
	return st != ASTER_STATUS_INTERNAL && statusToError[st] == target
}

func bodyStatus(op string, st C.int32_t) error {
	switch {
	case st == 0:
		return nil
	case st >= 1 && st <= 17:
		return &BodyError{Op: op, Code: int(st)}
	case st == C.ASTER_HTTP_BODY_INVALID_ARGUMENT:
		return fmt.Errorf("%s: %w", op, ErrInvalidArgument)
	case st == C.ASTER_HTTP_BODY_UNKNOWN_HANDLE:
		return fmt.Errorf("%s: unknown runtime or body table: %w", op, ErrNotFound)
	}
	return &BodyError{Op: op, Code: int(st)}
}

// ─── body table ──────────────────────────────────────────────────────────

// HTTPBodies is a body-handle table for foreign HTTP exchanges
// (aster_http_bodies_t), one per binding runtime, shared by every foreign
// HTTP service. Exchanges are addressed by the request's stream id. All calls
// are safe from any goroutine; Read and Write block the calling OS thread up
// to their timeout, so loop with short timeouts and check a context (the
// net/http adapter does).
type HTTPBodies struct {
	rt     *Runtime
	handle uint64
	closed atomic.Bool
}

// NewHTTPBodies creates a body table on rt.
func NewHTTPBodies(rt *Runtime) (*HTTPBodies, error) {
	var h C.aster_http_bodies_t
	if err := wrapStatus("aster_http_bodies_new", C.aster_http_bodies_new(C.uint64_t(rt.handle), &h)); err != nil {
		return nil, err
	}
	return &HTTPBodies{rt: rt, handle: uint64(h)}, nil
}

func (b *HTTPBodies) rtH() C.uint64_t { return C.uint64_t(b.rt.handle) }

// Handle is the native aster_http_bodies_t.
func (b *HTTPBodies) Handle() uint64 { return b.handle }

// OpenCount is the number of open exchanges (leak checks and tests).
func (b *HTTPBodies) OpenCount() (uint64, error) {
	var n C.uint64_t
	if err := wrapStatus("aster_http_bodies_open_count", C.aster_http_bodies_open_count(b.rtH(), C.uint64_t(b.handle), &n)); err != nil {
		return 0, err
	}
	return uint64(n), nil
}

// Read reads up to len(p) bytes of the request body of exchange streamID,
// waiting up to timeout (0 = return at once with ErrBodyTimeout when no data
// is ready). eof means the body ended (n is then 0); a short non-EOF read is
// normal. A timed-out read consumes nothing. Pull-based backpressure: Rust
// reads the transport only when this is called.
func (b *HTTPBodies) Read(streamID uint64, p []byte, timeout time.Duration) (n int, eof bool, err error) {
	if len(p) == 0 {
		return 0, false, nil
	}
	var outLen C.uintptr_t
	var outEOF C.uint32_t
	st := C.aster_http_body_read(b.rtH(), C.uint64_t(b.handle), C.uint64_t(streamID),
		(*C.uint8_t)(unsafe.Pointer(&p[0])), C.uintptr_t(len(p)), millis0(timeout), &outLen, &outEOF)
	if err := bodyStatus("aster_http_body_read", st); err != nil {
		return 0, false, err
	}
	return int(outLen), outEOF != 0, nil
}

// Write queues one response-body chunk (copied; at most the service's
// max_chunk), waiting up to timeout while the buffer is full; a timed-out
// write queued nothing (ErrBodyTimeout). ErrBodyCancelled means the peer is
// gone. Respond with the head (body mode Stream) first; writes made before
// it are queued up to response_buffer chunks.
func (b *HTTPBodies) Write(streamID uint64, chunk []byte, timeout time.Duration) error {
	var p *C.uint8_t
	if len(chunk) > 0 {
		p = (*C.uint8_t)(unsafe.Pointer(&chunk[0]))
	}
	return bodyStatus("aster_http_body_write",
		C.aster_http_body_write(b.rtH(), C.uint64_t(b.handle), C.uint64_t(streamID), p, C.uintptr_t(len(chunk)), millis0(timeout)))
}

// TryWrite queues one chunk if there is room now; queued is false when the
// buffer is full (nothing queued, try again later). Never blocks.
func (b *HTTPBodies) TryWrite(streamID uint64, chunk []byte) (queued bool, err error) {
	var p *C.uint8_t
	if len(chunk) > 0 {
		p = (*C.uint8_t)(unsafe.Pointer(&chunk[0]))
	}
	var out C.uint32_t
	st := C.aster_http_body_try_write(b.rtH(), C.uint64_t(b.handle), C.uint64_t(streamID), p, C.uintptr_t(len(chunk)), &out)
	if err := bodyStatus("aster_http_body_try_write", st); err != nil {
		return false, err
	}
	return out != 0, nil
}

// Finish ends the response body cleanly, with optional trailers (the relay
// hop does not carry trailers yet; an in-process consumer sees them). Queued
// chunks are still delivered. Malformed trailers are ErrBodyInvalidTrailers
// and leave the body open. Never blocks.
func (b *HTTPBodies) Finish(streamID uint64, trailers []HTTPHeader) error {
	var p *C.uint8_t
	var n int
	var enc []byte
	if len(trailers) > 0 {
		enc = EncodeExposeHTTPTrailers(trailers)
		p, n = (*C.uint8_t)(unsafe.Pointer(&enc[0])), len(enc)
	}
	return bodyStatus("aster_http_body_finish",
		C.aster_http_body_finish(b.rtH(), C.uint64_t(b.handle), C.uint64_t(streamID), p, C.uintptr_t(n)))
}

// Abort ends the response body with an error: queued chunks are discarded
// and the relay resets the stream, so the consumer sees a failed response,
// never a silently truncated one. message is logged, not sent. Never blocks.
func (b *HTTPBodies) Abort(streamID uint64, message string) error {
	var p *C.uint8_t
	if len(message) > 0 {
		m := []byte(message)
		p = (*C.uint8_t)(unsafe.Pointer(&m[0]))
		return bodyStatus("aster_http_body_abort",
			C.aster_http_body_abort(b.rtH(), C.uint64_t(b.handle), C.uint64_t(streamID), p, C.uintptr_t(len(m))))
	}
	return bodyStatus("aster_http_body_abort", C.aster_http_body_abort(b.rtH(), C.uint64_t(b.handle), C.uint64_t(streamID), nil, 0))
}

// Close frees the table. Services registered with it keep their own
// reference and keep working; Go just cannot reach their bodies any more.
// Idempotent.
func (b *HTTPBodies) Close() error {
	if b.closed.Swap(true) {
		return nil
	}
	return wrapStatus("aster_http_bodies_free", C.aster_http_bodies_free(b.rtH(), C.uint64_t(b.handle)))
}

// ─── expose registry ─────────────────────────────────────────────────────

// ExposeHTTPOptions are the limits of a foreign HTTP service; zero values are
// the defaults (inline request max 64 KiB, unbounded bodies, 1 MiB chunks,
// 8 queued response chunks).
type ExposeHTTPOptions struct {
	// InlineRequestMax: a request with a declared content-length at most this
	// travels inline in the upcall (and reaches the handler fully buffered).
	InlineRequestMax uint64
	// MaxRequestBody: a declared length over it is answered 413 without an
	// upcall; a streamed body that grows past it fails the read with
	// ErrBodyTooLarge.
	MaxRequestBody uint64
	// MaxResponseBody: an inline body over it becomes 502; a streamed one is
	// aborted by the write that crosses it.
	MaxResponseBody uint64
	// MaxChunk is the largest single body write and the most one read returns.
	MaxChunk uint64
	// ResponseBuffer is the number of response chunks queued before a write waits.
	ResponseBuffer uint32
}

// Expose is an expose registry on a node (aster_expose_t): the services
// published to peers, plus the reactor that dispatches their inbound relay
// streams. Deliver connections with Serve or HandleConnection.
type Expose struct {
	node   *Node
	rt     *Runtime
	handle uint64

	mu       sync.Mutex
	http     map[string]*Upcalls // service id -> upcall table serving its handler
	closed   bool
	serveOp  uint64
	serveCh  chan *Event
	stopServ chan struct{}
	servDone chan struct{}
	servErr  error
}

// NewExpose creates an expose registry for node. The node must have been
// started with the ALPN(s) the consumers connect on.
func NewExpose(node *Node) (*Expose, error) {
	var h C.aster_expose_t
	if err := wrapStatus("aster_expose_new", C.aster_expose_new(C.uint64_t(node.runtime.handle), C.uint64_t(node.handle), &h)); err != nil {
		return nil, err
	}
	return &Expose{node: node, rt: node.runtime, handle: uint64(h), http: map[string]*Upcalls{}}, nil
}

// Handle is the native aster_expose_t.
func (e *Expose) Handle() uint64 { return e.handle }

func (e *Expose) rtH() C.uint64_t { return C.uint64_t(e.rt.handle) }

// HandleConnection feeds one inbound connection (from Node.Accept) to the
// registry so its relay streams reach the exposed services. Call once per
// connection, before the peer opens streams on it; the Connection stays
// usable and owned by the caller.
func (e *Expose) HandleConnection(conn *Connection) error {
	return wrapStatus("aster_expose_handle_connection",
		C.aster_expose_handle_connection(e.rtH(), C.uint64_t(e.handle), C.uint64_t(conn.handle)))
}

// Serve owns the node's accept loop for expose: it accepts inbound
// connections of the node and feeds each whose ALPN is in alpns (none =
// every ALPN) to the registry; others are dropped. It returns once the loop
// is running; the loop runs until the node closes. Do not combine with
// Node.Accept on the same node (they drain the same queue): use
// HandleConnection there. Done / Err report the loop's end.
func (e *Expose) Serve(alpns ...string) error {
	e.mu.Lock()
	defer e.mu.Unlock()
	if e.closed {
		return fmt.Errorf("expose closed: %w", ErrAlreadyClosed)
	}
	if e.serveCh != nil {
		return fmt.Errorf("expose: Serve already running: %w", ErrInvalidArgument)
	}
	var a cArena
	defer a.free()
	var op C.aster_operation_t
	if err := wrapStatus("aster_expose_serve", C.aster_expose_serve(e.rtH(), C.uint64_t(e.node.handle),
		C.uint64_t(e.handle), a.list(alpns), 0, &op)); err != nil {
		return err
	}
	e.serveOp = uint64(op)
	e.serveCh = make(chan *Event, 1)
	e.stopServ = make(chan struct{})
	e.servDone = make(chan struct{})
	e.rt.Register(e.serveOp, e.serveCh)
	go func(ch chan *Event, stop, done chan struct{}) {
		defer close(done)
		select {
		case ev, ok := <-ch:
			if !ok || ev == nil {
				e.mu.Lock()
				e.servErr = fmt.Errorf("expose serve: runtime closed: %w", ErrAlreadyClosed)
				e.mu.Unlock()
				return
			}
			err := e.rt.eventError(ev, "aster_expose_serve")
			e.mu.Lock()
			e.servErr = err
			e.mu.Unlock()
		case <-stop:
		}
	}(e.serveCh, e.stopServ, e.servDone)
	return nil
}

// Done is closed when the Serve loop has ended (the node closed) or the
// registry was closed; nil before Serve.
func (e *Expose) Done() <-chan struct{} {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.servDone
}

// Err is why the Serve loop ended (typically the node's accept error on
// close); only meaningful after Done is closed.
func (e *Expose) Err() error {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.servErr
}

// Unexpose stops exposing serviceID (idempotent). Streams already being
// served finish.
func (e *Expose) Unexpose(serviceID string) error {
	var a cArena
	defer a.free()
	err := wrapStatus("aster_expose_unexpose", C.aster_expose_unexpose(e.rtH(), C.uint64_t(e.handle), a.str(serviceID)))
	e.mu.Lock()
	up := e.http[serviceID]
	delete(e.http, serviceID)
	e.mu.Unlock()
	if up != nil {
		up.removeHTTPService(serviceID)
	}
	return err
}

// ExposeSocket exposes a raw L4 splice to addr ("ip:port", a loopback server
// the program runs itself) under serviceID, node-wide. With a non-nil
// authorizer table every (connection, service) admission is put to Go as an
// expose-authorize decision (serve it with Upcalls.ServeExposeAuthorize; no
// answer follows the kind's failure mode, fail-closed by default). A nil
// authorizer admits EVERY peer that reaches the node's ALPN: do that only
// when the node itself is access-controlled (Gate 0). Re-registering an id
// replaces it.
func (e *Expose) ExposeSocket(serviceID, addr string, authorizer *Upcalls) error {
	var a cArena
	defer a.free()
	var auth C.uint64_t
	if authorizer != nil {
		auth = C.uint64_t(authorizer.handle)
	}
	return wrapStatus("aster_expose_socket",
		C.aster_expose_socket(e.rtH(), C.uint64_t(e.handle), a.str(serviceID), a.str(addr), auth))
}

// ExposeRoutePolicy accepts route registrations (acts as an edge) node-wide,
// granting what policy decides: each request is one route-policy upcall
// (kind 50) on up. With no usable answer the kind's failure mode applies to
// the whole request: fail-closed (default) grants nothing, fail-open grants
// every route. Routes land in an internal edge router (the C ABI has no
// public edge listener).
func (e *Expose) ExposeRoutePolicy(up *Upcalls, policy RoutePolicyFunc) error {
	if err := up.ServeRoutePolicy(policy); err != nil {
		return err
	}
	return wrapStatus("aster_expose_route_policy_foreign",
		C.aster_expose_route_policy_foreign(e.rtH(), C.uint64_t(e.handle), C.uint64_t(up.handle)))
}

// abandon drops op's registration and discards its completion when it
// arrives (the node closing ends an expose accept loop), without a native
// cancel: aster_operation_cancel does not stop a running operation.
func (r *Runtime) abandon(op uint64) {
	r.mu.Lock()
	defer r.mu.Unlock()
	delete(r.ops, op)
	r.abandoned[op] = struct{}{}
}

// Close stops Serve and frees the registry handle. Services registered on it
// keep serving connections that were already fed to it, so Unexpose first if
// they must stop. Idempotent.
func (e *Expose) Close() error {
	e.mu.Lock()
	if e.closed {
		e.mu.Unlock()
		return nil
	}
	e.closed = true
	serveOp, stop, done := e.serveOp, e.stopServ, e.servDone
	e.mu.Unlock()
	if stop != nil {
		close(stop)
		<-done
		e.rt.abandon(serveOp)
	}
	return wrapStatus("aster_expose_free", C.aster_expose_free(e.rtH(), C.uint64_t(e.handle)))
}

// ─── consumer side ───────────────────────────────────────────────────────

// HTTP version codes of HTTPRelayRequest.Version / ExposeHTTPRequest.Version.
const (
	HTTPVersion09 = 0
	HTTPVersion10 = 1
	HTTPVersion11 = 2
	HTTPVersion2  = 3
	HTTPVersion3  = 4
)

// HTTPRelayRequest is a buffered HTTP request to a service the peer exposed.
type HTTPRelayRequest struct {
	// ServiceID names the peer's exposed service.
	ServiceID string
	// Version is an HTTPVersion* code; the zero value means HTTP/1.1.
	Version uint8
	Method  string
	URI     string
	Headers []HTTPHeader
	Body    []byte
}

// HTTPRelayResponse is a buffered HTTP response.
type HTTPRelayResponse struct {
	Status  uint16
	Headers []HTTPHeader
	Body    []byte
}

// RelayRequest relays one buffered HTTP request to a service the peer
// exposed on this connection and collects the whole response. Any HTTP
// status is a response, including the 5xx a failing handler maps to; a
// transport failure, a refused request or a response that failed mid-body
// (the peer aborted it) is an error. Header names are lowercased. The
// consumer needs no Expose registry. A non-nil Body (even empty) is sent
// with a content-length header unless one is given. Streaming relay is not
// in the C ABI.
func (c *Connection) RelayRequest(ctx context.Context, req HTTPRelayRequest) (*HTTPRelayResponse, error) {
	version := req.Version
	if version == 0 {
		version = HTTPVersion11
	}
	headers := make([]HTTPHeader, len(req.Headers), len(req.Headers)+1)
	hasLength := false
	for i, h := range req.Headers {
		headers[i] = HTTPHeader{Name: lowerASCII(h.Name), Value: h.Value}
		hasLength = hasLength || headers[i].Name == "content-length"
	}
	if req.Body != nil && !hasLength {
		// A declared length lets the handler receive a small body inline;
		// without one the body is streamed (an empty one reads as EOF).
		headers = append(headers, HTTPHeader{Name: "content-length", Value: []byte(strconv.Itoa(len(req.Body)))})
	}
	c4 := EncodeExposeHTTPRequest(ExposeHTTPRequest{
		ServiceID: req.ServiceID, Version: version, Method: req.Method, URI: req.URI,
		Headers: headers, Body: HTTPBody{Inline: req.Body},
	})
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, c.runtime, "aster_expose_relay_request", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_expose_relay_request(C.uint64_t(c.runtime.handle), C.uint64_t(c.handle), a.bytes(c4), 0, op)
	})
	if err != nil {
		return nil, err
	}
	p, err := expect(c.runtime, ev, ASTER_EVENT_BYTES_RESULT, "aster_expose_relay_request")
	if err != nil {
		return nil, err
	}
	resp, err := DecodeExposeHTTPResponse(p)
	if err != nil {
		return nil, fmt.Errorf("aster_expose_relay_request: %w", err)
	}
	if resp.Body.Stream {
		return nil, fmt.Errorf("aster_expose_relay_request: streamed response body from a buffered relay: %w", ErrInternal)
	}
	return &HTTPRelayResponse{Status: resp.Status, Headers: resp.Headers, Body: resp.Body.Inline}, nil
}

func lowerASCII(s string) string {
	b := []byte(s)
	for i, c := range b {
		if c >= 'A' && c <= 'Z' {
			b[i] = c + 'a' - 'A'
		}
	}
	return string(b)
}

// RouteRegistration is a live route registration
// (aster_route_registration_t): the edge keeps the granted routes while it
// is held.
type RouteRegistration struct {
	rt     *Runtime
	handle uint64
	closed atomic.Bool
}

// Close ends the registration: the edge evicts its routes. Idempotent.
func (r *RouteRegistration) Close() error {
	if r == nil || r.closed.Swap(true) {
		return nil
	}
	err := wrapStatus("aster_route_registration_free", C.aster_route_registration_free(C.uint64_t(r.rt.handle), C.uint64_t(r.handle)))
	if errors.Is(err, ErrNotFound) {
		return nil
	}
	return err
}

// RequestRoute asks this connection's peer (an edge serving
// Expose.ExposeRoutePolicy) to route specs to us, attaching metadata for its
// policy. granted are the indexes into specs the peer granted (ascending);
// the returned registration keeps the routes live until Close. A refusal of
// the whole request is an error. If ctx ends first, a registration made
// meanwhile is freed.
func (c *Connection) RequestRoute(ctx context.Context, specs []RouteSpec, metadata []byte) (granted []uint32, reg *RouteRegistration, err error) {
	var a cArena
	defer a.free()
	var cspecs *C.aster_route_spec_t
	if len(specs) > 0 {
		base := a.alloc(unsafe.Sizeof(C.aster_route_spec_t{}) * uintptr(len(specs)))
		arr := unsafe.Slice((*C.aster_route_spec_t)(base), len(specs))
		for i, s := range specs {
			arr[i] = C.aster_route_spec_t{
				host: a.str(s.Host), service_id: a.str(s.ServiceID),
				port: C.uint32_t(s.Port), protocol: C.uint32_t(s.Protocol),
			}
		}
		cspecs = (*C.aster_route_spec_t)(base)
	}
	rt := c.runtime
	ev, err := startOwnedOp(ctx, rt, "aster_expose_request_route", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_expose_request_route(C.uint64_t(rt.handle), C.uint64_t(c.handle), cspecs,
			C.uintptr_t(len(specs)), a.bytes(metadata), 0, op)
	}, func(late *Event) {
		if late.Kind == ASTER_EVENT_BYTES_RESULT && late.Handle != 0 {
			C.aster_route_registration_free(C.uint64_t(rt.handle), C.uint64_t(late.Handle))
		}
	})
	if err != nil {
		return nil, nil, err
	}
	if ev.Kind != ASTER_EVENT_BYTES_RESULT {
		return nil, nil, rt.eventError(ev, "aster_expose_request_route")
	}
	handle := ev.Handle
	granted, derr := DecodeExposeRouteGrant(rt.takePayload(ev))
	if derr != nil {
		if handle != 0 {
			C.aster_route_registration_free(C.uint64_t(rt.handle), C.uint64_t(handle))
		}
		return nil, nil, fmt.Errorf("aster_expose_request_route: %w", derr)
	}
	if handle != 0 {
		reg = &RouteRegistration{rt: rt, handle: handle}
	}
	return granted, reg, nil
}

// RelayHTTP is RelayRequest over net/http types: it sends r (its body read
// fully into the buffered request) to serviceID and returns the buffered
// response. The request URI is r.URL.RequestURI() (or r.RequestURI when set).
func (c *Connection) RelayHTTP(ctx context.Context, serviceID string, r *http.Request) (*http.Response, error) {
	var body []byte
	if r.Body != nil {
		var err error
		if body, err = readAllLimited(r.Body, 1<<30); err != nil {
			return nil, err
		}
		if body == nil {
			body = []byte{}
		}
	}
	uri := r.RequestURI
	if uri == "" && r.URL != nil {
		uri = r.URL.RequestURI()
	}
	var hs []HTTPHeader
	if r.Host != "" {
		hs = append(hs, HTTPHeader{Name: "host", Value: []byte(r.Host)})
	}
	for k, vs := range r.Header {
		for _, v := range vs {
			hs = append(hs, HTTPHeader{Name: lowerASCII(k), Value: []byte(v)})
		}
	}
	resp, err := c.RelayRequest(ctx, HTTPRelayRequest{ServiceID: serviceID, Method: r.Method, URI: uri, Headers: hs, Body: body})
	if err != nil {
		return nil, err
	}
	return resp.toHTTP(r), nil
}
