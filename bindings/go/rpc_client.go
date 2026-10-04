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
	"errors"
	"fmt"
	"iter"
	"math"
	"sync"
	"time"
	"unsafe"
)

// CallOption configures one RPC call. The deadline comes from the context:
// it is sent rounded up to whole seconds (max 32767 s).
type CallOption func(*callOptions)

type callOptions struct {
	metadata  []MetadataEntry
	sessionID int32
}

// WithMetadata adds a request-metadata pair to the stream header.
func WithMetadata(key, value string) CallOption {
	return func(o *callOptions) { o.metadata = append(o.metadata, MetadataEntry{key, value}) }
}

// WithSessionID targets a session of a session-scoped service (0 = shared).
func WithSessionID(id int32) CallOption {
	return func(o *callOptions) { o.sessionID = id }
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

// RpcConnection carries RPC calls over one QUIC connection through the C ABI
// multiplexed-stream pool (aster_call_*): unary calls use the pooled fast
// path, streaming calls a dedicated substream. Safe for concurrent calls.
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

func deadlineSeconds(ctx context.Context) int16 {
	d, ok := ctx.Deadline()
	if !ok {
		return 0
	}
	secs := math.Ceil(time.Until(d).Seconds())
	switch {
	case secs < 1:
		return 1
	case secs > math.MaxInt16:
		return math.MaxInt16
	}
	return int16(secs)
}

func headerFrame(ctx context.Context, service, method string, o *callOptions) ([]byte, error) {
	b, err := EncodeStreamHeader(StreamHeader{
		Service: service, Method: method, Version: 1,
		Deadline: deadlineSeconds(ctx), Metadata: o.metadata, SessionID: o.sessionID,
	})
	if err != nil {
		return nil, err
	}
	return EncodeFrame(b, FlagHeader), nil
}

func buildOptions(opts []CallOption) *callOptions {
	o := &callOptions{}
	for _, f := range opts {
		f(o)
	}
	return o
}

// callError maps aster_call_* failures (ASTER_CALL_ERR_* or a status).
func callError(code C.int32_t, what string) error {
	switch code {
	case C.ASTER_CALL_ERR_POOL_FULL:
		return &RpcError{Code: StatusResourceExhausted, Message: what + ": stream pool full"}
	case C.ASTER_CALL_ERR_QUIC_LIMIT_REACHED:
		return &RpcError{Code: StatusResourceExhausted, Message: what + ": QUIC stream limit reached"}
	case C.ASTER_CALL_ERR_PEER_STREAM_LIMIT_TOO_LOW:
		return &RpcError{Code: StatusResourceExhausted, Message: what + ": peer stream limit too low"}
	case C.ASTER_CALL_ERR_STREAM_OPEN_FAILED:
		return &RpcError{Code: StatusUnavailable, Message: what + ": stream open failed: " + lastErrorMessage()}
	case C.ASTER_CALL_ERR_POOL_CLOSED:
		return &RpcError{Code: StatusUnavailable, Message: what + ": stream pool closed"}
	}
	return &RpcError{Code: StatusUnavailable, Message: fmt.Sprintf("%s: %v", what, statusError(code))}
}

// Call makes a unary call. Cancelling ctx stops waiting (the native call
// runs to completion or to the transport deadline).
func Call[Req, Resp any](ctx context.Context, rc *RpcConnection, service, method string, req *Req, opts ...CallOption) (*Resp, error) {
	o := buildOptions(opts)
	payload, err := rc.codec.Encode(req)
	if err != nil {
		return nil, fmt.Errorf("encode request: %w", err)
	}
	pair, err := headerFrame(ctx, service, method, o)
	if err != nil {
		return nil, err
	}
	body, err := EncodePayloadFrame(payload, FlagEndStream)
	if err != nil {
		return nil, err
	}
	pair = append(pair, body...)

	type result struct {
		resp    []byte
		flags   byte
		hasResp bool
		trailer []byte
		err     error
	}
	done := make(chan result, 1)
	go func() {
		var r result
		r.resp, r.flags, r.hasResp, r.trailer, r.err = rc.unaryNative(pair, uint32(o.sessionID))
		done <- r
	}()
	var r result
	select {
	case r = <-done:
	case <-ctx.Done():
		return nil, ctx.Err()
	}
	if r.err != nil {
		return nil, r.err
	}
	if err := trailerStatus(r.trailer); err != nil {
		return nil, err
	}
	if !r.hasResp {
		return nil, &RpcError{Code: StatusInternal, Message: "OK trailer with no response frame"}
	}
	return decodeResponse[Resp](rc.codec, r.resp, r.flags)
}

func trailerStatus(trailer []byte) error {
	if len(trailer) == 0 {
		return nil
	}
	st, err := DecodeRpcStatus(trailer)
	if err != nil {
		return &RpcError{Code: StatusInternal, Message: "malformed trailer: " + err.Error()}
	}
	return st.Err()
}

func decodeResponse[Resp any](codec Codec, payload []byte, flags byte) (*Resp, error) {
	p, err := PayloadOf(payload, flags)
	if err != nil {
		return nil, &RpcError{Code: StatusInternal, Message: "decompress response: " + err.Error()}
	}
	resp := new(Resp)
	if err := codec.Decode(p, resp); err != nil {
		return nil, &RpcError{Code: StatusInternal, Message: "decode response: " + err.Error()}
	}
	return resp, nil
}

func (rc *RpcConnection) unaryNative(pair []byte, sessionID uint32) (resp []byte, flags byte, hasResp bool, trailer []byte, err error) {
	in := C.CBytes(pair)
	defer C.free(in)
	var respPtr, trailerPtr *C.uint8_t
	var respLen, trailerLen C.uint32_t
	var respFlags C.uint8_t
	var respBuf, trailerBuf C.uint64_t
	r := C.aster_call_unary(rc.rt(), rc.conv(), C.uint32_t(sessionID), (*C.uint8_t)(in), C.uint32_t(len(pair)),
		&respPtr, &respLen, &respFlags, &respBuf, &trailerPtr, &trailerLen, &trailerBuf)
	if r != 0 {
		return nil, 0, false, nil, callError(r, "aster_call_unary")
	}
	if respBuf != 0 || respLen > 0 {
		hasResp = true
		resp = C.GoBytes(unsafe.Pointer(respPtr), C.int(respLen))
		flags = byte(respFlags)
	}
	if trailerLen > 0 {
		trailer = C.GoBytes(unsafe.Pointer(trailerPtr), C.int(trailerLen))
	}
	if respBuf != 0 {
		C.aster_call_buffer_release(rc.rt(), respBuf)
	}
	if trailerBuf != 0 {
		C.aster_call_buffer_release(rc.rt(), trailerBuf)
	}
	return resp, flags, hasResp, trailer, nil
}

// ─── Streaming ──────────────────────────────────────────────────────────────

const clientRecvPollMs = 100

// streamingCall is one call on a dedicated substream (aster_call_acquire_streaming).
type streamingCall struct {
	rc        *RpcConnection
	call      C.uint64_t
	succeeded bool
	once      sync.Once
}

func openStreaming(ctx context.Context, rc *RpcConnection, service, method string, o *callOptions) (*streamingCall, error) {
	header, err := headerFrame(ctx, service, method, o)
	if err != nil {
		return nil, err
	}
	var h C.aster_call_t
	if r := C.aster_call_acquire_streaming(rc.rt(), rc.conv(), &h); r != 0 {
		return nil, callError(r, "aster_call_acquire_streaming")
	}
	sc := &streamingCall{rc: rc, call: C.uint64_t(h)}
	if err := sc.sendFrame(header); err != nil {
		sc.close()
		return nil, err
	}
	return sc, nil
}

func (sc *streamingCall) sendFrame(frame []byte) error {
	p := C.CBytes(frame)
	defer C.free(p)
	if r := C.aster_call_send_frame(sc.rc.rt(), sc.call, (*C.uint8_t)(p), C.uint32_t(len(frame))); r != 0 {
		return callError(r, "aster_call_send_frame")
	}
	return nil
}

func (sc *streamingCall) send(v any, end bool) error {
	payload, err := sc.rc.codec.Encode(v)
	if err != nil {
		return fmt.Errorf("encode request: %w", err)
	}
	var flags byte
	if end {
		flags = FlagEndStream
	}
	frame, err := EncodePayloadFrame(payload, flags)
	if err != nil {
		return err
	}
	return sc.sendFrame(frame)
}

// sendAll sends every request; the last frame carries END_STREAM (an empty
// END_STREAM frame when there are none).
func sendAll[Req any](ctx context.Context, sc *streamingCall, reqs iter.Seq[*Req]) error {
	var pending *Req
	have := false
	for r := range reqs {
		if err := ctx.Err(); err != nil {
			return err
		}
		if have {
			if err := sc.send(pending, false); err != nil {
				return err
			}
		}
		pending, have = r, true
	}
	if have {
		return sc.send(pending, true)
	}
	return sc.sendFrame(EncodeFrame(nil, FlagEndStream))
}

// responses yields response payloads until the trailer; a non-OK trailer
// is yielded as an *RpcError.
func (sc *streamingCall) responses(ctx context.Context) iter.Seq2[[]byte, error] {
	return func(yield func([]byte, error) bool) {
		for {
			if err := ctx.Err(); err != nil {
				yield(nil, err)
				return
			}
			var ptr *C.uint8_t
			var n C.uint32_t
			var flags C.uint8_t
			var buf C.uint64_t
			r := C.aster_call_recv_frame(sc.rc.rt(), sc.call, clientRecvPollMs, &ptr, &n, &flags, &buf)
			if r < 0 {
				yield(nil, callError(r, "aster_call_recv_frame"))
				return
			}
			switch r {
			case C.ASTER_CALL_RECV_TIMEOUT:
				continue
			case C.ASTER_CALL_RECV_END_OF_STREAM:
				yield(nil, &RpcError{Code: StatusUnavailable, Message: "stream ended before trailer"})
				return
			}
			var payload []byte
			if n > 0 {
				payload = C.GoBytes(unsafe.Pointer(ptr), C.int(n))
			}
			if buf != 0 {
				C.aster_call_buffer_release(sc.rc.rt(), buf)
			}
			f := byte(flags)
			if f&FlagTrailer != 0 {
				if err := trailerStatus(payload); err != nil {
					yield(nil, err)
					return
				}
				sc.succeeded = true
				return
			}
			if f&FlagRowSchema != 0 {
				continue
			}
			p, err := PayloadOf(payload, f)
			if err != nil {
				yield(nil, &RpcError{Code: StatusInternal, Message: "decompress response: " + err.Error()})
				return
			}
			if !yield(p, nil) {
				return
			}
		}
	}
}

// close releases the substream after a clean trailer, else discards it.
func (sc *streamingCall) close() {
	sc.once.Do(func() {
		if sc.succeeded {
			C.aster_call_release(sc.rc.rt(), sc.call)
		} else {
			C.aster_call_discard(sc.rc.rt(), sc.call)
		}
	})
}

// CallServerStream makes a server-streaming call; range over the result. An
// error (transport, decode, or a non-OK trailer as *RpcError) is the last
// element. Breaking out early discards the substream.
func CallServerStream[Req, Resp any](ctx context.Context, rc *RpcConnection, service, method string, req *Req, opts ...CallOption) iter.Seq2[*Resp, error] {
	return func(yield func(*Resp, error) bool) {
		sc, err := openStreaming(ctx, rc, service, method, buildOptions(opts))
		if err != nil {
			yield(nil, err)
			return
		}
		defer sc.close()
		if err := sc.send(req, true); err != nil {
			yield(nil, err)
			return
		}
		for payload, err := range sc.responses(ctx) {
			if err != nil {
				yield(nil, err)
				return
			}
			resp := new(Resp)
			if err := rc.codec.Decode(payload, resp); err != nil {
				yield(nil, &RpcError{Code: StatusInternal, Message: "decode response: " + err.Error()})
				return
			}
			if !yield(resp, nil) {
				return
			}
		}
	}
}

// CallClientStream makes a client-streaming call: every request from reqs,
// then one response.
func CallClientStream[Req, Resp any](ctx context.Context, rc *RpcConnection, service, method string, reqs iter.Seq[*Req], opts ...CallOption) (*Resp, error) {
	sc, err := openStreaming(ctx, rc, service, method, buildOptions(opts))
	if err != nil {
		return nil, err
	}
	defer sc.close()
	if err := sendAll(ctx, sc, reqs); err != nil {
		return nil, err
	}
	var last []byte
	count := 0
	for payload, err := range sc.responses(ctx) {
		if err != nil {
			return nil, err
		}
		last, count = payload, count+1
	}
	if count == 0 {
		return nil, &RpcError{Code: StatusInternal, Message: "OK trailer with no response frame"}
	}
	if count > 1 {
		sc.succeeded = false
		return nil, &RpcError{Code: StatusInternal, Message: fmt.Sprintf("client-stream call received %d response frames", count)}
	}
	return decodeResponse[Resp](rc.codec, last, 0)
}

// CallBidiStream makes a bidi-streaming call: requests are sent on a
// goroutine while responses are yielded. A send error ends the iteration
// unless the server already ended the call.
func CallBidiStream[Req, Resp any](ctx context.Context, rc *RpcConnection, service, method string, reqs iter.Seq[*Req], opts ...CallOption) iter.Seq2[*Resp, error] {
	return func(yield func(*Resp, error) bool) {
		ctx, cancel := context.WithCancel(ctx)
		defer cancel()
		sc, err := openStreaming(ctx, rc, service, method, buildOptions(opts))
		if err != nil {
			yield(nil, err)
			return
		}
		sendErr := make(chan error, 1)
		go func() { sendErr <- sendAll(ctx, sc, reqs) }()
		defer func() {
			cancel()
			<-sendErr
			sc.close()
		}()
		for payload, err := range sc.responses(ctx) {
			if err != nil {
				select {
				case serr := <-sendErr:
					sendErr <- serr
					if serr != nil && !errors.Is(serr, context.Canceled) {
						err = errors.Join(err, serr)
					}
				default:
				}
				yield(nil, err)
				return
			}
			resp := new(Resp)
			if err := rc.codec.Decode(payload, resp); err != nil {
				yield(nil, &RpcError{Code: StatusInternal, Message: "decode response: " + err.Error()})
				return
			}
			if !yield(resp, nil) {
				return
			}
		}
	}
}
