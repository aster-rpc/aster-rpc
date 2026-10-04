//go:build cgo

package aster

import (
	"context"
	"errors"
	"iter"
	"testing"
	"time"
)

// ABI 4.0: cancellable unary calls run on their own native call
// (aster_rpc_open), so cancelling ctx stops the native call (streaming
// handlers also see the peer cancel); every native recv wait is bounded and
// observes ctx.
func TestRPC_CancellableUnaryAndBoundedRecv(t *testing.T) {
	unaryStarted := make(chan struct{}, 1)
	releaseUnary := make(chan struct{})
	unaryDone := make(chan error, 1)
	ingestDone := make(chan error, 1)
	svc := NewService("CancelTest", 1,
		Unary("echo", func(_ context.Context, r *testEchoRequest) (*testEchoReply, error) {
			return &testEchoReply{Message: r.Message, Data: []byte{}}, nil
		}),
		Unary("block", func(ctx context.Context, _ *testEchoRequest) (*testEchoReply, error) {
			unaryStarted <- struct{}{}
			<-releaseUnary
			unaryDone <- ctx.Err()
			return &testEchoReply{Data: []byte{}}, nil
		}),
		ClientStream("hold", func(ctx context.Context, reqs iter.Seq2[*testEchoRequest, error]) (*testEchoReply, error) {
			for _, err := range reqs {
				if err != nil {
					ingestDone <- err
					return nil, err
				}
			}
			ingestDone <- nil
			return &testEchoReply{Data: []byte{}}, nil
		}),
		BidiStream("silent", func(ctx context.Context, reqs iter.Seq2[*testEchoRequest, error], _ func(*testEchoReply) error) error {
			for _, err := range reqs {
				if err != nil {
					return err
				}
			}
			return nil
		}),
	)
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	// Both unary paths work: pooled (ctx that never ends) and per-call.
	for _, c := range []context.Context{context.Background(), ctx} {
		r, err := Call[testEchoRequest, testEchoReply](c, f.conn, "CancelTest", "echo", &testEchoRequest{Message: "x", Data: []byte{}})
		if err != nil || r.Message != "x" {
			t.Fatalf("echo: %v %+v", err, r)
		}
	}

	// Unary cancelled mid-flight (the handler is running): returns promptly
	// with CANCELLED. Rust's server watches peer cancels on streaming calls
	// only, so the handler runs on; its late answer is dropped and the call
	// leaves the server's table.
	cctx, ccancel := context.WithCancel(ctx)
	go func() {
		<-unaryStarted
		time.Sleep(100 * time.Millisecond)
		ccancel()
	}()
	start := time.Now()
	_, err := Call[testEchoRequest, testEchoReply](cctx, f.conn, "CancelTest", "block", &testEchoRequest{Data: []byte{}})
	if !errors.Is(err, context.Canceled) || !errors.Is(err, ErrRpcCancelled) {
		t.Fatalf("cancelled unary: %v", err)
	}
	if d := time.Since(start); d > 3*time.Second {
		t.Fatalf("cancelled unary took %v", d)
	}
	close(releaseUnary)
	select {
	case <-unaryDone:
	case <-time.After(10 * time.Second):
		t.Fatal("unary handler did not finish")
	}
	// The connection's pooled path still works after the cancelled call.
	if _, err := Call[testEchoRequest, testEchoReply](context.Background(), f.conn, "CancelTest", "echo", &testEchoRequest{Data: []byte{}}); err != nil {
		t.Fatalf("echo after cancel: %v", err)
	}

	// A server handler blocked pulling client-stream requests observes the
	// peer cancel (bounded aster_dispatch_recv + call context).
	cctx, ccancel = context.WithCancel(ctx)
	reqs := func(yield func(*testEchoRequest) bool) {
		if !yield(&testEchoRequest{Message: "one", Data: []byte{}}) {
			return
		}
		time.AfterFunc(300*time.Millisecond, ccancel)
		<-cctx.Done()
	}
	_, err = CallClientStream[testEchoRequest, testEchoReply](cctx, f.conn, "CancelTest", "hold", reqs)
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("cancelled client stream: %v", err)
	}
	select {
	case err := <-ingestDone:
		if err == nil {
			t.Fatal("client-stream handler saw a clean end instead of the cancel")
		}
	case <-time.After(10 * time.Second):
		t.Fatal("client-stream handler blocked in recv did not observe the cancel")
	}

	// A client blocked in Recv returns once its context is cancelled.
	cctx, ccancel = context.WithCancel(ctx)
	defer ccancel()
	b, err := OpenBidiStream[testEchoRequest, testEchoReply](cctx, f.conn, "CancelTest", "silent")
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	if err := b.Send(&testEchoRequest{Data: []byte{}}); err != nil {
		t.Fatal(err)
	}
	time.AfterFunc(300*time.Millisecond, ccancel)
	start = time.Now()
	if _, err := b.Recv(); !errors.Is(err, context.Canceled) {
		t.Fatalf("bidi recv after cancel: %v", err)
	}
	if d := time.Since(start); d > 3*time.Second {
		t.Fatalf("bidi recv took %v to observe the cancel", d)
	}

	deadline := time.Now().Add(5 * time.Second)
	for f.srv.LiveCalls() != 0 && time.Now().Before(deadline) {
		time.Sleep(20 * time.Millisecond)
	}
	if n := f.srv.LiveCalls(); n != 0 {
		t.Fatalf("%d live calls left", n)
	}
}

// recv on a call whose context ended (without the native cancel having woken
// it) returns from its bounded wait.
func TestRPC_RecvObservesContextBetweenSlices(t *testing.T) {
	svc := NewService("CancelTest2", 1,
		ServerStream("forever", func(ctx context.Context, _ *testEchoRequest, _ func(*testEchoReply) error) error {
			<-ctx.Done()
			return ctx.Err()
		}),
	)
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	cctx, ccancel := context.WithCancel(ctx)
	c, err := openCall(cctx, f.conn, PatternServerStream, "CancelTest2", "forever", buildOptions(nil))
	if err != nil {
		t.Fatal(err)
	}
	defer c.release()
	// Detach the AfterFunc so only the slice check can notice the cancel.
	c.stop()
	if err := c.send(mustEncode(t, f.conn, &testEchoRequest{Data: []byte{}}), true); err != nil {
		t.Fatal(err)
	}
	time.AfterFunc(200*time.Millisecond, ccancel)
	start := time.Now()
	if _, err := c.recv(); !errors.Is(err, context.Canceled) {
		t.Fatalf("recv: %v", err)
	}
	if d := time.Since(start); d > 2*time.Second {
		t.Fatalf("recv took %v", d)
	}
}

func mustEncode(t *testing.T, rc *RpcConnection, v any) []byte {
	t.Helper()
	b, err := rc.codec.Encode(v)
	if err != nil {
		t.Fatal(err)
	}
	return b
}
