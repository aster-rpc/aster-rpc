// The Go side of the InteropEcho contract (tests/interop/): a leg of the
// tests/interop/run.py matrix (contract / server / client / expose-server).
//
// Contract (must match tests/interop/rust/contract.rs, which documents the
// semantics of every method):
//
//	service InteropEcho v1
//	  echo(interop/EchoRequest) -> interop/EchoReply                (unary)
//	  generate(interop/EchoRequest) -> stream interop/EchoReply     (server stream)
//	  ingest(stream interop/EchoRequest) -> interop/EchoReply       (client stream)
//	  chat(stream interop/EchoRequest) -> stream interop/EchoReply  (bidi)
//	  hold(interop/EchoRequest) -> interop/EchoReply                (unary, blocks)
//	  cancelled(interop/EchoRequest) -> interop/EchoReply           (unary)
//	service InteropSession v1 (session-scoped)
//	  add(interop/EchoRequest) -> interop/EchoReply                 (unary)
//	interop/EchoRequest { message: string, count: varint32, data: binary }
//	interop/EchoReply   { message: string, count: varint32, data: binary, server_lang: string }
//
// Expose: `expose-server` serves HTTP service interop-http on ALPN
// aster-expose/interop/1 (GET /hello, POST /echo streamed back, header
// x-server-lang), admitting every peer.
//
// Usage: interop_echo contract | server | client <aster1-ticket> | expose-server
// Every mode prints CONTRACT_ID and SESSION_CONTRACT_ID first; servers print
// ADDRESS <aster1...> and serve until stdin closes (loopback, relays off);
// `client` prints CASE <name> OK|FAIL lines.
package main

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"iter"
	"math"
	"net/http"
	"os"
	"strings"
	"sync/atomic"
	"time"

	aster "aster-ffi"
)

const (
	exposeALPN    = "aster-expose/interop/1"
	exposeService = "interop-http"
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

// holdCancelled counts `hold` calls that observed a cancellation.
var holdCancelled atomic.Int32

// sessionState is one InteropSession instance.
type sessionState struct{ total, calls int32 }

func sessionService() *aster.Service {
	return aster.NewSessionService("InteropSession", 1, func(aster.SessionInfo) any { return &sessionState{} },
		aster.Unary("add", func(ctx context.Context, req *EchoRequest) (*EchoReply, error) {
			st, ok := aster.Session[*sessionState](ctx)
			if !ok {
				return nil, fmt.Errorf("no session instance")
			}
			st.total += req.Count
			st.calls++
			return &EchoReply{Message: fmt.Sprintf("calls=%d", st.calls), Count: st.total, Data: []byte{}, ServerLang: "go"}, nil
		}),
	)
}

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
		aster.BidiStream("chat", func(ctx context.Context, reqs iter.Seq2[*EchoRequest, error], send func(*EchoReply) error) error {
			var i int32
			for r, err := range reqs {
				if err != nil {
					return err
				}
				if err := send(&EchoReply{Message: r.Message, Count: i, Data: r.Data, ServerLang: "go"}); err != nil {
					return err
				}
				i++
			}
			return nil
		}),
		aster.Unary("hold", func(ctx context.Context, req *EchoRequest) (*EchoReply, error) {
			select {
			case <-ctx.Done():
				holdCancelled.Add(1)
				return nil, ctx.Err()
			case <-time.After(60 * time.Second):
				return &EchoReply{Message: "timeout", Count: -1, Data: []byte{}, ServerLang: "go"}, nil
			}
		}),
		aster.Unary("cancelled", func(_ context.Context, req *EchoRequest) (*EchoReply, error) {
			return &EchoReply{Message: "cancelled", Count: holdCancelled.Load(), Data: []byte{}, ServerLang: "go"}, nil
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
	sess := sessionService()
	cid, err := svc.ContractID()
	if err != nil {
		fmt.Fprintf(os.Stderr, "contract id: %v\n", err)
		os.Exit(2)
	}
	fmt.Printf("CONTRACT_ID %s\n", cid)
	scid, err := sess.ContractID()
	if err != nil {
		fmt.Fprintf(os.Stderr, "session contract id: %v\n", err)
		os.Exit(2)
	}
	fmt.Printf("SESSION_CONTRACT_ID %s\n", scid)

	switch {
	case len(os.Args) == 2 && os.Args[1] == "contract":
	case len(os.Args) == 2 && os.Args[1] == "server":
		if err := serve(svc, sess); err != nil {
			fmt.Fprintf(os.Stderr, "server: %v\n", err)
			os.Exit(1)
		}
	case len(os.Args) == 3 && os.Args[1] == "client":
		if !client(svc, sess, os.Args[2]) {
			os.Exit(1)
		}
	case len(os.Args) == 2 && os.Args[1] == "expose-server":
		if err := exposeServe(); err != nil {
			fmt.Fprintf(os.Stderr, "expose server: %v\n", err)
			os.Exit(1)
		}
	default:
		fmt.Fprintln(os.Stderr, "usage: interop_echo contract | server | client <aster1-ticket> | expose-server")
		os.Exit(2)
	}
}

func serve(svc, sess *aster.Service) error {
	ctx := context.Background()
	cfg := config()
	cfg.AllowAllConsumers = true
	srv, err := aster.StartServer(ctx, aster.ServerOptions{Config: cfg, Services: []*aster.Service{svc, sess}})
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

// exposeServe serves the interop-http service through expose.
func exposeServe() error {
	ctx := context.Background()
	node, err := aster.StartNode(ctx, config(), aster.NodeOptions{ExtraALPNs: []string{exposeALPN}})
	if err != nil {
		return err
	}
	defer node.Close()
	up, err := aster.NewUpcalls(node.Runtime(), 0)
	if err != nil {
		return err
	}
	defer up.Close()
	bodies, err := aster.NewHTTPBodies(node.Runtime())
	if err != nil {
		return err
	}
	defer bodies.Close()
	ex, err := aster.NewExpose(node)
	if err != nil {
		return err
	}
	defer ex.Close()
	if err := up.ServeExposeAuthorize(func(context.Context, aster.ExposeAuthorizeRequest) (bool, error) {
		return true, nil
	}); err != nil {
		return err
	}
	h := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("x-server-lang", "go")
		switch {
		case r.Method == "GET" && r.URL.Path == "/hello":
			_, _ = io.WriteString(w, "hello from go")
		case r.Method == "POST" && r.URL.Path == "/echo":
			w.WriteHeader(200)
			fl, _ := w.(http.Flusher)
			// Send the head now: a full-duplex consumer waits for it before
			// it uploads the first chunk.
			if fl != nil {
				fl.Flush()
			}
			buf := make([]byte, 32<<10)
			for {
				n, err := r.Body.Read(buf)
				if n > 0 {
					if _, werr := w.Write(buf[:n]); werr != nil {
						return
					}
					if fl != nil {
						fl.Flush()
					}
				}
				if err != nil {
					return
				}
			}
		default:
			w.WriteHeader(404)
		}
	})
	if err := ex.ExposeHTTP(up, bodies, exposeService, h, nil); err != nil {
		return err
	}
	if err := ex.Serve(exposeALPN); err != nil {
		return err
	}
	ticket, err := node.Ticket(ctx, 10*time.Second)
	if err != nil {
		return err
	}
	fmt.Printf("ADDRESS %s\n", ticket)
	in := bufio.NewScanner(os.Stdin)
	for in.Scan() {
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
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	lang, err := body(ctx)
	if err != nil {
		fmt.Printf("CASE %s FAIL %T: %v\n", name, err, err)
		return false
	}
	fmt.Printf("CASE %s OK server_lang=%s\n", name, lang)
	return true
}

func client(svc, sess *aster.Service, ticket string) bool {
	ctx := context.Background()
	cl, err := aster.StartClient(ctx, config())
	if err != nil {
		fmt.Printf("CASE connect FAIL %T: %v\n", err, err)
		return false
	}
	defer cl.Close()
	for _, sv := range []*aster.Service{svc, sess} {
		if err := cl.RegisterService(sv); err != nil {
			fmt.Printf("CASE connect FAIL %T: %v\n", err, err)
			return false
		}
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

	ok = runCase("bidi", func(ctx context.Context) (string, error) {
		call, err := aster.OpenBidiStream[EchoRequest, EchoReply](ctx, conn, "InteropEcho", "chat")
		if err != nil {
			return "", err
		}
		defer call.Close()
		lang := ""
		for i := int32(0); i < 3; i++ {
			req := EchoRequest{Message: fmt.Sprintf("c%d", i), Count: 100 + i, Data: []byte{byte(i), 9}}
			if err := call.Send(&req); err != nil {
				return "", fmt.Errorf("send %d: %w", i, err)
			}
			r, err := call.Recv()
			if err != nil {
				return "", fmt.Errorf("reply %d: %w", i, err)
			}
			if r.Count != i || r.Message != req.Message || !bytes.Equal(r.Data, req.Data) {
				return "", fmt.Errorf("reply %d mismatch: %+v", i, *r)
			}
			lang = r.ServerLang
		}
		if err := call.CloseSend(); err != nil {
			return "", err
		}
		if r, err := call.Recv(); !errors.Is(err, io.EOF) {
			return "", fmt.Errorf("expected end of stream, got %v %v", r, err)
		}
		return lang, nil
	}) && ok

	ok = runCase("deadline", func(ctx context.Context) (string, error) {
		before, err := cancelledCount(ctx, conn)
		if err != nil {
			return "", err
		}
		dctx, dcancel := context.WithTimeout(ctx, time.Second)
		defer dcancel()
		r, err := aster.Call[EchoRequest, EchoReply](dctx, conn, "InteropEcho", "hold", &EchoRequest{Message: "hold"})
		if err == nil {
			return "", fmt.Errorf("hold returned %+v instead of an error", *r)
		}
		var re *aster.RpcError
		if !errors.As(err, &re) || re.Code != aster.StatusDeadlineExceeded {
			return "", fmt.Errorf("expected DEADLINE_EXCEEDED, got %T: %w", err, err)
		}
		return "go", expectServerCancel(ctx, conn, before+1)
	}) && ok

	ok = runCase("cancel", func(ctx context.Context) (string, error) {
		before, err := cancelledCount(ctx, conn)
		if err != nil {
			return "", err
		}
		cctx, ccancel := context.WithCancel(ctx)
		time.AfterFunc(300*time.Millisecond, ccancel)
		r, err := aster.Call[EchoRequest, EchoReply](cctx, conn, "InteropEcho", "hold", &EchoRequest{Message: "hold"})
		ccancel()
		if err == nil {
			return "", fmt.Errorf("hold returned %+v before the client cancelled it", *r)
		}
		if !errors.Is(err, context.Canceled) && !errors.Is(err, aster.ErrCancelled) {
			return "", fmt.Errorf("expected CANCELLED, got %T: %w", err, err)
		}
		return "go", expectServerCancel(ctx, conn, before+1)
	}) && ok

	ok = runCase("session", func(ctx context.Context) (string, error) {
		var ids [2]uint32
		for i := range ids {
			id, err := conn.NewSessionID()
			if err != nil {
				return "", err
			}
			ids[i] = id
		}
		steps := []struct {
			s     int
			add   int32
			total int32
			calls string
		}{{0, 5, 5, "calls=1"}, {0, 7, 12, "calls=2"}, {1, 1, 1, "calls=1"}, {0, 0, 12, "calls=3"}, {1, 10, 11, "calls=2"}}
		lang := ""
		for i, st := range steps {
			r, err := aster.Call[EchoRequest, EchoReply](ctx, conn, "InteropSession", "add",
				&EchoRequest{Message: "add", Count: st.add}, aster.WithSession(ids[st.s]))
			if err != nil {
				return "", fmt.Errorf("step %d: %w", i, err)
			}
			if r.Count != st.total || r.Message != st.calls {
				return "", fmt.Errorf("step %d: got total %d %q, want total %d %q", i, r.Count, r.Message, st.total, st.calls)
			}
			lang = r.ServerLang
		}
		if lang == "" {
			return "", fmt.Errorf("empty server_lang")
		}
		return lang, nil
	}) && ok
	return ok
}

func cancelledCount(ctx context.Context, conn *aster.RpcConnection) (int32, error) {
	pctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	r, err := aster.Call[EchoRequest, EchoReply](pctx, conn, "InteropEcho", "cancelled", &EchoRequest{Message: "probe"})
	if err != nil {
		return 0, fmt.Errorf("cancelled(): %w", err)
	}
	return r.Count, nil
}

// expectServerCancel polls `cancelled` until it reaches want (5 s).
func expectServerCancel(ctx context.Context, conn *aster.RpcConnection, want int32) error {
	var seen int32
	for i := 0; i < 50; i++ {
		var err error
		if seen, err = cancelledCount(ctx, conn); err != nil {
			return err
		}
		if seen >= want {
			return nil
		}
		time.Sleep(100 * time.Millisecond)
	}
	return fmt.Errorf("server never observed the cancellation (count %d, want %d)", seen, want)
}
