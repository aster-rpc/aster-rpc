//go:build cgo

package aster

import (
	"context"
	"errors"
	"fmt"
	"iter"
	"log"
	"strconv"
	"sync"
	"sync/atomic"
	"time"
)

// ServerOptions configure StartServer.
type ServerOptions struct {
	// Config is the node configuration (zero value = Rust defaults).
	Config AsterConfig
	// Services to serve.
	Services []*Service
	// Codec for payloads (default: a ForyCodec; service types are registered on it).
	Codec Codec
	// Gate is the Gate-0 policy. Default: a gate owned by the server, in
	// TrustOpenDev when Config.AllowAllConsumers is set, else TrustProtected
	// (only admitted peers reach aster/1).
	Gate *Gate
	// Interceptors run OnRequest (with the raw first request payload) before
	// every handler; an error rejects the call (an *RpcError keeps its code).
	Interceptors []Interceptor
	// RingCapacity sizes the reactor ring (default 256).
	RingCapacity uint32
	// OnConnectionClosed, if set, runs on the poll goroutine when a peer
	// connection closes (drop per-connection state). Must not block.
	OnConnectionClosed func(connectionID uint64, peerID string)
	// Logger receives submit failures (default: the standard logger).
	Logger *log.Logger
}

// AsterServer serves Services over aster/1: it starts a Node from an
// AsterConfig, attaches a reactor and dispatches calls to typed handlers.
// Every call ends with exactly one AEv1 RpcStatus trailer: a handler error
// becomes its *RpcError code, a context deadline DEADLINE_EXCEEDED, anything
// else INTERNAL.
//
//	srv, err := aster.StartServer(ctx, aster.ServerOptions{Config: cfg, Services: []*aster.Service{svc}})
//	defer srv.Close()
//	ticket, err := srv.Ticket(ctx)
type AsterServer struct {
	node      *Node
	gate      *Gate
	ownedGate bool
	reactor   *Reactor
	codec     Codec
	services  map[string]*Service
	icpts     []Interceptor
	onClosed  func(uint64, string)
	logger    *log.Logger

	ctx     context.Context
	cancel  context.CancelFunc
	pollWG  sync.WaitGroup
	calls   sync.WaitGroup
	closing atomic.Bool
	closeMu sync.Mutex
}

// StartServer starts a node and serves opts.Services on it.
func StartServer(ctx context.Context, opts ServerOptions) (*AsterServer, error) {
	services := make(map[string]*Service, len(opts.Services))
	for _, s := range opts.Services {
		if _, dup := services[s.Name]; dup {
			return nil, fmt.Errorf("service %s registered twice: %w", s.Name, ErrInvalidArgument)
		}
		if s.Scope != ScopeShared {
			return nil, fmt.Errorf("service %s: session-scoped services are not served by the Go binding yet: %w", s.Name, ErrUnsupported)
		}
		services[s.Name] = s
	}
	codec := opts.Codec
	if codec == nil {
		codec = NewForyCodec()
	}
	if fc, ok := codec.(*ForyCodec); ok {
		for _, s := range services {
			if err := s.RegisterTypes(fc); err != nil {
				return nil, err
			}
		}
	}

	gate, owned := opts.Gate, false
	if gate == nil {
		var err error
		if gate, err = NewGate(nil); err != nil {
			return nil, err
		}
		owned = true
		mode := TrustProtected
		if opts.Config.AllowAllConsumers {
			mode = TrustOpenDev
		}
		if err := errors.Join(gate.SetTrustMode(mode), gate.SetAdmissionALPNs(ConsumerAdmissionALPN)); err != nil {
			_ = gate.Close()
			return nil, err
		}
	}
	fail := func(err error) (*AsterServer, error) {
		if owned {
			_ = gate.Close()
		}
		return nil, err
	}
	node, err := StartNode(ctx, opts.Config, NodeOptions{Gate: gate, ExtraALPNs: []string{AsterALPN, ConsumerAdmissionALPN}})
	if err != nil {
		return fail(err)
	}
	reactor, err := NewReactor(node.runtime, node, opts.RingCapacity)
	if err != nil {
		_ = node.Close()
		return fail(err)
	}
	logger := opts.Logger
	if logger == nil {
		logger = log.Default()
	}
	sctx, cancel := context.WithCancel(context.Background())
	s := &AsterServer{
		node: node, gate: gate, ownedGate: owned, reactor: reactor, codec: codec,
		services: services, icpts: opts.Interceptors, onClosed: opts.OnConnectionClosed,
		logger: logger, ctx: sctx, cancel: cancel,
	}
	s.pollWG.Add(1)
	go s.pollLoop()
	return s, nil
}

// Node is the server's node.
func (s *AsterServer) Node() *Node { return s.node }

// Gate is the Gate-0 policy guarding the node (admit peers on it unless dev
// mode is on).
func (s *AsterServer) Gate() *Gate { return s.gate }

// NodeID is the server's node id (64 hex).
func (s *AsterServer) NodeID() (string, error) { return s.node.NodeID() }

// Ticket returns an aster1 ticket for the server (waits up to 5 s for direct
// addresses).
func (s *AsterServer) Ticket(ctx context.Context) (string, error) {
	return s.node.Ticket(ctx, 5*time.Second)
}

// Close stops dispatching, cancels in-flight handlers (their contexts end),
// waits for them, then closes the reactor, node and owned gate.
func (s *AsterServer) Close() error {
	s.closeMu.Lock()
	defer s.closeMu.Unlock()
	if s.closing.Swap(true) {
		return nil
	}
	s.cancel()
	s.pollWG.Wait()
	s.calls.Wait()
	err := errors.Join(s.reactor.Close(), s.node.Close())
	if s.ownedGate {
		err = errors.Join(err, s.gate.Close())
	}
	return err
}

func (s *AsterServer) pollLoop() {
	defer s.pollWG.Done()
	for !s.closing.Load() {
		// Poll also wakes handlers blocked on a QUEUE_FULL submit.
		calls, err := s.reactor.Poll(32, 100)
		if err != nil {
			time.Sleep(10 * time.Millisecond)
			continue
		}
		for _, c := range calls {
			switch c.Kind {
			case ReactorEventConnectionClosed:
				if s.onClosed != nil {
					s.onClosed(c.ConnectionID, c.PeerID)
				}
			case ReactorEventCall:
				s.calls.Add(1)
				go s.dispatch(c)
			}
		}
	}
}

func (s *AsterServer) dispatch(in ReactorCall) {
	defer s.calls.Done()
	fail := func(st RpcStatus) {
		if err := s.submitTrailer(in.CallID, st); err != nil {
			s.logger.Printf("aster: trailer for call %d failed: %v", in.CallID, err)
		}
	}
	header, err := DecodeStreamHeader(in.Header)
	if err != nil {
		fail(RpcStatus{Code: StatusInvalidArgument, Message: "malformed stream header: " + err.Error()})
		return
	}
	if header.SerializationMode != XlangMode {
		fail(RpcStatus{Code: StatusInvalidArgument, Message: fmt.Sprintf("unsupported serialization mode: %d", header.SerializationMode)})
		return
	}
	svc, ok := s.services[header.Service]
	if !ok {
		fail(RpcStatus{Code: StatusUnimplemented, Message: "unknown service: " + header.Service})
		return
	}
	method, ok := svc.methods[header.Method]
	if !ok {
		fail(RpcStatus{Code: StatusUnimplemented, Message: fmt.Sprintf("unknown method: %s/%s", header.Service, header.Method)})
		return
	}
	if header.SessionID != 0 {
		fail(RpcStatus{Code: StatusFailedPrecondition, Message: fmt.Sprintf(
			"service %s is shared-scoped but the call carries session %d", header.Service, header.SessionID)})
		return
	}

	ctx, cancel := context.WithCancel(s.ctx)
	defer cancel()
	info := &CallInfo{
		Service: header.Service, Method: header.Method, Pattern: method.Pattern,
		PeerID: in.PeerID, ConnectionID: in.ConnectionID, SessionID: header.SessionID,
		Metadata: header.Metadata,
	}
	if header.Deadline > 0 {
		info.Deadline = time.Now().Add(time.Duration(header.Deadline) * time.Second)
		var dcancel context.CancelFunc
		ctx, dcancel = context.WithDeadline(ctx, info.Deadline)
		defer dcancel()
	}
	ctx = context.WithValue(ctx, callInfoKey{}, info)

	call := &serverCall{
		reactor: s.reactor, callID: in.CallID, first: in.Request, firstFlags: in.RequestFlags,
		codec: s.codec, base: s.ctx,
	}
	err = s.intercept(info, method, in, call)
	if err == nil {
		err = invokeRecover(ctx, method, call)
	}
	if err == nil {
		return // the method completed the call
	}
	st := StatusFromError(err)
	if ctx.Err() == context.DeadlineExceeded && s.ctx.Err() == nil {
		st = RpcStatus{Code: StatusDeadlineExceeded, Message: "deadline exceeded"}
	} else if s.ctx.Err() != nil && st.Code == StatusCancelled {
		st = RpcStatus{Code: StatusUnavailable, Message: "server shutting down"}
	}
	if err := call.complete(st); err != nil {
		s.logger.Printf("aster: trailer for call %d failed: %v", in.CallID, err)
	}
}

func (s *AsterServer) intercept(info *CallInfo, m *Method, in ReactorCall, call *serverCall) error {
	if len(s.icpts) == 0 {
		return nil
	}
	md := make(map[string]string, len(info.Metadata))
	for _, e := range info.Metadata {
		md[e.Key] = e.Value
	}
	cc := &CallContext{
		Service: info.Service, Method: info.Method, CallID: strconv.FormatUint(in.CallID, 10),
		Peer: info.PeerID, Metadata: md, IsStreaming: m.Pattern != PatternUnary,
		Pattern: m.Pattern.String(), Idempotent: m.Idempotent,
	}
	if !info.Deadline.IsZero() {
		d := info.Deadline
		cc.Deadline = &d
	}
	first, err := call.firstPayload()
	if err != nil {
		return err
	}
	_, err = ApplyRequestInterceptors(s.icpts, cc, first)
	return err
}

func (s *AsterServer) submitTrailer(callID uint64, st RpcStatus) error {
	frame, err := trailerFrame(st)
	if err != nil {
		return err
	}
	return s.reactor.SubmitTrailer(s.ctx, callID, frame)
}

// serverCall is one inbound call: the inline first request and the response
// side. Every exit path ends it with exactly one RpcStatus trailer.
type serverCall struct {
	reactor    *Reactor
	callID     uint64
	first      []byte
	firstFlags byte
	codec      Codec
	// base is the server context: QUEUE_FULL waits end when the server stops.
	base      context.Context
	completed atomic.Bool
}

const serverRecvPollMs = 100

func (c *serverCall) firstPayload() ([]byte, error) {
	p, err := PayloadOf(c.first, c.firstFlags)
	if err != nil {
		return nil, &RpcError{Code: StatusInvalidArgument, Message: "decompress request: " + err.Error()}
	}
	return p, nil
}

func (c *serverCall) decodeFirst(dest any) error {
	p, err := c.firstPayload()
	if err != nil {
		return err
	}
	if err := c.codec.Decode(p, dest); err != nil {
		return &RpcError{Code: StatusInvalidArgument, Message: "decode request: " + err.Error()}
	}
	return nil
}

// requests yields every request payload of a client-stream / bidi call: the
// inline first frame, then frames pulled from the reactor until END_STREAM.
// An empty inline frame (a client that sent no messages) is skipped.
func (c *serverCall) requests(ctx context.Context) iter.Seq2[[]byte, error] {
	return func(yield func([]byte, error) bool) {
		if len(c.first) > 0 {
			p, err := c.firstPayload()
			if !yield(p, err) || err != nil {
				return
			}
		}
		if c.firstFlags&FlagEndStream != 0 {
			return
		}
		for {
			if err := ctx.Err(); err != nil {
				yield(nil, err)
				return
			}
			st, payload, flags, err := c.reactor.RecvFrame(c.callID, serverRecvPollMs)
			if err != nil {
				yield(nil, &RpcError{Code: StatusCancelled, Message: err.Error()})
				return
			}
			switch st {
			case RecvEndOfStream:
				return
			case RecvTimeout:
				continue
			}
			if len(payload) > 0 {
				p, err := PayloadOf(payload, flags)
				if err != nil {
					yield(nil, &RpcError{Code: StatusInvalidArgument, Message: "decompress request: " + err.Error()})
					return
				}
				if !yield(p, nil) {
					return
				}
			}
			if flags&FlagEndStream != 0 {
				return
			}
		}
	}
}

func (c *serverCall) send(ctx context.Context, v any) error {
	if c.reactor.IsCancelled(c.callID) {
		return &RpcError{Code: StatusCancelled, Message: "call cancelled by the peer"}
	}
	if err := ctx.Err(); err != nil {
		return err
	}
	payload, err := c.codec.Encode(v)
	if err != nil {
		return fmt.Errorf("encode response: %w", err)
	}
	frame, err := EncodePayloadFrame(payload, 0)
	if err != nil {
		return err
	}
	return c.reactor.SubmitFrame(ctx, c.callID, frame)
}

// completeUnary sends the single response and the OK trailer in one submit.
func (c *serverCall) completeUnary(v any) error {
	if c.completed.Swap(true) {
		return fmt.Errorf("call %d already completed", c.callID)
	}
	payload, err := c.codec.Encode(v)
	if err != nil {
		c.completed.Store(false)
		return fmt.Errorf("encode response: %w", err)
	}
	frame, err := EncodePayloadFrame(payload, 0)
	if err != nil {
		c.completed.Store(false)
		return err
	}
	trailer, err := trailerFrame(RpcStatus{Code: StatusOK})
	if err != nil {
		return err
	}
	return c.reactor.SubmitContext(c.base, c.callID, ReactorResponse{ResponseFrame: frame, TrailerFrame: trailer})
}

// complete ends the call with st (no-op once completed).
func (c *serverCall) complete(st RpcStatus) error {
	if c.completed.Swap(true) {
		return nil
	}
	frame, err := trailerFrame(st)
	if err != nil {
		return err
	}
	return c.reactor.SubmitTrailer(c.base, c.callID, frame)
}

// invokeRecover runs the handler, turning a panic into an INTERNAL error.
func invokeRecover(ctx context.Context, m *Method, call *serverCall) (err error) {
	defer func() {
		if r := recover(); r != nil {
			err = &RpcError{Code: StatusInternal, Message: fmt.Sprintf("handler panic: %v", r)}
		}
	}()
	return m.invoke(ctx, call)
}
