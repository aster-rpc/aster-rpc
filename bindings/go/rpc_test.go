//go:build cgo

package aster

import (
	"context"
	"errors"
	"fmt"
	"iter"
	"math/rand"
	"reflect"
	"slices"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

type testEchoRequest struct {
	Message string `aster:"name=message"`
	Count   int32  `aster:"name=count"`
	Data    []byte `fory:"type=bytes" aster:"name=data"`
}

func (testEchoRequest) AsterWireType() string { return "interop/EchoRequest" }

type testEchoReply struct {
	Message    string `aster:"name=message"`
	Count      int32  `aster:"name=count"`
	Data       []byte `fory:"type=bytes" aster:"name=data"`
	ServerLang string `aster:"name=server_lang"`
}

func (testEchoReply) AsterWireType() string { return "interop/EchoReply" }

// The InteropEcho contract id shared by every binding (tests/interop/).
const interopEchoContractID = "c5aa690b04f415990755ae87a2a608b14f41cf07306117028942b461af081c77"

func interopService() *Service {
	return NewService("InteropEcho", 1,
		Unary("echo", func(_ context.Context, r *testEchoRequest) (*testEchoReply, error) {
			return &testEchoReply{Message: r.Message, Count: r.Count + 1, Data: r.Data, ServerLang: "go"}, nil
		}),
		ServerStream("generate", func(_ context.Context, r *testEchoRequest, send func(*testEchoReply) error) error {
			for i := int32(0); i < r.Count; i++ {
				if err := send(&testEchoReply{Message: r.Message, Count: i, Data: r.Data, ServerLang: "go"}); err != nil {
					return err
				}
			}
			return nil
		}),
		ClientStream("ingest", func(_ context.Context, reqs iter.Seq2[*testEchoRequest, error]) (*testEchoReply, error) {
			var n int32
			last := ""
			for r, err := range reqs {
				if err != nil {
					return nil, err
				}
				n++
				last = r.Message
			}
			return &testEchoReply{Message: last, Count: n, Data: []byte{}, ServerLang: "go"}, nil
		}),
	)
}

func TestContract_InteropEchoID(t *testing.T) {
	id, err := interopService().ContractID()
	if err != nil {
		t.Fatal(err)
	}
	if id != interopEchoContractID {
		t.Fatalf("contract id %s, want %s", id, interopEchoContractID)
	}
}

func TestContract_TypeMappingErrors(t *testing.T) {
	type untaggedBytes struct{ Data []byte }
	if _, err := newTypeResolver().field(reflectField(untaggedBytes{}, "Data")); err == nil || !strings.Contains(err.Error(), "type=bytes") {
		t.Fatalf("untagged []byte: %v", err)
	}
	type noWireType struct{ X int32 }
	if _, err := TypeHash(noWireType{}); err == nil {
		t.Fatal("expected an error for a type without AsterWireType")
	}
}

// testCfg is a loopback, relay-free config.
func testCfg() AsterConfig {
	cfg := DefaultConfig()
	cfg.RelayMode, cfg.BindAddr = "disabled", "127.0.0.1:0"
	return cfg
}

type rpcFixture struct {
	srv  *AsterServer
	cl   *AsterClient
	conn *RpcConnection
}

func newRPCFixture(t *testing.T, opts ServerOptions) *rpcFixture {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	if opts.Config.BindAddr == "" {
		opts.Config = testCfg()
	}
	srv, err := StartServer(ctx, opts)
	if err != nil {
		t.Fatalf("StartServer: %v", err)
	}
	t.Cleanup(func() { _ = srv.Close() })
	ticket, err := srv.Ticket(ctx)
	if err != nil {
		t.Fatal(err)
	}
	cl, err := StartClient(ctx, testCfg())
	if err != nil {
		t.Fatalf("StartClient: %v", err)
	}
	t.Cleanup(func() { _ = cl.Close() })
	conn, err := cl.ConnectTicket(ctx, ticket)
	if err != nil {
		t.Fatalf("connect: %v", err)
	}
	return &rpcFixture{srv: srv, cl: cl, conn: conn}
}

func TestRPC_AllPatterns(t *testing.T) {
	bidi := BidiStream("double", func(ctx context.Context, reqs iter.Seq2[*testEchoRequest, error], send func(*testEchoReply) error) error {
		for r, err := range reqs {
			if err != nil {
				return err
			}
			if err := send(&testEchoReply{Message: r.Message + r.Message, Count: r.Count * 2, Data: []byte{}, ServerLang: "go"}); err != nil {
				return err
			}
		}
		return nil
	})
	svc := interopService()
	svc.addMethod(bidi)
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	// Unary, incl. a payload above the zstd threshold.
	big := strings.Repeat("héllo ", 2000)
	for _, msg := range []string{"hi", big} {
		req := &testEchoRequest{Message: msg, Count: 41, Data: []byte{1, 2, 3}}
		reply, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "echo", req)
		if err != nil {
			t.Fatalf("unary: %v", err)
		}
		if reply.Message != msg || reply.Count != 42 || string(reply.Data) != "\x01\x02\x03" || reply.ServerLang != "go" {
			t.Fatalf("unary reply %+v", reply)
		}
	}

	// Server stream.
	var got []int32
	for r, err := range CallServerStream[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "generate", &testEchoRequest{Message: "s", Count: 5}) {
		if err != nil {
			t.Fatalf("server stream: %v", err)
		}
		got = append(got, r.Count)
	}
	if !slices.Equal(got, []int32{0, 1, 2, 3, 4}) {
		t.Fatalf("server stream %v", got)
	}

	// Client stream, including zero requests.
	reqs := func(n int) iter.Seq[*testEchoRequest] {
		return func(yield func(*testEchoRequest) bool) {
			for i := 0; i < n; i++ {
				if !yield(&testEchoRequest{Message: fmt.Sprintf("m%d", i), Count: int32(i)}) {
					return
				}
			}
		}
	}
	r, err := CallClientStream[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "ingest", reqs(4))
	if err != nil || r.Count != 4 || r.Message != "m3" {
		t.Fatalf("client stream: %v %+v", err, r)
	}
	// Rust's client refuses a client-stream call with no request.
	if _, err = CallClientStream[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "ingest", reqs(0)); !errors.Is(err, ErrRpcInvalidArgument) {
		t.Fatalf("empty client stream: %v", err)
	}

	// Bidi.
	var bidiGot []string
	for r, err := range CallBidiStream[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "double", reqs(3)) {
		if err != nil {
			t.Fatalf("bidi: %v", err)
		}
		bidiGot = append(bidiGot, r.Message)
	}
	if !slices.Equal(bidiGot, []string{"m0m0", "m1m1", "m2m2"}) {
		t.Fatalf("bidi %v", bidiGot)
	}
}

func TestRPC_ErrorsAndTrailers(t *testing.T) {
	var sawMeta atomic.Value
	svc := NewService("Errs", 1,
		Unary("fail", func(_ context.Context, r *testEchoRequest) (*testEchoReply, error) {
			return nil, &RpcError{Code: StatusNotFound, Message: "no " + r.Message, Details: map[string]string{"k": "v"}}
		}),
		Unary("plain", func(context.Context, *testEchoRequest) (*testEchoReply, error) {
			return nil, errors.New("boom")
		}),
		Unary("panic", func(context.Context, *testEchoRequest) (*testEchoReply, error) {
			panic("kaboom")
		}),
		Unary("slow", func(ctx context.Context, _ *testEchoRequest) (*testEchoReply, error) {
			info, _ := CallInfoFromContext(ctx)
			sawMeta.Store(info.Metadata)
			<-ctx.Done()
			return nil, ctx.Err()
		}),
		ServerStream("streamfail", func(_ context.Context, _ *testEchoRequest, send func(*testEchoReply) error) error {
			if err := send(&testEchoReply{Message: "first", Data: []byte{}}); err != nil {
				return err
			}
			return &RpcError{Code: StatusAborted, Message: "mid-stream"}
		}),
	)
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	req := &testEchoRequest{Message: "x", Data: []byte{}}

	_, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Errs", "fail", req)
	var rpcErr *RpcError
	if !errors.As(err, &rpcErr) || rpcErr.Code != StatusNotFound || rpcErr.Message != "no x" || rpcErr.Details["k"] != "v" {
		t.Fatalf("fail: %v", err)
	}
	if !errors.Is(err, ErrRpcNotFound) {
		t.Fatalf("errors.Is ErrRpcNotFound: %v", err)
	}
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Errs", "plain", req); !errors.Is(err, ErrRpcInternal) {
		t.Fatalf("plain: %v", err)
	}
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Errs", "panic", req); !errors.Is(err, ErrRpcInternal) {
		t.Fatalf("panic: %v", err)
	}
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Errs", "nope", req); !errors.Is(err, ErrRpcUnimplemented) {
		t.Fatalf("unknown method: %v", err)
	}
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Nope", "x", req); !errors.Is(err, ErrRpcNotFound) {
		t.Fatalf("unknown service: %v", err)
	}

	// The deadline travels in the header (whole seconds); the server ends the
	// call with DEADLINE_EXCEEDED.
	// short-wait-ok: the slow call must fail on this deadline; its expiry is under test
	dctx, dcancel := context.WithTimeout(ctx, 1500*time.Millisecond)
	_, err = Call[testEchoRequest, testEchoReply](dctx, f.conn, "Errs", "slow", req, WithMetadata("trace", "abc"))
	dcancel()
	if err == nil {
		t.Fatal("slow: expected an error")
	}
	if md, _ := sawMeta.Load().([]MetadataEntry); len(md) != 1 || md[0] != (MetadataEntry{"trace", "abc"}) {
		t.Fatalf("metadata %v", md)
	}
	// Server-side deadline (2 s on the wire) with a client that keeps waiting.
	start := time.Now()
	// short-wait-ok: the slow call must fail on this deadline; its expiry is under test
	sctx, scancel := context.WithTimeout(ctx, 1900*time.Millisecond)
	defer scancel()
	errCh := make(chan error, 1)
	go func() {
		_, err := Call[testEchoRequest, testEchoReply](sctx, f.conn, "Errs", "slow", req)
		errCh <- err
	}()
	if err := <-errCh; err == nil || time.Since(start) > 10*time.Second {
		t.Fatalf("slow (deadline): %v", err)
	}

	var items int
	var streamErr error
	for _, err := range CallServerStream[testEchoRequest, testEchoReply](ctx, f.conn, "Errs", "streamfail", req) {
		if err != nil {
			streamErr = err
			break
		}
		items++
	}
	if items != 1 || !errors.Is(streamErr, ErrRpcAborted) {
		t.Fatalf("streamfail: %d items, %v", items, streamErr)
	}
}

// Rust enforces the method's default timeout: the client (no deadline of
// its own) gets DEADLINE_EXCEEDED and the handler's context ends.
func TestRPC_DeadlineExceededTrailer(t *testing.T) {
	handlerErr := make(chan error, 1)
	svc := NewService("D", 1, Unary("wait", func(ctx context.Context, _ *testEchoRequest) (*testEchoReply, error) {
		<-ctx.Done()
		handlerErr <- ctx.Err()
		return nil, ctx.Err()
	}, Timeout(500*time.Millisecond)))
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
	start := time.Now()
	_, err := Call[testEchoRequest, testEchoReply](context.Background(), f.conn, "D", "wait", &testEchoRequest{Data: []byte{}})
	if !errors.Is(err, ErrRpcDeadlineExceeded) {
		t.Fatalf("got %v, want DEADLINE_EXCEEDED", err)
	}
	if d := time.Since(start); d < 400*time.Millisecond || d > 10*time.Second {
		t.Fatalf("took %v", d)
	}
	if err := <-handlerErr; !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("handler ctx: %v", err)
	}
	// A client deadline is enforced natively and matches both sentinels.
	// short-wait-ok: the client deadline's DEADLINE_EXCEEDED is the behaviour under test
	ctx, cancel := context.WithTimeout(context.Background(), 300*time.Millisecond)
	defer cancel()
	_, err = Call[testEchoRequest, testEchoReply](ctx, f.conn, "D", "wait", &testEchoRequest{Data: []byte{}})
	if !errors.Is(err, ErrRpcDeadlineExceeded) || !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("client deadline: %v", err)
	}
}

func TestContract_DynamicJSONMatchesServiceContract(t *testing.T) {
	svc := NewService("Guarded", 2,
		Unary("a", func(context.Context, *testEchoRequest) (*testEchoReply, error) { return nil, nil },
			Requires(RequireAnyOf("admin", "ops")), Timeout(1500*time.Millisecond), Idempotent(), MaxFrameSize(4096)),
		ServerStream("b", func(context.Context, *testEchoRequest, func(*testEchoReply) error) error { return nil }),
	).WithRequires(RequireRole("user"))
	dyn, err := svc.ContractID()
	if err != nil {
		t.Fatal(err)
	}
	js, err := ServiceContractJSON(svc)
	if err != nil {
		t.Fatal(err)
	}
	legacy, err := ComputeContractID(js)
	if err != nil {
		t.Fatal(err)
	}
	if dyn != string(legacy) {
		t.Fatalf("dynamic %s != contract %s", dyn, legacy)
	}
	sess := NewSessionService("S", 1, func(SessionInfo) any { return nil },
		Unary("x", func(context.Context, *testEchoRequest) (*testEchoReply, error) { return nil, nil }))
	dyn, _ = sess.ContractID()
	js, _ = ServiceContractJSON(sess)
	legacy, _ = ComputeContractID(js)
	if dyn != string(legacy) {
		t.Fatalf("session: dynamic %s != contract %s", dyn, legacy)
	}
}

type counter struct {
	n      int32
	closed chan struct{}
}

func (c *counter) Close() error { c.closed <- struct{}{}; return nil }

func TestRPC_SessionScoped(t *testing.T) {
	closed := make(chan struct{}, 2)
	var made atomic.Int32
	svc := NewSessionService("Counter", 1, func(info SessionInfo) any {
		made.Add(1)
		if info.SessionID == 0 || info.PeerID == "" {
			t.Errorf("session info %+v", info)
		}
		return &counter{closed: closed}
	},
		Unary("add", func(ctx context.Context, r *testEchoRequest) (*testEchoReply, error) {
			c, ok := Session[*counter](ctx)
			if !ok {
				return nil, errors.New("no session")
			}
			c.n += r.Count
			return &testEchoReply{Count: c.n, Data: []byte{}}, nil
		}),
	)
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc, interopService()}})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	add := func(sid uint32, n int32) (int32, error) {
		r, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Counter", "add", &testEchoRequest{Count: n, Data: []byte{}}, WithSession(sid))
		if err != nil {
			return 0, err
		}
		return r.Count, nil
	}
	s1, err := f.conn.NewSessionID()
	if err != nil {
		t.Fatal(err)
	}
	s2, _ := f.conn.NewSessionID()
	if s1 == 0 || s2 == s1 {
		t.Fatalf("session ids %d %d", s1, s2)
	}
	for i, want := range []int32{1, 3, 6} {
		if got, err := add(s1, int32(i+1)); err != nil || got != want {
			t.Fatalf("s1 add: %d %v (want %d)", got, err, want)
		}
	}
	if got, err := add(s2, 10); err != nil || got != 10 {
		t.Fatalf("s2 add: %d %v", got, err)
	}
	if made.Load() != 2 {
		t.Fatalf("%d instances", made.Load())
	}
	// Rust's session rules: a session service without a session, and a
	// shared service with one, are FAILED_PRECONDITION.
	if _, err := add(0, 1); !errors.Is(err, ErrRpcFailedPrecondition) {
		t.Fatalf("no session: %v", err)
	}
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "echo", &testEchoRequest{Data: []byte{}}, WithSession(s1)); !errors.Is(err, ErrRpcFailedPrecondition) {
		t.Fatalf("shared with session: %v", err)
	}
	// Closing the connection reaps the sessions: instances are closed.
	_ = f.conn.Close()
	select {
	case <-closed:
	case <-time.After(10 * time.Second):
		t.Fatal("session instance not closed after the connection closed")
	}
}

func TestRPC_RequiresAndAttributes(t *testing.T) {
	seen := make(chan []MetadataEntry, 1)
	svc := NewService("Admin", 1,
		Unary("op", func(ctx context.Context, _ *testEchoRequest) (*testEchoReply, error) {
			info, _ := CallInfoFromContext(ctx)
			seen <- info.Attributes
			return &testEchoReply{Data: []byte{}}, nil
		}, Requires(RequireRole("admin"))),
	)
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	req := &testEchoRequest{Data: []byte{}}
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Admin", "op", req); !errors.Is(err, ErrRpcPermissionDenied) {
		t.Fatalf("without role: %v", err)
	}
	select {
	case <-seen:
		t.Fatal("denied call invoked handler")
	default:
	}
	peer, _ := f.cl.Node().NodeID()
	if err := f.srv.SetPeerAttributes(peer, map[string]string{"aster.role": "admin", "team": "x"}); err != nil {
		t.Fatal(err)
	}
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Admin", "op", req); err != nil {
		t.Fatalf("with role: %v", err)
	}
	if attrs := <-seen; len(attrs) != 2 || attrs[0].Key != "aster.role" {
		t.Fatalf("attributes %v", attrs)
	}
}

func TestRPC_CancelAndBidiPingPong(t *testing.T) {
	handlerDone := make(chan error, 1)
	svc := NewService("Chat", 1,
		BidiStream("echo", func(ctx context.Context, reqs iter.Seq2[*testEchoRequest, error], send func(*testEchoReply) error) error {
			for r, err := range reqs {
				if err != nil {
					return err
				}
				if err := send(&testEchoReply{Message: r.Message, Data: []byte{}}); err != nil {
					return err
				}
			}
			return nil
		}),
		ServerStream("forever", func(ctx context.Context, _ *testEchoRequest, send func(*testEchoReply) error) error {
			<-ctx.Done()
			handlerDone <- ctx.Err()
			return ctx.Err()
		}),
	)
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	// Ping-pong through the iterator API: the next request is produced only
	// after the previous reply arrived (requests are sent as yielded).
	replies := make(chan string, 1)
	reqs := func(yield func(*testEchoRequest) bool) {
		for i := 0; i < 3; i++ {
			if !yield(&testEchoRequest{Message: fmt.Sprintf("p%d", i), Data: []byte{}}) {
				return
			}
			select {
			case <-replies:
			case <-ctx.Done():
				return
			}
		}
	}
	var got []string
	for r, err := range CallBidiStream[testEchoRequest, testEchoReply](ctx, f.conn, "Chat", "echo", reqs) {
		if err != nil {
			t.Fatalf("bidi: %v", err)
		}
		got = append(got, r.Message)
		replies <- r.Message
	}
	if !slices.Equal(got, []string{"p0", "p1", "p2"}) {
		t.Fatalf("bidi %v", got)
	}

	// Cancelling the client context cancels the call natively: the server
	// handler's context ends (peer cancel) and the client sees CANCELLED.
	cctx, ccancel := context.WithCancel(ctx)
	time.AfterFunc(300*time.Millisecond, ccancel)
	var streamErr error
	for _, err := range CallServerStream[testEchoRequest, testEchoReply](cctx, f.conn, "Chat", "forever", &testEchoRequest{Data: []byte{}}) {
		streamErr = err
	}
	if !errors.Is(streamErr, context.Canceled) {
		t.Fatalf("client: %v", streamErr)
	}
	select {
	case err := <-handlerDone:
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("handler ctx: %v", err)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("server handler not cancelled")
	}
	deadline := time.Now().Add(waitBudget())
	for f.srv.LiveCalls() != 0 && time.Now().Before(deadline) {
		time.Sleep(20 * time.Millisecond)
	}
	if n := f.srv.LiveCalls(); n != 0 {
		t.Fatalf("%d live calls left", n)
	}
}

func TestRPC_FrameLimitAndConnectionEvents(t *testing.T) {
	closedCh := make(chan string, 1)
	nonRPC := make(chan string, 1)
	svc := NewService("Small", 1, Unary("put", func(context.Context, *testEchoRequest) (*testEchoReply, error) {
		return &testEchoReply{Data: []byte{}}, nil
	}, MaxFrameSize(1024)))
	cfg := testCfg()
	cfg.ALPNs = []string{customALPN}
	f := newRPCFixture(t, ServerOptions{
		Config:             cfg,
		Services:           []*Service{svc},
		OnConnectionClosed: func(_ uint64, peer string) { closedCh <- peer },
		OnNonRPC: func(alpn string, conn *Connection) {
			nonRPC <- alpn
			_ = conn.Close("")
		},
	})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Small", "put", &testEchoRequest{Data: []byte{1}}); err != nil {
		t.Fatalf("small: %v", err)
	}
	big := make([]byte, 64<<10)
	rand.New(rand.NewSource(1)).Read(big)
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Small", "put", &testEchoRequest{Data: big}); err == nil {
		t.Fatal("expected the frame limit to reject a 64 KiB request")
	}
	// A non-RPC ALPN connection is handed over as NON_RPC.
	addr, err := f.srv.Node().NodeAddr()
	if err != nil {
		t.Fatal(err)
	}
	if conn, err := f.cl.Node().Connect(ctx, addr, customALPN); err == nil {
		select {
		case alpn := <-nonRPC:
			if alpn != customALPN {
				t.Fatalf("alpn %q", alpn)
			}
		case <-time.After(10 * time.Second):
			t.Fatal("no NON_RPC event")
		}
		_ = conn.Close("")
	} else {
		t.Fatalf("connect custom alpn: %v", err)
	}
	peer, _ := f.cl.Node().NodeID()
	_ = f.conn.Close()
	select {
	case p := <-closedCh:
		if p != peer {
			t.Fatalf("closed peer %q != %q", p, peer)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("no CONNECTION_CLOSED")
	}
}

type denyInterceptor struct{ BaseInterceptor }

func (denyInterceptor) OnRequest(ctx *CallContext, req any) (any, error) {
	if ctx.Metadata["token"] != "ok" {
		return nil, &RpcError{Code: StatusPermissionDenied, Message: "bad token"}
	}
	return req, nil
}

func TestRPC_Interceptor(t *testing.T) {
	f := newRPCFixture(t, ServerOptions{Services: []*Service{interopService()}, Interceptors: []Interceptor{denyInterceptor{}}})
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	req := &testEchoRequest{Message: "x", Data: []byte{}}
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "echo", req); !errors.Is(err, ErrRpcPermissionDenied) {
		t.Fatalf("no token: %v", err)
	}
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "echo", req, WithMetadata("token", "ok")); err != nil {
		t.Fatalf("token: %v", err)
	}
}

// customALPN is an application ALPN a server lists in Config.ALPNs.
const customALPN = "example.custom/1"

// An explicit Gate (the escape hatch) admits only the peers it names; the
// default server without enrollment is open.
func TestRPC_ProtectedGateRejectsUnadmitted(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	gate, err := NewGate(nil)
	if err != nil {
		t.Fatal(err)
	}
	defer gate.Close()
	srv, err := StartServer(ctx, ServerOptions{Config: testCfg(), Gate: gate, Services: []*Service{interopService()}})
	if err != nil {
		t.Fatal(err)
	}
	defer srv.Close()
	ticket, err := srv.Ticket(ctx)
	if err != nil {
		t.Fatal(err)
	}
	cl, err := StartClient(ctx, testCfg())
	if err != nil {
		t.Fatal(err)
	}
	defer cl.Close()
	req := &testEchoRequest{Message: "x", Data: []byte{}}
	call := func() error {
		cctx, ccancel := context.WithTimeout(ctx, 10*time.Second)
		defer ccancel()
		conn, err := cl.ConnectTicket(cctx, ticket)
		if err != nil {
			return err
		}
		_, err = Call[testEchoRequest, testEchoReply](cctx, conn, "InteropEcho", "echo", req)
		if err != nil {
			_ = conn.Close()
			cl.mu.Lock()
			clear(cl.conns)
			cl.mu.Unlock()
		}
		return err
	}
	if err := call(); err == nil {
		t.Fatal("expected the protected gate to reject an un-admitted peer")
	}
	id, _ := cl.Node().NodeID()
	if err := srv.Gate().Admit(id); err != nil {
		t.Fatal(err)
	}
	if ok, err := srv.Gate().IsAdmitted(id); err != nil || !ok {
		t.Fatalf("IsAdmitted: %v %v", ok, err)
	}
	if err := call(); err != nil {
		t.Fatalf("admitted peer: %v", err)
	}
}

func TestRPC_ConcurrentUnary(t *testing.T) {
	f := newRPCFixture(t, ServerOptions{Services: []*Service{interopService()}})
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	errs := make(chan error, 64)
	for i := 0; i < 64; i++ {
		go func(i int32) {
			r, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "echo", &testEchoRequest{Message: "c", Count: i, Data: []byte{}})
			if err == nil && r.Count != i+1 {
				err = fmt.Errorf("count %d != %d", r.Count, i+1)
			}
			errs <- err
		}(int32(i))
	}
	for i := 0; i < 64; i++ {
		if err := <-errs; err != nil {
			t.Fatal(err)
		}
	}
}

func reflectField(v any, name string) reflect.StructField {
	f, _ := reflect.TypeOf(v).FieldByName(name)
	return f
}

// The codec pools Fory instances: concurrent encode/decode with types
// registered lazily while instances are in use.
func TestForyCodec_ConcurrentPool(t *testing.T) {
	c := NewForyCodec()
	errs := make(chan error, 32)
	for g := 0; g < 32; g++ {
		go func(g int) {
			for i := 0; i < 200; i++ {
				var err error
				if (g+i)%2 == 0 {
					var b []byte
					if b, err = c.Encode(&testEchoRequest{Message: fmt.Sprint(i), Count: int32(i), Data: []byte{byte(i)}}); err == nil {
						var r testEchoRequest
						if err = c.Decode(b, &r); err == nil && (r.Count != int32(i) || r.Message != fmt.Sprint(i)) {
							err = fmt.Errorf("round trip %+v", r)
						}
					}
				} else {
					var b []byte
					if b, err = c.Encode(&testEchoReply{ServerLang: "go", Count: int32(g), Data: []byte{}}); err == nil {
						var r testEchoReply
						if err = c.Decode(b, &r); err == nil && (r.Count != int32(g) || r.ServerLang != "go") {
							err = fmt.Errorf("round trip %+v", r)
						}
					}
				}
				if err != nil {
					errs <- err
					return
				}
			}
			errs <- nil
		}(g)
	}
	for g := 0; g < 32; g++ {
		if err := <-errs; err != nil {
			t.Fatal(err)
		}
	}
}
