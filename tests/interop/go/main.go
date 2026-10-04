// The Go side of the InteropEcho contract (tests/interop/): a leg of the
// tests/interop/run.py matrix (contract / server / client).
//
// Contract (must match tests/interop/rust/contract.rs):
//
//	service InteropEcho v1
//	  echo(interop/EchoRequest) -> interop/EchoReply                (unary)
//	  generate(interop/EchoRequest) -> stream interop/EchoReply     (server stream)
//	  ingest(stream interop/EchoRequest) -> interop/EchoReply       (client stream)
//	interop/EchoRequest { message: string, count: varint32, data: binary }
//	interop/EchoReply   { message: string, count: varint32, data: binary, server_lang: string }
//
// Usage: interop_echo contract | server | client <aster1-ticket>
// Every mode prints CONTRACT_ID <hex> first; `server` prints ADDRESS <aster1...>
// and serves until stdin closes (loopback, relays off); `client` prints
// CASE <name> OK|FAIL lines.
package main

import (
	"bufio"
	"bytes"
	"context"
	"fmt"
	"iter"
	"math"
	"os"
	"strings"
	"time"

	aster "aster-ffi"
)

// Byte fields need `fory:"type=bytes"`: untagged, Fory Go writes a []byte
// field as list<uint8> instead of binary.
type EchoRequest struct {
	Message string
	Count   int32
	Data    []byte `fory:"type=bytes"`
}

func (EchoRequest) AsterWireType() string { return "interop/EchoRequest" }

type EchoReply struct {
	Message    string
	Count      int32
	Data       []byte `fory:"type=bytes"`
	ServerLang string
}

func (EchoReply) AsterWireType() string { return "interop/EchoReply" }

// service has the same semantics in every language.
func service() *aster.Service {
	return aster.NewService("InteropEcho", 1,
		aster.Unary("echo", func(_ context.Context, req *EchoRequest) (*EchoReply, error) {
			return &EchoReply{Message: req.Message, Count: req.Count + 1, Data: req.Data, ServerLang: "go"}, nil
		}),
		aster.ServerStream("generate", func(ctx context.Context, req *EchoRequest, send func(*EchoReply) error) error {
			for i := int32(0); i < max(req.Count, 0); i++ {
				if err := send(&EchoReply{Message: req.Message, Count: i, Data: req.Data, ServerLang: "go"}); err != nil {
					return err
				}
			}
			return nil
		}),
		aster.ClientStream("ingest", func(ctx context.Context, reqs iter.Seq2[*EchoRequest, error]) (*EchoReply, error) {
			var n int32
			last := ""
			for r, err := range reqs {
				if err != nil {
					return nil, err
				}
				n++
				last = r.Message
			}
			return &EchoReply{Message: last, Count: n, Data: []byte{}, ServerLang: "go"}, nil
		}),
	)
}

func config() aster.AsterConfig {
	cfg, err := aster.ConfigFromEnv()
	if err != nil {
		fmt.Fprintf(os.Stderr, "config: %v\n", err)
		os.Exit(2)
	}
	cfg.RelayMode, cfg.BindAddr = "disabled", "127.0.0.1:0"
	return cfg
}

func main() {
	svc := service()
	cid, err := svc.ContractID()
	if err != nil {
		fmt.Fprintf(os.Stderr, "contract id: %v\n", err)
		os.Exit(2)
	}
	fmt.Printf("CONTRACT_ID %s\n", cid)

	switch {
	case len(os.Args) == 2 && os.Args[1] == "contract":
	case len(os.Args) == 2 && os.Args[1] == "server":
		if err := serve(svc); err != nil {
			fmt.Fprintf(os.Stderr, "server: %v\n", err)
			os.Exit(1)
		}
	case len(os.Args) == 3 && os.Args[1] == "client":
		if !client(svc, os.Args[2]) {
			os.Exit(1)
		}
	default:
		fmt.Fprintln(os.Stderr, "usage: interop_echo contract | server | client <aster1-ticket>")
		os.Exit(2)
	}
}

func serve(svc *aster.Service) error {
	ctx := context.Background()
	cfg := config()
	cfg.AllowAllConsumers = true
	srv, err := aster.StartServer(ctx, aster.ServerOptions{Config: cfg, Services: []*aster.Service{svc}})
	if err != nil {
		return err
	}
	defer srv.Close()
	ticket, err := srv.Ticket(ctx)
	if err != nil {
		return err
	}
	if addr, err := srv.Node().NodeAddr(); err == nil {
		fmt.Fprintf(os.Stderr, "node addr: %+v\n", *addr)
	}
	fmt.Printf("ADDRESS %s\n", ticket)
	in := bufio.NewScanner(os.Stdin)
	for in.Scan() {
		// serve until stdin closes
	}
	return nil
}

type namedCase struct {
	name string
	req  EchoRequest
}

// cases must match cases() in the other drivers.
func cases() []namedCase {
	data := make([]byte, 20_000)
	for i := range data {
		data[i] = byte(i % 251)
	}
	return []namedCase{
		{"small", EchoRequest{Message: "hello from go", Count: 41, Data: []byte{0, 1, 2, 255}}},
		// > 4096 bytes on the wire and highly compressible: exercises zstd.
		{"zstd", EchoRequest{Message: strings.Repeat("héllo ✓ ", 200), Count: -123_456, Data: data}},
		// A single string field longer than 4095 bytes (Fory string-length edge).
		{"long_string", EchoRequest{Message: strings.Repeat("x", 5000), Count: math.MaxInt32 - 1, Data: []byte{}}},
	}
}

func check(req *EchoRequest, reply *EchoReply) (string, error) {
	switch {
	case reply.Message != req.Message:
		return "", fmt.Errorf("message mismatch (len %d vs %d)", len(reply.Message), len(req.Message))
	case reply.Count != req.Count+1:
		return "", fmt.Errorf("count %d != %d", reply.Count, req.Count+1)
	case !bytes.Equal(reply.Data, req.Data):
		return "", fmt.Errorf("data mismatch (len %d vs %d)", len(reply.Data), len(req.Data))
	case reply.ServerLang == "":
		return "", fmt.Errorf("empty server_lang")
	}
	return reply.ServerLang, nil
}

func runCase(name string, body func(ctx context.Context) (string, error)) bool {
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	lang, err := body(ctx)
	if err != nil {
		fmt.Printf("CASE %s FAIL %T: %v\n", name, err, err)
		return false
	}
	fmt.Printf("CASE %s OK server_lang=%s\n", name, lang)
	return true
}

func client(svc *aster.Service, ticket string) bool {
	ctx := context.Background()
	cl, err := aster.StartClient(ctx, config())
	if err != nil {
		fmt.Printf("CASE connect FAIL %T: %v\n", err, err)
		return false
	}
	defer cl.Close()
	if err := cl.RegisterService(svc); err != nil {
		fmt.Printf("CASE connect FAIL %T: %v\n", err, err)
		return false
	}
	cctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	conn, err := cl.ConnectTicket(cctx, ticket)
	cancel()
	if err != nil {
		fmt.Printf("CASE connect FAIL %T: %v\n", err, err)
		return false
	}

	ok := true
	for _, c := range cases() {
		req := c.req
		ok = runCase(c.name, func(ctx context.Context) (string, error) {
			reply, err := aster.Call[EchoRequest, EchoReply](ctx, conn, "InteropEcho", "echo", &req)
			if err != nil {
				return "", err
			}
			return check(&req, reply)
		}) && ok
	}

	ok = runCase("server_stream", func(ctx context.Context) (string, error) {
		req := EchoRequest{Message: "stream", Count: 3, Data: []byte{7, 8, 9}}
		var got []*EchoReply
		for r, err := range aster.CallServerStream[EchoRequest, EchoReply](ctx, conn, "InteropEcho", "generate", &req) {
			if err != nil {
				return "", err
			}
			got = append(got, r)
		}
		if len(got) != 3 {
			return "", fmt.Errorf("%d replies != 3", len(got))
		}
		for i, r := range got {
			if r.Count != int32(i) || r.Message != req.Message || !bytes.Equal(r.Data, req.Data) {
				return "", fmt.Errorf("reply %d mismatch: %+v", i, *r)
			}
		}
		return got[0].ServerLang, nil
	}) && ok

	ok = runCase("client_stream", func(ctx context.Context) (string, error) {
		reqs := func(yield func(*EchoRequest) bool) {
			for i := 0; i < 3; i++ {
				if !yield(&EchoRequest{Message: fmt.Sprintf("m%d", i), Count: int32(i), Data: []byte{byte(i)}}) {
					return
				}
			}
		}
		r, err := aster.CallClientStream[EchoRequest, EchoReply](ctx, conn, "InteropEcho", "ingest", reqs)
		if err != nil {
			return "", err
		}
		if r.Count != 3 || r.Message != "m2" {
			return "", fmt.Errorf("reply mismatch: %d %s", r.Count, r.Message)
		}
		if r.ServerLang == "" {
			return "", fmt.Errorf("empty server_lang")
		}
		return r.ServerLang, nil
	}) && ok
	return ok
}
