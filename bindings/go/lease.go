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
	"sync/atomic"
	"time"
	"unsafe"
)

// C ABI 4.1: leases, fenced resources and the topology admitted filter.
//
// A lease serializer is the serialization point of leases (aster::lease):
// either Go's own (NewForeignLeaseSerializer, served on an Upcalls table with
// Upcalls.ServeLeaseSerializer) or Rust's in-memory reference
// (NewMemoryLeaseSerializer, optionally guarded by a Go authorizer). Held
// leases renew in the background in Rust.

// ErrLeaseDenied matches (errors.Is) a refused acquire; errors.As to
// *LeaseDeniedError for the reason and the snapshot.
var ErrLeaseDenied = errors.New("lease denied")

// ErrMutationOutcomeUnknown is returned by FencedResourceHandle.Mutate when
// the resource gave no answer (timeout, handler error, closed table): the
// write may or may not have been applied -- a write cannot be failed open.
var ErrMutationOutcomeUnknown = errors.New("fenced mutation outcome unknown")

// LeaseDeniedError is an acquire (or revoke) the serializer refused.
// Snapshot.RemainingMs is a back-off hint.
type LeaseDeniedError struct {
	Reason   DenyReason
	Snapshot LeaseSnapshot
}

func (e *LeaseDeniedError) Error() string {
	return fmt.Sprintf("lease %q denied: %s", e.Snapshot.Resource, e.Reason)
}

// Is makes errors.Is(err, ErrLeaseDenied) true.
func (e *LeaseDeniedError) Is(target error) bool { return target == ErrLeaseDenied }

// LeaseStatus is the holder-side status of a Lease (aster_lease_status_t).
type LeaseStatus uint32

const (
	LeaseHeld LeaseStatus = C.ASTER_LEASE_STATUS_HELD
	// LeaseStatusReleased: released by this holder.
	LeaseStatusReleased LeaseStatus = C.ASTER_LEASE_STATUS_RELEASED
	// LeaseStatusFenced: a renewal was refused (a newer epoch exists, or the
	// lease was revoked).
	LeaseStatusFenced LeaseStatus = C.ASTER_LEASE_STATUS_FENCED
	// LeaseStatusLost: the self-limit passed without a successful renewal.
	LeaseStatusLost LeaseStatus = C.ASTER_LEASE_STATUS_LOST
)

func (s LeaseStatus) String() string {
	switch s {
	case LeaseHeld:
		return "held"
	case LeaseStatusReleased:
		return "released"
	case LeaseStatusFenced:
		return "fenced"
	case LeaseStatusLost:
		return "lost"
	}
	return fmt.Sprintf("lease_status(%d)", uint32(s))
}

// LeaseOptions are the timing of an acquire. The zero value is the defaults
// (30 s ttl, 10 s renew, 1 s holder margin); with a TTL, a zero RenewEvery is
// TTL/3 and a zero HolderMargin TTL/10 capped at 1 s.
type LeaseOptions struct {
	TTL          time.Duration
	RenewEvery   time.Duration
	HolderMargin time.Duration
}

// LeaseSerializerHandle is a Rust-side lease serializer (aster_lease_serializer_t)
// that leases are acquired from.
type LeaseSerializerHandle struct {
	rt     *Runtime
	handle uint64
	closed atomic.Bool
}

// NewForeignLeaseSerializer creates a serializer whose transitions and
// snapshots are upcalls on up (serve them with Upcalls.ServeLeaseSerializer).
// A failed upcall is a backend error, never read as a denial.
func NewForeignLeaseSerializer(up *Upcalls) (*LeaseSerializerHandle, error) {
	var h C.aster_lease_serializer_t
	if err := wrapCall("aster_lease_serializer_foreign_new",
		func() C.int32_t { return C.aster_lease_serializer_foreign_new(up.rtH(), C.uint64_t(up.handle), &h) }); err != nil {
		return nil, err
	}
	return &LeaseSerializerHandle{rt: up.rt, handle: uint64(h)}, nil
}

// NewMemoryLeaseSerializer creates Rust's in-memory reference serializer. With
// a non-nil authorizer table every transition is first put to Go as a
// LEASE_AUTHORIZE decision (serve it with Upcalls.ServeLeaseAuthorizer); a
// refusal (or no answer under fail-closed) is Denied(Unauthorized). nil =
// everyone is authorized.
func NewMemoryLeaseSerializer(rt *Runtime, authorizer *Upcalls) (*LeaseSerializerHandle, error) {
	var auth C.uint64_t
	if authorizer != nil {
		auth = C.uint64_t(authorizer.handle)
	}
	var h C.aster_lease_serializer_t
	if err := wrapCall("aster_lease_serializer_memory_new",
		func() C.int32_t { return C.aster_lease_serializer_memory_new(C.uint64_t(rt.handle), auth, &h) }); err != nil {
		return nil, err
	}
	return &LeaseSerializerHandle{rt: rt, handle: uint64(h)}, nil
}

// Handle is the native aster_lease_serializer_t.
func (s *LeaseSerializerHandle) Handle() uint64 { return s.handle }

func (s *LeaseSerializerHandle) rtH() C.uint64_t { return C.uint64_t(s.rt.handle) }

// Acquire tries to acquire resource for holder (a hex node id, or any stable
// actor name the serializer understands) with one ACQUIRE. On success the
// Lease renews in the background until released or closed. A refusal is a
// *LeaseDeniedError (errors.Is ErrLeaseDenied). If ctx ends first, a lease
// granted meanwhile is released rather than leaked.
func (s *LeaseSerializerHandle) Acquire(ctx context.Context, resource, holder string, opts LeaseOptions) (*Lease, error) {
	var a cArena
	defer a.free()
	ev, err := startOwnedOp(ctx, s.rt, "aster_lease_acquire", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_lease_acquire(s.rtH(), C.uint64_t(s.handle), a.str(resource), a.str(holder),
			C.uint64_t(opts.TTL.Milliseconds()), C.uint64_t(opts.RenewEvery.Milliseconds()),
			C.uint64_t(opts.HolderMargin.Milliseconds()), 0, op)
	}, func(late *Event) { releaseLateLease(s.rt, late) })
	if err != nil {
		return nil, err
	}
	if ev.Kind != ASTER_EVENT_LEASE_ACQUIRED {
		return nil, s.rt.eventError(ev, "aster_lease_acquire")
	}
	handle := ev.Handle
	out, derr := DecodeTransitionOutcome(s.rt.takePayload(ev))
	if derr != nil {
		freeLeaseHandle(s.rt, handle)
		return nil, fmt.Errorf("aster_lease_acquire: %w", derr)
	}
	switch out.Kind {
	case TransitionGranted:
		if handle == 0 {
			return nil, fmt.Errorf("aster_lease_acquire: granted without a handle: %w", ErrInternal)
		}
		return &Lease{rt: s.rt, handle: handle, granted: out.Fence}, nil
	case TransitionDenied:
		return nil, &LeaseDeniedError{Reason: out.Reason, Snapshot: out.Snapshot}
	}
	freeLeaseHandle(s.rt, handle)
	return nil, fmt.Errorf("aster_lease_acquire: serializer answered %d to an acquire: %w", out.Kind, ErrInternal)
}

// Snapshot is an advisory view of resource at the serializer (stale the
// moment it returns).
func (s *LeaseSerializerHandle) Snapshot(ctx context.Context, resource string) (LeaseSnapshot, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, s.rt, "aster_lease_snapshot", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_lease_snapshot(s.rtH(), C.uint64_t(s.handle), a.str(resource), 0, op)
	})
	if err != nil {
		return LeaseSnapshot{}, err
	}
	p, err := expect(s.rt, ev, ASTER_EVENT_BYTES_RESULT, "aster_lease_snapshot")
	if err != nil {
		return LeaseSnapshot{}, err
	}
	return DecodeLeaseSnapshot(p)
}

// Revoke REVOKEs resource at the serializer on behalf of authority (advances
// the fence): TransitionRevoked, or TransitionDenied with a reason.
func (s *LeaseSerializerHandle) Revoke(ctx context.Context, resource, authority string) (TransitionOutcome, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, s.rt, "aster_lease_revoke", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_lease_revoke(s.rtH(), C.uint64_t(s.handle), a.str(resource), a.str(authority), 0, op)
	})
	if err != nil {
		return TransitionOutcome{}, err
	}
	p, err := expect(s.rt, ev, ASTER_EVENT_BYTES_RESULT, "aster_lease_revoke")
	if err != nil {
		return TransitionOutcome{}, err
	}
	return DecodeTransitionOutcome(p)
}

// Close frees the serializer handle. Leases acquired from it keep their own
// reference and keep renewing. Idempotent.
func (s *LeaseSerializerHandle) Close() error {
	if s.closed.Swap(true) {
		return nil
	}
	return wrapCall("aster_lease_serializer_free", func() C.int32_t { return C.aster_lease_serializer_free(s.rtH(), C.uint64_t(s.handle)) })
}

// Lease is a held lease (aster_lease_t): it renews in the background, gates
// fence access on the holder's self-limit and reports status changes. Close
// stops renewing without releasing (the lease lapses by TTL); Release hands
// it off cleanly.
type Lease struct {
	rt      *Runtime
	handle  uint64
	granted Fence
	done    atomic.Bool
}

// GrantedFence is the fence issued at acquire.
func (l *Lease) GrantedFence() Fence { return l.granted }

func (l *Lease) rtH() C.uint64_t { return C.uint64_t(l.rt.handle) }

// Fence returns the current fence for a protected action. Read it at send
// time and do not cache it. ok is false once the holder must stop acting: the
// lease was fenced, lost, released, or its self-limit passed.
func (l *Lease) Fence() (fence Fence, ok bool) {
	if l.done.Load() {
		return Fence{}, false
	}
	var epoch C.uint64_t
	if st := C.aster_lease_fence(l.rtH(), C.uint64_t(l.handle), &epoch); st != 0 {
		return Fence{}, false
	}
	return Fence{Resource: l.granted.Resource, Epoch: uint64(epoch), Holder: l.granted.Holder}, true
}

// Status is the holder-side status.
func (l *Lease) Status() (LeaseStatus, error) {
	if l.done.Load() {
		return LeaseStatusReleased, nil
	}
	var st C.uint32_t
	if err := wrapCall("aster_lease_status", func() C.int32_t { return C.aster_lease_status(l.rtH(), C.uint64_t(l.handle), &st) }); err != nil {
		return 0, err
	}
	return LeaseStatus(st), nil
}

// Watch blocks until the lease leaves LeaseHeld (fenced, lost or released)
// and returns the new status, or ctx.Err() if ctx ends first. A lease closed
// meanwhile reports LeaseStatusReleased.
func (l *Lease) Watch(ctx context.Context) (LeaseStatus, error) {
	if l.done.Load() {
		return LeaseStatusReleased, nil
	}
	ev, err := startOp(ctx, l.rt, "aster_lease_watch", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_lease_watch(l.rtH(), C.uint64_t(l.handle), 0, op)
	})
	if errors.Is(err, ErrNotFound) {
		return LeaseStatusReleased, nil // the handle was consumed meanwhile
	}
	if err != nil {
		return 0, err
	}
	if ev.Kind != ASTER_EVENT_LEASE_STATUS {
		return 0, l.rt.eventError(ev, "aster_lease_watch")
	}
	l.rt.releaseEventBuffer(ev)
	if ev.Status != 0 {
		return LeaseStatusReleased, nil
	}
	return LeaseStatus(ev.Flags), nil
}

// Release stops renewing and RELEASEs at the serializer, which advances the
// fence (this holder's in-flight writes are rejected from here on). The
// handle is consumed whatever the outcome; on a serializer failure the lease
// lapses by TTL and the error is returned.
func (l *Lease) Release(ctx context.Context) error {
	if l.done.Swap(true) {
		return nil
	}
	ev, err := startOp(ctx, l.rt, "aster_lease_release", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_lease_release(l.rtH(), C.uint64_t(l.handle), 0, op)
	})
	if err != nil {
		return err
	}
	_, err = expect(l.rt, ev, ASTER_EVENT_UNIT_RESULT, "aster_lease_release")
	return err
}

// Close stops renewing without releasing: the lease lapses at the serializer
// by TTL. Prefer Release. Idempotent.
func (l *Lease) Close() error {
	if l.done.Swap(true) {
		return nil
	}
	freeLeaseHandle(l.rt, l.handle)
	return nil
}

func freeLeaseHandle(rt *Runtime, handle uint64) {
	if handle != 0 {
		C.aster_lease_free(C.uint64_t(rt.handle), C.uint64_t(handle))
	}
}

// releaseLateLease disposes of a lease granted after its Acquire gave up:
// RELEASE it (so others need not wait out the TTL), falling back to free.
func releaseLateLease(rt *Runtime, ev *Event) {
	if ev.Kind != ASTER_EVENT_LEASE_ACQUIRED || ev.Handle == 0 {
		return
	}
	handle := ev.Handle
	var op C.aster_operation_t
	if C.aster_lease_release(C.uint64_t(rt.handle), C.uint64_t(handle), 0, &op) != 0 {
		freeLeaseHandle(rt, handle)
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if rev, err := rt.await(ctx, uint64(op)); err == nil {
		rt.releaseEventBuffer(rev)
	}
}

// ─── fenced resource ─────────────────────────────────────────────────────

// FencedResourceHandle is a fenced resource implemented in Go
// (aster_fenced_resource_t): each Mutate is a FENCED_MUTATE upcall that must
// evaluate the fence rule inside the resource's own serialization point (see
// FencedResource). Serve it with Upcalls.ServeFencedResource.
type FencedResourceHandle struct {
	rt     *Runtime
	handle uint64
	closed atomic.Bool
}

// NewForeignFencedResource creates the handle over up.
func NewForeignFencedResource(up *Upcalls) (*FencedResourceHandle, error) {
	var h C.aster_fenced_resource_t
	if err := wrapCall("aster_fenced_resource_foreign_new",
		func() C.int32_t { return C.aster_fenced_resource_foreign_new(up.rtH(), C.uint64_t(up.handle), &h) }); err != nil {
		return nil, err
	}
	return &FencedResourceHandle{rt: up.rt, handle: uint64(h)}, nil
}

// Handle is the native aster_fenced_resource_t.
func (f *FencedResourceHandle) Handle() uint64 { return f.handle }

// Mutate applies op (opaque bytes) under fence as the authenticated writer.
// The result is Applied with the output, or Rejected with the fence
// rejection. Any other failure is ErrMutationOutcomeUnknown (timeout,
// binding error, closed table); ctx ending first is ctx.Err() with the
// outcome likewise unknown.
func (f *FencedResourceHandle) Mutate(ctx context.Context, fence Fence, writer string, op []byte) (MutateResult, error) {
	var a cArena
	defer a.free()
	rtH := C.uint64_t(f.rt.handle)
	ev, err := startOp(ctx, f.rt, "aster_fenced_mutate", func(o *C.aster_operation_t) C.int32_t {
		return C.aster_fenced_mutate(rtH, C.uint64_t(f.handle), a.str(fence.Resource), C.uint64_t(fence.Epoch),
			a.str(fence.Holder), a.str(writer), a.bytes(op), 0, o)
	})
	if err != nil {
		return MutateResult{}, err
	}
	if ev.Kind != ASTER_EVENT_BYTES_RESULT {
		return MutateResult{}, fmt.Errorf("%w: %v", ErrMutationOutcomeUnknown, f.rt.eventError(ev, "aster_fenced_mutate"))
	}
	res, derr := DecodeFencedMutateResponse(f.rt.takePayload(ev))
	if derr != nil {
		return MutateResult{}, fmt.Errorf("%w: %v", ErrMutationOutcomeUnknown, derr)
	}
	return res, nil
}

// Close frees the handle. Idempotent.
func (f *FencedResourceHandle) Close() error {
	if f.closed.Swap(true) {
		return nil
	}
	return wrapCall("aster_fenced_resource_free",
		func() C.int32_t { return C.aster_fenced_resource_free(C.uint64_t(f.rt.handle), C.uint64_t(f.handle)) })
}

// ─── topology admitted filter ────────────────────────────────────────────

// AdmittedFilterOptions tune the cached, batched admission filter; zero
// values are the defaults (30 s, 1 s, 256).
type AdmittedFilterOptions struct {
	// TTL is how long an answer is served before it is refreshed.
	TTL time.Duration
	// FailureRetry is how long a failed batch's fallback answer is cached.
	FailureRetry time.Duration
	// MaxBatch is the most node ids per upcall.
	MaxBatch uint32
}

// AdmittedFilterStats are the filter's counters.
type AdmittedFilterStats struct {
	Hits, Stale, Misses, Upcalls uint64
}

// AdmittedFilterHandle is a topology read-time admission filter answered by
// Go (aster_topology_admitted_t, kind 16, batched). Lookups never block: a
// fresh cached answer is served; a stale one is served and refreshed; a miss
// gets the kind's failure-mode answer (default: not admitted) and is
// resolved in the background. Serve the question with
// Upcalls.ServeAdmittedFilter and install the filter with Node.EnableShared.
type AdmittedFilterHandle struct {
	rt     *Runtime
	handle uint64
	closed atomic.Bool
}

// NewAdmittedFilter creates a filter whose questions are upcalls on up.
func NewAdmittedFilter(up *Upcalls, opts AdmittedFilterOptions) (*AdmittedFilterHandle, error) {
	var h C.aster_topology_admitted_t
	if err := wrapCall("aster_topology_admitted_new",
		func() C.int32_t {
			return C.aster_topology_admitted_new(up.rtH(), C.uint64_t(up.handle), millis0(opts.TTL),
				millis0(opts.FailureRetry), C.uint32_t(opts.MaxBatch), &h)
		}); err != nil {
		return nil, err
	}
	return &AdmittedFilterHandle{rt: up.rt, handle: uint64(h)}, nil
}

// millis0 converts a duration to ms, keeping 0 as "default".
func millis0(d time.Duration) C.uint32_t {
	if d <= 0 {
		return 0
	}
	return millis32(d)
}

// Handle is the native aster_topology_admitted_t.
func (f *AdmittedFilterHandle) Handle() uint64 { return f.handle }

func (f *AdmittedFilterHandle) rtH() C.uint64_t { return C.uint64_t(f.rt.handle) }

// IsAdmitted looks nodeID (64 hex) up exactly as the swarm's filter does. It
// never blocks; a miss queues a resolution.
func (f *AdmittedFilterHandle) IsAdmitted(nodeID string) (bool, error) {
	var a cArena
	defer a.free()
	var out C.uint32_t
	if err := wrapCall("aster_topology_admitted_check",
		func() C.int32_t {
			return C.aster_topology_admitted_check(f.rtH(), C.uint64_t(f.handle), a.str(nodeID), &out)
		}); err != nil {
		return false, err
	}
	return out != 0, nil
}

// Set pushes an answer for nodeID (e.g. on revocation); served for the TTL.
func (f *AdmittedFilterHandle) Set(nodeID string, admitted bool) error {
	var a cArena
	defer a.free()
	return wrapCall("aster_topology_admitted_set",
		func() C.int32_t {
			return C.aster_topology_admitted_set(f.rtH(), C.uint64_t(f.handle), a.str(nodeID), boolToUint32(admitted))
		})
}

// Invalidate forgets nodeID's answer (the next lookup re-resolves).
func (f *AdmittedFilterHandle) Invalidate(nodeID string) error {
	var a cArena
	defer a.free()
	return wrapCall("aster_topology_admitted_invalidate",
		func() C.int32_t {
			return C.aster_topology_admitted_invalidate(f.rtH(), C.uint64_t(f.handle), a.str(nodeID))
		})
}

// Clear forgets every answer.
func (f *AdmittedFilterHandle) Clear() error { return f.Invalidate("") }

// Prefetch resolves nodeIDs now with one upcall and caches the answers (warm
// the cache before Node.EnableShared). A failed upcall is an error; the
// fallback answer is then cached for the failure-retry time.
func (f *AdmittedFilterHandle) Prefetch(ctx context.Context, nodeIDs []string) error {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, f.rt, "aster_topology_admitted_prefetch", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_topology_admitted_prefetch(f.rtH(), C.uint64_t(f.handle), a.list(nodeIDs), 0, op)
	})
	if err != nil {
		return err
	}
	_, err = expect(f.rt, ev, ASTER_EVENT_UNIT_RESULT, "aster_topology_admitted_prefetch")
	return err
}

// Stats returns the filter's counters.
func (f *AdmittedFilterHandle) Stats() (AdmittedFilterStats, error) {
	var out C.aster_topology_admitted_stats_t
	if err := wrapCall("aster_topology_admitted_stats",
		func() C.int32_t { return C.aster_topology_admitted_stats(f.rtH(), C.uint64_t(f.handle), &out) }); err != nil {
		return AdmittedFilterStats{}, err
	}
	return AdmittedFilterStats{uint64(out.hits), uint64(out.stale), uint64(out.misses), uint64(out.upcalls)}, nil
}

// Close frees the handle; a swarm it was installed in keeps using it.
// Idempotent.
func (f *AdmittedFilterHandle) Close() error {
	if f.closed.Swap(true) {
		return nil
	}
	return wrapCall("aster_topology_admitted_free", func() C.int32_t { return C.aster_topology_admitted_free(f.rtH(), C.uint64_t(f.handle)) })
}

// EnableShared joins a topology swarm (topology v2): import the shared
// namespace from its 32-byte secret, publish this node's measurements and
// derive the swarm-wide cluster view. The node must run with
// AsterConfig.EnableMonitoring. filter (nil = every holder of the secret
// counts) installs a read-time admission filter; it must belong to this
// node's runtime and its Upcalls table must stay open while the swarm runs.
// bootstrap are extra peers (64-hex node ids) to sync with; positionRefresh 0
// = 60 s.
func (n *Node) EnableShared(ctx context.Context, secret []byte, filter *AdmittedFilterHandle,
	bootstrap []string, positionRefresh time.Duration) error {
	if len(secret) != 32 {
		return fmt.Errorf("topology secret must be 32 bytes, got %d: %w", len(secret), ErrInvalidArgument)
	}
	if filter != nil && filter.rt != n.runtime {
		return fmt.Errorf("topology filter belongs to another runtime: %w", ErrInvalidArgument)
	}
	var a cArena
	defer a.free()
	key := a.alloc(32)
	C.memcpy(key, unsafe.Pointer(&secret[0]), 32)
	var fh C.aster_topology_admitted_t
	if filter != nil {
		fh = C.aster_topology_admitted_t(filter.handle)
	}
	ev, err := startOp(ctx, n.runtime, "aster_topology_enable_shared", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_topology_enable_shared(C.uint64_t(n.runtime.handle), C.uint64_t(n.handle),
			(*C.uint8_t)(key), fh, a.list(bootstrap), millis0(positionRefresh), 0, op)
	})
	if err != nil {
		return err
	}
	_, err = expect(n.runtime, ev, ASTER_EVENT_UNIT_RESULT, "aster_topology_enable_shared")
	return err
}
