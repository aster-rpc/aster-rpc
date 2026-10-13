//go:build cgo

package aster

/*
#include <stdint.h>
#include <stdlib.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"errors"
	"fmt"
	"iter"
	"log/slog"
	"sort"
	"strconv"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"
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
	// Gate 3 (Requires) already ran in Rust.
	Interceptors []Interceptor
	// RingCapacity sizes the dispatch event ring (default 256).
	RingCapacity uint32
	// HandlerTimeout caps every call (Rust ends it with DEADLINE_EXCEEDED).
	// 0 = Rust's default (300 s); negative = no cap.
	HandlerTimeout time.Duration
	// MaxSessionsPerConnection caps session-scoped sessions per connection
	// (default 1024; enforced in Rust: RESOURCE_EXHAUSTED).
	MaxSessionsPerConnection int
	// DisableNodeInfo stops serving the baseline aster.ops.NodeInfo service.
	DisableNodeInfo bool
	// Topology explicitly serves the baseline aster.net.Topology service.
	// Config.EnableMonitoring must be true. Default false reveals no topology RPC.
	Topology bool
	// TopologyRequirement is enforced natively before topology RPC handlers.
	TopologyRequirement *Requirement
	// OnConnectionClosed, if set, runs on the poll goroutine when a peer
	// connection closes (drop per-connection state). Must not block.
	OnConnectionClosed func(connectionID uint64, peerID string)
	// OnNonRPC, if set, receives (on its own goroutine) every accepted
	// connection on an ALPN other than aster/1 (e.g. the consumer-admission
	// ALPN); the handler owns conn and must Close it. Default: closed.
	OnNonRPC func(alpn string, conn *Connection)
	// Logger receives the server's own messages, such as response failures
	// (default: the logger set with SetLogger, else slog.Default()). Rust log
	// records go to SetLogger's logger.
	Logger *slog.Logger
	// Enrollment, if set, makes Enrollment the node's admission instead of a
	// Gate (C ABI 5.1; incompatible with Gate): only peers that complete the
	// enrollment handshake reach aster/1, and the server answers inbound
	// handshakes. Reach it with AsterServer.Enrollment.
	Enrollment *EnrollmentConfig
	// EnrollmentIssuer, with Enrollment, makes the node a delegated issuer
	// and serves aster.enrollment.IssuerV1 renewals. Reach it with
	// AsterServer.EnrollmentIssuer.
	EnrollmentIssuer *IssuerConfig
	// Health, if set, starts an ops listener (/healthz, /readyz, /metrics)
	// for the server's node and ties its readiness to the server: ready once
	// the server serves, not ready first on Close. Reach it with
	// AsterServer.Health. Nil = the listener Config asks for (HealthPort /
	// HealthEnabled, i.e. ASTER_HEALTH_* or [health]), if any.
	Health *HealthConfig
}

// AsterServer serves Services over aster/1 through Rust's aster::rpc::Server
// (ABI 3.5 dynamic dispatch): Rust decodes headers, routes, enforces Gate 3
// (Requires), sessions, deadlines and frame limits, and serves NodeInfo; Go
// runs the handlers. Every call ends with exactly one AEv1 RpcStatus: a
// handler error becomes its *RpcError code, a context deadline
// DEADLINE_EXCEEDED, anything else INTERNAL.
//
//	srv, err := aster.StartServer(ctx, aster.ServerOptions{Config: cfg, Services: []*aster.Service{svc}})
//	defer srv.Close()
//	ticket, err := srv.Ticket(ctx)
type AsterServer struct {
	node      *Node
	gate      *Gate
	ownedGate bool
	ds        *dispatchServer
	codec     Codec
	byIndex   []*Service // service_idx -> service
	icpts     []Interceptor
	onClosed  func(uint64, string)
	onNonRPC  func(string, *Connection)
	logger    *slog.Logger
	health    *HealthServer
	issuer    *EnrollmentIssuer

	ctx     context.Context
	cancel  context.CancelFunc
	pollWG  sync.WaitGroup
	calls   sync.WaitGroup
	closing atomic.Bool
	closeMu sync.Mutex

	// mu guards cancels and sessions.
	mu sync.Mutex
	// cancels: each live call's context cancel, fired on CANCELLED.
	cancels map[uint64]callCancel
	// sessions: Rust session_key -> instance record (created on the poll
	// goroutine ahead of the session's SESSION_CLOSED).
	sessions map[uint64]*sessionRecord
	// peers caches the peer id per connection (poll goroutine only).
	peers map[uint64]string
}

const pollBatch = 64

// StartServer starts a node and serves opts.Services on it.
func StartServer(ctx context.Context, opts ServerOptions) (*AsterServer, error) {
	seen := make(map[string]bool, len(opts.Services))
	jsons := make([]string, 0, len(opts.Services))
	for _, s := range opts.Services {
		key := s.Name + "@" + strconv.Itoa(int(s.Version))
		if seen[key] {
			return nil, fmt.Errorf("service %s v%d registered twice: %w", s.Name, s.Version, ErrInvalidArgument)
		}
		seen[key] = true
		if s.Scope == ScopeSession && s.newSession == nil {
			return nil, fmt.Errorf("service %s: session-scoped services need a factory (NewSessionService): %w", s.Name, ErrInvalidArgument)
		}
		s.lower()
		if s.contractEr != nil {
			return nil, fmt.Errorf("service %s: %w", s.Name, s.contractEr)
		}
		jsons = append(jsons, s.dynJSON)
	}
	codec := opts.Codec
	if codec == nil {
		codec = NewForyCodec()
	}
	if fc, ok := codec.(*ForyCodec); ok {
		for _, s := range opts.Services {
			if err := s.RegisterTypes(fc); err != nil {
				return nil, err
			}
		}
	}

	if opts.EnrollmentIssuer != nil && opts.Enrollment == nil {
		return nil, fmt.Errorf("EnrollmentIssuer needs Enrollment: %w", ErrInvalidArgument)
	}
	if opts.Enrollment != nil && opts.Gate != nil {
		return nil, fmt.Errorf("Enrollment and Gate both decide admission; set one: %w", ErrInvalidArgument)
	}
	gate, owned := opts.Gate, false
	if gate == nil && opts.Enrollment == nil {
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
	node, err := StartNode(ctx, opts.Config, NodeOptions{Gate: gate, ExtraALPNs: []string{AsterALPN, ConsumerAdmissionALPN}, Enrollment: opts.Enrollment})
	if err != nil {
		return fail(err)
	}
	var issuer *EnrollmentIssuer
	if opts.EnrollmentIssuer != nil {
		if issuer, err = node.Enrollment().NewIssuer(*opts.EnrollmentIssuer); err != nil {
			_ = node.Close()
			return fail(err)
		}
	}
	EnsureObservability()
	var health *HealthServer
	healthCfg := opts.Health
	if healthCfg == nil && opts.Config.IsHealthEnabled() {
		healthCfg = &HealthConfig{Host: opts.Config.HealthHost, Port: opts.Config.HealthPort}
	}
	if healthCfg != nil {
		if health, err = StartHealth(node, healthCfg); err != nil {
			_ = node.Close()
			return fail(err)
		}
	}
	settings := serverSettings{
		health:              health,
		disableNodeInfo:     opts.DisableNodeInfo,
		topology:            opts.Topology,
		topologyRequirement: opts.TopologyRequirement,
		ringCapacity:        opts.RingCapacity,
		enrollment:          node.Enrollment(),
		issuer:              issuer,
	}
	if opts.MaxSessionsPerConnection > 0 {
		settings.maxSessionsPerConnection = uint64(opts.MaxSessionsPerConnection)
	}
	switch {
	case opts.HandlerTimeout < 0:
		settings.setHandlerTimeout, settings.handlerTimeoutMs = true, 0
	case opts.HandlerTimeout > 0:
		settings.setHandlerTimeout = true
		settings.handlerTimeoutMs = uint64(max(opts.HandlerTimeout.Milliseconds(), 1))
	}
	ds, err := startDispatchServer(node, jsons, settings)
	if err != nil {
		if health != nil {
			_ = health.Stop()
		}
		_ = node.Close()
		return fail(err)
	}
	logger := opts.Logger
	if logger == nil {
		logger = currentLogger()
	}
	sctx, cancel := context.WithCancel(context.Background())
	s := &AsterServer{
		node: node, gate: gate, ownedGate: owned, ds: ds, codec: codec, issuer: issuer,
		byIndex: append([]*Service(nil), opts.Services...), icpts: opts.Interceptors,
		onClosed: opts.OnConnectionClosed, onNonRPC: opts.OnNonRPC,
		logger: logger, health: health, ctx: sctx, cancel: cancel,
		cancels: map[uint64]callCancel{}, sessions: map[uint64]*sessionRecord{},
		peers: map[uint64]string{},
	}
	s.pollWG.Add(1)
	go s.pollLoop()
	return s, nil
}

// Health is the ops listener started from ServerOptions.Health (nil if none).
func (s *AsterServer) Health() *HealthServer { return s.health }

// Node is the server's node.
func (s *AsterServer) Node() *Node { return s.node }

// Gate is the Gate-0 policy guarding the node (admit peers on it unless dev
// mode is on); nil when Enrollment decides admission.
func (s *AsterServer) Gate() *Gate { return s.gate }

// Enrollment is the node's Enrollment (ServerOptions.Enrollment), or nil.
func (s *AsterServer) Enrollment() *Enrollment { return s.node.Enrollment() }

// EnrollmentIssuer is the delegated issuer (ServerOptions.EnrollmentIssuer), or nil.
func (s *AsterServer) EnrollmentIssuer() *EnrollmentIssuer { return s.issuer }

// NodeID is the server's node id (64 hex).
func (s *AsterServer) NodeID() (string, error) { return s.node.NodeID() }

// Ticket returns an aster1 ticket for the server (waits up to 5 s for direct
// addresses).
func (s *AsterServer) Ticket(ctx context.Context) (string, error) {
	return s.node.Ticket(ctx, 5*time.Second)
}

// SetPeerAttributes sets the attributes Gate 3 checks Requires against for
// calls from peer (64-hex node id), and that handlers see in
// CallInfo.Attributes. Empty or nil clears them.
func (s *AsterServer) SetPeerAttributes(peer string, attributes map[string]string) error {
	entries := make([]MetadataEntry, 0, len(attributes))
	for k, v := range attributes {
		entries = append(entries, MetadataEntry{k, v})
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].Key < entries[j].Key })
	return s.ds.setPeerAttributes(peer, encodeKV(entries))
}

// LiveCalls is the number of calls in Rust's call table (diagnostics).
func (s *AsterServer) LiveCalls() uint64 { return s.ds.liveCalls() }

// QueueFullHits counts responses that hit a full response queue and waited
// for capacity.
func (s *AsterServer) QueueFullHits() uint64 { return s.ds.queueFullHits.Load() }

// Close stops dispatching, cancels in-flight handlers (their contexts end),
// waits for them, then destroys the dispatch server and closes the node and
// owned gate. Session instances still open are closed.
func (s *AsterServer) Close() error {
	s.closeMu.Lock()
	defer s.closeMu.Unlock()
	if s.closing.Swap(true) {
		return nil
	}
	s.cancel()
	s.pollWG.Wait()
	// A client-stream / bidi handler blocked on its next request returns
	// once the connection closes, so bound the wait before tearing down.
	callsDone := make(chan struct{})
	go func() { s.calls.Wait(); close(callsDone) }()
	select {
	case <-callsDone:
	case <-time.After(2 * time.Second):
	}
	err := s.ds.destroy() // clears the attached listener's ready flag first
	if s.health != nil {
		err = errors.Join(err, s.health.Stop())
	}
	s.mu.Lock()
	sessions := s.sessions
	s.sessions = map[uint64]*sessionRecord{}
	s.mu.Unlock()
	for _, rec := range sessions {
		rec.close()
	}
	err = errors.Join(err, s.node.Close())
	select { // the closed connections end any remaining request waits
	case <-callsDone:
	case <-time.After(2 * time.Second):
	}
	if s.ownedGate {
		err = errors.Join(err, s.gate.Close())
	}
	return err
}

func (s *AsterServer) pollLoop() {
	defer s.pollWG.Done()
	buf := (*C.aster_dispatch_event_t)(C.calloc(pollBatch, C.size_t(unsafe.Sizeof(C.aster_dispatch_event_t{}))))
	if buf == nil {
		panic("aster: calloc failed")
	}
	defer C.free(unsafe.Pointer(buf))
	for !s.closing.Load() {
		for _, ev := range s.ds.poll(buf, pollBatch, 100) {
			s.onEvent(&ev)
		}
	}
}

func (s *AsterServer) onEvent(ev *C.aster_dispatch_event_t) {
	switch ev.kind {
	case dispatchEventCall:
		s.onCall(ev)
	case dispatchEventResponseWritable:
		s.ds.capacityEvent(uint64(ev.call_id), true)
	case dispatchEventResponseClosed:
		s.ds.capacityEvent(uint64(ev.call_id), false)
	case dispatchEventCancelled:
		// Peer cancel (no trailer), deadline / corrupt request (Rust already
		// answered): end the handler's context.
		s.mu.Lock()
		cc, ok := s.cancels[uint64(ev.call_id)]
		s.mu.Unlock()
		if !ok {
			break
		}
		if ev.reason == C.ASTER_CANCEL_REASON_DEADLINE && !cc.deadline.IsZero() {
			// The handler's context carries the same deadline: let it end
			// that way (ctx.Err() == DeadlineExceeded, not Canceled), with
			// a backstop should the local clock lag Rust's.
			time.AfterFunc(time.Until(cc.deadline)+deadlineCancelGrace, cc.cancel)
			break
		}
		cc.cancel()
	case dispatchEventSessionClosed:
		s.mu.Lock()
		rec := s.sessions[uint64(ev.session_key)]
		delete(s.sessions, uint64(ev.session_key))
		s.mu.Unlock()
		if rec != nil {
			go rec.close()
		}
	case dispatchEventConnectionClosed:
		connID := uint64(ev.connection_id)
		peer := s.peers[connID]
		if peer == "" {
			peer = cString(ev.peer_ptr, ev.peer_len)
		}
		delete(s.peers, connID)
		s.ds.releaseBuffer(ev.buffer)
		if s.onClosed != nil {
			s.onClosed(connID, peer)
		}
	case dispatchEventNonRPC:
		alpn := cString(ev.request_ptr, ev.request_len)
		s.ds.releaseBuffer(ev.buffer)
		conn := &Connection{handle: uint64(ev.connection_handle), runtime: s.node.runtime}
		if s.onNonRPC == nil {
			_ = conn.Close("")
			return
		}
		go s.onNonRPC(alpn, conn)
	default:
		// A newer minor ABI's event kind: release what it carries.
		s.ds.releaseBuffer(ev.buffer)
	}
}

func cString(p *C.uint8_t, n C.uint32_t) string {
	if p == nil || n == 0 {
		return ""
	}
	return C.GoStringN((*C.char)(unsafe.Pointer(p)), C.int(n))
}

// inbound is one CALL event, copied out on the poll goroutine.
type inbound struct {
	callID     uint64
	svc        *Service
	method     *Method
	flags      uint32
	connID     uint64
	sessionID  int32
	deadline   time.Time
	request    []byte
	peer       string
	session    *sessionRecord
	ctx        context.Context // cancelled by the call's CANCELLED event
	cancel     context.CancelFunc
	firstIsEnd bool
}

func (s *AsterServer) onCall(ev *C.aster_dispatch_event_t) {
	in := inbound{
		callID:    uint64(ev.call_id),
		flags:     uint32(ev.flags),
		connID:    uint64(ev.connection_id),
		sessionID: int32(ev.session_id),
	}
	in.firstIsEnd = in.flags&dispatchFlagFirstIsEnd != 0
	if ev.request_len > 0 {
		in.request = C.GoBytes(unsafe.Pointer(ev.request_ptr), C.int(ev.request_len))
	}
	peer := cString(ev.peer_ptr, ev.peer_len)
	// HTTP/WT calls have connection ID zero and resolve a principal per call.
	// Only authenticated Iroh peers can share a connection identity cache.
	if in.flags&dispatchFlagIrohAuthenticated != 0 {
		if cached, ok := s.peers[in.connID]; ok {
			peer = cached
		} else {
			s.peers[in.connID] = peer
		}
	}
	in.peer = peer
	s.ds.releaseBuffer(ev.buffer)
	if d := int64(ev.deadline_unix_ms); d > 0 {
		in.deadline = time.UnixMilli(d)
	}

	svcIdx, methodIdx := int(ev.service_idx), int(ev.method_idx)
	if svcIdx >= len(s.byIndex) || methodIdx >= len(s.byIndex[svcIdx].order) {
		st, _ := EncodeRpcStatus(RpcStatus{Code: StatusUnimplemented, Message: fmt.Sprintf("no handler for service %d method %d", svcIdx, methodIdx)})
		_ = s.ds.respond(s.ctx, in.callID, nil, st)
		return
	}
	in.svc = s.byIndex[svcIdx]
	in.method = in.svc.order[methodIdx]
	if key := uint64(ev.session_key); key != 0 {
		s.mu.Lock()
		rec := s.sessions[key]
		if rec == nil {
			rec = &sessionRecord{factory: in.svc.newSession, info: SessionInfo{
				Service: in.svc.Name, PeerID: peer, ConnectionID: in.connID, SessionID: in.sessionID,
			}}
			s.sessions[key] = rec
		}
		s.mu.Unlock()
		in.session = rec
	}
	// Every pattern, unary included: Rust delivers a client's cancel of a
	// unary call as CANCELLED{PEER} too. Registered here, ahead of the call's
	// CANCELLED event.
	in.ctx, in.cancel = context.WithCancel(s.ctx)
	if !in.deadline.IsZero() {
		cctx, ccancel := in.ctx, in.cancel
		dctx, dcancel := context.WithDeadline(cctx, in.deadline)
		in.ctx, in.cancel = dctx, func() { dcancel(); ccancel() }
	}
	s.mu.Lock()
	s.cancels[in.callID] = callCancel{cancel: in.cancel, deadline: in.deadline}
	s.mu.Unlock()
	s.calls.Add(1)
	go s.serve(in)
}

// callCancel ends one live call's handler context.
type callCancel struct {
	cancel   context.CancelFunc
	deadline time.Time
}

// deadlineCancelGrace: how long after its deadline a call cancelled by Rust
// for DEADLINE waits for its own context deadline before being cancelled.
const deadlineCancelGrace = 250 * time.Millisecond

func (s *AsterServer) serve(in inbound) {
	defer s.calls.Done()
	ctx := in.ctx
	if ctx == nil {
		ctx = s.ctx
	} else {
		defer func() {
			s.mu.Lock()
			delete(s.cancels, in.callID)
			s.mu.Unlock()
			in.cancel()
		}()
	}
	info := &CallInfo{
		Service: in.svc.Name, Method: in.method.Name, Pattern: in.method.Pattern,
		PeerID: in.peer, ConnectionID: in.connID, SessionID: in.sessionID, CallID: in.callID,
		Deadline: in.deadline, IrohAuthenticated: in.flags&dispatchFlagIrohAuthenticated != 0,
	}
	if in.flags&dispatchFlagHasMetadata != 0 {
		info.Metadata = s.ds.kvOut(in.callID, false)
	}
	if in.flags&dispatchFlagHasAttributes != 0 {
		info.Attributes = s.ds.kvOut(in.callID, true)
	}
	if !in.deadline.IsZero() {
		var dcancel context.CancelFunc
		ctx, dcancel = context.WithDeadline(ctx, in.deadline)
		defer dcancel()
	}
	ctx = context.WithValue(ctx, callInfoKey{}, info)
	// The server span's context and the inbound baggage ride in ctx: spans and
	// logs started from a handler nest under the call, and outbound calls
	// continue the trace.
	ctx = s.ds.callTraceContext(ctx, in.callID, in.flags, info.Metadata)

	call := &serverCall{
		ds: s.ds, callID: in.callID, first: in.request, firstIsEnd: in.firstIsEnd,
		streaming: in.method.Pattern == PatternServerStream || in.method.Pattern == PatternBidiStream,
		codec:     s.codec, base: s.ctx,
	}
	err := s.intercept(info, in.method, call)
	if err == nil {
		err = invokeRecover(ctx, in.method, in.session, call)
	}
	if err == nil {
		return // the method completed the call
	}
	st := StatusFromError(err)
	if errors.Is(ctx.Err(), context.DeadlineExceeded) && s.ctx.Err() == nil {
		st = RpcStatus{Code: StatusDeadlineExceeded, Message: "deadline exceeded"}
	} else if s.ctx.Err() != nil && st.Code == StatusCancelled {
		st = RpcStatus{Code: StatusUnavailable, Message: "server shutting down"}
	}
	if err := call.complete(st); err != nil && !errors.Is(err, errCallGone) {
		s.logger.LogAttrs(ctx, slog.LevelWarn, "aster: trailer failed",
			slog.Uint64("call_id", in.callID), slog.String("service", in.svc.Name),
			slog.String("method", in.method.Name), slog.Any("error", err))
	}
}

func (s *AsterServer) intercept(info *CallInfo, m *Method, call *serverCall) error {
	if len(s.icpts) == 0 {
		return nil
	}
	md := make(map[string]string, len(info.Metadata))
	for _, e := range info.Metadata {
		md[e.Key] = e.Value
	}
	attrs := make(map[string]string, len(info.Attributes))
	for _, e := range info.Attributes {
		attrs[e.Key] = e.Value
	}
	cc := &CallContext{
		Service: info.Service, Method: info.Method, CallID: strconv.FormatUint(info.CallID, 10),
		Peer: info.PeerID, Metadata: md, Attributes: attrs, IsStreaming: m.Pattern != PatternUnary,
		Pattern: m.Pattern.String(), Idempotent: m.Idempotent,
	}
	if info.SessionID != 0 {
		cc.SessionID = strconv.Itoa(int(info.SessionID))
	}
	if !info.Deadline.IsZero() {
		d := info.Deadline
		cc.Deadline = &d
	}
	_, err := ApplyRequestInterceptors(s.icpts, cc, call.first)
	return err
}

// sessionRecord holds one session's instance; the factory runs on the
// session's first call (on that call's goroutine).
type sessionRecord struct {
	mu      sync.Mutex
	factory func(SessionInfo) any
	info    SessionInfo
	inst    any
	made    bool
	closed  bool
}

func (r *sessionRecord) get() (any, error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.closed {
		return nil, &RpcError{Code: StatusNotFound, Message: "session closed"}
	}
	if !r.made {
		r.made = true // a panicking factory is not retried
		r.inst = r.factory(r.info)
	}
	return r.inst, nil
}

func (r *sessionRecord) close() {
	r.mu.Lock()
	r.closed = true
	inst, made := r.inst, r.made
	r.inst = nil
	r.mu.Unlock()
	if made && inst != nil {
		closeInstance(inst)
	}
}

// serverCall is one inbound call: the first request payload (already
// decompressed by Rust) and the response side. Every exit path ends it with
// exactly one RpcStatus.
type serverCall struct {
	ds         *dispatchServer
	callID     uint64
	first      []byte
	firstIsEnd bool
	streaming  bool // server-stream / bidi: the trailer is a finish
	codec      Codec
	// base is the server context: QUEUE_FULL waits end when the server stops.
	base      context.Context
	completed atomic.Bool
}

func (c *serverCall) decodeFirst(dest any) error {
	if err := c.codec.Decode(c.first, dest); err != nil {
		return &RpcError{Code: StatusInvalidArgument, Message: "decode request: " + err.Error()}
	}
	return nil
}

// requests yields every request payload of a client-stream / bidi call: the
// first (from the CALL event), then payloads pulled from Rust until the end
// of the request stream. An empty first payload (a client that sent no
// messages) is skipped.
func (c *serverCall) requests(ctx context.Context) iter.Seq2[[]byte, error] {
	return func(yield func([]byte, error) bool) {
		if len(c.first) > 0 {
			if !yield(c.first, nil) {
				return
			}
		}
		if c.firstIsEnd {
			return
		}
		for {
			if err := ctx.Err(); err != nil {
				yield(nil, err)
				return
			}
			// Bounded wait: a payload, the end of the request stream, the
			// call / connection being over, or TIMEOUT -- then ctx (cancelled
			// on a peer cancel / deadline, or by the server stopping) and the
			// call's state are checked before waiting again.
			st, payload, err := c.ds.recv(c.callID, recvSliceMs)
			if err != nil {
				yield(nil, &RpcError{Code: StatusInternal, Message: err.Error()})
				return
			}
			switch st {
			case recvEnd:
				// Rust also ends the request stream when the peer cancels
				// (FLAG_CANCEL / reset): that is not a clean end.
				if ctx.Err() != nil {
					yield(nil, ctx.Err())
				} else if c.ds.isCancelled(c.callID) {
					yield(nil, errCallGone)
				}
				return
			case recvTimeout:
				if c.ds.isCancelled(c.callID) {
					yield(nil, errCallGone)
					return
				}
				continue
			case recvGone:
				yield(nil, errCallGone)
				return
			}
			if !yield(payload, nil) {
				return
			}
		}
	}
}

func (c *serverCall) send(ctx context.Context, v any) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	payload, err := c.codec.Encode(v)
	if err != nil {
		return fmt.Errorf("encode response: %w", err)
	}
	if len(payload) == 0 {
		return fmt.Errorf("encode response: empty payload: %w", ErrInvalidArgument)
	}
	return c.ds.send(c.base, c.callID, payload)
}

// respond sends the single response with an OK status (unary / client-stream).
func (c *serverCall) respond(v any) error {
	if c.completed.Swap(true) {
		return fmt.Errorf("call %d already completed", c.callID)
	}
	payload, err := c.codec.Encode(v)
	if err != nil {
		c.completed.Store(false)
		return fmt.Errorf("encode response: %w", err)
	}
	if err := c.ds.respond(c.base, c.callID, payload, nil); err != nil && !errors.Is(err, errCallGone) {
		return err
	}
	return nil
}

// complete ends the call with st (no-op once completed).
func (c *serverCall) complete(st RpcStatus) error {
	if c.completed.Swap(true) {
		return nil
	}
	var status []byte
	if st.Code != StatusOK {
		var err error
		if status, err = EncodeRpcStatus(st); err != nil {
			c.ds.abandon(c.callID)
			return err
		}
	}
	if c.streaming {
		return c.ds.finish(c.base, c.callID, status)
	}
	return c.ds.respond(c.base, c.callID, nil, status)
}

// invokeRecover resolves the session instance and runs the handler, turning
// a panic into an INTERNAL error.
func invokeRecover(ctx context.Context, m *Method, session *sessionRecord, call *serverCall) (err error) {
	defer func() {
		if r := recover(); r != nil {
			err = &RpcError{Code: StatusInternal, Message: fmt.Sprintf("handler panic: %v", r)}
		}
	}()
	if session != nil {
		inst, err := session.get()
		if err != nil {
			return err
		}
		ctx = context.WithValue(ctx, sessionKey{}, inst)
	}
	return m.invoke(ctx, call)
}
