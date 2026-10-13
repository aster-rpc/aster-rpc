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
	"encoding/binary"
	"fmt"
	"net/netip"
	"sync/atomic"
	"time"
	"unsafe"
)

// newDefaultRuntime starts a runtime with Rust's default worker count and a
// queue sized for servers.
func newDefaultRuntime() (*Runtime, error) {
	return NewRuntime(context.Background(), RuntimeConfig{WorkerThreads: 0, EventQueueCapacity: 4096})
}

// NodeOptions are the binding-level options of StartNode.
type NodeOptions struct {
	// Gate attaches a Gate-0 policy (decided in Rust); the node then runs on
	// the gate's runtime. Without one, a config with EnableHooks raises
	// admission events on Node.Admissions.
	Gate *Gate
	// Runtime runs the node on an existing runtime (ignored when Gate is set).
	Runtime *Runtime
	// ExtraALPNs are accepted in addition to AsterConfig.ALPNs.
	ExtraALPNs []string
	// AdmissionBuffer sizes the Admissions channel (default 64). A request
	// that does not fit is allowed immediately.
	AdmissionBuffer int
}

// StartNode starts a node from cfg through the Rust aster::Node::start
// (aster_node_start). The node owns a reference to its runtime; Close
// releases it. Like Rust Node::start it ignores cfg.Enrollment: StartServer
// and StartClient install Enrollment.
func StartNode(ctx context.Context, cfg AsterConfig, opts NodeOptions) (*Node, error) {
	return startNode(ctx, cfg, opts, -1)
}

// startNode starts a node; side >= 0 (enrollmentSide*) installs Enrollment
// from cfg when it enables it (aster_node_start_enrolled).
func startNode(ctx context.Context, cfg AsterConfig, opts NodeOptions, side int) (*Node, error) {
	var rt *Runtime
	switch {
	case opts.Gate != nil:
		rt = opts.Gate.runtime.retain()
	case opts.Runtime != nil:
		rt = opts.Runtime.retain()
	default:
		var err error
		if rt, err = newDefaultRuntime(); err != nil {
			return nil, err
		}
	}
	node, err := startNodeOn(ctx, rt, cfg, opts, side)
	if err != nil {
		_ = rt.release()
		return nil, err
	}
	return node, nil
}

func startNodeOn(ctx context.Context, rt *Runtime, cfg AsterConfig, opts NodeOptions, side int) (*Node, error) {
	if side >= 0 && opts.Gate != nil && cfg.IsEnrollmentEnabled() {
		return nil, fmt.Errorf("enrollment decides admission; a Gate cannot be set with it: %w", ErrInvalidArgument)
	}
	healthSettings := &nodeHealthSettings{host: cfg.HealthHost, port: cfg.HealthPort, enabled: cfg.IsHealthEnabled()}
	var a cArena
	defer a.free()
	native, err := cfg.toNative(&a, opts.ExtraALPNs)
	if err != nil {
		return nil, err
	}
	var gate C.aster_gate_t
	if opts.Gate != nil {
		gate = C.aster_gate_t(opts.Gate.handle)
	}
	var op C.aster_operation_t
	// The config is validated and copied before the start call returns.
	if side >= 0 && opts.Gate == nil {
		trust, err := cfg.trustJSON()
		if err != nil {
			return nil, err
		}
		// Rust validates the whole enrollment plan here (fail closed) and,
		// when the config does not enable enrollment, starts a plain node.
		if err := wrapStatus("aster_node_start_enrolled", C.aster_node_start_enrolled(
			C.uint64_t(rt.handle), &native, a.bytes(trust), C.uint32_t(side), 0, &op)); err != nil {
			return nil, err
		}
	} else if err := wrapStatus("aster_node_start", C.aster_node_start(C.uint64_t(rt.handle), &native, gate, 0, &op)); err != nil {
		return nil, err
	}
	ev, err := rt.await(ctx, uint64(op))
	if err != nil {
		return nil, fmt.Errorf("aster_node_start: %w", err)
	}
	if ev.Kind != ASTER_EVENT_NODE_CREATED {
		return nil, rt.eventError(ev, "aster_node_start")
	}
	rt.releaseEventBuffer(ev)
	n := &Node{
		handle:         ev.Handle,
		runtime:        rt,
		connections:    make(chan AcceptedAster, 64),
		ownsRuntime:    true,
		healthSettings: healthSettings,
	}
	rt.registerAcceptHandler(n.handle, n)
	if ev.Related != 0 {
		// Enrollment consumes the node's hooks; it owns admission.
		n.enrollment = newEnrollment(rt, ev.Related)
		return n, nil
	}
	if opts.Gate == nil && cfg.EnableHooks {
		size := opts.AdmissionBuffer
		if size <= 0 {
			size = 64
		}
		n.admissions = make(chan *AdmissionRequest, size)
		rt.registerInbound(n.handle, n.onAdmissionEvent)
	}
	return n, nil
}

// Runtime returns the runtime that owns this node.
func (n *Node) Runtime() *Runtime { return n.runtime }

// Handle returns the native node handle.
func (n *Node) Handle() uint64 { return n.handle }

// Docs returns the document operations for this node.
func (n *Node) Docs() *Docs {
	return &Docs{node: n, runtime: n.runtime}
}

// SetGate attaches (or with nil detaches) a Gate-0 policy. The gate must
// share this node's runtime; nodes started without hooks return ErrUnsupported.
func (n *Node) SetGate(g *Gate) error {
	var h C.aster_gate_t
	if g != nil {
		if g.runtime != n.runtime {
			return fmt.Errorf("gate must be created on the node's runtime: %w", ErrInvalidArgument)
		}
		h = C.aster_gate_t(g.handle)
	}
	return wrapStatus("aster_node_set_gate", C.aster_node_set_gate(C.uint64_t(n.runtime.handle), C.uint64_t(n.handle), h))
}

// Ticket returns an aster1 ticket for this node: its direct ip:port addresses
// (waiting up to wait for them to appear) and its relay if that is an ip:port.
func (n *Node) Ticket(ctx context.Context, wait time.Duration) (string, error) {
	addr, err := n.NodeAddr()
	if err != nil {
		return "", err
	}
	deadline := time.Now().Add(wait)
	for len(addr.DirectAddresses) == 0 && time.Now().Before(deadline) {
		select {
		case <-ctx.Done():
			return "", ctx.Err()
		case <-time.After(50 * time.Millisecond):
		}
		if addr, err = n.NodeAddr(); err != nil {
			return "", err
		}
	}
	return TicketFor(addr)
}

// TicketFor encodes an aster1 ticket for addr (only ip:port addresses are
// representable).
func TicketFor(addr *NodeAddr) (string, error) {
	content := TicketContent{EndpointID: addr.EndpointID}
	if isIPPort(addr.RelayURL) {
		content.RelayAddr = addr.RelayURL
	}
	for _, d := range addr.DirectAddresses {
		if isIPPort(d) {
			content.DirectAddrs = append(content.DirectAddrs, d)
		}
	}
	return EncodeTicket(content)
}

// NodeAddrFromTicket decodes an aster1 ticket into a dialable address.
func NodeAddrFromTicket(ticket string) (*NodeAddr, error) {
	t, err := DecodeTicket(ticket)
	if err != nil {
		return nil, err
	}
	return &NodeAddr{EndpointID: t.EndpointID, DirectAddresses: t.DirectAddrs}, nil
}

func isIPPort(s string) bool {
	ap, err := netip.ParseAddrPort(s)
	return err == nil && ap.Port() != 0
}

// Connect dials addr on alpn. Cancelling ctx cancels the dial.
func (n *Node) Connect(ctx context.Context, addr *NodeAddr, alpn string) (*Connection, error) {
	return connectCore(ctx, n.runtime, n.handle, addr, alpn)
}

// AddNodeAddr teaches the node a peer's addresses (e.g. before a blob fetch).
func (n *Node) AddNodeAddr(addr *NodeAddr) error {
	var a cArena
	defer a.free()
	native := C.aster_node_addr_t{
		endpoint_id:      a.str(addr.EndpointID),
		relay_url:        a.str(addr.RelayURL),
		direct_addresses: a.list(addr.DirectAddresses),
	}
	return wrapStatus("aster_add_node_addr", C.aster_add_node_addr(C.uint64_t(n.runtime.handle), C.uint64_t(n.handle), native))
}

// Accept accepts the next inbound connection on one of the node's ALPNs
// (aster_accept takes it from the node's ALPN accept queue).
func (n *Node) Accept(ctx context.Context) (*Connection, error) {
	var op C.aster_operation_t
	if err := wrapStatus("aster_accept", C.aster_accept(C.uint64_t(n.runtime.handle), C.uint64_t(n.handle), 0, &op)); err != nil {
		return nil, err
	}
	ev, err := n.runtime.await(ctx, uint64(op))
	if err != nil {
		return nil, err
	}
	if ev.Kind != ASTER_EVENT_CONNECTION_ACCEPTED {
		return nil, n.runtime.eventError(ev, "aster_accept")
	}
	n.runtime.releaseEventBuffer(ev)
	return &Connection{handle: ev.Handle, runtime: n.runtime}, nil
}

func connectCore(ctx context.Context, rt *Runtime, handle uint64, addr *NodeAddr, alpn string) (*Connection, error) {
	if addr == nil {
		return nil, fmt.Errorf("connect: nil address: %w", ErrInvalidArgument)
	}
	var a cArena
	defer a.free()
	nodeAddr := (*C.aster_node_addr_t)(a.alloc(unsafe.Sizeof(C.aster_node_addr_t{})))
	nodeAddr.endpoint_id = a.str(addr.EndpointID)
	nodeAddr.relay_url = a.str(addr.RelayURL)
	nodeAddr.direct_addresses = a.list(addr.DirectAddresses)
	cfg := (*C.aster_connect_config_t)(a.alloc(unsafe.Sizeof(C.aster_connect_config_t{})))
	cfg.struct_size = C.uint32_t(unsafe.Sizeof(C.aster_connect_config_t{}))
	cfg.node_id = a.str(addr.EndpointID)
	cfg.alpn = a.str(alpn)
	cfg.addr = nodeAddr
	var op C.aster_operation_t
	if err := wrapStatus("aster_connect", C.aster_connect(C.uint64_t(rt.handle), C.uint64_t(handle), cfg, 0, &op)); err != nil {
		return nil, err
	}
	ev, err := rt.await(ctx, uint64(op))
	if err != nil {
		return nil, fmt.Errorf("connect %s: %w", alpn, err)
	}
	if ev.Kind != ASTER_EVENT_CONNECTED {
		return nil, rt.eventError(ev, "aster_connect")
	}
	rt.releaseEventBuffer(ev)
	return &Connection{handle: ev.Handle, runtime: rt}, nil
}

// ─── Admission (Gate 0 without a Gate) ──────────────────────────────────────

// AdmissionRequest is a pending Gate-0 decision: an outbound dial
// (ASTER_EVENT_HOOK_BEFORE_CONNECT) or an inbound connection after its
// handshake (ASTER_EVENT_HOOK_AFTER_CONNECT). Answer exactly once with Allow
// or Deny; unanswered requests time out into the config's HookFailureMode.
type AdmissionRequest struct {
	runtime    *Runtime
	invocation uint64
	answered   atomic.Bool

	// Inbound is true for an inbound connection, false for an outbound dial.
	Inbound bool
	// PeerID is the remote node id (64 hex).
	PeerID string
	ALPN   string
	// IsAlive (inbound only): the connection was alive when the event was raised.
	IsAlive bool
}

// Allow accepts the connection.
func (r *AdmissionRequest) Allow() error { return r.respond(HookAllow, 0, "") }

// Deny rejects it; an inbound connection is closed with code and reason.
func (r *AdmissionRequest) Deny(code uint32, reason string) error {
	return r.respond(HookDeny, code, reason)
}

func (r *AdmissionRequest) respond(decision HookDecision, code uint32, reason string) error {
	if r.answered.Swap(true) {
		return fmt.Errorf("admission request already answered: %w", ErrInvalidArgument)
	}
	var a cArena
	defer a.free()
	s := C.aster_admission_respond(C.uint64_t(r.runtime.handle), C.uint64_t(r.invocation),
		C.uint32_t(decision), C.uint32_t(code), a.str(reason))
	if s == C.ASTER_STATUS_NOT_FOUND { // already timed out
		return nil
	}
	return wrapStatus("aster_admission_respond", s)
}

// Admissions delivers admission requests for a node started with
// EnableHooks and no Gate (nil otherwise). The channel is closed by Close.
func (n *Node) Admissions() <-chan *AdmissionRequest { return n.admissions }

func (n *Node) onAdmissionEvent(ev *Event) {
	inbound := ev.Kind == ASTER_EVENT_HOOK_AFTER_CONNECT
	req := parseAdmission(n.runtime, ev.Related, inbound, cDataToGoBytes(ev.DataPtr, ev.DataLen))
	n.mu.RLock()
	defer n.mu.RUnlock()
	if n.closed {
		_ = req.Allow()
		return
	}
	select {
	case n.admissions <- req:
	default:
		_ = req.Allow()
	}
}

// parseAdmission decodes [u32 LE id len][id][u32 LE alpn len][alpn]
// (+ [u8 is_alive] after connect).
func parseAdmission(rt *Runtime, invocation uint64, inbound bool, p []byte) *AdmissionRequest {
	off := 0
	next := func() string {
		if len(p) < off+4 {
			return ""
		}
		n := int(binary.LittleEndian.Uint32(p[off:]))
		off += 4
		n = min(n, len(p)-off)
		s := string(p[off : off+n])
		off += n
		return s
	}
	peer := next()
	alpn := next()
	alive := inbound && off < len(p) && p[off] != 0
	return &AdmissionRequest{runtime: rt, invocation: invocation, Inbound: inbound, PeerID: peer, ALPN: alpn, IsAlive: alive}
}
