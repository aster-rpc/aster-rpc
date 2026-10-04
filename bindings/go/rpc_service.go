//go:build cgo

package aster

import (
	"context"
	"fmt"
	"iter"
	"reflect"
	"sync"
	"time"
)

// MethodPattern is an RPC method shape (contract MethodPattern).
type MethodPattern int

const (
	PatternUnary MethodPattern = iota
	PatternServerStream
	PatternClientStream
	PatternBidiStream
)

// String returns the contract name: unary, server_stream, client_stream, bidi_stream.
func (p MethodPattern) String() string {
	switch p {
	case PatternUnary:
		return "unary"
	case PatternServerStream:
		return "server_stream"
	case PatternClientStream:
		return "client_stream"
	default:
		return "bidi_stream"
	}
}

// ServiceScope is the service instance scope (contract ScopeKind).
type ServiceScope int

const (
	// ScopeShared: one implementation serves every caller.
	ScopeShared ServiceScope = iota
	// ScopeSession: one instance per (connection, session id); not served by
	// the Go server yet.
	ScopeSession
)

// CallInfo describes the inbound call a handler is serving; get it with
// CallInfoFromContext.
type CallInfo struct {
	Service string
	Method  string
	Pattern MethodPattern
	// PeerID is the caller's node id (64 hex).
	PeerID string
	// ConnectionID is the reactor connection id: key per-connection state on it.
	ConnectionID uint64
	SessionID    int32
	// Metadata is the request metadata from the stream header.
	Metadata []MetadataEntry
	// Deadline is when the caller's deadline expires (zero if none); the
	// handler's context carries it too.
	Deadline time.Time
}

type callInfoKey struct{}

// CallInfoFromContext returns the CallInfo of the call a handler serves.
func CallInfoFromContext(ctx context.Context) (*CallInfo, bool) {
	ci, ok := ctx.Value(callInfoKey{}).(*CallInfo)
	return ci, ok
}

// Method is one method of a Service. Build it with Unary, ServerStream,
// ClientStream or BidiStream.
type Method struct {
	Name       string
	Pattern    MethodPattern
	Idempotent bool
	reqType    reflect.Type
	respType   reflect.Type
	invoke     func(ctx context.Context, call *serverCall) error
}

// MethodOption configures a Method.
type MethodOption func(*Method)

// Idempotent marks a method idempotent (part of the contract).
func Idempotent() MethodOption { return func(m *Method) { m.Idempotent = true } }

func newMethod[Req, Resp any](name string, p MethodPattern, opts []MethodOption,
	invoke func(ctx context.Context, call *serverCall) error) *Method {
	m := &Method{
		Name: name, Pattern: p,
		reqType:  reflect.TypeFor[Req](),
		respType: reflect.TypeFor[Resp](),
		invoke:   invoke,
	}
	for _, o := range opts {
		o(m)
	}
	return m
}

// Unary defines a unary method: one request, one response.
func Unary[Req, Resp any](name string, h func(ctx context.Context, req *Req) (*Resp, error), opts ...MethodOption) *Method {
	return newMethod[Req, Resp](name, PatternUnary, opts, func(ctx context.Context, c *serverCall) error {
		req := new(Req)
		if err := c.decodeFirst(req); err != nil {
			return err
		}
		resp, err := h(ctx, req)
		if err != nil {
			return err
		}
		return c.completeUnary(resp)
	})
}

// ServerStream defines a server-streaming method: one request; the handler
// calls send for each response and returns nil to end the stream with OK.
func ServerStream[Req, Resp any](name string, h func(ctx context.Context, req *Req, send func(*Resp) error) error, opts ...MethodOption) *Method {
	return newMethod[Req, Resp](name, PatternServerStream, opts, func(ctx context.Context, c *serverCall) error {
		req := new(Req)
		if err := c.decodeFirst(req); err != nil {
			return err
		}
		if err := h(ctx, req, func(r *Resp) error { return c.send(ctx, r) }); err != nil {
			return err
		}
		return c.complete(RpcStatus{Code: StatusOK})
	})
}

// ClientStream defines a client-streaming method: a stream of requests (a
// range-over-func iterator; a non-nil error ends it), one response.
func ClientStream[Req, Resp any](name string, h func(ctx context.Context, reqs iter.Seq2[*Req, error]) (*Resp, error), opts ...MethodOption) *Method {
	return newMethod[Req, Resp](name, PatternClientStream, opts, func(ctx context.Context, c *serverCall) error {
		resp, err := h(ctx, requestsOf[Req](ctx, c))
		if err != nil {
			return err
		}
		return c.completeUnary(resp)
	})
}

// BidiStream defines a bidi-streaming method: requests and responses flow
// independently; return nil to end the call with OK.
func BidiStream[Req, Resp any](name string, h func(ctx context.Context, reqs iter.Seq2[*Req, error], send func(*Resp) error) error, opts ...MethodOption) *Method {
	return newMethod[Req, Resp](name, PatternBidiStream, opts, func(ctx context.Context, c *serverCall) error {
		if err := h(ctx, requestsOf[Req](ctx, c), func(r *Resp) error { return c.send(ctx, r) }); err != nil {
			return err
		}
		return c.complete(RpcStatus{Code: StatusOK})
	})
}

func requestsOf[Req any](ctx context.Context, c *serverCall) iter.Seq2[*Req, error] {
	return func(yield func(*Req, error) bool) {
		for payload, err := range c.requests(ctx) {
			if err != nil {
				yield(nil, err)
				return
			}
			req := new(Req)
			if err := c.codec.Decode(payload, req); err != nil {
				yield(nil, &RpcError{Code: StatusInvalidArgument, Message: "decode request: " + err.Error()})
				return
			}
			if !yield(req, nil) {
				return
			}
		}
	}
}

// Service is a named, versioned set of methods served by AsterServer.
//
//	svc := aster.NewService("InteropEcho", 1,
//	    aster.Unary("echo", echo),
//	    aster.ServerStream("generate", generate),
//	)
//
// Request and response types are structs implementing WireTyped; the server
// registers them with its codec and the contract id is derived from them.
type Service struct {
	Name    string
	Version int32
	Scope   ServiceScope
	methods map[string]*Method

	once       sync.Once
	contractID string
	contractEr error
}

// NewService builds a shared-scope service. It panics on a duplicate method
// name (a programming error).
func NewService(name string, version int32, methods ...*Method) *Service {
	s := &Service{Name: name, Version: version, methods: make(map[string]*Method, len(methods))}
	for _, m := range methods {
		if _, dup := s.methods[m.Name]; dup {
			panic(fmt.Sprintf("aster: duplicate method %s.%s", name, m.Name))
		}
		s.methods[m.Name] = m
	}
	return s
}

// Method returns the named method.
func (s *Service) Method(name string) (*Method, bool) {
	m, ok := s.methods[name]
	return m, ok
}

// Methods returns every method (unordered).
func (s *Service) Methods() []*Method {
	out := make([]*Method, 0, len(s.methods))
	for _, m := range s.methods {
		out = append(out, m)
	}
	return out
}

// ContractID is the 64-hex contract id: BLAKE3 of the canonical
// ServiceContract, computed in Rust.
func (s *Service) ContractID() (string, error) {
	s.once.Do(func() {
		js, err := ServiceContractJSON(s)
		if err != nil {
			s.contractEr = err
			return
		}
		id, err := ComputeContractID(js)
		s.contractID, s.contractEr = string(id), err
	})
	return s.contractID, s.contractEr
}

// RegisterTypes registers every request/response type of s with codec.
func (s *Service) RegisterTypes(codec *ForyCodec) error {
	for _, m := range s.methods {
		for _, t := range []reflect.Type{m.reqType, m.respType} {
			if err := codec.RegisterWireTypes(t); err != nil {
				return err
			}
		}
	}
	return nil
}
