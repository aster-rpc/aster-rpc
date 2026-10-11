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
	"runtime"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"
)

// C ABI 4.1 upcalls (design: docs/_internal/bindings/upcall-design.md §4).
//
// Several aster extension points are Rust traits (lease serializer and
// authorizer, fenced resource, topology admitted filter, expose authorize /
// HTTP handler / route policy). A binding cannot hand Rust a function
// pointer, so Rust asks Go a question as an event and Go answers by id:
// one Upcalls table per runtime, one poll goroutine, one goroutine per
// upcall so a slow handler holds nothing else up. When Rust stops waiting
// (timeout, abandoned caller, shutdown) the handler's context is cancelled.

// FailureMode is what Rust does with a decision kind (lease authorize,
// topology admitted, expose authorize / route policy) when the binding gives
// no usable answer (error, timeout, overload, shutdown, undecodable answer).
type FailureMode uint32

const (
	// FailureModeOpen answers "allow".
	FailureModeOpen FailureMode = C.ASTER_HOOK_FAILURE_FAIL_OPEN
	// FailureModeClosed answers "deny" (the default of every kind).
	FailureModeClosed FailureMode = C.ASTER_HOOK_FAILURE_FAIL_CLOSED
)

// DefaultUpcallCapacity is ASTER_UPCALL_DEFAULT_CAPACITY.
const DefaultUpcallCapacity = C.ASTER_UPCALL_DEFAULT_CAPACITY

// Event kinds raised by the 4.1 operations.
const (
	ASTER_EVENT_LEASE_ACQUIRED = C.ASTER_EVENT_LEASE_ACQUIRED
	ASTER_EVENT_LEASE_STATUS   = C.ASTER_EVENT_LEASE_STATUS
)

// ErrUpcallUnimplemented, returned by a handler, answers UNIMPLEMENTED
// ("this binding does not serve the kind") instead of ERROR.
var ErrUpcallUnimplemented = errors.New("upcall: not implemented")

// ErrUpcallsClosed is returned by operations on a closed table.
var ErrUpcallsClosed = fmt.Errorf("upcalls closed: %w", ErrAlreadyClosed)

// UpcallRequest is one upcall being served.
type UpcallRequest struct {
	// ID is the invocation id Rust waits on.
	ID   uint64
	Kind UpcallKind
	// Payload is the kind's UCv1 request (a private copy).
	Payload []byte
	// Deadline is when Rust stops waiting for the answer (informational:
	// Rust enforces it, and for expose HTTP it bounds the response head
	// only, so it is not applied to the handler's context).
	Deadline time.Time
	// CancelReason is set on CANCEL events only.
	CancelReason CancelReason

	up       *Upcalls
	answered atomic.Bool
}

// Respond answers the upcall now (the first answer wins; a handler that has
// answered early may keep running, e.g. to stream a response body, and its
// return value is then ignored). It reports whether Rust accepted the
// answer: false when it no longer waits (answered, cancelled, timed out) --
// the normal outcome of a late answer.
func (r *UpcallRequest) Respond(status UpcallStatus, payload []byte) bool {
	if !r.answered.CompareAndSwap(false, true) {
		return false
	}
	return r.up.respond(r.ID, status, payload)
}

// Answered reports whether Respond has been called.
func (r *UpcallRequest) Answered() bool { return r.answered.Load() }

// UpcallHandler serves one upcall kind: the UCv1 response on success. An
// error answers ERROR with its message (ErrUpcallUnimplemented answers
// UNIMPLEMENTED). ctx is cancelled when Rust stops waiting and when the
// table closes; a panic answers ERROR. Handlers run concurrently.
type UpcallHandler func(ctx context.Context, req *UpcallRequest) ([]byte, error)

const (
	upcallBatch  = 32
	upcallWaitMs = 100
	closeGrace   = 2 * time.Second
	minPollTime  = 5 * time.Millisecond
)

// Upcalls is one upcall table (aster_upcalls_t) with its poll goroutine.
type Upcalls struct {
	rt     *Runtime
	handle uint64

	mu       sync.RWMutex
	handlers map[UpcallKind]UpcallHandler

	inMu     sync.Mutex
	inflight map[uint64]context.CancelFunc

	ctx    context.Context
	cancel context.CancelFunc

	closing   atomic.Bool
	closeOnce sync.Once
	closeErr  error
	pollDone  chan struct{}
	handlerWG sync.WaitGroup
	cancels   atomic.Int64

	http httpRegistry // foreign HTTP services served on this table (expose_http.go)
}

// NewUpcalls creates a table queueing at most capacity upcalls (0 =
// DefaultUpcallCapacity; beyond it upcalls are refused at once: decisions
// take their failure mode, other kinds fail) and starts its poll goroutine.
// Every kind starts with the default policy: 5 s, fail closed. The runtime
// must outlive the table; Close the table first.
func NewUpcalls(rt *Runtime, capacity uint32) (*Upcalls, error) {
	if rt == nil {
		return nil, fmt.Errorf("upcalls: nil runtime: %w", ErrInvalidArgument)
	}
	var h C.aster_upcalls_t
	if err := wrapStatus("aster_upcalls_new", C.aster_upcalls_new(C.uint64_t(rt.handle), C.uint32_t(capacity), &h)); err != nil {
		return nil, err
	}
	ctx, cancel := context.WithCancel(context.Background())
	u := &Upcalls{
		rt:       rt,
		handle:   uint64(h),
		handlers: make(map[UpcallKind]UpcallHandler),
		inflight: make(map[uint64]context.CancelFunc),
		ctx:      ctx,
		cancel:   cancel,
		pollDone: make(chan struct{}),
	}
	go u.pollLoop()
	return u, nil
}

// Runtime is the runtime the table lives on.
func (u *Upcalls) Runtime() *Runtime { return u.rt }

// Handle is the native aster_upcalls_t.
func (u *Upcalls) Handle() uint64 { return u.handle }

// CancelCount is the number of CANCEL events received so far (diagnostics).
func (u *Upcalls) CancelCount() int64 { return u.cancels.Load() }

func (u *Upcalls) rtH() C.uint64_t { return C.uint64_t(u.rt.handle) }

// SetPolicy sets the timeout and failure mode of one kind (default 5 s, fail
// closed). The failure mode matters for decision kinds. For expose HTTP the
// timeout bounds the response head only.
func (u *Upcalls) SetPolicy(kind UpcallKind, timeout time.Duration, mode FailureMode) error {
	if kind == UpcallCancel {
		return fmt.Errorf("upcalls: CANCEL has no policy: %w", ErrInvalidArgument)
	}
	return wrapStatus("aster_upcall_set_policy",
		C.aster_upcall_set_policy(u.rtH(), C.uint64_t(u.handle), C.uint32_t(kind), millis32(timeout), C.uint32_t(mode)))
}

// SetDefaultPolicy sets the policy of every kind without its own.
func (u *Upcalls) SetDefaultPolicy(timeout time.Duration, mode FailureMode) error {
	return wrapStatus("aster_upcall_set_default_policy",
		C.aster_upcall_set_default_policy(u.rtH(), C.uint64_t(u.handle), millis32(timeout), C.uint32_t(mode)))
}

func millis32(d time.Duration) C.uint32_t {
	ms := d.Milliseconds()
	if ms < 1 {
		ms = 1
	}
	if ms > 1<<31-1 {
		ms = 1<<31 - 1
	}
	return C.uint32_t(ms)
}

// HandleKind registers h for kind (replacing any earlier one). A kind without a
// handler answers UNIMPLEMENTED.
func (u *Upcalls) HandleKind(kind UpcallKind, h UpcallHandler) error {
	if kind == UpcallCancel {
		return fmt.Errorf("upcalls: CANCEL is not answerable: %w", ErrInvalidArgument)
	}
	u.mu.Lock()
	defer u.mu.Unlock()
	if h == nil {
		delete(u.handlers, kind)
	} else {
		u.handlers[kind] = h
	}
	return nil
}

func (u *Upcalls) handler(kind UpcallKind) UpcallHandler {
	u.mu.RLock()
	defer u.mu.RUnlock()
	return u.handlers[kind]
}

// pollLoop drains aster_upcall_poll until the table closes. The native poll
// blocks its OS thread for at most upcallWaitMs, so it needs no wake-up on
// close (aster_upcalls_close also ends a wait).
func (u *Upcalls) pollLoop() {
	defer close(u.pollDone)
	var events [upcallBatch]C.aster_upcall_event_t
	for !u.closing.Load() && !u.rt.closed.Load() {
		t0 := time.Now()
		var cn C.uint32_t
		st := C.aster_upcall_poll(u.rtH(), C.uint64_t(u.handle), &events[0], upcallBatch, upcallWaitMs, &cn)
		n := int(cn)
		if st != C.ASTER_STATUS_OK || n == 0 {
			// A healthy poll waited; a refusal (bad handle, closed runtime)
			// or an instant 0 must not spin.
			if time.Since(t0) < minPollTime {
				time.Sleep(minPollTime)
			}
			continue
		}
		for i := 0; i < n; i++ {
			u.dispatch(u.readEvent(&events[i]))
			events[i] = C.aster_upcall_event_t{}
		}
	}
}

// readEvent copies the event's payload into Go memory and releases the
// native buffer exactly once (buffer 0 = nothing to release).
func (u *Upcalls) readEvent(ev *C.aster_upcall_event_t) *UpcallRequest {
	req := &UpcallRequest{
		ID:   uint64(ev.invocation_id),
		Kind: UpcallKind(ev.kind),
		up:   u,
	}
	if ev.deadline_unix_ms != 0 {
		req.Deadline = time.UnixMilli(int64(ev.deadline_unix_ms))
	}
	if ev.payload_ptr != nil && ev.payload_len > 0 {
		req.Payload = C.GoBytes(unsafe.Pointer(ev.payload_ptr), C.int(ev.payload_len))
	} else {
		req.Payload = []byte{}
	}
	if ev.buffer != 0 {
		C.aster_upcall_buffer_release(u.rtH(), C.uint64_t(u.handle), ev.buffer)
	}
	req.CancelReason = CancelReason(ev.cancel_reason)
	return req
}

func (u *Upcalls) dispatch(req *UpcallRequest) {
	if req.Kind == UpcallCancel {
		u.cancels.Add(1)
		u.inMu.Lock()
		cancel := u.inflight[req.ID]
		delete(u.inflight, req.ID)
		u.inMu.Unlock()
		if cancel != nil {
			cancel()
		}
		return
	}
	h := u.handler(req.Kind)
	if h == nil {
		req.Respond(UpcallStatusUnimplemented, nil)
		return
	}
	ctx, cancel := context.WithCancel(u.ctx)
	u.inMu.Lock()
	u.inflight[req.ID] = cancel
	u.inMu.Unlock()
	u.handlerWG.Add(1)
	go func() {
		defer u.handlerWG.Done()
		defer func() {
			u.inMu.Lock()
			delete(u.inflight, req.ID)
			u.inMu.Unlock()
			cancel()
		}()
		u.serve(ctx, h, req)
	}()
}

func (u *Upcalls) serve(ctx context.Context, h UpcallHandler, req *UpcallRequest) {
	var (
		out []byte
		err error
	)
	func() {
		defer func() {
			if p := recover(); p != nil {
				err = fmt.Errorf("handler panic: %v", p)
			}
		}()
		out, err = h(ctx, req)
	}()
	switch {
	case req.Answered():
		// answered early (streaming); nothing more to say
	case errors.Is(err, ErrUpcallUnimplemented):
		req.Respond(UpcallStatusUnimplemented, nil)
	case err != nil:
		req.Respond(UpcallStatusError, []byte(err.Error()))
	default:
		req.Respond(UpcallStatusOK, out)
	}
}

// respond answers invocation id; false when Rust no longer waits for it.
func (u *Upcalls) respond(id uint64, status UpcallStatus, payload []byte) bool {
	if u.closing.Load() {
		return false
	}
	var p *C.uint8_t
	if len(payload) > 0 {
		p = (*C.uint8_t)(unsafe.Pointer(&payload[0]))
	}
	st := C.aster_upcall_respond(u.rtH(), C.uint64_t(u.handle), C.uint64_t(id), C.uint32_t(status), p, C.uint32_t(len(payload)))
	runtime.KeepAlive(payload)
	return st == 0
}

// Respond answers invocation id directly (for raw consumers of the table).
// Returns false when Rust no longer waits for it.
func (u *Upcalls) Respond(id uint64, status UpcallStatus, payload []byte) bool {
	return u.respond(id, status, payload)
}

// CloseTable closes the native table only: pending upcalls fail now
// (decisions take their failure mode) and each gets a CANCEL{shutdown}.
// Handlers and the poll goroutine keep running; use Close to stop them.
func (u *Upcalls) CloseTable() error {
	return wrapStatus("aster_upcalls_close", C.aster_upcalls_close(u.rtH(), C.uint64_t(u.handle)))
}

// Close fails pending upcalls (decisions take their failure mode), stops the
// poll goroutine, cancels running handlers (waiting up to two seconds for
// them) and frees the native table. Adapters built on the table (lease
// serializers, fenced resources, filters, exposed services) keep their
// handles but can no longer reach Go. Idempotent. Close it before the
// runtime.
func (u *Upcalls) Close() error {
	u.closeOnce.Do(func() {
		if err := u.CloseTable(); err != nil && !errors.Is(err, ErrNotFound) {
			u.closeErr = err
		}
		u.closing.Store(true)
		<-u.pollDone
		u.cancel()
		done := make(chan struct{})
		go func() { u.handlerWG.Wait(); close(done) }()
		select {
		case <-done:
		case <-time.After(closeGrace):
			// A handler ignoring its context cannot stop us freeing the
			// table; its late respond is a no-op (closing, then NOT_FOUND).
		}
		if st := C.aster_upcalls_free(u.rtH(), C.uint64_t(u.handle)); st != 0 && u.closeErr == nil {
			if err := statusError(st); !errors.Is(err, ErrNotFound) && !errors.Is(err, ErrAlreadyClosed) {
				u.closeErr = fmt.Errorf("aster_upcalls_free: %w", err)
			}
		}
	})
	return u.closeErr
}

// ─── typed serving ───────────────────────────────────────────────────────

// LeaseSerializer is the serialization point of leases implemented in Go: it
// must apply each transition atomically (the state machine of aster::lease).
// An error is a backend fault, never a denial.
type LeaseSerializer interface {
	Transition(ctx context.Context, resource string, op LeaseOp) (TransitionOutcome, error)
	Snapshot(ctx context.Context, resource string) (LeaseSnapshot, error)
}

// LeaseAuthorizer decides whether an actor may perform a lease transition
// (guards Rust's memory serializer). An error follows the kind's failure
// mode.
type LeaseAuthorizer interface {
	Authorize(ctx context.Context, op LeaseOp) (bool, error)
}

// LeaseAuthorizerFunc adapts a function to LeaseAuthorizer.
type LeaseAuthorizerFunc func(ctx context.Context, op LeaseOp) (bool, error)

// Authorize implements LeaseAuthorizer.
func (f LeaseAuthorizerFunc) Authorize(ctx context.Context, op LeaseOp) (bool, error) {
	return f(ctx, op)
}

// FencedResource is a resource implemented in Go that Rust applies fenced
// mutations to. Mutate must evaluate the fence rule (CheckFence) inside the
// resource's own serialization point, atomically with the write: the adapter
// cannot enforce that.
type FencedResource interface {
	Mutate(ctx context.Context, fence Fence, writer string, op []byte) (MutateResult, error)
}

// AdmittedFilter answers the topology read-time admission question for a
// batch of node ids (lowercase hex): one answer per id, same order.
type AdmittedFilter interface {
	Admitted(ctx context.Context, nodeIDs []string) ([]bool, error)
}

// AdmittedFilterFunc adapts a function to AdmittedFilter.
type AdmittedFilterFunc func(ctx context.Context, nodeIDs []string) ([]bool, error)

// Admitted implements AdmittedFilter.
func (f AdmittedFilterFunc) Admitted(ctx context.Context, ids []string) ([]bool, error) {
	return f(ctx, ids)
}

// ServeLeaseSerializer serves the lease transition and snapshot kinds with s.
func (u *Upcalls) ServeLeaseSerializer(s LeaseSerializer) error {
	if err := u.HandleKind(UpcallLeaseTransition, func(ctx context.Context, req *UpcallRequest) ([]byte, error) {
		r, err := DecodeLeaseTransitionRequest(req.Payload)
		if err != nil {
			return nil, err
		}
		o, err := s.Transition(ctx, r.Resource, r.Op)
		if err != nil {
			return nil, err
		}
		return EncodeTransitionOutcome(o), nil
	}); err != nil {
		return err
	}
	return u.HandleKind(UpcallLeaseSnapshot, func(ctx context.Context, req *UpcallRequest) ([]byte, error) {
		res, err := DecodeLeaseSnapshotRequest(req.Payload)
		if err != nil {
			return nil, err
		}
		snap, err := s.Snapshot(ctx, res)
		if err != nil {
			return nil, err
		}
		return EncodeLeaseSnapshot(snap), nil
	})
}

// ServeLeaseAuthorizer serves the lease authorize decision kind with a.
func (u *Upcalls) ServeLeaseAuthorizer(a LeaseAuthorizer) error {
	return u.HandleKind(UpcallLeaseAuthorize, func(ctx context.Context, req *UpcallRequest) ([]byte, error) {
		op, err := DecodeLeaseAuthorizeRequest(req.Payload)
		if err != nil {
			return nil, err
		}
		ok, err := a.Authorize(ctx, op)
		if err != nil {
			return nil, err
		}
		return EncodeDecision(ok), nil
	})
}

// ServeFencedResource serves the fenced mutate kind with r.
func (u *Upcalls) ServeFencedResource(r FencedResource) error {
	return u.HandleKind(UpcallFencedMutate, func(ctx context.Context, req *UpcallRequest) ([]byte, error) {
		m, err := DecodeFencedMutateRequest(req.Payload)
		if err != nil {
			return nil, err
		}
		res, err := r.Mutate(ctx, m.Fence, m.Writer, m.Op)
		if err != nil {
			return nil, err
		}
		return EncodeFencedMutateResponse(res), nil
	})
}

// ServeAdmittedFilter serves the topology admitted kind with f.
func (u *Upcalls) ServeAdmittedFilter(f AdmittedFilter) error {
	return u.HandleKind(UpcallTopologyAdmitted, func(ctx context.Context, req *UpcallRequest) ([]byte, error) {
		ids, err := DecodeTopologyAdmittedRequest(req.Payload)
		if err != nil {
			return nil, err
		}
		answers, err := f.Admitted(ctx, ids)
		if err != nil {
			return nil, err
		}
		if len(answers) != len(ids) {
			return nil, fmt.Errorf("AdmittedFilter must answer once per id (%d), got %d", len(ids), len(answers))
		}
		return EncodeTopologyAdmittedResponse(answers), nil
	})
}

// ExposeAuthorizeFunc decides whether a peer may reach an exposed service
// (kind 48). Metadata is the opaque registration metadata the peer sent.
type ExposeAuthorizeFunc func(ctx context.Context, req ExposeAuthorizeRequest) (bool, error)

// ServeExposeAuthorize serves the expose authorize decision kind: one
// handler decides for every service registered with this table as its
// authorizer (ExposeAuthorizeRequest.ServiceID tells them apart).
func (u *Upcalls) ServeExposeAuthorize(f ExposeAuthorizeFunc) error {
	return u.HandleKind(UpcallExposeAuthorize, func(ctx context.Context, req *UpcallRequest) ([]byte, error) {
		r, err := DecodeExposeAuthorizeRequest(req.Payload)
		if err != nil {
			return nil, err
		}
		ok, err := f(ctx, r)
		if err != nil {
			return nil, err
		}
		return EncodeDecision(ok), nil
	})
}

// RoutePolicyFunc decides which of the requested routes an edge grants
// (kind 50): the indexes into req.Specs, strictly ascending.
type RoutePolicyFunc func(ctx context.Context, req ExposeRoutePolicyRequest) ([]uint32, error)

// ServeRoutePolicy serves the expose route policy kind.
func (u *Upcalls) ServeRoutePolicy(f RoutePolicyFunc) error {
	return u.HandleKind(UpcallExposeRoutePolicy, func(ctx context.Context, req *UpcallRequest) ([]byte, error) {
		r, err := DecodeExposeRoutePolicyRequest(req.Payload)
		if err != nil {
			return nil, err
		}
		granted, err := f(ctx, r)
		if err != nil {
			return nil, err
		}
		return EncodeExposeRouteGrant(granted), nil
	})
}

// ─── async operations that return native handles ─────────────────────────

// lateCompletionWait gives abandoned owned operations time to finish before
// requesting native cancellation and collecting the sole terminal completion.
const lateCompletionWait = 5 * time.Minute

// startOwnedOp runs an async FFI call whose completion may own a native handle.
// When ctx ends first, a background waiter collects the completion and passes
// it to cleanup. After lateCompletionWait it requests native cancellation, but
// still collects a success that already committed before cancellation won.
// cleanup must ignore cancellation/error events and release successful owners.
func startOwnedOp(ctx context.Context, rt *Runtime, what string,
	call func(op *C.aster_operation_t) C.int32_t, cleanup func(*Event)) (*Event, error) {
	var op C.aster_operation_t
	if err := wrapStatus(what, call(&op)); err != nil {
		return nil, err
	}
	id := uint64(op)
	ch := make(chan *Event, 1)
	rt.Register(id, ch)
	select {
	case ev, ok := <-ch:
		if !ok || ev == nil {
			return nil, fmt.Errorf("%s: runtime closed: %w", what, ErrAlreadyClosed)
		}
		return ev, nil
	case <-ctx.Done():
		go func() {
			t := time.NewTimer(lateCompletionWait)
			defer t.Stop()
			var ev *Event
			select {
			case ev = <-ch:
			case <-t.C:
				ev = rt.cancelAndCollect(id, ch)
			}
			if ev != nil {
				if cleanup != nil {
					cleanup(ev)
				}
				rt.releaseEventBuffer(ev)
			}
		}()
		return nil, fmt.Errorf("%s: %w", what, ctx.Err())
	}
}
