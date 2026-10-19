//go:build cgo

package aster

import (
	"context"
	"errors"
	"net/http"
	"sync/atomic"
	"testing"
	"time"
)

// drainService: "hold" counts itself started and waits for release (or its
// context); "ping" answers at once.
func drainService(started *atomic.Int32, cancelled *atomic.Int32, release <-chan struct{}) *Service {
	return NewService("DrainEcho", 1,
		Unary("hold", func(ctx context.Context, r *testEchoRequest) (*testEchoReply, error) {
			started.Add(1)
			select {
			case <-release:
				return &testEchoReply{Message: r.Message, Data: []byte{}}, nil
			case <-ctx.Done():
				cancelled.Add(1)
				return nil, ctx.Err()
			}
		}),
		Unary("ping", func(_ context.Context, r *testEchoRequest) (*testEchoReply, error) {
			return &testEchoReply{Message: r.Message, Data: []byte{}}, nil
		}),
	)
}

// drainEventually polls cond until it holds (generous bound).
func drainEventually(t *testing.T, what string, cond func() bool) {
	t.Helper()
	for deadline := time.Now().Add(20 * time.Second); time.Now().Before(deadline); time.Sleep(25 * time.Millisecond) {
		if cond() {
			return
		}
	}
	t.Fatalf("timed out waiting for %s", what)
}

func TestDrain_InFlightCompletesNewCallsRefusedReadinessOff(t *testing.T) {
	var started, cancelled atomic.Int32
	release := make(chan struct{})
	f := newRPCFixture(t, ServerOptions{
		Services: []*Service{drainService(&started, &cancelled, release)},
		Health:   &HealthConfig{Host: "127.0.0.1"},
	})
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	addr, err := f.srv.Health().Addr()
	if err != nil {
		t.Fatal(err)
	}
	readyz := func() int {
		resp, err := http.Get("http://" + addr + "/readyz")
		if err != nil {
			return -1
		}
		_ = resp.Body.Close()
		return resp.StatusCode
	}
	if code := readyz(); code != 200 {
		t.Fatalf("readyz before drain: %d", code)
	}

	held := make(chan error, 1)
	go func() {
		r, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "DrainEcho", "hold", &testEchoRequest{Message: "held"})
		if err == nil && r.Message != "held" {
			err = errors.New("wrong reply " + r.Message)
		}
		held <- err
	}()
	drainEventually(t, "the held call in its handler", func() bool { return started.Load() == 1 })

	type drained struct {
		r   DrainReport
		err error
	}
	done := make(chan drained, 1)
	go func() {
		r, err := f.srv.Drain(ctx, 30*time.Second)
		done <- drained{r, err}
	}()

	var refused error
	drainEventually(t, "a new call refused as draining", func() bool {
		_, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "DrainEcho", "ping", &testEchoRequest{Message: "p"})
		refused = err
		return err != nil
	})
	if !IsDraining(refused) || !errors.Is(refused, ErrRpcUnavailable) {
		t.Fatalf("refusal is not a draining UNAVAILABLE: %v", refused)
	}
	drainEventually(t, "readiness off", func() bool { return readyz() != 200 })
	select {
	case d := <-done:
		t.Fatalf("drain ended while a call was held: %+v %v", d.r, d.err)
	default:
	}

	close(release)
	if err := <-held; err != nil {
		t.Fatalf("held call: %v", err)
	}
	d := <-done
	if d.err != nil {
		t.Fatalf("Drain: %v", d.err)
	}
	r := d.r
	// A probe admitted just before the drain began counts in flight too.
	if r.CallsInFlight < 1 || r.CallsCompleted != r.CallsInFlight || r.CallsCancelled != 0 || r.CallsRefused < 1 ||
		r.ConnectionsClosed != 1 || r.DeadlineReached || r.Elapsed >= 30*time.Second {
		t.Fatalf("report %+v", r)
	}
	if cancelled.Load() != 0 {
		t.Fatal("a completed call was cancelled")
	}
	if _, err := f.srv.Drain(ctx, 0); !errors.Is(err, ErrAlreadyClosed) {
		t.Fatalf("second drain: %v", err)
	}
}

func TestDrain_ContextCancelCutsGraceShort(t *testing.T) {
	var started, cancelled atomic.Int32
	f := newRPCFixture(t, ServerOptions{Services: []*Service{drainService(&started, &cancelled, make(chan struct{}))}})
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	go func() {
		_, _ = Call[testEchoRequest, testEchoReply](ctx, f.conn, "DrainEcho", "hold", &testEchoRequest{Message: "stuck"})
	}()
	drainEventually(t, "the call in its handler", func() bool { return started.Load() == 1 })

	dctx, dcancel := context.WithCancel(ctx)
	time.AfterFunc(200*time.Millisecond, dcancel)
	begin := time.Now()
	r, err := f.srv.Drain(dctx, time.Minute)
	if err != nil {
		t.Fatalf("Drain: %v", err)
	}
	if time.Since(begin) > 20*time.Second || !r.DeadlineReached || r.CallsCancelled != 1 {
		t.Fatalf("cancel did not cut the drain short: %+v after %v", r, time.Since(begin))
	}
	if _, err := f.srv.Drain(ctx, -time.Second); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("negative grace: %v", err)
	}
}
