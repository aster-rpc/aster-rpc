//go:build cgo

package aster

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
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
	// ScopeSession: one instance per (connection, session id), created by the
	// service's session factory on the session's first call (see
	// NewSessionService). Rust keeps the sessions (graveyard, per-connection
	// cap); the binding keeps the instances.
	ScopeSession
)

// RequirementKind is a Gate 3 capability requirement kind.
type RequirementKind string

const (
	RequireKindRole  RequirementKind = "role"
	RequireKindAnyOf RequirementKind = "any_of"
	RequireKindAllOf RequirementKind = "all_of"
)

// Requirement is a Gate 3 capability requirement (contract
// CapabilityRequirement). Rust enforces it against the caller's attributes
// (AsterServer.SetPeerAttributes) before the handler runs; a caller that
// does not meet it gets PERMISSION_DENIED. It is part of the contract id.
type Requirement struct {
	Kind  RequirementKind `json:"kind"`
	Roles []string        `json:"roles"`
}

// RequireRole requires one role.
func RequireRole(role string) *Requirement {
	return &Requirement{Kind: RequireKindRole, Roles: []string{role}}
}

// RequireAnyOf requires at least one of roles.
func RequireAnyOf(roles ...string) *Requirement {
	return &Requirement{Kind: RequireKindAnyOf, Roles: roles}
}

// RequireAllOf requires every one of roles.
func RequireAllOf(roles ...string) *Requirement {
	return &Requirement{Kind: RequireKindAllOf, Roles: roles}
}

// CallInfo describes the inbound call a handler is serving; get it with
// CallInfoFromContext.
type CallInfo struct {
	Service string
	Method  string
	Pattern MethodPattern
	// PeerID is the caller's node id (64 hex).
	PeerID string
	// ConnectionID identifies the peer connection: key per-connection state
	// on it (ServerOptions.OnConnectionClosed reports its end).
	ConnectionID uint64
	// SessionID is the wire session id (0 for shared services).
	SessionID int32
	// CallID is the server's id for this call (unique per server).
	CallID uint64
	// Metadata is the request metadata from the stream header.
	Metadata []MetadataEntry
	// Attributes are the caller's attributes as Gate 3 saw them
	// (AsterServer.SetPeerAttributes), sorted by key.
	Attributes []MetadataEntry
	// Deadline is when Rust ends the call with DEADLINE_EXCEEDED: the
	// earliest of the caller's deadline, the method's Timeout and the
	// server's HandlerTimeout (zero if none). The handler's context carries
	// it too.
	Deadline time.Time
	// IrohAuthenticated reports whether the peer id was authenticated by
	// the QUIC handshake.
	IrohAuthenticated bool
}

type callInfoKey struct{}

// CallInfoFromContext returns the CallInfo of the call a handler serves.
func CallInfoFromContext(ctx context.Context) (*CallInfo, bool) {
	ci, ok := ctx.Value(callInfoKey{}).(*CallInfo)
	return ci, ok
}

// SessionInfo is passed to a session-scoped service's factory.
type SessionInfo struct {
	Service string
	// PeerID is the caller's node id (64 hex).
	PeerID       string
	ConnectionID uint64
	// SessionID is the client's wire session id.
	SessionID int32
}

type sessionKey struct{}

// SessionFromContext returns the session instance a session-scoped
// service's handler runs in (nil, false for shared services).
func SessionFromContext(ctx context.Context) (any, bool) {
	v := ctx.Value(sessionKey{})
	return v, v != nil
}

// Session returns the session instance as T.
//
//	counter, _ := aster.Session[*Counter](ctx)
func Session[T any](ctx context.Context) (T, bool) {
	v, ok := ctx.Value(sessionKey{}).(T)
	return v, ok
}

// Method is one method of a Service. Build it with Unary, ServerStream,
// ClientStream or BidiStream.
type Method struct {
	Name       string
	Pattern    MethodPattern
	Idempotent bool
	// Timeout is the method's default timeout (contract default_timeout;
	// enforced in Rust). 0 = none.
	Timeout time.Duration
	// MaxFrameSize caps request frames (enforced in Rust; not in the
	// contract). 0 = the 16 MiB default.
	MaxFrameSize uint32
	// Requires is the method's Gate 3 requirement (part of the contract).
	Requires *Requirement
	reqType  reflect.Type
	respType reflect.Type
	invoke   func(ctx context.Context, call *serverCall) error
}

// MethodOption configures a Method.
type MethodOption func(*Method)

// Idempotent marks a method idempotent (part of the contract).
func Idempotent() MethodOption { return func(m *Method) { m.Idempotent = true } }

// Timeout sets the method's default timeout (part of the contract): Rust
// ends a call still running after d with DEADLINE_EXCEEDED.
func Timeout(d time.Duration) MethodOption { return func(m *Method) { m.Timeout = d } }

// MaxFrameSize caps the method's request frames (Rust rejects larger ones).
func MaxFrameSize(n uint32) MethodOption { return func(m *Method) { m.MaxFrameSize = n } }

// Requires sets the method's Gate 3 requirement (part of the contract).
func Requires(r *Requirement) MethodOption { return func(m *Method) { m.Requires = r } }

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
		return c.respond(resp)
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
		return c.respond(resp)
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
// Configure a service (Requires) before serving it or asking its ContractID.
type Service struct {
	Name    string
	Version int32
	Scope   ServiceScope
	// Requires is the service-wide Gate 3 requirement (part of the contract).
	Requires *Requirement
	methods  map[string]*Method
	order    []*Method
	// newSession builds a session instance (ScopeSession).
	newSession func(SessionInfo) any

	once       sync.Once
	dynJSON    string
	contractID string
	contractEr error
}

// NewService builds a shared-scope service. It panics on a duplicate method
// name (a programming error).
func NewService(name string, version int32, methods ...*Method) *Service {
	s := &Service{Name: name, Version: version, methods: make(map[string]*Method, len(methods))}
	for _, m := range methods {
		s.addMethod(m)
	}
	return s
}

// NewSessionService builds a session-scoped service: one instance per
// (connection, session id). newSession runs on the session's first call;
// handlers reach the instance with Session[T](ctx) / SessionFromContext. When
// the session ends (its connection closes) an instance implementing
// io.Closer (or Close()) is closed. Clients open sessions with
// RpcConnection.NewSessionID and call with WithSession.
func NewSessionService(name string, version int32, newSession func(SessionInfo) any, methods ...*Method) *Service {
	s := NewService(name, version, methods...)
	s.Scope = ScopeSession
	s.newSession = newSession
	return s
}

func (s *Service) addMethod(m *Method) {
	if _, dup := s.methods[m.Name]; dup {
		panic(fmt.Sprintf("aster: duplicate method %s.%s", s.Name, m.Name))
	}
	s.methods[m.Name] = m
	s.order = append(s.order, m)
}

// WithRequires sets the service-wide Gate 3 requirement; returns s.
func (s *Service) WithRequires(r *Requirement) *Service {
	s.Requires = r
	return s
}

// Method returns the named method.
func (s *Service) Method(name string) (*Method, bool) {
	m, ok := s.methods[name]
	return m, ok
}

// Methods returns every method in registration order.
func (s *Service) Methods() []*Method {
	return append([]*Method(nil), s.order...)
}

func (s *Service) lower() {
	s.once.Do(func() {
		s.dynJSON, s.contractEr = DynamicServiceJSON(s)
		if s.contractEr != nil {
			return
		}
		s.contractID, s.contractEr = DynamicServiceContractID(s.dynJSON)
	})
}

// ContractID is the 64-hex contract id: BLAKE3 of the canonical
// ServiceContract, computed in Rust from the service's registration JSON
// (the id the server publishes).
func (s *Service) ContractID() (string, error) {
	s.lower()
	return s.contractID, s.contractEr
}

// RegisterTypes registers every request/response type of s with codec.
func (s *Service) RegisterTypes(codec *ForyCodec) error {
	for _, m := range s.order {
		for _, t := range []reflect.Type{m.reqType, m.respType} {
			if err := codec.RegisterWireTypes(t); err != nil {
				return err
			}
		}
	}
	return nil
}

// DynamicServiceJSON returns the service's JSON registration form (design
// dynamic-dispatch §1.5) handed to aster_server_add_service_json. Method
// order is registration order (the CALL event's method_idx).
func DynamicServiceJSON(svc *Service) (string, error) {
	r := newTypeResolver()
	type typeRef struct {
		Hash    string `json:"hash"`
		Display string `json:"display,omitempty"`
	}
	type limits struct {
		MaxFrameSize uint32 `json:"max_frame_size"`
	}
	type methodJSON struct {
		Name           string       `json:"name"`
		Pattern        string       `json:"pattern"`
		Request        typeRef      `json:"request"`
		Response       typeRef      `json:"response"`
		Idempotent     bool         `json:"idempotent,omitempty"`
		DefaultTimeout float64      `json:"default_timeout,omitempty"`
		Requires       *Requirement `json:"requires,omitempty"`
		Limits         *limits      `json:"limits,omitempty"`
	}
	ref := func(t reflect.Type) (typeRef, error) {
		h, err := r.hash(t)
		if err != nil {
			return typeRef{}, err
		}
		tag, _ := wireTypeOf(derefType(t))
		return typeRef{Hash: h, Display: tag}, nil
	}
	methods := make([]methodJSON, 0, len(svc.order))
	for _, m := range svc.order {
		req, err := ref(m.reqType)
		if err != nil {
			return "", fmt.Errorf("%s.%s request: %w", svc.Name, m.Name, err)
		}
		resp, err := ref(m.respType)
		if err != nil {
			return "", fmt.Errorf("%s.%s response: %w", svc.Name, m.Name, err)
		}
		mj := methodJSON{
			Name: m.Name, Pattern: m.Pattern.String(), Request: req, Response: resp,
			Idempotent: m.Idempotent, DefaultTimeout: m.Timeout.Seconds(), Requires: m.Requires,
		}
		if m.MaxFrameSize > 0 {
			mj.Limits = &limits{m.MaxFrameSize}
		}
		methods = append(methods, mj)
	}
	scope := "shared"
	if svc.Scope == ScopeSession {
		scope = "session"
	}
	b, err := json.Marshal(struct {
		Name     string       `json:"name"`
		Version  int32        `json:"version"`
		Scope    string       `json:"scope"`
		Requires *Requirement `json:"requires,omitempty"`
		Methods  []methodJSON `json:"methods"`
	}{svc.Name, svc.Version, scope, svc.Requires, methods})
	return string(b), err
}

func derefType(t reflect.Type) reflect.Type {
	for t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	return t
}

// closeInstance closes a session instance that implements io.Closer or Close().
func closeInstance(v any) {
	switch c := v.(type) {
	case io.Closer:
		_ = c.Close()
	case interface{ Close() }:
		c.Close()
	}
}
