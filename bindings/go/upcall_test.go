//go:build cgo

package aster

import (
	"context"
	"errors"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// C ABI 4.1 end to end: Rust lease handles over a Go serializer (with
// background renewals), a Go authorizer guarding Rust's memory serializer, a
// Go fenced resource, the topology admission filter, failure modes, timeouts
// and cancellation, UNIMPLEMENTED, and lifetime (close, goroutine leaks).
// Ported from the Java UpcallsE2ETest.

const upcallWait = 15 * time.Second

// leakCheck returns a func that fails the test if goroutines started since
// the call are still running a moment after the test's cleanups. Call it
// first (`leakCheck(t)`) so it runs last.
func leakCheck(t *testing.T) {
	t.Helper()
	before := runtime.NumGoroutine()
	t.Cleanup(func() {
		t.Helper()
		deadline := time.Now().Add(5 * time.Second)
		for {
			n := runtime.NumGoroutine()
			if n <= before {
				return
			}
			if time.Now().After(deadline) {
				buf := make([]byte, 1<<16)
				buf = buf[:runtime.Stack(buf, true)]
				t.Errorf("goroutine leak: %d running, %d before\n%s", n, before, buf)
				return
			}
			time.Sleep(20 * time.Millisecond)
		}
	})
}

func testRuntime(t *testing.T) *Runtime {
	t.Helper()
	rt, err := NewRuntime(t.Context(), DefaultRuntimeConfig())
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = rt.Close() })
	return rt
}

func testUpcalls(t *testing.T, rt *Runtime) *Upcalls {
	t.Helper()
	up, err := NewUpcalls(rt, 0)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = up.Close() })
	return up
}

func upCtx(t *testing.T) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(t.Context(), upcallWait)
	t.Cleanup(cancel)
	return ctx
}

// goSerializer is a minimal Go lease serialization point (the state machine
// of aster::lease).
type goSerializer struct {
	mu   sync.Mutex
	rows map[string]*goRow
	seen []LeaseOp
	// delay, when set, holds every acquire this long (cancellable).
	delay time.Duration
}

type goRow struct {
	epoch     uint64
	holder    string
	held      bool
	ttl       time.Duration
	expiresAt time.Time
}

func newGoSerializer() *goSerializer { return &goSerializer{rows: map[string]*goRow{}} }

func (s *goSerializer) snap(resource string, r *goRow, now time.Time) LeaseSnapshot {
	out := LeaseSnapshot{Resource: resource, Epoch: r.epoch, Held: r.held}
	if r.held {
		h := r.holder
		rem := uint64(0)
		if d := r.expiresAt.Sub(now); d > 0 {
			rem = uint64(d.Milliseconds())
		}
		out.Holder, out.RemainingMs = &h, &rem
	}
	return out
}

func (s *goSerializer) Transition(ctx context.Context, resource string, op LeaseOp) (TransitionOutcome, error) {
	if op.Kind == LeaseAcquire && s.delay > 0 {
		select {
		case <-time.After(s.delay):
		case <-ctx.Done():
			return TransitionOutcome{}, ctx.Err()
		}
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	s.seen = append(s.seen, op)
	now := time.Now()
	r := s.rows[resource]
	if r == nil {
		r = &goRow{}
		s.rows[resource] = r
	}
	deny := func(reason DenyReason) TransitionOutcome {
		return TransitionOutcome{Kind: TransitionDenied, Reason: reason, Snapshot: s.snap(resource, r, now)}
	}
	check := func() (DenyReason, bool) {
		switch {
		case !r.held:
			return DenyNotHeld, false
		case r.epoch != op.Epoch:
			return DenyEpochMismatch, false
		case r.holder != op.Actor:
			return DenyNotHolder, false
		}
		return 0, true
	}
	switch op.Kind {
	case LeaseAcquire:
		if r.held && now.Before(r.expiresAt) {
			return deny(DenyHeldByOther), nil
		}
		r.epoch++
		r.holder, r.held = op.Actor, true
		r.ttl = time.Duration(op.TTLMs) * time.Millisecond
		r.expiresAt = now.Add(r.ttl)
		return TransitionOutcome{Kind: TransitionGranted, Fence: Fence{Resource: resource, Epoch: r.epoch, Holder: r.holder}}, nil
	case LeaseRenew:
		if reason, ok := check(); !ok {
			return deny(reason), nil
		}
		r.expiresAt = now.Add(r.ttl)
		return TransitionOutcome{Kind: TransitionRenewed}, nil
	case LeaseRelease:
		if reason, ok := check(); !ok {
			return deny(reason), nil
		}
		r.epoch++
		r.held, r.holder = false, ""
		return TransitionOutcome{Kind: TransitionReleased}, nil
	}
	r.epoch++
	r.held, r.holder = false, ""
	return TransitionOutcome{Kind: TransitionRevoked}, nil
}

func (s *goSerializer) Snapshot(ctx context.Context, resource string) (LeaseSnapshot, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	r := s.rows[resource]
	if r == nil {
		r = &goRow{}
		s.rows[resource] = r
	}
	return s.snap(resource, r, time.Now()), nil
}

func (s *goSerializer) count(kind LeaseOpKind) int {
	s.mu.Lock()
	defer s.mu.Unlock()
	n := 0
	for _, op := range s.seen {
		if op.Kind == kind {
			n++
		}
	}
	return n
}

func eventually(t *testing.T, what string, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(upcallWait)
	for !cond() {
		if time.Now().After(deadline) {
			t.Fatalf("timed out waiting for %s", what)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func TestUpcalls_GoSerializerBacksRustLeaseHandles(t *testing.T) {
	leakCheck(t)
	ctx := upCtx(t)
	rt := testRuntime(t)
	up := testUpcalls(t, rt)
	gs := newGoSerializer()
	if err := up.ServeLeaseSerializer(gs); err != nil {
		t.Fatal(err)
	}
	ser, err := NewForeignLeaseSerializer(up)
	if err != nil {
		t.Fatal(err)
	}
	defer ser.Close()

	lease, err := ser.Acquire(ctx, "tree/42", "alice", LeaseOptions{TTL: 600 * time.Millisecond})
	if err != nil {
		t.Fatal(err)
	}
	if want := (Fence{"tree/42", 1, "alice"}); lease.GrantedFence() != want {
		t.Fatalf("granted fence %+v, want %+v", lease.GrantedFence(), want)
	}
	if f, ok := lease.Fence(); !ok || f.Epoch != 1 {
		t.Fatalf("Fence() = %+v, %v", f, ok)
	}
	if st, err := lease.Status(); err != nil || st != LeaseHeld {
		t.Fatalf("Status() = %v, %v", st, err)
	}

	// bob is refused with the holder and a back-off hint.
	_, err = ser.Acquire(ctx, "tree/42", "bob", LeaseOptions{TTL: 600 * time.Millisecond})
	var denied *LeaseDeniedError
	if !errors.Is(err, ErrLeaseDenied) || !errors.As(err, &denied) {
		t.Fatalf("bob: %v", err)
	}
	if denied.Reason != DenyHeldByOther || denied.Snapshot.Holder == nil || *denied.Snapshot.Holder != "alice" {
		t.Fatalf("denied = %+v", denied)
	}

	// Renewals reach Go (ttl 600 ms -> every 200 ms).
	eventually(t, "two renewals", func() bool { return gs.count(LeaseRenew) >= 2 })
	if _, ok := lease.Fence(); !ok {
		t.Fatal("fence gone while renewing")
	}

	snap, err := ser.Snapshot(ctx, "tree/42")
	if err != nil || !snap.Held || snap.Holder == nil || *snap.Holder != "alice" {
		t.Fatalf("snapshot = %+v, %v", snap, err)
	}

	// Watch completes when the lease is released.
	watched := make(chan LeaseStatus, 1)
	go func() {
		st, err := lease.Watch(ctx)
		if err != nil {
			t.Errorf("Watch: %v", err)
		}
		watched <- st
	}()
	time.Sleep(100 * time.Millisecond) // let the watch register
	if err := lease.Release(ctx); err != nil {
		t.Fatal(err)
	}
	select {
	case st := <-watched:
		if st != LeaseStatusReleased {
			t.Fatalf("watch = %v", st)
		}
	case <-ctx.Done():
		t.Fatal("watch never completed")
	}
	if _, ok := lease.Fence(); ok {
		t.Fatal("fence available after release")
	}
	gs.mu.Lock()
	sawRelease := false
	for _, op := range gs.seen {
		if op == (LeaseOp{Kind: LeaseRelease, Actor: "alice", Epoch: 1}) {
			sawRelease = true
		}
	}
	gs.mu.Unlock()
	if !sawRelease {
		t.Fatalf("serializer never saw the release: %+v", gs.seen)
	}

	// Revoke fences the next holder's renewal.
	b, err := ser.Acquire(ctx, "tree/42", "bob", LeaseOptions{TTL: 600 * time.Millisecond})
	if err != nil {
		t.Fatal(err)
	}
	if b.GrantedFence().Epoch <= 1 {
		t.Fatalf("epoch did not advance: %+v", b.GrantedFence())
	}
	out, err := ser.Revoke(ctx, "tree/42", "root")
	if err != nil || out.Kind != TransitionRevoked {
		t.Fatalf("revoke = %+v, %v", out, err)
	}
	if st, err := b.Watch(ctx); err != nil || st != LeaseStatusFenced {
		t.Fatalf("watch after revoke = %v, %v", st, err)
	}
	if _, ok := b.Fence(); ok {
		t.Fatal("fence available after revoke")
	}
	_ = b.Close()
	_ = b.Close() // idempotent
}

func TestUpcalls_GoAuthorizerGuardsTheMemorySerializer(t *testing.T) {
	leakCheck(t)
	ctx := upCtx(t)
	rt := testRuntime(t)
	up := testUpcalls(t, rt)
	if err := up.ServeLeaseAuthorizer(LeaseAuthorizerFunc(func(_ context.Context, op LeaseOp) (bool, error) {
		return op.Actor != "mallory", nil
	})); err != nil {
		t.Fatal(err)
	}
	ser, err := NewMemoryLeaseSerializer(rt, up)
	if err != nil {
		t.Fatal(err)
	}
	defer ser.Close()
	var denied *LeaseDeniedError
	if _, err := ser.Acquire(ctx, "db", "mallory", LeaseOptions{}); !errors.As(err, &denied) || denied.Reason != DenyUnauthorized {
		t.Fatalf("mallory: %v", err)
	}
	a, err := ser.Acquire(ctx, "db", "alice", LeaseOptions{})
	if err != nil {
		t.Fatal(err)
	}
	_ = a.Close()

	open, err := NewMemoryLeaseSerializer(rt, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer open.Close()
	m, err := open.Acquire(ctx, "db2", "mallory", LeaseOptions{})
	if err != nil {
		t.Fatalf("open serializer: %v", err)
	}
	_ = m.Close()
}

func TestUpcalls_FailureModeAppliesWhenTheAuthorizerHangsOrFails(t *testing.T) {
	leakCheck(t)
	ctx := upCtx(t)
	rt := testRuntime(t)
	up := testUpcalls(t, rt)
	cancelled := make(chan struct{}, 4)
	if err := up.ServeLeaseAuthorizer(LeaseAuthorizerFunc(func(c context.Context, op LeaseOp) (bool, error) {
		if op.Actor == "thrower" {
			return false, errors.New("boom")
		}
		select {
		case <-c.Done(): // Rust gave up on us
			cancelled <- struct{}{}
			return false, c.Err()
		case <-time.After(10 * time.Second):
			return true, nil
		}
	})); err != nil {
		t.Fatal(err)
	}
	if err := up.SetPolicy(UpcallLeaseAuthorize, 150*time.Millisecond, FailureModeClosed); err != nil {
		t.Fatal(err)
	}
	ser, err := NewMemoryLeaseSerializer(rt, up)
	if err != nil {
		t.Fatal(err)
	}
	defer ser.Close()

	var denied *LeaseDeniedError
	if _, err := ser.Acquire(ctx, "r", "slow", LeaseOptions{}); !errors.As(err, &denied) || denied.Reason != DenyUnauthorized {
		t.Fatalf("hung authorizer, fail-closed: %v", err)
	}
	select {
	case <-cancelled:
	case <-ctx.Done():
		t.Fatal("the handler's context was never cancelled")
	}
	if up.CancelCount() < 1 {
		t.Fatalf("CancelCount = %d", up.CancelCount())
	}
	if _, err := ser.Acquire(ctx, "r", "thrower", LeaseOptions{}); !errors.As(err, &denied) {
		t.Fatalf("erroring authorizer, fail-closed: %v", err)
	}

	if err := up.SetPolicy(UpcallLeaseAuthorize, 150*time.Millisecond, FailureModeOpen); err != nil {
		t.Fatal(err)
	}
	l, err := ser.Acquire(ctx, "r", "thrower", LeaseOptions{})
	if err != nil {
		t.Fatalf("erroring authorizer, fail-open: %v", err)
	}
	_ = l.Close()
}

func TestUpcalls_MissingHandlerAndHandlerErrorsFailTheRustCall(t *testing.T) {
	leakCheck(t)
	ctx := upCtx(t)
	rt := testRuntime(t)
	up := testUpcalls(t, rt)
	ser, err := NewForeignLeaseSerializer(up)
	if err != nil {
		t.Fatal(err)
	}
	defer ser.Close()

	_, err = ser.Acquire(ctx, "r", "a", LeaseOptions{})
	if err == nil || !strings.Contains(err.Error(), "not implemented") {
		t.Fatalf("no handler: %v", err)
	}

	up.HandleKind(UpcallLeaseTransition, func(context.Context, *UpcallRequest) ([]byte, error) {
		return nil, errors.New("disk on fire")
	})
	if _, err = ser.Acquire(ctx, "r", "a", LeaseOptions{}); err == nil || !strings.Contains(err.Error(), "disk on fire") {
		t.Fatalf("handler error: %v", err)
	}

	up.HandleKind(UpcallLeaseTransition, func(context.Context, *UpcallRequest) ([]byte, error) {
		panic("oops")
	})
	if _, err = ser.Acquire(ctx, "r", "a", LeaseOptions{}); err == nil || !strings.Contains(err.Error(), "oops") {
		t.Fatalf("handler panic: %v", err)
	}

	// Garbage in an OK answer is a decode error, never a grant.
	up.HandleKind(UpcallLeaseTransition, func(context.Context, *UpcallRequest) ([]byte, error) {
		return []byte{1, 2, 3}, nil
	})
	if _, err = ser.Acquire(ctx, "r", "a", LeaseOptions{}); err == nil || errors.Is(err, ErrLeaseDenied) {
		t.Fatalf("garbage answer: %v", err)
	}

	// ErrUpcallUnimplemented answers UNIMPLEMENTED.
	up.HandleKind(UpcallLeaseTransition, func(context.Context, *UpcallRequest) ([]byte, error) {
		return nil, ErrUpcallUnimplemented
	})
	if _, err = ser.Acquire(ctx, "r", "a", LeaseOptions{}); err == nil || !strings.Contains(err.Error(), "not implemented") {
		t.Fatalf("explicit unimplemented: %v", err)
	}
	if err := up.HandleKind(UpcallCancel, nil); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("handler for CANCEL: %v", err)
	}
}

func TestUpcalls_AcquireGivenUpByContextNeverLeaksALease(t *testing.T) {
	leakCheck(t)
	ctx := upCtx(t)
	rt := testRuntime(t)
	up := testUpcalls(t, rt)

	// 1. The handler holds the acquire past the kind's timeout: Rust stops
	// waiting and the handler's context is cancelled.
	gs := newGoSerializer()
	gs.delay = 30 * time.Second
	if err := up.SetPolicy(UpcallLeaseTransition, 400*time.Millisecond, FailureModeClosed); err != nil {
		t.Fatal(err)
	}
	if err := up.ServeLeaseSerializer(gs); err != nil {
		t.Fatal(err)
	}
	ser, err := NewForeignLeaseSerializer(up)
	if err != nil {
		t.Fatal(err)
	}
	defer ser.Close()
	short, cancel := context.WithTimeout(ctx, 100*time.Millisecond)
	defer cancel()
	if _, err := ser.Acquire(short, "r", "a", LeaseOptions{TTL: time.Minute}); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("Acquire = %v, want deadline exceeded", err)
	}
	eventually(t, "CANCEL{timeout} for the held acquire", func() bool { return up.CancelCount() >= 1 })

	// 2. The serializer grants after the caller's context ended: the lease is
	// released in the background instead of renewing forever.
	gs2 := newGoSerializer()
	gs2.delay = 300 * time.Millisecond
	up2 := testUpcalls(t, rt)
	if err := up2.ServeLeaseSerializer(gs2); err != nil {
		t.Fatal(err)
	}
	ser2, err := NewForeignLeaseSerializer(up2)
	if err != nil {
		t.Fatal(err)
	}
	defer ser2.Close()
	c, cancel2 := context.WithTimeout(ctx, 100*time.Millisecond)
	defer cancel2()
	if _, err := ser2.Acquire(c, "late", "a", LeaseOptions{TTL: time.Minute}); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("Acquire = %v, want deadline exceeded", err)
	}
	eventually(t, "the late grant to be released", func() bool { return gs2.count(LeaseRelease) == 1 })
	if s, err := ser2.Snapshot(ctx, "late"); err != nil || s.Held {
		t.Fatalf("late lease left held: %+v, %v", s, err)
	}

	// 3. Racing deadlines around the grant: whatever the outcome, no lease
	// stays held.
	gs3 := newGoSerializer()
	gs3.delay = 100 * time.Millisecond
	up3 := testUpcalls(t, rt)
	if err := up3.ServeLeaseSerializer(gs3); err != nil {
		t.Fatal(err)
	}
	ser3, err := NewForeignLeaseSerializer(up3)
	if err != nil {
		t.Fatal(err)
	}
	defer ser3.Close()
	for i := 0; i < 8; i++ {
		c, cancel := context.WithTimeout(ctx, 90*time.Millisecond+time.Duration(i)*4*time.Millisecond)
		l, err := ser3.Acquire(c, "race", "a", LeaseOptions{TTL: time.Minute})
		cancel()
		if err == nil {
			_ = l.Release(ctx)
		}
	}
	eventually(t, "every raced acquire to reach the serializer", func() bool { return gs3.count(LeaseAcquire) == 8 })
	eventually(t, "the race lease to be free", func() bool {
		s, err := ser3.Snapshot(ctx, "race")
		return err == nil && !s.Held
	})
}

func TestUpcalls_GoFencedResource(t *testing.T) {
	leakCheck(t)
	ctx := upCtx(t)
	rt := testRuntime(t)
	up := testUpcalls(t, rt)
	// The resource's own row: epoch 7 held by alice.
	if err := up.ServeFencedResource(fencedFunc(func(_ context.Context, f Fence, writer string, op []byte) (MutateResult, error) {
		if rej := CheckFence(7, true, "alice", f, writer); rej != nil {
			return MutateResult{Rejection: rej}, nil
		}
		out := []byte(op)
		for i, j := 0, len(out)-1; i < j; i, j = i+1, j-1 {
			out[i], out[j] = out[j], out[i]
		}
		return MutateResult{Output: out}, nil
	})); err != nil {
		t.Fatal(err)
	}
	res, err := NewForeignFencedResource(up)
	if err != nil {
		t.Fatal(err)
	}
	defer res.Close()

	abc := []byte("abc")
	ok, err := res.Mutate(ctx, Fence{"tree/42", 7, "alice"}, "alice", abc)
	if err != nil || !ok.Applied() || string(ok.Output) != "cba" {
		t.Fatalf("applied = %+v, %v", ok, err)
	}
	stale, err := res.Mutate(ctx, Fence{"tree/42", 6, "alice"}, "alice", abc)
	if err != nil || stale.Applied() || *stale.Rejection != (FenceRejection{RejectStaleEpoch, 6, 7}) {
		t.Fatalf("stale = %+v, %v", stale, err)
	}
	imp, err := res.Mutate(ctx, Fence{"tree/42", 7, "alice"}, "mallory", abc)
	if err != nil || imp.Applied() || imp.Rejection.Reason != RejectNotHolder {
		t.Fatalf("impostor = %+v, %v", imp, err)
	}

	// A resource that errors leaves the outcome unknown (never failed open).
	if err := up.ServeFencedResource(fencedFunc(func(context.Context, Fence, string, []byte) (MutateResult, error) {
		return MutateResult{}, errors.New("disk on fire")
	})); err != nil {
		t.Fatal(err)
	}
	if _, err := res.Mutate(ctx, Fence{"tree/42", 7, "alice"}, "alice", abc); !errors.Is(err, ErrMutationOutcomeUnknown) {
		t.Fatalf("erroring resource: %v", err)
	}
}

type fencedFunc func(context.Context, Fence, string, []byte) (MutateResult, error)

func (f fencedFunc) Mutate(ctx context.Context, fe Fence, w string, op []byte) (MutateResult, error) {
	return f(ctx, fe, w, op)
}

func TestUpcalls_AdmittedFilterIsBatchedAndCached(t *testing.T) {
	leakCheck(t)
	ctx := upCtx(t)
	rt := testRuntime(t)
	up := testUpcalls(t, rt)
	var mu sync.Mutex
	var batches [][]string
	if err := up.ServeAdmittedFilter(AdmittedFilterFunc(func(_ context.Context, ids []string) ([]bool, error) {
		mu.Lock()
		batches = append(batches, append([]string(nil), ids...))
		mu.Unlock()
		out := make([]bool, len(ids))
		for i, id := range ids {
			out[i] = strings.HasPrefix(id, "a")
		}
		return out, nil
	})); err != nil {
		t.Fatal(err)
	}
	nBatches := func() int { mu.Lock(); defer mu.Unlock(); return len(batches) }
	a, b, c := strings.Repeat("a", 64), strings.Repeat("b", 64), strings.Repeat("c", 64)

	f, err := NewAdmittedFilter(up, AdmittedFilterOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	if err := f.Prefetch(ctx, []string{a, b}); err != nil {
		t.Fatal(err)
	}
	mu.Lock()
	if len(batches) != 1 || len(batches[0]) != 2 || batches[0][0] != a || batches[0][1] != b {
		t.Fatalf("batches after prefetch: %v", batches)
	}
	mu.Unlock()
	if ok, err := f.IsAdmitted(a); err != nil || !ok {
		t.Fatalf("a = %v, %v", ok, err)
	}
	if ok, err := f.IsAdmitted(b); err != nil || ok {
		t.Fatalf("b = %v, %v", ok, err)
	}
	if ok, err := f.IsAdmitted(c); err != nil || ok { // miss: fail-closed answer
		t.Fatalf("c = %v, %v", ok, err)
	}
	eventually(t, "background resolution of c", func() bool { return nBatches() >= 2 })

	if err := f.Set(b, true); err != nil {
		t.Fatal(err)
	}
	if ok, _ := f.IsAdmitted(b); !ok {
		t.Fatal("pushed answer not served")
	}
	if err := f.Clear(); err != nil {
		t.Fatal(err)
	}
	st, err := f.Stats()
	if err != nil || st.Hits != 3 || st.Misses != 1 || st.Upcalls < 2 {
		t.Fatalf("stats = %+v, %v", st, err)
	}
	if _, err := f.IsAdmitted("zz"); err == nil {
		t.Fatal("a malformed node id was accepted")
	}

	// A failing filter caches the fallback (fail-closed) and Prefetch reports it.
	up2 := testUpcalls(t, rt)
	f2, err := NewAdmittedFilter(up2, AdmittedFilterOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer f2.Close()
	if err := f2.Prefetch(ctx, []string{a}); err == nil {
		t.Fatal("prefetch without a handler succeeded")
	}
	if ok, _ := f2.IsAdmitted(a); ok {
		t.Fatal("failed filter admitted a node")
	}
}

func TestUpcalls_EnableSharedInstallsTheGoFilter(t *testing.T) {
	leakCheck(t)
	ctx := upCtx(t)
	cfg := DefaultConfig()
	cfg.RelayMode = "disabled"
	cfg.BindAddr = "127.0.0.1:0"
	cfg.EnableMonitoring = true
	node, err := StartNode(ctx, cfg, NodeOptions{})
	if err != nil {
		t.Fatal(err)
	}
	// Close order matters: the filter and table before the node (and so its runtime).
	defer node.Close()
	up := testUpcalls(t, node.Runtime())
	if err := up.ServeAdmittedFilter(AdmittedFilterFunc(func(_ context.Context, ids []string) ([]bool, error) {
		return make([]bool, len(ids)), nil
	})); err != nil {
		t.Fatal(err)
	}
	f, err := NewAdmittedFilter(up, AdmittedFilterOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	if err := node.EnableShared(ctx, make([]byte, 31), f, nil, 0); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("short secret: %v", err)
	}
	secret := []byte(strings.Repeat("\x42", 32))
	if err := node.EnableShared(ctx, secret, f, nil, time.Second); err != nil {
		t.Fatal(err)
	}
	if _, err := f.Stats(); err != nil {
		t.Fatal(err)
	}
}

func TestUpcalls_CloseFailsPendingUpcallsAndStopsGoroutines(t *testing.T) {
	leakCheck(t)
	ctx := upCtx(t)
	rt, err := NewRuntime(ctx, DefaultRuntimeConfig())
	if err != nil {
		t.Fatal(err)
	}
	defer rt.Close()
	up, err := NewUpcalls(rt, 0)
	if err != nil {
		t.Fatal(err)
	}
	entered := make(chan struct{})
	var cancelled atomic.Bool
	up.HandleKind(UpcallLeaseTransition, func(c context.Context, _ *UpcallRequest) ([]byte, error) {
		close(entered)
		<-c.Done()
		cancelled.Store(true)
		return nil, c.Err()
	})
	ser, err := NewForeignLeaseSerializer(up)
	if err != nil {
		t.Fatal(err)
	}
	defer ser.Close()
	errc := make(chan error, 1)
	go func() {
		_, err := ser.Acquire(ctx, "r", "a", LeaseOptions{})
		errc <- err
	}()
	select {
	case <-entered:
	case <-ctx.Done():
		t.Fatal("handler never ran")
	}
	if err := up.Close(); err != nil {
		t.Fatal(err)
	}
	if !cancelled.Load() {
		t.Fatal("Close returned before the handler's context was cancelled")
	}
	select {
	case err := <-errc:
		if err == nil {
			t.Fatal("pending acquire succeeded")
		}
	case <-ctx.Done():
		t.Fatal("pending acquire never failed")
	}
	// Idempotent, and the table is gone.
	if err := up.Close(); err != nil {
		t.Fatal(err)
	}
	if up.Respond(1, UpcallStatusOK, nil) {
		t.Fatal("respond succeeded on a closed table")
	}
}

func TestUpcalls_PolicyValidation(t *testing.T) {
	leakCheck(t)
	rt := testRuntime(t)
	up := testUpcalls(t, rt)
	if err := up.SetPolicy(UpcallLeaseAuthorize, time.Second, FailureModeOpen); err != nil {
		t.Fatal(err)
	}
	if err := up.SetPolicy(UpcallCancel, time.Second, FailureModeOpen); err == nil {
		t.Fatal("a policy for CANCEL was accepted")
	}
	if err := up.SetDefaultPolicy(2*time.Second, FailureModeClosed); err != nil {
		t.Fatal(err)
	}
}

// A raw consumer: answering an unknown or already answered id is a no-op.
func TestUpcalls_RespondToUnknownInvocationIsIgnored(t *testing.T) {
	leakCheck(t)
	rt := testRuntime(t)
	up := testUpcalls(t, rt)
	if up.Respond(999, UpcallStatusOK, []byte{1}) {
		t.Fatal("respond to a never-issued id was accepted")
	}
}

// Stress: many concurrent upcalls answered out of order; every one is
// answered exactly once (run with -race, -count=20).
func TestUpcalls_ConcurrentUpcallsAnswerOutOfOrder(t *testing.T) {
	leakCheck(t)
	ctx := upCtx(t)
	rt := testRuntime(t)
	up := testUpcalls(t, rt)
	var calls atomic.Int64
	if err := up.ServeLeaseAuthorizer(LeaseAuthorizerFunc(func(_ context.Context, op LeaseOp) (bool, error) {
		calls.Add(1)
		// Later ops answer sooner, so answers come back reversed.
		time.Sleep(time.Duration(50-op.TTLMs%50) * time.Millisecond / 5)
		return op.TTLMs%2 == 0, nil
	})); err != nil {
		t.Fatal(err)
	}
	ser, err := NewMemoryLeaseSerializer(rt, up)
	if err != nil {
		t.Fatal(err)
	}
	defer ser.Close()
	const n = 40
	var wg sync.WaitGroup
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			res := "res/" + string(rune('a'+i%26)) + string(rune('A'+i/26))
			l, err := ser.Acquire(ctx, res, "actor", LeaseOptions{TTL: time.Duration(1000+i) * time.Millisecond})
			even := (1000+i)%2 == 0
			switch {
			case even && err != nil:
				t.Errorf("acquire %d (allowed): %v", i, err)
			case !even && !errors.Is(err, ErrLeaseDenied):
				t.Errorf("acquire %d (denied): %v", i, err)
			}
			if l != nil {
				_ = l.Release(ctx)
			}
		}(i)
	}
	wg.Wait()
	if calls.Load() < n {
		t.Fatalf("handler ran %d times, want at least %d", calls.Load(), n)
	}
}
