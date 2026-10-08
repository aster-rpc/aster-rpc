//go:build cgo

package aster

import (
	"context"
	"errors"
	"sync/atomic"
	"testing"
	"time"
)

type frameBoundaryCodec struct {
	Codec
	payload []byte
}

func (c frameBoundaryCodec) Encode(any) ([]byte, error) { return c.payload, nil }

func TestRPC_ConfiguredFrameBoundaryRefusesOneByteOverAndRecovers(t *testing.T) {
	var invoked atomic.Int32
	service := NewService("FrameBoundary", 1, Unary("put", func(_ context.Context, request *testEchoRequest) (*testEchoReply, error) {
		invoked.Add(1)
		return &testEchoReply{Count: int32(len(request.Data))}, nil
	}, MaxFrameSize(1024)))
	f := newRPCFixture(t, ServerOptions{Services: []*Service{service}})
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	base := f.conn.codec
	defer func() { f.conn.codec = base }()
	payloads := map[int][]byte{}
	// Real Fory payloads: the native frame adds one flag byte, below compression threshold.
	for n := 850; n < 1100; n++ {
		payload, err := base.Encode(&testEchoRequest{Data: make([]byte, n)})
		if err != nil {
			t.Fatal(err)
		}
		if len(payload) == 1023 || len(payload) == 1024 {
			payloads[len(payload)] = payload
		}
	}
	for _, n := range []int{1023, 1024} {
		if payloads[n] == nil {
			t.Fatalf("no encoded payload of length %d", n)
		}
	}
	f.conn.codec = frameBoundaryCodec{base, payloads[1023]}
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "FrameBoundary", "put", &testEchoRequest{}); err != nil {
		t.Fatalf("at cap: %v", err)
	}
	if invoked.Load() != 1 {
		t.Fatal("at-cap handler not invoked exactly once")
	}
	f.conn.codec = frameBoundaryCodec{base, payloads[1024]}
	_, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "FrameBoundary", "put", &testEchoRequest{})
	if err == nil {
		t.Fatal("one byte over returned success")
	}
	var rpcErr *RpcError
	if !errors.As(err, &rpcErr) || rpcErr.Code == StatusOK {
		t.Fatalf("missing public non-OK RPC mapping: %v", err)
	}
	t.Logf("over-cap public status=%v message=%q", rpcErr.Code, rpcErr.Message)
	if invoked.Load() != 1 {
		t.Fatal("over-cap request reached handler")
	}
	if f.srv.LiveCalls() != 0 {
		t.Fatal("over-cap left native calls")
	}
	f.conn.codec = base
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "FrameBoundary", "put", &testEchoRequest{}); err != nil {
		t.Fatalf("recovery: %v", err)
	}
	if invoked.Load() != 2 {
		t.Fatal("recovery handler not invoked")
	}
}
