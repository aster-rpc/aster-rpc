//go:build cgo

// AsterServer echo example: a typed service served through Rust dynamic
// dispatch (ABI 3.5), called over the pooled RPC client.
package main

import (
	"context"
	"fmt"
	"iter"
	"os"
	"time"

	aster "sdk.getaster.now/go"
)

type EchoRequest struct {
	Message string
}

func (EchoRequest) AsterWireType() string { return "example/EchoRequest" }

type EchoReply struct {
	Message string
	Seq     int32
}

func (EchoReply) AsterWireType() string { return "example/EchoReply" }

func fail(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "   FAIL: "+format+"\n", args...)
	os.Exit(1)
}

func main() {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	fmt.Println("=== AsterServer Echo Example ===")

	svc := aster.NewService("EchoService", 1,
		aster.Unary("echo", func(ctx context.Context, r *EchoRequest) (*EchoReply, error) {
			info, _ := aster.CallInfoFromContext(ctx)
			fmt.Printf("   Server: %s from %s...\n", info.Method, info.PeerID[:8])
			return &EchoReply{Message: r.Message}, nil
		}),
		aster.BidiStream("chat", func(_ context.Context, reqs iter.Seq2[*EchoRequest, error], send func(*EchoReply) error) error {
			var n int32
			for r, err := range reqs {
				if err != nil {
					return err
				}
				n++
				if err := send(&EchoReply{Message: r.Message, Seq: n}); err != nil {
					return err
				}
			}
			return nil
		}),
	)

	// 0. Contract id, computed in Rust from the registration JSON.
	fmt.Println("\n0. Computing contract_id via Rust FFI...")
	contractID, err := svc.ContractID()
	if err != nil || len(contractID) != 64 {
		fail("contract id %q: %v", contractID, err)
	}
	fmt.Printf("   contract_id: %s\n   PASS\n", contractID)

	// 1. Start the server (open gate, loopback).
	fmt.Println("\n1. Starting AsterServer...")
	cfg := aster.DefaultConfig()
	cfg.RelayMode, cfg.BindAddr, cfg.AllowAllConsumers = "disabled", "127.0.0.1:0", true
	srv, err := aster.StartServer(ctx, aster.ServerOptions{Config: cfg, Services: []*aster.Service{svc}})
	if err != nil {
		fail("StartServer: %v", err)
	}
	defer srv.Close()
	ticket, err := srv.Ticket(ctx)
	if err != nil {
		fail("ticket: %v", err)
	}
	nodeID, _ := srv.NodeID()
	fmt.Printf("   Server started! Node ID: %s...\n", nodeID[:16])

	// 2. Client.
	fmt.Println("\n2. Connecting a client...")
	ccfg := aster.DefaultConfig()
	ccfg.RelayMode, ccfg.BindAddr = "disabled", "127.0.0.1:0"
	cl, err := aster.StartClient(ctx, ccfg)
	if err != nil {
		fail("StartClient: %v", err)
	}
	defer cl.Close()
	conn, err := cl.ConnectTicket(ctx, ticket)
	if err != nil {
		fail("connect: %v", err)
	}
	fmt.Println("   Connected!")

	// 3. Unary.
	fmt.Println("\n3. Unary echo...")
	reply, err := aster.Call[EchoRequest, EchoReply](ctx, conn, "EchoService", "echo", &EchoRequest{Message: "Hello, Aster!"})
	if err != nil || reply.Message != "Hello, Aster!" {
		fail("echo: %v %+v", err, reply)
	}
	fmt.Printf("   Received: %q\n   PASS\n", reply.Message)

	// 4. Bidi ping-pong: each request waits for the previous reply.
	fmt.Println("\n4. Bidi ping-pong...")
	b, err := aster.OpenBidiStream[EchoRequest, EchoReply](ctx, conn, "EchoService", "chat")
	if err != nil {
		fail("open bidi: %v", err)
	}
	defer b.Close()
	for i, msg := range []string{"ping", "pong", "done"} {
		if err := b.Send(&EchoRequest{Message: msg}); err != nil {
			fail("send: %v", err)
		}
		r, err := b.Recv()
		if err != nil || r.Message != msg || r.Seq != int32(i+1) {
			fail("recv: %v %+v", err, r)
		}
		fmt.Printf("   %s -> %s #%d\n", msg, r.Message, r.Seq)
	}
	if err := b.CloseSend(); err != nil {
		fail("close send: %v", err)
	}
	if _, err := b.Recv(); err == nil {
		fail("expected the end of the stream")
	}
	fmt.Println("   PASS")

	fmt.Println("\n=== SUCCESS: AsterServer echo round-trip works! ===")
}
