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
	"errors"
	"fmt"
	"io"
	"iter"
	"math"
	"sync"
	"time"
	"unsafe"
)

// CallOption configures one RPC call. The deadline comes from the context:
// Rust enforces it client-side (DEADLINE_EXCEEDED) and sends it rounded up
// to whole seconds.
type CallOption func(*callOptions)

type callOptions struct {
	metadata  []MetadataEntry
	sessionID uint32
	version   int32
}

// WithMetadata adds a request-metadata pair to the stream header.
func WithMetadata(key, value string) CallOption {
	return func(o *callOptions) { o.metadata = append(o.metadata, MetadataEntry{key, value}) }
}

// WithSession targets a session of a session-scoped service: id comes from
// RpcConnection.NewSessionID (0 = shared). The server creates the session on
// the first call carrying it.
func WithSession(id uint32) CallOption {
	return func(o *callOptions) { o.sessionID = id }
}

// WithSessionID is WithSession with the wire's int32 type.
func WithSessionID(id int32) CallOption {
	return func(o *callOptions) { o.sessionID = uint32(id) }
}

// WithVersion sets the service version (default 1).
func WithVersion(v int32) CallOption {
	return func(o *callOptions) { o.version = v }
}

func buildOptions(opts []CallOption) *callOptions {
	o := &callOptions{version: 1}
	for _, f := range opts {
		f(o)
	}
	return o
}

// AsterClient is an RPC client: a node, a codec and a cache of RpcConnections.
//
//	client, err := aster.StartClient(ctx, cfg)
//	defer client.Close()
//	conn, err := client.ConnectTicket(ctx, ticket)
//	reply, err := aster.Call[EchoRequest, EchoReply](ctx, conn, "InteropEcho", "echo", &req)
//
// Calls go straight to aster/1 (open gate); consumer admission is not
// performed.
type AsterClient struct {
	node     *Node
	ownsNode bool
	codec    Codec

	mu    sync.Mutex
	conns map[string]*RpcConnection
}

// StartClient starts a client node (adds aster/1 to cfg.ALPNs).
func StartClient(ctx context.Context, cfg AsterConfig) (*AsterClient, error) {
	node, err := StartNode(ctx, cfg, NodeOptions{ExtraALPNs: []string{AsterALPN}})
	if err != nil {
		return nil, err
	}
	return &AsterClient{node: node, ownsNode: true, codec: NewForyCodec(), conns: map[string]*RpcConnection{}}, nil
}

// NewClient is a client on an existing node, which the caller keeps owning.
// A nil codec means a new ForyCodec.
func NewClient(node *Node, codec Codec) *AsterClient {
	if codec == nil {
		codec = NewForyCodec()
	}
	return &AsterClient{node: node, codec: codec, conns: map[string]*RpcConnection{}}
}

// Node is the client's node.
func (c *AsterClient) Node() *Node { return c.node }

// Codec is the client's payload codec.
func (c *AsterClient) Codec() Codec { return c.codec }

// RegisterService registers the request/response types of svc with the
// client's ForyCodec (WireTyped types are also registered on first use).
func (c *AsterClient) RegisterService(svc *Service) error {
	if fc, ok := c.codec.(*ForyCodec); ok {
		return svc.RegisterTypes(fc)
	}
	return nil
}

// Connect returns a (cached) RPC connection to addr.
func (c *AsterClient) Connect(ctx context.Context, addr *NodeAddr) (*RpcConnection, error) {
	c.mu.Lock()
	if rc, ok := c.conns[addr.EndpointID]; ok {
		c.mu.Unlock()
		return rc, nil
	}
	c.mu.Unlock()
	conn, err := c.node.Connect(ctx, addr, AsterALPN)
	if err != nil {
		return nil, err
	}
	rc := &RpcConnection{conn: conn, codec: c.codec}
	c.mu.Lock()
	defer c.mu.Unlock()
	if prev, ok := c.conns[addr.EndpointID]; ok {
		_ = conn.Close("duplicate")
		return prev, nil
	}
	c.conns[addr.EndpointID] = rc
	return rc, nil
}

// ConnectTicket connects to the peer named by an aster1 ticket.
func (c *AsterClient) ConnectTicket(ctx context.Context, ticket string) (*RpcConnection, error) {
	addr, err := NodeAddrFromTicket(ticket)
	if err != nil {
		return nil, err
	}
	return c.Connect(ctx, addr)
}

// Close closes every cached connection and, if the client started it, the node.
func (c *AsterClient) Close() error {
	c.mu.Lock()
	conns := c.conns
	c.conns = map[string]*RpcConnection{}
	c.mu.Unlock()
	var err error
	for _, rc := range conns {
		_ = rc.Close()
	}
	if c.ownsNode {
		err = c.node.Close()
	}
	return err
}

// RpcConnection carries RPC calls over one QUIC connection through Rust's
// pooled aster::rpc::RpcConnection (aster_rpc_*): unary calls share the
// connection's pooled streams, streaming calls get their own. Safe for
// concurrent calls.
type RpcConnection struct {
	conn  *Connection
	codec Codec
}

// NewRpcConnection wraps an aster/1 connection.
func NewRpcConnection(conn *Connection, codec Codec) *RpcConnection {
	if codec == nil {
		codec = NewForyCodec()
	}
	return &RpcConnection{conn: conn, codec: codec}
}

// Connection is the underlying QUIC connection.
func (rc *RpcConnection) Connection() *Connection { return rc.conn }

// Close closes the connection.
func (rc *RpcConnection) Close() error { return rc.conn.Close("") }

func (rc *RpcConnection) rt() C.uint64_t   { return C.uint64_t(rc.conn.runtime.handle) }
func (rc *RpcConnection) conv() C.uint64_t { return C.uint64_t(rc.conn.handle) }

// NewSessionID returns a fresh session id from the connection's shared
// counter (aster_connection_next_session_id: Rust and binding clients on the
// connection never collide). Pass it with WithSession.
func (rc *RpcConnection) NewSessionID() (uint32, error) {
	var id C.uint32_t
	if err := statusError(C.aster_connection_next_session_id(rc.rt(), rc.conv(), &id)); err != nil {
		return 0, fmt.Errorf("aster_connection_next_session_id: %w", err)
	}
	return uint32(id), nil
}

// nativeOptions is an aster_call_options_t plus its strings in one C
// allocation (the struct holds pointers, so it cannot live in Go memory).
type nativeOptions struct {
	p unsafe.Pointer
}

func (n nativeOptions) ptr() *C.aster_call_options_t { return (*C.aster_call_options_t)(n.p) }
func (n nativeOptions) free()                        { C.free(n.p) }

// lowerOptions builds the native options; the deadline is the time left on
// ctx (rounded up to whole ms), and an expired ctx fails here.
func lowerOptions(ctx context.Context, service, method string, o *callOptions) (nativeOptions, error) {
	var deadlineMs uint32
	if d, ok := ctx.Deadline(); ok {
		left := time.Until(d)
		if left <= 0 {
			return nativeOptions{}, ctxError(ctx, &RpcError{Code: StatusDeadlineExceeded, Message: "deadline exceeded before the call"})
		}
		ms := math.Ceil(float64(left) / float64(time.Millisecond))
		deadlineMs = uint32(min(ms, math.MaxUint32-1))
	}
	md := encodeKV(o.metadata)
	size := unsafe.Sizeof(C.aster_call_options_t{})
	total := size + uintptr(len(service)+len(method)+len(md))
	p := C.calloc(1, C.size_t(total))
	if p == nil {
		panic("aster: calloc failed")
	}
	opts := (*C.aster_call_options_t)(p)
	opts.struct_size = C.uint32_t(size)
	opts.version = C.int32_t(o.version)
	opts.session_id = C.uint32_t(o.sessionID)
	opts.deadline_ms = C.uint32_t(deadlineMs)
	off := size
	put := func(b []byte) C.struct_aster_bytes_t {
		if len(b) == 0 {
			return C.struct_aster_bytes_t{}
		}
		dst := unsafe.Add(p, off)
		C.memcpy(dst, unsafe.Pointer(&b[0]), C.size_t(len(b)))
		off += uintptr(len(b))
		return C.struct_aster_bytes_t{ptr: (*C.uint8_t)(dst), len: C.uintptr_t(len(b))}
	}
	opts.service = put([]byte(service))
	opts.method = put([]byte(method))
	opts.metadata = put(md)
	return nativeOptions{p}, nil
}

// ctxError joins a CANCELLED / DEADLINE_EXCEEDED status with ctx's error
// when ctx ended, so errors.Is matches both (ErrRpcDeadlineExceeded and
// context.DeadlineExceeded).
func ctxError(ctx context.Context, err *RpcError) error {
	if ce := ctx.Err(); ce != nil && (err.Code == StatusCancelled || err.Code == StatusDeadlineExceeded) {
		return fmt.Errorf("%w: %w", err, ce)
	}
	// The native timer (armed from ctx's deadline, rounded up to ms) can
	// fire before Go's own context timer has marked ctx done.
	if d, ok := ctx.Deadline(); ok && err.Code == StatusDeadlineExceeded && !time.Now().Before(d) {
		return fmt.Errorf("%w: %w", err, context.DeadlineExceeded)
	}
	return err
}

// statusFromBytes decodes an AEv1 RpcStatus; code 0 is nil.
func statusFromBytes(ctx context.Context, code int32, b []byte) error {
	if code == 0 && len(b) == 0 {
		return nil
	}
	st, err := DecodeRpcStatus(b)
	if err != nil {
		if code == 0 {
			return &RpcError{Code: StatusInternal, Message: "malformed trailer: " + err.Error()}
		}
		st = RpcStatus{Code: StatusCode(code), Message: "malformed status: " + err.Error()}
	}
	if st.Code == StatusOK {
		return nil
	}
	return ctxError(ctx, st.Err().(*RpcError))
}

func (rc *RpcConnection) releaseBuffer(b C.uint64_t) {
	if b != 0 {
		C.aster_call_buffer_release(rc.rt(), b)
	}
}

// unary runs aster_rpc_unary (blocking) and returns the response payload or
// the call's failure.
func (rc *RpcConnection) unary(ctx context.Context, opts nativeOptions, req []byte) ([]byte, error) {
	p, n := cBytesArg(req)
	var respPtr, statusPtr *C.uint8_t
	var respLen, statusLen C.uint32_t
	var respBuf, statusBuf C.uint64_t
	var code C.int32_t
	r := C.aster_rpc_unary(rc.rt(), rc.conv(), opts.ptr(), p, n,
		&respPtr, &respLen, &respBuf, &code, &statusPtr, &statusLen, &statusBuf)
	if r != C.ASTER_STATUS_OK {
		if r == C.ASTER_STATUS_NOT_FOUND {
			return nil, &RpcError{Code: StatusUnavailable, Message: "connection closed"}
		}
		return nil, fmt.Errorf("aster_rpc_unary: %w", statusError(r))
	}
	if code != 0 {
		var st []byte
		if statusLen > 0 {
			st = C.GoBytes(unsafe.Pointer(statusPtr), C.int(statusLen))
		}
		rc.releaseBuffer(statusBuf)
		rc.releaseBuffer(respBuf)
		return nil, statusFromBytes(ctx, int32(code), st)
	}
	resp := []byte{}
	if respLen > 0 {
		resp = C.GoBytes(unsafe.Pointer(respPtr), C.int(respLen))
	}
	rc.releaseBuffer(respBuf)
	rc.releaseBuffer(statusBuf)
	return resp, nil
}

// Call makes a unary call. With a context that can never end
// (ctx.Done() == nil) it takes the pooled fast path (aster_rpc_unary: one
// blocking native call). Otherwise -- like .NET with a cancellable token --
// it runs the same wire exchange on a call of its own (aster_rpc_open), so
// cancelling ctx cancels the native call (aster_call_cancel: FLAG_CANCEL +
// stream reset) and the server sees the cancel. The context's deadline is
// enforced natively either way.
func Call[Req, Resp any](ctx context.Context, rc *RpcConnection, service, method string, req *Req, opts ...CallOption) (*Resp, error) {
	payload, err := rc.codec.Encode(req)
	if err != nil {
		return nil, fmt.Errorf("encode request: %w", err)
	}
	o := buildOptions(opts)
	var resp []byte
	if ctx.Done() == nil {
		nopts, err := lowerOptions(ctx, service, method, o)
		if err != nil {
			return nil, err
		}
		resp, err = rc.unary(ctx, nopts, payload)
		nopts.free()
		if err != nil {
			return nil, err
		}
	} else if resp, err = rc.cancellableUnary(ctx, service, method, o, payload); err != nil {
		return nil, err
	}
	return decodeResponse[Resp](rc.codec, resp)
}

// cancellableUnary sends header + request on a dedicated stream (opened with
// the server-stream pattern: the same frames as a unary call, header and
// request in one write) and reads the single response.
func (rc *RpcConnection) cancellableUnary(ctx context.Context, service, method string, o *callOptions, payload []byte) ([]byte, error) {
	if ctx.Err() != nil {
		return nil, ctxFailure(ctx)
	}
	c, err := openCall(ctx, rc, PatternServerStream, service, method, o)
	if err != nil {
		return nil, err
	}
	defer c.release()
	if err := c.send(payload, true); err != nil && !errors.Is(err, errCallClosed) {
		return nil, err
	}
	return c.readSingle("unary")
}

func decodeResponse[Resp any](codec Codec, payload []byte) (*Resp, error) {
	resp := new(Resp)
	if err := codec.Decode(payload, resp); err != nil {
		return nil, &RpcError{Code: StatusInternal, Message: "decode response: " + err.Error()}
	}
	return resp, nil
}

// ─── Streaming ──────────────────────────────────────────────────────────────

// errCallClosed: the call is over; its trailer says why.
var errCallClosed = errors.New("aster: call is over")

// rpcCall is one aster_rpc_open call. Cancelling its context cancels it
// natively (aster_call_cancel: FLAG_CANCEL + stream reset), which also wakes
// a blocked recv.
type rpcCall struct {
	rc   *RpcConnection
	ctx  context.Context
	h    C.aster_call_t
	stop func() bool
	once sync.Once
}

func openCall(ctx context.Context, rc *RpcConnection, pattern MethodPattern, service, method string, o *callOptions) (*rpcCall, error) {
	nopts, err := lowerOptions(ctx, service, method, o)
	if err != nil {
		return nil, err
	}
	defer nopts.free()
	var h C.aster_call_t
	if err := statusError(C.aster_rpc_open(rc.rt(), rc.conv(), C.uint8_t(pattern), nopts.ptr(), &h)); err != nil {
		if errors.Is(err, ErrNotFound) {
			return nil, &RpcError{Code: StatusUnavailable, Message: "connection closed"}
		}
		return nil, fmt.Errorf("aster_rpc_open: %w", err)
	}
	c := &rpcCall{rc: rc, ctx: ctx, h: h}
	c.stop = context.AfterFunc(ctx, c.cancel)
	return c, nil
}

// ctxFailure is the error for a call whose context ended.
func ctxFailure(ctx context.Context) error {
	code := StatusCancelled
	if errors.Is(ctx.Err(), context.DeadlineExceeded) {
		code = StatusDeadlineExceeded
	}
	return ctxError(ctx, &RpcError{Code: code, Message: ctx.Err().Error()})
}

// readSingle reads the one response of a unary / client-stream call.
func (c *rpcCall) readSingle(kind string) ([]byte, error) {
	var last []byte
	count := 0
	for {
		b, err := c.recv()
		if err == io.EOF {
			break
		}
		if err != nil {
			return nil, err
		}
		last, count = b, count+1
	}
	if count != 1 {
		return nil, &RpcError{Code: StatusInternal, Message: fmt.Sprintf("%s call received %d response frames", kind, count)}
	}
	return last, nil
}

// cancel cancels the call (no-op once it is over or released).
func (c *rpcCall) cancel() { C.aster_call_cancel(c.rc.rt(), c.h) }

// release frees the call, cancelling it if unfinished.
func (c *rpcCall) release() {
	c.once.Do(func() {
		c.stop()
		C.aster_call_release(c.rc.rt(), c.h)
	})
}

// send sends one request payload; errCallClosed when the call is over.
func (c *rpcCall) send(payload []byte, end bool) error {
	p, n := cBytesArg(payload)
	var e C.uint8_t
	if end {
		e = 1
	}
	switch r := C.aster_call_send_payload(c.rc.rt(), c.h, p, n, e); r {
	case C.ASTER_STATUS_OK:
		return nil
	case C.ASTER_STATUS_ALREADY_CLOSED, C.ASTER_STATUS_NOT_FOUND:
		return errCallClosed
	default:
		return fmt.Errorf("aster_call_send_payload: %w", statusError(r))
	}
}

// recv returns the next response payload, or (nil, io.EOF) at an OK
// trailer, or the trailer's error. Each native wait is bounded
// (recvSliceMs); between slices the call's context is checked, so a recv
// returns once ctx ends even if the native cancel has not woken it.
func (c *rpcCall) recv() ([]byte, error) {
	var ptr *C.uint8_t
	var n C.uint32_t
	var buf C.uint64_t
	var trailer C.uint8_t
	for {
		r := C.aster_call_recv_payload(c.rc.rt(), c.h, recvSliceMs, &ptr, &n, &buf, &trailer)
		if r == C.ASTER_STATUS_TIMEOUT {
			if c.ctx.Err() != nil {
				c.cancel()
				return nil, ctxFailure(c.ctx)
			}
			continue
		}
		switch r {
		case C.ASTER_STATUS_OK:
		case C.ASTER_STATUS_ALREADY_CLOSED, C.ASTER_STATUS_NOT_FOUND:
			return nil, io.EOF
		case C.ASTER_STATUS_CANCELLED:
			if c.ctx.Err() != nil {
				return nil, ctxFailure(c.ctx)
			}
			return nil, &RpcError{Code: StatusCancelled, Message: "call cancelled"}
		default:
			return nil, fmt.Errorf("aster_call_recv_payload: %w", statusError(r))
		}
		break
	}
	var b []byte
	if n > 0 {
		b = C.GoBytes(unsafe.Pointer(ptr), C.int(n))
	}
	c.rc.releaseBuffer(buf)
	if trailer != 0 {
		if err := statusFromBytes(c.ctx, 0, b); err != nil {
			// A ctx deadline also fires the native cancel (AfterFunc); when
			// that wins the race with the native deadline timer the trailer
			// says CANCELLED -- report the deadline.
			if errors.Is(err, ErrRpcCancelled) && errors.Is(c.ctx.Err(), context.DeadlineExceeded) {
				return nil, ctxFailure(c.ctx)
			}
			return nil, err
		}
		return nil, io.EOF
	}
	if b == nil {
		b = []byte{}
	}
	return b, nil
}

// CallServerStream makes a server-streaming call; range over the result. An
// error (transport, decode, or a non-OK trailer as *RpcError) is the last
// element. Breaking out early cancels the call.
func CallServerStream[Req, Resp any](ctx context.Context, rc *RpcConnection, service, method string, req *Req, opts ...CallOption) iter.Seq2[*Resp, error] {
	return func(yield func(*Resp, error) bool) {
		payload, err := rc.codec.Encode(req)
		if err != nil {
			yield(nil, fmt.Errorf("encode request: %w", err))
			return
		}
		c, err := openCall(ctx, rc, PatternServerStream, service, method, buildOptions(opts))
		if err != nil {
			yield(nil, err)
			return
		}
		defer c.release()
		if err := c.send(payload, true); err != nil && !errors.Is(err, errCallClosed) {
			yield(nil, err)
			return
		}
		for {
			b, err := c.recv()
			if err == io.EOF {
				return
			}
			if err != nil {
				yield(nil, err)
				return
			}
			resp, err := decodeResponse[Resp](rc.codec, b)
			if !yield(resp, err) || err != nil {
				return
			}
		}
	}
}

// CallClientStream makes a client-streaming call: every request from reqs
// (each sent as it is produced), then one response.
func CallClientStream[Req, Resp any](ctx context.Context, rc *RpcConnection, service, method string, reqs iter.Seq[*Req], opts ...CallOption) (*Resp, error) {
	c, err := openCall(ctx, rc, PatternClientStream, service, method, buildOptions(opts))
	if err != nil {
		return nil, err
	}
	defer c.release()
	closed := false
	for r := range reqs {
		payload, err := rc.codec.Encode(r)
		if err != nil {
			return nil, fmt.Errorf("encode request: %w", err)
		}
		if err := c.send(payload, false); err != nil {
			if errors.Is(err, errCallClosed) {
				closed = true // the trailer says why
				break
			}
			return nil, err
		}
	}
	if !closed && ctx.Err() != nil {
		// The context ended while requests were produced: cancel rather than
		// half-close, so the server sees a cancel, not a clean end of stream
		// (the AfterFunc cancel may not have run yet).
		c.cancel()
		return nil, ctxFailure(ctx)
	}
	if !closed {
		if err := c.send(nil, true); err != nil && !errors.Is(err, errCallClosed) {
			return nil, err
		}
	}
	last, err := c.readSingle("client-stream")
	if err != nil {
		return nil, err
	}
	return decodeResponse[Resp](rc.codec, last)
}

// BidiCall is an open bidi-streaming call: Send and Recv may run on
// different goroutines (one sender, one receiver). Close it when done
// (an unfinished call is cancelled).
type BidiCall[Req, Resp any] struct {
	c     *rpcCall
	codec Codec
}

// OpenBidiStream opens a bidi-streaming call. Cancelling ctx cancels it.
func OpenBidiStream[Req, Resp any](ctx context.Context, rc *RpcConnection, service, method string, opts ...CallOption) (*BidiCall[Req, Resp], error) {
	c, err := openCall(ctx, rc, PatternBidiStream, service, method, buildOptions(opts))
	if err != nil {
		return nil, err
	}
	return &BidiCall[Req, Resp]{c: c, codec: rc.codec}, nil
}

// Send sends one request immediately. After the call is over it returns an
// error matching io.ErrClosedPipe; Recv then returns the trailer's outcome.
func (b *BidiCall[Req, Resp]) Send(req *Req) error {
	payload, err := b.codec.Encode(req)
	if err != nil {
		return fmt.Errorf("encode request: %w", err)
	}
	if err := b.c.send(payload, false); err != nil {
		if errors.Is(err, errCallClosed) {
			return io.ErrClosedPipe
		}
		return err
	}
	return nil
}

// CloseSend ends the request stream.
func (b *BidiCall[Req, Resp]) CloseSend() error {
	if err := b.c.send(nil, true); err != nil && !errors.Is(err, errCallClosed) {
		return err
	}
	return nil
}

// Recv returns the next response; io.EOF once the call ended OK, else the
// trailer's error (*RpcError).
func (b *BidiCall[Req, Resp]) Recv() (*Resp, error) {
	p, err := b.c.recv()
	if err != nil {
		return nil, err
	}
	return decodeResponse[Resp](b.codec, p)
}

// Cancel cancels the call (FLAG_CANCEL + stream reset).
func (b *BidiCall[Req, Resp]) Cancel() { b.c.cancel() }

// Close releases the call, cancelling it if unfinished.
func (b *BidiCall[Req, Resp]) Close() { b.c.release() }

// CallBidiStream makes a bidi-streaming call: each request from reqs is
// sent as soon as the iterator yields it (on its own goroutine) while
// responses are yielded, so ping-pong protocols work. A send failure ends
// the request side; the trailer reports the outcome. Breaking out early
// cancels the call.
func CallBidiStream[Req, Resp any](ctx context.Context, rc *RpcConnection, service, method string, reqs iter.Seq[*Req], opts ...CallOption) iter.Seq2[*Resp, error] {
	return func(yield func(*Resp, error) bool) {
		b, err := OpenBidiStream[Req, Resp](ctx, rc, service, method, opts...)
		if err != nil {
			yield(nil, err)
			return
		}
		sendErr := make(chan error, 1)
		go func() {
			for r := range reqs {
				if err := b.Send(r); err != nil {
					if errors.Is(err, io.ErrClosedPipe) {
						err = nil
					}
					sendErr <- err
					return
				}
			}
			if ctx.Err() != nil { // ended while producing: cancel, don't half-close
				b.Cancel()
				sendErr <- nil
				return
			}
			sendErr <- b.CloseSend()
		}()
		// Release cancels an unfinished call, which fails a pending Send.
		defer b.Close()
		for {
			resp, err := b.Recv()
			if err == io.EOF {
				return
			}
			if err != nil {
				select {
				case serr := <-sendErr:
					if serr != nil {
						err = errors.Join(err, serr)
					}
				default:
				}
				yield(nil, err)
				return
			}
			if !yield(resp, nil) {
				return
			}
		}
	}
}
