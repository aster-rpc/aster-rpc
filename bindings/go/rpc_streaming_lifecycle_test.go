//go:build cgo

package aster

import (
	"context"
	"errors"
	"io"
	"iter"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func TestRPC_ClientStreamEarlyResponseWithParkedProducer(t *testing.T) {
	for _, reject := range []bool{false, true} {
		t.Run(map[bool]string{false: "success", true: "rejection"}[reject], func(t *testing.T) {
			parked, release, produced := make(chan struct{}), make(chan struct{}), make(chan struct{})
			var releaseOnce sync.Once
			unpark := func() { releaseOnce.Do(func() { close(release) }) }
			delivered := make(chan struct{})
			late := make(chan bool, 1)
			svc := NewService("Lifecycle", 1,
				ClientStream("upload", func(_ context.Context, reqs iter.Seq2[*testEchoRequest, error]) (*testEchoReply, error) {
					for req, err := range reqs {
						if err != nil {
							return nil, err
						}
						if req.Message != "first" {
							t.Errorf("request %q", req.Message)
						}
						close(delivered)
						<-parked
						if reject {
							return nil, &RpcError{Code: StatusInvalidArgument, Message: "early rejection"}
						}
						return &testEchoReply{Message: "early answer", Data: []byte{}}, nil
					}
					return nil, errors.New("no request")
				}),
				Unary("echo", func(_ context.Context, req *testEchoRequest) (*testEchoReply, error) {
					return &testEchoReply{Message: req.Message, Data: []byte{}}, nil
				}),
			)
			f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()
			reqs := func(yield func(*testEchoRequest) bool) {
				defer close(produced)
				if !yield(&testEchoRequest{Message: "first", Data: []byte{}}) {
					return
				}
				close(parked)
				<-release // deliberately does not observe context cancellation
				if reject {
					panic("late source failure")
				}
				late <- yield(&testEchoRequest{Message: "late", Data: []byte{}})
			}
			type outcome struct {
				reply *testEchoReply
				err   error
			}
			result := make(chan outcome, 1)
			go func() {
				reply, err := CallClientStream[testEchoRequest, testEchoReply](ctx, f.conn, "Lifecycle", "upload", reqs)
				result <- outcome{reply, err}
			}()
			defer func() {
				unpark()
				select {
				case <-produced:
				case <-time.After(3 * time.Second):
					t.Error("source teardown hung")
				}
			}()
			select {
			case <-delivered:
			case <-time.After(3 * time.Second):
				t.Fatal("server did not receive first request")
			}
			select {
			case <-parked:
			case <-time.After(3 * time.Second):
				t.Fatal("producer did not park")
			}
			deadline := time.Now().Add(3 * time.Second)
			for f.srv.LiveCalls() != 0 && time.Now().Before(deadline) {
				time.Sleep(5 * time.Millisecond)
			}
			if n := f.srv.LiveCalls(); n != 0 {
				t.Fatalf("native terminal not emitted: liveCalls=%d", n)
			}
			select {
			case got := <-result:
				if reject {
					var rpcErr *RpcError
					if !errors.As(got.err, &rpcErr) || rpcErr.Code != StatusInvalidArgument {
						t.Fatalf("early status: %v", got.err)
					}
				} else if got.err != nil || got.reply.Message != "early answer" {
					t.Fatalf("early answer: %+v %v", got.reply, got.err)
				}
			case <-time.After(2 * time.Second):
				t.Fatal("public client-stream call stayed pending after native server terminal while producer parked")
			}
			unpark()
			select {
			case <-produced:
			case <-time.After(3 * time.Second):
				t.Fatal("producer did not leave after release")
			}
			if !reject {
				if continued := <-late; continued {
					t.Fatal("SDK accepted a late request after terminal")
				}
			}
			reply, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Lifecycle", "echo", &testEchoRequest{Message: "after", Data: []byte{}})
			if err != nil || reply.Message != "after" {
				t.Fatalf("followup: %+v %v", reply, err)
			}
		})
	}
}

type lifecycleEncodeFailure struct {
	Codec
	err error
}

func (c lifecycleEncodeFailure) Encode(any) ([]byte, error) { return nil, c.err }

func TestRPC_ClientStreamLocalFailures(t *testing.T) {
	for _, phase := range []string{"panic", "encode", "nil-encode"} {
		t.Run(phase, func(t *testing.T) {
			f := newRPCFixture(t, ServerOptions{Services: []*Service{interopService()}})
			ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
			defer cancel()
			original := f.conn.codec
			failure := errors.New("local encoding failure")
			if phase != "panic" {
				f.conn.codec = lifecycleEncodeFailure{original, failure}
			}
			reqs := func(yield func(*testEchoRequest) bool) {
				if phase == "panic" {
					panic(failure)
				}
				if phase == "nil-encode" {
					// A yielded nil still goes through the codec; it is not
					// an iterator that produced no requests.
					yield(nil)
					return
				}
				yield(&testEchoRequest{Data: []byte{}})
			}
			if phase == "panic" {
				panicked := false
				func() {
					defer func() {
						if p := recover(); p != nil {
							panicked = true
							if p != failure {
								t.Errorf("panic changed: %v", p)
							}
						}
					}()
					_, _ = CallClientStream[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "ingest", reqs)
				}()
				if !panicked {
					t.Fatal("source panic did not propagate on caller")
				}
			} else {
				if _, err := CallClientStream[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "ingest", reqs); !errors.Is(err, failure) {
					t.Fatalf("local error lost: %v", err)
				}
			}
			f.conn.codec = original
			if n := f.srv.LiveCalls(); n != 0 {
				t.Fatalf("live calls after local failure: %d", n)
			}
			if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "echo", &testEchoRequest{Data: []byte{}}); err != nil {
				t.Fatalf("followup: %v", err)
			}
		})
	}
}

func TestRPC_ClientStreamEmptyValidationAndRecovery(t *testing.T) {
	var dispatched atomic.Int32
	svc := interopService()
	svc.addMethod(ClientStream("emptyguard", func(_ context.Context, requests iter.Seq2[*testEchoRequest, error]) (*testEchoReply, error) {
		dispatched.Add(1)
		var count int32
		for _, err := range requests {
			if err != nil {
				return nil, err
			}
			count++
		}
		return &testEchoReply{Count: count, Data: []byte{}}, nil
	}))
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	empty := func(func(*testEchoRequest) bool) {}
	if reply, err := CallClientStream[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "emptyguard", empty); reply != nil || !errors.Is(err, ErrRpcInvalidArgument) {
		t.Fatalf("empty request validation: reply=%+v error=%v", reply, err)
	}
	if dispatched.Load() != 0 || f.srv.LiveCalls() != 0 {
		t.Fatalf("empty request dispatched=%d live=%d", dispatched.Load(), f.srv.LiveCalls())
	}
	// An item with zero-valued fields is a request, not an empty source.
	one := func(yield func(*testEchoRequest) bool) { yield(&testEchoRequest{Data: []byte{}}) }
	if reply, err := CallClientStream[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "emptyguard", one); err != nil || reply.Count != 1 {
		t.Fatalf("nonempty followup: reply=%+v error=%v", reply, err)
	}
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "echo", &testEchoRequest{Data: []byte{}}); err != nil {
		t.Fatalf("healthy unary followup: %v", err)
	}
}

func TestRPC_ClientStreamEmptyContextPriority(t *testing.T) {
	f := newRPCFixture(t, ServerOptions{Services: []*Service{interopService()}})
	for _, deadline := range []bool{false, true} {
		t.Run(map[bool]string{false: "cancel", true: "deadline"}[deadline], func(t *testing.T) {
			var ctx context.Context
			var cancel context.CancelFunc
			if deadline {
				ctx, cancel = context.WithTimeout(context.Background(), 200*time.Millisecond)
			} else {
				ctx, cancel = context.WithCancel(context.Background())
			}
			defer cancel()
			source := func(func(*testEchoRequest) bool) {
				if !deadline {
					cancel()
				}
				<-ctx.Done()
			}
			_, err := CallClientStream[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "ingest", source)
			want := ErrRpcCancelled
			if deadline {
				want = ErrRpcDeadlineExceeded
			}
			if !errors.Is(err, want) || errors.Is(err, ErrRpcInvalidArgument) {
				t.Fatalf("context must precede empty validation: %v", err)
			}
		})
	}
}

func TestRPC_BidiEmptyAndRepeatedFinish(t *testing.T) {
	var dispatched atomic.Int32
	svc := interopService()
	svc.addMethod(BidiStream("chat", func(_ context.Context, requests iter.Seq2[*testEchoRequest, error], send func(*testEchoReply) error) error {
		dispatched.Add(1)
		for req, err := range requests {
			if err != nil {
				return err
			}
			if err := send(&testEchoReply{Message: req.Message, Data: []byte{}}); err != nil {
				return err
			}
		}
		return nil
	}))
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	empty, err := OpenBidiStream[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "chat")
	if err != nil {
		t.Fatal(err)
	}
	defer empty.Close()
	if err := empty.CloseSend(); err != nil {
		t.Fatalf("empty finish: %v", err)
	}
	if err := empty.CloseSend(); err != nil {
		t.Fatalf("repeated empty finish: %v", err)
	}
	if _, err := empty.Recv(); !errors.Is(err, ErrRpcInvalidArgument) {
		t.Fatalf("empty response half: %v", err)
	}
	if dispatched.Load() != 0 {
		t.Fatal("empty stream reached handler")
	}
	call, err := OpenBidiStream[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "chat")
	if err != nil {
		t.Fatal(err)
	}
	defer call.Close()
	if err := call.Send(&testEchoRequest{Message: "one", Data: []byte{}}); err != nil {
		t.Fatal(err)
	}
	if err := call.CloseSend(); err != nil {
		t.Fatal(err)
	}
	if err := call.CloseSend(); err != nil {
		t.Fatalf("repeated finish: %v", err)
	}
	reply, err := call.Recv()
	if err != nil || reply.Message != "one" {
		t.Fatalf("reply: %+v %v", reply, err)
	}
	if _, err := call.Recv(); err != io.EOF {
		t.Fatalf("trailer: %v", err)
	}
	if dispatched.Load() != 1 {
		t.Fatalf("handler count: %d", dispatched.Load())
	}
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "echo", &testEchoRequest{Data: []byte{}}); err != nil {
		t.Fatal(err)
	}
}

func TestRPC_ClientStreamCancellationWithParkedProducer(t *testing.T) {
	for _, deadline := range []bool{false, true} {
		t.Run(map[bool]string{false: "cancel", true: "deadline"}[deadline], func(t *testing.T) {
			parked, release, produced, delivered, cancelled := make(chan struct{}), make(chan struct{}), make(chan struct{}), make(chan struct{}), make(chan struct{})
			svc := NewService("CancelParked", 1, ClientStream("upload", func(ctx context.Context, reqs iter.Seq2[*testEchoRequest, error]) (*testEchoReply, error) {
				for _, err := range reqs {
					if err != nil {
						return nil, err
					}
					close(delivered)
					<-ctx.Done()
					close(cancelled)
					return nil, ctx.Err()
				}
				return nil, errors.New("no first request")
			}))
			f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
			ctx, cancel := context.WithCancel(context.Background())
			if deadline {
				cancel()
				ctx, cancel = context.WithTimeout(context.Background(), 500*time.Millisecond)
			}
			defer cancel()
			reqs := func(yield func(*testEchoRequest) bool) {
				defer close(produced)
				if !yield(&testEchoRequest{Data: []byte{}}) {
					return
				}
				close(parked)
				<-release
			}
			result := make(chan error, 1)
			go func() {
				_, err := CallClientStream[testEchoRequest, testEchoReply](ctx, f.conn, "CancelParked", "upload", reqs)
				result <- err
			}()
			defer func() {
				close(release)
				select {
				case <-produced:
				case <-time.After(3 * time.Second):
					t.Error("source did not exit")
				}
			}()
			select {
			case <-delivered:
			case <-time.After(3 * time.Second):
				t.Fatal("first request not delivered")
			}
			select {
			case <-parked:
			case <-time.After(3 * time.Second):
				t.Fatal("source not parked")
			}
			if !deadline {
				cancel()
			}
			select {
			case err := <-result:
				if deadline {
					if !errors.Is(err, context.DeadlineExceeded) || !errors.Is(err, ErrRpcDeadlineExceeded) {
						t.Fatalf("deadline: %v", err)
					}
				} else if !errors.Is(err, context.Canceled) || !errors.Is(err, ErrRpcCancelled) {
					t.Fatalf("cancel: %v", err)
				}
			case <-time.After(3 * time.Second):
				t.Fatal("parked producer blocked cancellation")
			}
			select {
			case <-cancelled:
			case <-time.After(3 * time.Second):
				t.Fatal("handler did not cancel")
			}
			until := time.Now().Add(3 * time.Second)
			for f.srv.LiveCalls() != 0 && time.Now().Before(until) {
				time.Sleep(5 * time.Millisecond)
			}
			if n := f.srv.LiveCalls(); n != 0 {
				t.Fatalf("live calls: %d", n)
			}
		})
	}
}

func TestRPC_BidiAbandonmentWithParkedProducer(t *testing.T) {
	parked, release, produced, cancelled := make(chan struct{}), make(chan struct{}), make(chan struct{}), make(chan struct{})
	var refused atomic.Bool
	svc := NewService("Abandon", 1, BidiStream("chat", func(ctx context.Context, reqs iter.Seq2[*testEchoRequest, error], send func(*testEchoReply) error) error {
		for _, err := range reqs {
			if err != nil {
				return err
			}
			<-parked
			if err := send(&testEchoReply{Message: "first reply", Data: []byte{}}); err != nil {
				return err
			}
			<-ctx.Done()
			close(cancelled)
			return ctx.Err()
		}
		return nil
	}))
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	reqs := func(yield func(*testEchoRequest) bool) {
		defer close(produced)
		if !yield(&testEchoRequest{Data: []byte{}}) {
			return
		}
		close(parked)
		<-release
		refused.Store(!yield(&testEchoRequest{Message: "late", Data: []byte{}}))
	}
	var once sync.Once
	unpark := func() { once.Do(func() { close(release) }) }
	defer unpark()
	for reply, err := range CallBidiStream[testEchoRequest, testEchoReply](ctx, f.conn, "Abandon", "chat", reqs) {
		if err != nil || reply.Message != "first reply" {
			t.Fatalf("reply: %+v %v", reply, err)
		}
		break
	}
	select {
	case <-cancelled:
	case <-time.After(3 * time.Second):
		t.Fatal("handler not cancelled after response abandonment")
	}
	until := time.Now().Add(3 * time.Second)
	for f.srv.LiveCalls() != 0 && time.Now().Before(until) {
		time.Sleep(5 * time.Millisecond)
	}
	if n := f.srv.LiveCalls(); n != 0 {
		t.Fatalf("native live calls after abandonment: %d", n)
	}
	unpark()
	select {
	case <-produced:
	case <-time.After(3 * time.Second):
		t.Fatal("released producer did not exit")
	}
	if !refused.Load() {
		t.Fatal("late bidi request accepted after abandonment")
	}
}

// A released wrapper must refuse late operations before consulting native state.
// Clearing rc makes an accidental native operation deterministic rather than racing a handle.
func TestRPC_ReleasedCallRefusesLateProducerOperations(t *testing.T) {
	f := newRPCFixture(t, ServerOptions{Services: []*Service{interopService()}})
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	c, err := openCall(ctx, f.conn, PatternClientStream, "InteropEcho", "ingest", &callOptions{})
	if err != nil {
		t.Fatal(err)
	}
	c.release()
	c.rc = nil
	if err := c.send([]byte("late"), false); !errors.Is(err, errCallClosed) {
		t.Fatalf("late send: %v", err)
	}
	c.cancel()
	c.release()
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "echo", &testEchoRequest{Data: []byte{}}); err != nil {
		t.Fatalf("followup: %v", err)
	}
}

func TestRPC_FlowControlledSendCancellationDrainsBeforeRelease(t *testing.T) {
	entered := make(chan struct{})
	svc := NewService("Pressure", 1, ClientStream("upload", func(ctx context.Context, reqs iter.Seq2[*testEchoRequest, error]) (*testEchoReply, error) {
		for _, err := range reqs {
			if err != nil {
				return nil, err
			}
			close(entered)
			<-ctx.Done()
			return nil, ctx.Err()
		}
		return nil, errors.New("no request")
	}))
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc, interopService()}})
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	c, err := openCall(ctx, f.conn, PatternClientStream, "Pressure", "upload", buildOptions(nil))
	if err != nil {
		t.Fatal(err)
	}
	data := make([]byte, 2*1024*1024)
	state := uint32(0x12345678)
	for i := range data {
		state ^= state << 13
		state ^= state >> 17
		state ^= state << 5
		data[i] = byte(state)
	}
	payload, err := f.conn.codec.Encode(&testEchoRequest{Data: data})
	if err != nil {
		c.release()
		t.Fatal(err)
	}
	sent := make(chan error, 1)
	var completed atomic.Int32
	go func() {
		for i := 0; i < 32; i++ {
			if err := c.send(payload, false); err != nil {
				sent <- err
				return
			}
			completed.Add(1)
		}
		sent <- nil
	}()
	read := make(chan error, 1)
	go func() { _, err := c.readSingle("client-stream"); read <- err }()
	select {
	case <-entered:
	case <-time.After(3 * time.Second):
		cancel()
		t.Fatal("handler not entered")
	}
	// Peer deliberately stops reading. Require sustained lack of send progress,
	// with the bounded producer still unfinished, before testing cancellation.
	previous := completed.Load()
	stalled := false
	for i := 0; i < 30; i++ {
		time.Sleep(20 * time.Millisecond)
		now := completed.Load()
		if now == previous && now > 0 {
			stalled = true
			break
		}
		previous = now
	}
	cancel()
	select {
	case err := <-sent:
		if err == nil {
			t.Error("bounded producer finished before backpressure")
		}
	case <-time.After(3 * time.Second):
		t.Fatal("cancel could not wake send")
	}
	select {
	case <-read:
	case <-time.After(3 * time.Second):
		t.Fatal("reader did not drain")
	}
	c.release()
	if !stalled {
		t.Fatal("backpressure not established")
	}
	t.Logf("completed payloads before cancellation: %d (2 MiB each, cap 32)", completed.Load())
	followCtx, followCancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer followCancel()
	if _, err := Call[testEchoRequest, testEchoReply](followCtx, f.conn, "InteropEcho", "echo", &testEchoRequest{Data: []byte{}}); err != nil {
		t.Fatalf("followup: %v", err)
	}
}

func TestRPC_BidiCloseDrainsPendingRecv(t *testing.T) {
	entered := make(chan struct{})
	svc := NewService("CloseRecv", 1, BidiStream("chat", func(ctx context.Context, requests iter.Seq2[*testEchoRequest, error], send func(*testEchoReply) error) error {
		for _, err := range requests {
			if err != nil {
				return err
			}
			close(entered)
			<-ctx.Done()
			return ctx.Err()
		}
		return nil
	}))
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc, interopService()}})
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	b, err := OpenBidiStream[testEchoRequest, testEchoReply](ctx, f.conn, "CloseRecv", "chat")
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	if err := b.Send(&testEchoRequest{Data: []byte{}}); err != nil {
		t.Fatal(err)
	}
	select {
	case <-entered:
	case <-time.After(time.Second):
		t.Fatal("handler not entered")
	}
	received := make(chan error, 1)
	go func() { _, err := b.Recv(); received <- err }()
	select {
	case err := <-received:
		t.Fatalf("recv unexpectedly ended: %v", err)
	case <-time.After(20 * time.Millisecond):
	}
	closed := make(chan struct{})
	go func() { b.Close(); close(closed) }()
	select {
	case <-closed:
	case <-time.After(time.Second):
		t.Fatal("Close did not drain recv")
	}
	select {
	case err := <-received:
		if err == nil {
			t.Fatal("closed recv succeeded")
		}
	case <-time.After(time.Second):
		t.Fatal("recv not joined")
	}
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "echo", &testEchoRequest{Data: []byte{}}); err != nil {
		t.Fatalf("followup: %v", err)
	}
}
