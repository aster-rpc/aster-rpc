//go:build cgo

package aster

import (
	"context"
	"errors"
	"fmt"
	"io"
	"iter"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// ─── unit tests (no network) ────────────────────────────────────────────────

func TestCircuitBreaker_BeforeCallAndHalfOpenSlot(t *testing.T) {
	cb := NewCircuitBreakerInterceptor()
	cb.FailureThreshold, cb.RecoveryTimeout = 2, 0.05
	for i := 0; i < 2; i++ {
		if err := cb.BeforeCall(); err != nil {
			t.Fatal(err)
		}
		cb.RecordFailure(&RpcError{Code: StatusUnavailable})
	}
	if cb.State() != "open" {
		t.Fatalf("state %s", cb.State())
	}
	if err := cb.BeforeCall(); !errors.Is(err, ErrRpcUnavailable) || !strings.Contains(err.Error(), "open") {
		t.Fatalf("open breaker: %v", err)
	}
	time.Sleep(70 * time.Millisecond)
	if err := cb.BeforeCall(); err != nil { // the half-open probe
		t.Fatal(err)
	}
	if err := cb.BeforeCall(); err == nil { // the slot is taken
		t.Fatal("second probe was admitted")
	}
	// The probe ends with an error that says nothing about service health:
	// the slot is released and the breaker stays half-open.
	cb.RecordFailure(&RpcError{Code: StatusNotFound})
	if cb.State() != "half_open" {
		t.Fatalf("state %s", cb.State())
	}
	if err := cb.BeforeCall(); err != nil {
		t.Fatalf("slot not released: %v", err)
	}
	cb.RecordSuccess()
	if cb.State() != "closed" {
		t.Fatalf("state %s", cb.State())
	}
	// A failed probe re-opens.
	cb.FailureThreshold = 1
	_ = cb.BeforeCall()
	cb.RecordFailure(&RpcError{Code: StatusInternal})
	time.Sleep(70 * time.Millisecond)
	_ = cb.BeforeCall()
	cb.RecordFailure(&RpcError{Code: StatusUnavailable})
	if cb.State() != "open" {
		t.Fatalf("state %s", cb.State())
	}
}

func TestCircuitBreaker_Concurrent(t *testing.T) {
	cb := NewCircuitBreakerInterceptor()
	cb.FailureThreshold, cb.RecoveryTimeout, cb.HalfOpenMaxCalls = 5, 0.001, 2
	var wg sync.WaitGroup
	for g := 0; g < 16; g++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := 0; i < 200; i++ {
				if cb.BeforeCall() != nil {
					continue
				}
				if (g+i)%3 == 0 {
					cb.RecordSuccess()
				} else {
					cb.RecordFailure(&RpcError{Code: StatusUnavailable})
				}
				_ = cb.State()
			}
		}()
	}
	wg.Wait()
}

func TestClientInterceptors_MetadataFor(t *testing.T) {
	orig := []MetadataEntry{{"a", "1"}, {"b", "2"}}
	if got := metadataFor(orig, map[string]string{"a": "1", "b": "2"}); &got[0] != &orig[0] {
		t.Fatal("untouched metadata must be passed through")
	}
	got := metadataFor(orig, map[string]string{"a": "9", "z": "3", "c": "4"})
	want := []MetadataEntry{{"a", "9"}, {"c", "4"}, {"z", "3"}}
	if !equalMeta(got, want) {
		t.Fatalf("got %v want %v", got, want)
	}
}

func TestClientInterceptors_Typed(t *testing.T) {
	if r, err := typed[testEchoRequest](&testEchoRequest{Message: "a"}, "request"); err != nil || r.Message != "a" {
		t.Fatal(r, err)
	}
	if r, err := typed[testEchoRequest](testEchoRequest{Message: "b"}, "request"); err != nil || r.Message != "b" {
		t.Fatal(r, err)
	}
	for _, bad := range []any{nil, "x", (*testEchoRequest)(nil), &testEchoReply{}} {
		if _, err := typed[testEchoRequest](bad, "request"); !errors.Is(err, ErrRpcInternal) {
			t.Fatalf("%T: %v", bad, err)
		}
	}
}

// ─── e2e fixture ────────────────────────────────────────────────────────────

// flaky is a service whose calls fail with UNAVAILABLE while healthy is false
// or for the first failFirst calls of each method.
type flaky struct {
	failFirst atomic.Int32
	calls     atomic.Int32 // unary + client-stream attempts that reached the server
	healthy   atomic.Bool
	failCode  atomic.Int32 // 0 = UNAVAILABLE
	lastMeta  atomic.Value // map[string]string seen by the server interceptor
	ingested  atomic.Value // []string of the last client stream
}

func (f *flaky) fail() error {
	n := f.calls.Add(1)
	code := StatusUnavailable
	if c := f.failCode.Load(); c != 0 {
		code = StatusCode(c)
	}
	if !f.healthy.Load() && n <= f.failFirst.Load() {
		return &RpcError{Code: code, Message: "synthetic failure"}
	}
	return nil
}

func (f *flaky) service(idempotent bool) *Service {
	var opts []MethodOption
	if idempotent {
		opts = append(opts, Idempotent())
	}
	return NewService("Flaky", 1,
		Unary("echo", func(_ context.Context, r *testEchoRequest) (*testEchoReply, error) {
			if err := f.fail(); err != nil {
				return nil, err
			}
			return &testEchoReply{Message: r.Message, Count: r.Count + 1, Data: []byte{}, ServerLang: "go"}, nil
		}, opts...),
		Unary("always_fail", func(_ context.Context, r *testEchoRequest) (*testEchoReply, error) {
			f.calls.Add(1)
			if f.healthy.Load() {
				return &testEchoReply{Message: "ok", Data: []byte{}}, nil
			}
			return nil, &RpcError{Code: StatusUnavailable, Message: "down"}
		}),
		Unary("slow", func(ctx context.Context, r *testEchoRequest) (*testEchoReply, error) {
			f.calls.Add(1)
			select {
			case <-ctx.Done():
			case <-time.After(3 * time.Second):
			}
			return &testEchoReply{Message: "slow", Data: []byte{}}, nil
		}),
		ServerStream("generate", func(_ context.Context, r *testEchoRequest, send func(*testEchoReply) error) error {
			for i := int32(0); i < r.Count; i++ {
				if err := send(&testEchoReply{Message: r.Message, Count: i, Data: []byte{}}); err != nil {
					return err
				}
			}
			if r.Message == "fail-after" {
				return &RpcError{Code: StatusNotFound, Message: "stream failed"}
			}
			return nil
		}),
		ClientStream("ingest", func(_ context.Context, reqs iter.Seq2[*testEchoRequest, error]) (*testEchoReply, error) {
			var msgs []string
			for r, err := range reqs {
				if err != nil {
					return nil, err
				}
				msgs = append(msgs, r.Message)
			}
			f.ingested.Store(msgs)
			if err := f.fail(); err != nil {
				return nil, err
			}
			return &testEchoReply{Message: strings.Join(msgs, ","), Count: int32(len(msgs)), Data: []byte{}}, nil
		}, opts...),
		BidiStream("double", func(_ context.Context, reqs iter.Seq2[*testEchoRequest, error], send func(*testEchoReply) error) error {
			for r, err := range reqs {
				if err != nil {
					return err
				}
				if strings.HasPrefix(r.Message, "boom") {
					return &RpcError{Code: StatusNotFound, Message: "bidi failed"}
				}
				if err := send(&testEchoReply{Message: r.Message + r.Message, Count: r.Count, Data: []byte{}}); err != nil {
					return err
				}
			}
			return nil
		}),
	)
}

type metaSpy struct {
	BaseInterceptor
	f *flaky
}

func (m metaSpy) OnRequest(ctx *CallContext, req any) (any, error) {
	cp := map[string]string{}
	for k, v := range ctx.Metadata {
		cp[k] = v
	}
	m.f.lastMeta.Store(cp)
	return req, nil
}

type clientFixture struct {
	f    *flaky
	cl   *AsterClient
	conn *RpcConnection
}

func newClientFixture(t *testing.T, idempotent bool, copts ...ClientOption) *clientFixture {
	t.Helper()
	f := &flaky{}
	svc := f.service(idempotent)
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	opts := ServerOptions{Services: []*Service{svc}, Interceptors: []Interceptor{metaSpy{f: f}}}
	opts.Config = testCfg()
	srv, err := StartServer(ctx, opts)
	if err != nil {
		t.Fatalf("StartServer: %v", err)
	}
	t.Cleanup(func() { _ = srv.Close() })
	ticket, err := srv.Ticket(ctx)
	if err != nil {
		t.Fatal(err)
	}
	cl, err := StartClient(ctx, testCfg(), copts...)
	if err != nil {
		t.Fatalf("StartClient: %v", err)
	}
	t.Cleanup(func() { _ = cl.Close() })
	conn, err := cl.ConnectTicket(ctx, ticket)
	if err != nil {
		t.Fatalf("connect: %v", err)
	}
	return &clientFixture{f: f, cl: cl, conn: conn}
}

func echo(ctx context.Context, f *clientFixture, msg string, opts ...CallOption) (*testEchoReply, error) {
	return Call[testEchoRequest, testEchoReply](ctx, f.conn, "Flaky", "echo", &testEchoRequest{Message: msg, Data: []byte{}}, opts...)
}

func fastRetry(attempts int) *RetryInterceptor {
	r := NewRetryInterceptor()
	r.Policy = RetryPolicy{MaxAttempts: attempts, Backoff: ExponentialBackoff{InitialMs: 1, MaxMs: 5, Multiplier: 2}}
	return r
}

func testCtx(t *testing.T) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	t.Cleanup(cancel)
	return ctx
}

// attemptSpy records the Attempt number of every OnRequest.
type attemptSpy struct {
	BaseInterceptor
	mu       sync.Mutex
	attempts []int
}

func (a *attemptSpy) OnRequest(ctx *CallContext, req any) (any, error) {
	a.mu.Lock()
	a.attempts = append(a.attempts, ctx.Attempt)
	a.mu.Unlock()
	return req, nil
}

// ─── retry ──────────────────────────────────────────────────────────────────

func TestClientInterceptors_RetryIdempotent(t *testing.T) {
	spy := &attemptSpy{}
	f := newClientFixture(t, true, WithClientInterceptors(spy, fastRetry(3)))
	f.f.failFirst.Store(2)
	// The idempotent flag comes from the registered service.
	if err := f.cl.RegisterService(f.f.service(true)); err != nil {
		t.Fatal(err)
	}
	reply, err := echo(testCtx(t), f, "hi")
	if err != nil || reply.Message != "hi" || reply.Count != 1 {
		t.Fatalf("%+v %v", reply, err)
	}
	if n := f.f.calls.Load(); n != 3 {
		t.Fatalf("server saw %d calls, want 3", n)
	}
	if !slices.Equal(spy.attempts, []int{1, 2, 3}) {
		t.Fatalf("attempts %v", spy.attempts)
	}
}

func TestClientInterceptors_RetryOnlyWhereAllowed(t *testing.T) {
	ctx := testCtx(t)
	t.Run("not idempotent", func(t *testing.T) {
		f := newClientFixture(t, false, WithClientInterceptors(fastRetry(3)))
		f.f.failFirst.Store(1)
		if err := f.cl.RegisterService(f.f.service(false)); err != nil {
			t.Fatal(err)
		}
		if _, err := echo(ctx, f, "x"); !errors.Is(err, ErrRpcUnavailable) {
			t.Fatalf("%v", err)
		}
		if n := f.f.calls.Load(); n != 1 {
			t.Fatalf("%d calls", n)
		}
	})
	t.Run("unknown service defaults to not idempotent, WithIdempotent overrides", func(t *testing.T) {
		f := newClientFixture(t, true, WithClientInterceptors(fastRetry(3)))
		f.f.failFirst.Store(1)
		if _, err := echo(ctx, f, "x"); !errors.Is(err, ErrRpcUnavailable) {
			t.Fatalf("%v", err)
		}
		f.f.calls.Store(0)
		f.f.failFirst.Store(1) // the first attempt of the next call fails
		if _, err := echo(ctx, f, "x", WithIdempotent(true)); err != nil {
			t.Fatalf("%v", err)
		}
		if n := f.f.calls.Load(); n != 2 {
			t.Fatalf("%d calls, want 2 (one retry)", n)
		}
	})
	t.Run("exhausted", func(t *testing.T) {
		f := newClientFixture(t, true, WithClientInterceptors(fastRetry(3)))
		f.f.failFirst.Store(100)
		if _, err := echo(ctx, f, "x", WithIdempotent(true)); !errors.Is(err, ErrRpcUnavailable) {
			t.Fatalf("%v", err)
		}
		if n := f.f.calls.Load(); n != 3 {
			t.Fatalf("%d calls, want 3", n)
		}
	})
	t.Run("code not retryable", func(t *testing.T) {
		f := newClientFixture(t, true, WithClientInterceptors(fastRetry(3)))
		f.f.failFirst.Store(100)
		f.f.failCode.Store(int32(StatusNotFound))
		if _, err := echo(ctx, f, "x", WithIdempotent(true)); !errors.Is(err, ErrRpcNotFound) {
			t.Fatalf("%v", err)
		}
		if n := f.f.calls.Load(); n != 1 {
			t.Fatalf("%d calls", n)
		}
	})
	t.Run("custom retryable codes", func(t *testing.T) {
		r := fastRetry(2)
		r.RetryableCodes = map[StatusCode]bool{StatusNotFound: true}
		f := newClientFixture(t, true, WithClientInterceptors(r))
		f.f.failFirst.Store(1)
		f.f.failCode.Store(int32(StatusNotFound))
		if _, err := echo(ctx, f, "x", WithIdempotent(true)); err != nil {
			t.Fatalf("%v", err)
		}
	})
}

func TestClientInterceptors_RetryBackoffBoundedByContext(t *testing.T) {
	r := NewRetryInterceptor()
	r.Policy = RetryPolicy{MaxAttempts: 5, Backoff: ExponentialBackoff{InitialMs: 10_000, MaxMs: 10_000, Multiplier: 1}}
	f := newClientFixture(t, true, WithClientInterceptors(r))
	f.f.failFirst.Store(100)
	ctx, cancel := context.WithTimeout(context.Background(), 400*time.Millisecond)
	defer cancel()
	start := time.Now()
	_, err := echo(ctx, f, "x", WithIdempotent(true))
	if !errors.Is(err, ErrRpcDeadlineExceeded) || !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("%v", err)
	}
	if d := time.Since(start); d > 3*time.Second {
		t.Fatalf("backoff slept past the deadline: %v", d)
	}
	if n := f.f.calls.Load(); n != 1 {
		t.Fatalf("%d calls", n)
	}
}

// ─── circuit breaker ────────────────────────────────────────────────────────

func TestClientInterceptors_CircuitBreakerEndToEnd(t *testing.T) {
	cb := NewCircuitBreakerInterceptor()
	cb.FailureThreshold, cb.RecoveryTimeout = 2, 0.2
	f := newClientFixture(t, false, WithClientInterceptors(cb))
	ctx := testCtx(t)
	call := func() (*testEchoReply, error) {
		return Call[testEchoRequest, testEchoReply](ctx, f.conn, "Flaky", "always_fail", &testEchoRequest{Data: []byte{}})
	}
	for i := 0; i < 2; i++ {
		if _, err := call(); !errors.Is(err, ErrRpcUnavailable) {
			t.Fatalf("call %d: %v", i, err)
		}
	}
	if cb.State() != "open" {
		t.Fatalf("state %s", cb.State())
	}
	// Rejected locally: the server never sees it.
	if _, err := call(); !errors.Is(err, ErrRpcUnavailable) || !strings.Contains(err.Error(), "circuit breaker is open") {
		t.Fatalf("open: %v", err)
	}
	if n := f.f.calls.Load(); n != 2 {
		t.Fatalf("server saw %d calls, want 2", n)
	}
	// After the recovery timeout one probe goes through; success closes it.
	f.f.healthy.Store(true)
	time.Sleep(300 * time.Millisecond)
	if r, err := call(); err != nil || r.Message != "ok" {
		t.Fatalf("probe: %v %v", r, err)
	}
	if cb.State() != "closed" {
		t.Fatalf("state %s", cb.State())
	}
	// A breaker is not run as a hook on streams: they pass while it is open.
	f.f.healthy.Store(false)
	cb.FailureThreshold = 1
	_, _ = call()
	if cb.State() != "open" {
		t.Fatalf("state %s", cb.State())
	}
	n := 0
	for _, err := range CallServerStream[testEchoRequest, testEchoReply](ctx, f.conn, "Flaky", "generate", &testEchoRequest{Count: 2, Data: []byte{}}) {
		if err != nil {
			t.Fatal(err)
		}
		n++
	}
	if n != 2 {
		t.Fatalf("%d stream items", n)
	}
}

func TestClientInterceptors_BreakerAndRetryTogether(t *testing.T) {
	cb := NewCircuitBreakerInterceptor()
	cb.FailureThreshold, cb.RecoveryTimeout = 2, 60
	f := newClientFixture(t, true, WithClientInterceptors(cb, fastRetry(5)))
	f.f.failFirst.Store(100)
	_, err := echo(testCtx(t), f, "x", WithIdempotent(true))
	// Two real failures open the breaker; the third attempt is refused
	// locally, and that refusal is not retried.
	if !errors.Is(err, ErrRpcUnavailable) || !strings.Contains(err.Error(), "circuit breaker is open") {
		t.Fatalf("%v", err)
	}
	if n := f.f.calls.Load(); n != 2 {
		t.Fatalf("server saw %d calls, want 2", n)
	}
}

func TestClientInterceptors_BreakerConcurrentRace(t *testing.T) {
	cb := NewCircuitBreakerInterceptor()
	cb.FailureThreshold, cb.RecoveryTimeout, cb.HalfOpenMaxCalls = 3, 0.01, 2
	f := newClientFixture(t, false, WithClientInterceptors(cb, fastRetry(2)))
	f.f.failFirst.Store(1 << 20)
	ctx := testCtx(t)
	var wg sync.WaitGroup
	for g := 0; g < 8; g++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := 0; i < 10; i++ {
				_, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Flaky", "always_fail", &testEchoRequest{Data: []byte{}}, WithIdempotent(true))
				if !errors.Is(err, ErrRpcUnavailable) {
					t.Errorf("%v", err)
					return
				}
				_ = cb.State()
			}
		}()
	}
	wg.Wait()
}

// ─── deadline ───────────────────────────────────────────────────────────────

func TestClientInterceptors_Deadline(t *testing.T) {
	di := NewDeadlineInterceptor()
	f := newClientFixture(t, true, WithClientInterceptors(di, fastRetry(3)))
	t.Run("expired before send", func(t *testing.T) {
		ctx, cancel := context.WithDeadline(context.Background(), time.Now().Add(-time.Second))
		defer cancel()
		if _, err := echo(ctx, f, "x", WithIdempotent(true)); !errors.Is(err, ErrRpcDeadlineExceeded) {
			t.Fatalf("%v", err)
		}
		if n := f.f.calls.Load(); n != 0 {
			t.Fatalf("server saw %d calls", n)
		}
	})
	t.Run("propagated into the native call, not retried", func(t *testing.T) {
		ctx, cancel := context.WithTimeout(context.Background(), 300*time.Millisecond)
		defer cancel()
		start := time.Now()
		_, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Flaky", "slow", &testEchoRequest{Data: []byte{}}, WithIdempotent(true))
		if !errors.Is(err, ErrRpcDeadlineExceeded) {
			t.Fatalf("%v", err)
		}
		if d := time.Since(start); d > 2500*time.Millisecond {
			t.Fatalf("took %v", d)
		}
		if n := f.f.calls.Load(); n != 1 {
			t.Fatalf("server saw %d calls, want 1 (DEADLINE_EXCEEDED is not retried)", n)
		}
	})
	t.Run("CallContext carries the deadline", func(t *testing.T) {
		var seen *time.Time
		var remaining *float64
		ic := &funcInterceptor{onRequest: func(cc *CallContext, req any) (any, error) {
			seen, remaining = cc.Deadline, cc.RemainingSeconds()
			return req, nil
		}}
		ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
		defer cancel()
		if _, err := echo(ctx, f, "x", WithInterceptors(ic)); err != nil {
			t.Fatal(err)
		}
		if d, _ := ctx.Deadline(); seen == nil || !seen.Equal(d) || remaining == nil || *remaining < 10 {
			t.Fatalf("deadline %v remaining %v", seen, remaining)
		}
	})
}

// ─── user interceptors ──────────────────────────────────────────────────────

type funcInterceptor struct {
	BaseInterceptor
	onRequest  func(*CallContext, any) (any, error)
	onResponse func(*CallContext, any) (any, error)
	onError    func(*CallContext, *RpcError) *RpcError
}

func (f *funcInterceptor) OnRequest(cc *CallContext, r any) (any, error) {
	if f.onRequest != nil {
		return f.onRequest(cc, r)
	}
	return r, nil
}
func (f *funcInterceptor) OnResponse(cc *CallContext, r any) (any, error) {
	if f.onResponse != nil {
		return f.onResponse(cc, r)
	}
	return r, nil
}
func (f *funcInterceptor) OnError(cc *CallContext, e *RpcError) *RpcError {
	if f.onError != nil {
		return f.onError(cc, e)
	}
	return e
}

type orderLog struct {
	mu  sync.Mutex
	log []string
}

func (o *orderLog) add(s string) { o.mu.Lock(); o.log = append(o.log, s); o.mu.Unlock() }

func orderInterceptor(name string, l *orderLog) *funcInterceptor {
	return &funcInterceptor{
		onRequest:  func(_ *CallContext, r any) (any, error) { l.add("req:" + name); return r, nil },
		onResponse: func(_ *CallContext, r any) (any, error) { l.add("resp:" + name); return r, nil },
		onError:    func(_ *CallContext, e *RpcError) *RpcError { l.add("err:" + name); return e },
	}
}

func TestClientInterceptors_OrderAndMutation(t *testing.T) {
	l := &orderLog{}
	mut := &funcInterceptor{
		onRequest: func(cc *CallContext, r any) (any, error) {
			req := r.(*testEchoRequest)
			cc.Metadata["x-added"] = "yes"
			cc.Metadata["token"] = "t1"
			return &testEchoRequest{Message: req.Message + "+req", Data: []byte{}}, nil
		},
		onResponse: func(_ *CallContext, r any) (any, error) {
			rep := r.(*testEchoReply)
			rep.Message += "+resp"
			return rep, nil
		},
	}
	f := newClientFixture(t, false, WithClientInterceptors(orderInterceptor("A", l), mut, orderInterceptor("B", l)))
	ctx := testCtx(t)
	reply, err := echo(ctx, f, "m", WithMetadata("token", "t0"), WithMetadata("keep", "k"))
	if err != nil || reply.Message != "m+req+resp" {
		t.Fatalf("%+v %v", reply, err)
	}
	if got := strings.Join(l.log, " "); got != "req:A req:B resp:A resp:B" {
		t.Fatalf("order %q", got)
	}
	meta, _ := f.f.lastMeta.Load().(map[string]string)
	if meta["x-added"] != "yes" || meta["token"] != "t1" || meta["keep"] != "k" {
		t.Fatalf("server saw metadata %v", meta)
	}

	// Errors run in reverse order and may be rewritten or swallowed.
	l.log = nil
	f.f.failFirst.Store(100)
	if _, err := echo(ctx, f, "m"); !errors.Is(err, ErrRpcUnavailable) {
		t.Fatalf("%v", err)
	}
	if got := strings.Join(l.log, " "); got != "req:A req:B err:B err:A" {
		t.Fatalf("order %q", got)
	}
	rewrite := &funcInterceptor{onError: func(_ *CallContext, e *RpcError) *RpcError {
		return &RpcError{Code: StatusNotFound, Message: "rewritten: " + e.Message}
	}}
	if _, err := echo(ctx, f, "m", WithInterceptors(rewrite)); !errors.Is(err, ErrRpcNotFound) || !strings.Contains(err.Error(), "rewritten") {
		t.Fatalf("%v", err)
	}
	swallow := &funcInterceptor{onError: func(*CallContext, *RpcError) *RpcError { return nil }}
	if r, err := echo(ctx, f, "m", WithInterceptors(swallow)); err != nil || r == nil || r.Message != "" {
		t.Fatalf("swallowed error: %+v %v", r, err)
	}
	// An interceptor that rejects the request ends the call before the server.
	f.f.calls.Store(0)
	deny := &funcInterceptor{onRequest: func(*CallContext, any) (any, error) {
		return nil, &RpcError{Code: StatusPermissionDenied, Message: "no"}
	}}
	if _, err := echo(ctx, f, "m", WithInterceptors(deny)); !errors.Is(err, ErrRpcPermissionDenied) {
		t.Fatalf("%v", err)
	}
	if n := f.f.calls.Load(); n != 0 {
		t.Fatalf("server saw %d calls", n)
	}
	// A hook returning the wrong type is an INTERNAL error, not a panic.
	bad := &funcInterceptor{onRequest: func(*CallContext, any) (any, error) { return "nope", nil }}
	if _, err := echo(ctx, f, "m", WithInterceptors(bad)); !errors.Is(err, ErrRpcInternal) {
		t.Fatalf("%v", err)
	}
	// A plain error from a hook is UNKNOWN.
	plain := &funcInterceptor{onRequest: func(*CallContext, any) (any, error) { return nil, fmt.Errorf("boom") }}
	if _, err := echo(ctx, f, "m", WithInterceptors(plain)); !errors.Is(err, ErrRpcUnknown) {
		t.Fatalf("%v", err)
	}
}

func TestClientInterceptors_CallContextFields(t *testing.T) {
	f := newClientFixture(t, true)
	if err := f.cl.RegisterService(f.f.service(true)); err != nil {
		t.Fatal(err)
	}
	var cc CallContext
	spy := &funcInterceptor{onRequest: func(c *CallContext, r any) (any, error) { cc = *c; return r, nil }}
	if _, err := echo(testCtx(t), f, "x", WithInterceptors(spy), WithSession(0)); err != nil {
		t.Fatal(err)
	}
	if cc.Service != "Flaky" || cc.Method != "echo" || cc.Pattern != "unary" || !cc.Idempotent || cc.IsStreaming ||
		cc.Attempt != 1 || cc.CallID == "" || cc.Peer == "" || cc.Deadline == nil {
		t.Fatalf("%+v", cc)
	}
}

// ─── streams ────────────────────────────────────────────────────────────────

func TestClientInterceptors_ServerStream(t *testing.T) {
	var seen atomic.Int32
	var gotErr atomic.Value
	ic := &funcInterceptor{
		onRequest: func(cc *CallContext, r any) (any, error) {
			if cc.Pattern != "server_stream" || !cc.IsStreaming {
				t.Errorf("pattern %q", cc.Pattern)
			}
			return r, nil
		},
		onResponse: func(_ *CallContext, r any) (any, error) {
			seen.Add(1)
			rep := r.(*testEchoReply)
			rep.Message = "<" + rep.Message + ">"
			return rep, nil
		},
		onError: func(_ *CallContext, e *RpcError) *RpcError { gotErr.Store(e.Code); return e },
	}
	// A retry interceptor does nothing on a stream.
	f := newClientFixture(t, true, WithClientInterceptors(ic, fastRetry(3)))
	ctx := testCtx(t)
	var got []string
	var last error
	for r, err := range CallServerStream[testEchoRequest, testEchoReply](ctx, f.conn, "Flaky", "generate", &testEchoRequest{Message: "fail-after", Count: 3, Data: []byte{}}) {
		if err != nil {
			last = err
			break
		}
		got = append(got, r.Message)
	}
	if !slices.Equal(got, []string{"<fail-after>", "<fail-after>", "<fail-after>"}) || !errors.Is(last, ErrRpcNotFound) {
		t.Fatalf("%v %v", got, last)
	}
	if seen.Load() != 3 || gotErr.Load() != StatusNotFound {
		t.Fatalf("hooks: %d responses, error %v", seen.Load(), gotErr.Load())
	}
	// A swallowed error ends the stream cleanly.
	n := 0
	swallow := &funcInterceptor{onError: func(*CallContext, *RpcError) *RpcError { return nil }}
	for _, err := range CallServerStream[testEchoRequest, testEchoReply](ctx, f.conn, "Flaky", "generate", &testEchoRequest{Message: "fail-after", Count: 2, Data: []byte{}}, WithInterceptors(swallow)) {
		if err != nil {
			t.Fatalf("%v", err)
		}
		n++
	}
	if n != 2 {
		t.Fatalf("%d items", n)
	}
}

func replayable(msgs ...string) iter.Seq[*testEchoRequest] {
	return func(yield func(*testEchoRequest) bool) {
		for i, m := range msgs {
			if !yield(&testEchoRequest{Message: m, Count: int32(i), Data: []byte{}}) {
				return
			}
		}
	}
}

func TestClientInterceptors_ClientStream(t *testing.T) {
	upper := &funcInterceptor{onRequest: func(cc *CallContext, r any) (any, error) {
		cc.Metadata["x-stream"] = "1"
		req := r.(*testEchoRequest)
		return &testEchoRequest{Message: strings.ToUpper(req.Message), Count: req.Count, Data: []byte{}}, nil
	}}
	f := newClientFixture(t, true, WithClientInterceptors(upper, fastRetry(3)))
	if err := f.cl.RegisterService(f.f.service(true)); err != nil {
		t.Fatal(err)
	}
	ctx := testCtx(t)
	f.f.failFirst.Store(1) // the first attempt fails after the server read the stream
	r, err := CallClientStream[testEchoRequest, testEchoReply](ctx, f.conn, "Flaky", "ingest", replayable("a", "b", "c"))
	if err != nil || r.Message != "A,B,C" || r.Count != 3 {
		t.Fatalf("%+v %v", r, err)
	}
	if n := f.f.calls.Load(); n != 2 {
		t.Fatalf("server saw %d attempts, want 2", n)
	}
	// Metadata set by the hook on the first message reached the header.
	if meta, _ := f.f.lastMeta.Load().(map[string]string); meta["x-stream"] != "1" {
		t.Fatalf("metadata %v", meta)
	}

	// A hook that fails mid-stream aborts the call with its error.
	boom := &funcInterceptor{onRequest: func(_ *CallContext, r any) (any, error) {
		if r.(*testEchoRequest).Message == "BAD" {
			return nil, &RpcError{Code: StatusInvalidArgument, Message: "bad message"}
		}
		return r, nil
	}}
	f.f.healthy.Store(true)
	if _, err := CallClientStream[testEchoRequest, testEchoReply](ctx, f.conn, "Flaky", "ingest", replayable("a", "bad", "c"), WithInterceptors(boom)); !errors.Is(err, ErrRpcInvalidArgument) {
		t.Fatalf("%v", err)
	}
	// The caller's own cancellation is still CANCELLED (and not retried).
	cctx, cancel := context.WithCancel(ctx)
	cancel()
	if _, err := CallClientStream[testEchoRequest, testEchoReply](cctx, f.conn, "Flaky", "ingest", replayable("a")); !errors.Is(err, ErrRpcCancelled) {
		t.Fatalf("%v", err)
	}
}

func TestClientInterceptors_Bidi(t *testing.T) {
	var reqs, resps atomic.Int32
	var gotErr atomic.Value
	ic := &funcInterceptor{
		onRequest: func(cc *CallContext, r any) (any, error) {
			if cc.Pattern != "bidi_stream" {
				t.Errorf("pattern %q", cc.Pattern)
			}
			reqs.Add(1)
			req := r.(*testEchoRequest)
			return &testEchoRequest{Message: req.Message + "!", Count: req.Count, Data: []byte{}}, nil
		},
		onResponse: func(_ *CallContext, r any) (any, error) { resps.Add(1); return r, nil },
		onError:    func(_ *CallContext, e *RpcError) *RpcError { gotErr.Store(e.Code); return e },
	}
	f := newClientFixture(t, false, WithClientInterceptors(ic))
	ctx := testCtx(t)
	var got []string
	for r, err := range CallBidiStream[testEchoRequest, testEchoReply](ctx, f.conn, "Flaky", "double", replayable("a", "b")) {
		if err != nil {
			t.Fatal(err)
		}
		got = append(got, r.Message)
	}
	if !slices.Equal(got, []string{"a!a!", "b!b!"}) || reqs.Load() != 2 || resps.Load() != 2 {
		t.Fatalf("%v %d %d", got, reqs.Load(), resps.Load())
	}

	// Concurrent Send / Recv with hooks (run under -race), then a failure.
	b, err := OpenBidiStream[testEchoRequest, testEchoReply](ctx, f.conn, "Flaky", "double")
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	const n = 50
	done := make(chan error, 1)
	go func() {
		for i := 0; i < n; i++ {
			if err := b.Send(&testEchoRequest{Message: "p", Count: int32(i), Data: []byte{}}); err != nil {
				done <- err
				return
			}
		}
		done <- b.Send(&testEchoRequest{Message: "boom", Data: []byte{}})
	}()
	for i := 0; i < n; i++ {
		r, err := b.Recv()
		if err != nil || r.Count != int32(i) {
			t.Fatalf("recv %d: %+v %v", i, r, err)
		}
	}
	if err := <-done; err != nil {
		t.Fatal(err)
	}
	if _, err := b.Recv(); !errors.Is(err, ErrRpcNotFound) {
		t.Fatalf("%v", err)
	}
	if gotErr.Load() != StatusNotFound {
		t.Fatalf("error hook saw %v", gotErr.Load())
	}
	// A request hook that fails fails Send, not the call.
	bad := &funcInterceptor{onRequest: func(*CallContext, any) (any, error) {
		return nil, &RpcError{Code: StatusPermissionDenied, Message: "no"}
	}}
	b2, err := OpenBidiStream[testEchoRequest, testEchoReply](ctx, f.conn, "Flaky", "double", WithInterceptors(bad))
	if err != nil {
		t.Fatal(err)
	}
	defer b2.Close()
	if err := b2.Send(&testEchoRequest{Data: []byte{}}); !errors.Is(err, ErrRpcPermissionDenied) {
		t.Fatalf("%v", err)
	}
	b2.Cancel()
	if _, err := b2.Recv(); err == nil || errors.Is(err, io.EOF) {
		t.Fatalf("recv after cancel: %v", err)
	}
}

// Per-call and per-client interceptors compose (client's first), and a
// connection built with NewRpcConnection takes the option too.
func TestClientInterceptors_PerCallAndNewRpcConnection(t *testing.T) {
	l := &orderLog{}
	f := newClientFixture(t, false, WithClientInterceptors(orderInterceptor("client", l)))
	ctx := testCtx(t)
	if _, err := echo(ctx, f, "x", WithInterceptors(orderInterceptor("call", l))); err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(l.log, " "); got != "req:client req:call resp:client resp:call" {
		t.Fatalf("%q", got)
	}
	l2 := &orderLog{}
	rc := NewRpcConnection(f.conn.Connection(), nil, WithClientInterceptors(orderInterceptor("raw", l2)))
	if _, err := Call[testEchoRequest, testEchoReply](ctx, rc, "Flaky", "echo", &testEchoRequest{Message: "y", Data: []byte{}}); err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(l2.log, " "); got != "req:raw resp:raw" {
		t.Fatalf("%q", got)
	}
	// No interceptors: the plain path still works.
	if _, err := Call[testEchoRequest, testEchoReply](ctx, NewRpcConnection(f.conn.Connection(), nil), "Flaky", "echo", &testEchoRequest{Data: []byte{}}); err != nil {
		t.Fatal(err)
	}
}

// Concurrent intercepted calls on one connection (run with -race -count=20).
func TestClientInterceptors_ConcurrentCalls(t *testing.T) {
	var reqs atomic.Int32
	count := &funcInterceptor{onRequest: func(_ *CallContext, r any) (any, error) { reqs.Add(1); return r, nil }}
	f := newClientFixture(t, true, WithClientInterceptors(count, fastRetry(3), NewDeadlineInterceptor(), NewCircuitBreakerInterceptor()))
	ctx := testCtx(t)
	var wg sync.WaitGroup
	for g := 0; g < 8; g++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := 0; i < 5; i++ {
				msg := fmt.Sprintf("g%d-%d", g, i)
				r, err := echo(ctx, f, msg)
				if err != nil || r.Message != msg {
					t.Errorf("%s: %+v %v", msg, r, err)
					return
				}
			}
		}()
	}
	wg.Wait()
	if reqs.Load() != 40 {
		t.Fatalf("%d request hooks, want 40", reqs.Load())
	}
}

func TestClientInterceptors_NativeBreakerAftermath(t *testing.T) {
	for _, aftermath := range []string{"later_refusal", "cancelled_probe"} {
		t.Run(aftermath, func(t *testing.T) {
			first := NewCircuitBreakerInterceptor()
			first.FailureThreshold, first.RecoveryTimeout = 1, 0
			second := NewCircuitBreakerInterceptor()
			second.FailureThreshold, second.RecoveryTimeout = 1, 0.15
			breakers := []Interceptor{first}
			if aftermath == "later_refusal" {
				breakers = append(breakers, second)
			}
			f := newClientFixture(t, false, WithClientInterceptors(breakers...))
			f.f.failFirst.Store(1)
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			if _, err := echo(ctx, f, "seed"); !errors.Is(err, ErrRpcUnavailable) {
				t.Fatalf("seed: %v", err)
			}
			if aftermath == "later_refusal" {
				if _, err := echo(ctx, f, "refused"); !errors.Is(err, ErrRpcUnavailable) {
					t.Fatalf("refusal: %v", err)
				}
				if f.f.calls.Load() != 1 {
					t.Fatal("refused call reached peer")
				}
				time.Sleep(180 * time.Millisecond)
			} else {
				probeCtx, stop := context.WithCancel(ctx)
				defer stop()
				done := make(chan error, 1)
				go func() {
					_, err := Call[testEchoRequest, testEchoReply](probeCtx, f.conn, "Flaky", "slow", &testEchoRequest{Data: []byte{}})
					done <- err
				}()
				until := time.Now().Add(2 * time.Second)
				for f.f.calls.Load() != 2 {
					if time.Now().After(until) {
						t.Fatal("probe never reached peer")
					}
					time.Sleep(time.Millisecond)
				}
				stop()
				select {
				case err := <-done:
					if !errors.Is(err, context.Canceled) {
						t.Fatalf("cancel: %v", err)
					}
				case <-ctx.Done():
					t.Fatal("probe cancellation hung")
				}
			}
			if first.State() != "half_open" {
				t.Fatalf("abandoned probe state %s", first.State())
			}
			reply, err := echo(ctx, f, "healthy-probe")
			if err != nil || reply.Message != "healthy-probe" {
				t.Fatalf("healthy probe: %v %v", reply, err)
			}
			if first.State() != "closed" {
				t.Fatalf("probe failed to close breaker: %s", first.State())
			}
			if aftermath == "later_refusal" && second.State() != "closed" {
				t.Fatal("later breaker did not recover")
			}
		})
	}
}
