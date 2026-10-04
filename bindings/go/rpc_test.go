//go:build cgo

package aster

import (
	"context"
	"errors"
	"fmt"
	"iter"
	"reflect"
	"slices"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

type testEchoRequest struct {
	Message string
	Count   int32
	Data    []byte `fory:"type=bytes"`
}

func (testEchoRequest) AsterWireType() string { return "interop/EchoRequest" }

type testEchoReply struct {
	Message    string
	Count      int32
	Data       []byte `fory:"type=bytes"`
	ServerLang string
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
		opts.Config.AllowAllConsumers = true
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
	svc.methods[bidi.Name] = bidi
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
	r, err = CallClientStream[testEchoRequest, testEchoReply](ctx, f.conn, "InteropEcho", "ingest", reqs(0))
	if err != nil || r.Count != 0 {
		t.Fatalf("empty client stream: %v %+v", err, r)
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
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Nope", "x", req); !errors.Is(err, ErrRpcUnimplemented) {
		t.Fatalf("unknown service: %v", err)
	}

	// The deadline travels in the header (whole seconds); the server ends the
	// call with DEADLINE_EXCEEDED.
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

func TestRPC_DeadlineExceededTrailer(t *testing.T) {
	svc := NewService("D", 1, Unary("wait", func(ctx context.Context, _ *testEchoRequest) (*testEchoReply, error) {
		<-ctx.Done()
		return nil, ctx.Err()
	}))
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
	// Hand-built header with a 1 s deadline; wait for the trailer without a
	// client-side context deadline.
	h, _ := EncodeStreamHeader(StreamHeader{Service: "D", Method: "wait", Version: 1, Deadline: 1})
	codec := NewForyCodec()
	payload, _ := codec.Encode(&testEchoRequest{Data: []byte{}})
	body, _ := EncodePayloadFrame(payload, FlagEndStream)
	pair := append(EncodeFrame(h, FlagHeader), body...)
	_, _, _, trailer, err := f.conn.unaryNative(pair, 0)
	if err != nil {
		t.Fatal(err)
	}
	st, err := DecodeRpcStatus(trailer)
	if err != nil || st.Code != StatusDeadlineExceeded {
		t.Fatalf("trailer %+v %v", st, err)
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

func TestRPC_ProtectedGateRejectsUnadmitted(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	srv, err := StartServer(ctx, ServerOptions{Config: testCfg(), Services: []*Service{interopService()}})
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
