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
//	  traceEcho(interop/EchoRequest) -> interop/EchoReply           (unary)
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
// Catalogue chain: chain-client <java-chain-server-ticket> (Java forwards to a .NET chain-server).
// Every mode prints CONTRACT_ID and SESSION_CONTRACT_ID first; servers print
// ADDRESS <aster1...> and serve until stdin closes (loopback, relays off);
// `client` prints CASE <name> OK|FAIL lines.
package main

import (
	"bufio"
	"bytes"
	"context"
	"crypto/rand"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"iter"
	"log/slog"
	"math"
	"net/http"
	"os"
	"strconv"
	"strings"
	"sync"
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
		// The handler context carries the dispatch server span.
		aster.Unary("traceEcho", func(ctx context.Context, req *EchoRequest) (*EchoReply, error) {
			return &EchoReply{Message: aster.CurrentTraceID(ctx), Data: []byte{}, ServerLang: "go"}, nil
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
	case len(os.Args) == 3 && os.Args[1] == "chain-client":
		if !chainClient(svc, os.Args[2]) {
			os.Exit(1)
		}
	case len(os.Args) == 3 && os.Args[1] == "campaign-client":
		if err := campaignClient(svc, os.Args[2]); err != nil {
			fmt.Fprintln(os.Stderr, "campaign:", err)
			os.Exit(1)
		}
	case len(os.Args) == 2 && os.Args[1] == "expose-server":
		if err := exposeServe(); err != nil {
			fmt.Fprintf(os.Stderr, "expose server: %v\n", err)
			os.Exit(1)
		}
	default:
		fmt.Fprintln(os.Stderr, "usage: interop_echo contract | server | client <aster1-ticket> | chain-client <java-ticket> | expose-server")
		os.Exit(2)
	}
}

const chainTraceID = "4bf92f3577b34da6a3ce929d0e0e4736"
const chainTenant = "acme catalogue"

type campaignCommand struct {
	Cmd           string   `json:"cmd"`
	Command       string   `json:"command"`
	Op            string   `json:"op"`
	WindowID      string   `json:"window_id"`
	Duration      float64  `json:"duration_s"`
	Phase         string   `json:"phase"`
	Expected      []string `json:"expected_errors"`
	ExpectedFlush []string `json:"expected_flush_errors"`
	Ticket        string   `json:"ticket"`
	Generation    int      `json:"generation"`
	SlowMS        int      `json:"slow_ms"`
}

type campaignStats struct {
	mu                                                                           sync.Mutex
	completed, unary, stream, cancel, unexpected, integrity, emitted, emitErrors uint64
	attempted                                                                    uint64
	hist                                                                         [27]uint64
	errorClasses                                                                 map[string]uint64
	errorSamples                                                                 map[string]string
	firstSuccess, lastSuccess                                                    int64
	active                                                                       atomic.Int32
	high                                                                         atomic.Int32
}

func campaignError(err error) string {
	if errors.Is(err, context.Canceled) {
		return "cancelled"
	}
	var rpc *aster.RpcError
	if errors.As(err, &rpc) {
		switch rpc.Code {
		case aster.StatusDeadlineExceeded:
			return "deadline_exceeded"
		case aster.StatusUnavailable:
			return "unavailable"
		case aster.StatusCancelled:
			return "cancelled"
		}
	}
	if errors.Is(err, context.DeadlineExceeded) {
		return "context_deadline_exceeded"
	}
	return "local_error"
}

func campaignExpected(command campaignCommand, class string) bool {
	if command.Phase != "fault" || (class != "unavailable" && class != "deadline_exceeded" && class != "cancelled" && class != "context_deadline_exceeded") {
		return false
	}
	for _, want := range command.Expected {
		if want == class {
			return true
		}
	}
	return false
}

func campaignWindow(conn *aster.RpcConnection, command campaignCommand, generation int, emit bool) (map[string]any, error) {
	stats := &campaignStats{errorClasses: make(map[string]uint64), errorSamples: make(map[string]string), firstSuccess: -1, lastSuccess: -1}
	marker := "campaign:go:" + command.WindowID + ";"
	data := make([]byte, 16384)
	for i := range data {
		data[i] = byte((i*131 + i/8) % 251)
	}
	jobs := make(chan uint64, 128)
	produceCtx, stopProducer := context.WithTimeout(context.Background(), time.Duration(command.Duration*float64(time.Second)))
	defer stopProducer()
	started := time.Now()
	var workers sync.WaitGroup
	for worker := 0; worker < 4; worker++ {
		workers.Add(1)
		go func() {
			defer workers.Done()
			for {
				select {
				case <-produceCtx.Done():
					return
				case sequence := <-jobs:
					if produceCtx.Err() != nil {
						return
					}
					stats.active.Add(1)
					callStarted := time.Now()
					kind := sequence % 4
					budget := 2 * time.Second
					if kind == 3 {
						budget = 10 * time.Second
					}
					ctx, cancel := context.WithTimeout(context.Background(), budget)
					var err error
					bad := false
					req := &EchoRequest{Message: "campaign", Count: 41, Data: data}
					if kind == 3 {
						timer := time.AfterFunc(50*time.Millisecond, cancel)
						_, err = aster.Call[EchoRequest, EchoReply](ctx, conn, "InteropEcho", "hold", req)
						timer.Stop()
						if err == nil {
							bad = true
						}
					} else if kind == 1 {
						req.Count = 5
						count := 0
						for reply, recvErr := range aster.CallServerStream[EchoRequest, EchoReply](ctx, conn, "InteropEcho", "generate", req) {
							if recvErr != nil {
								err = recvErr
								break
							}
							if reply.Count != int32(count) || reply.Message != req.Message || !bytes.Equal(reply.Data, data) || reply.ServerLang != "rust" {
								bad = true
							}
							count++
							if command.SlowMS > 0 {
								time.Sleep(time.Duration(command.SlowMS) * time.Millisecond)
							}
						}
						if err == nil && count != 5 {
							bad = true
						}
					} else {
						var reply *EchoReply
						reply, err = aster.Call[EchoRequest, EchoReply](ctx, conn, "InteropEcho", "echo", req)
						if err == nil {
							_, checkErr := check(req, reply)
							bad = checkErr != nil || reply.ServerLang != "rust"
						}
					}
					cancel()
					stats.active.Add(-1)
					stats.mu.Lock()
					stats.attempted++
					if bad {
						stats.integrity++
					}
					success := err == nil && !bad
					if err != nil {
						class := campaignError(err)
						stats.errorClasses[class]++
						sample := err.Error()
						if len(sample) > 1024 {
							sample = sample[:1024]
						}
						stats.errorSamples[class] = sample
						if kind == 3 && class == "cancelled" {
							success = true
						} else if !campaignExpected(command, class) {
							stats.unexpected++
						}
					}
					if success {
						stats.completed++
						if kind == 3 {
							stats.cancel++
						} else if kind == 1 {
							stats.stream++
						} else {
							stats.unary++
						}
						micros := max(int64(1), time.Since(callStarted).Microseconds())
						bin := 0
						for bin < 26 && micros > (int64(1)<<bin) {
							bin++
						}
						stats.hist[bin]++
						if kind != 3 {
							elapsed := time.Since(started).Milliseconds()
							if stats.firstSuccess < 0 {
								stats.firstSuccess = elapsed
							}
							stats.lastSuccess = elapsed
						}
					}
					stats.mu.Unlock()
					if success && emit {
						emitErr := aster.EmitLog(slog.LevelInfo, "app.campaign", marker, nil, nil)
						stats.mu.Lock()
						if emitErr != nil {
							stats.emitErrors++
							stats.unexpected++
						} else {
							stats.emitted++
						}
						stats.mu.Unlock()
					}
				}
			}
		}()
	}
	var sequence uint64
produce:
	for {
		select {
		case <-produceCtx.Done():
			break produce
		case jobs <- sequence:
			sequence++
			depth := int32(len(jobs))
			for old := stats.high.Load(); depth > old && !stats.high.CompareAndSwap(old, depth); old = stats.high.Load() {
			}
		}
	}
	drained := make(chan struct{})
	go func() { workers.Wait(); close(drained) }()
	select {
	case <-drained:
	case <-time.After(4 * time.Second):
		return nil, fmt.Errorf("campaign workers did not drain within 4 s")
	}
	discarded := len(jobs)
	for len(jobs) > 0 {
		<-jobs
	}
	flushErrors := 0
	flushClass, flushMessage := "", ""
	flushStarted := time.Now()
	if emit {
		if err := aster.FlushObservability(500 * time.Millisecond); err != nil {
			flushErrors++
			flushClass = "sdk"
			if errors.Is(err, aster.ErrTimeout) {
				flushClass = "timeout"
			}
			flushMessage = err.Error()
			expected := false
			if command.Phase == "fault" {
				for _, want := range command.ExpectedFlush {
					if want == flushClass {
						expected = true
					}
				}
			}
			if !expected {
				stats.unexpected++
			}
		}
	}
	return map[string]any{"phase": "campaign_window", "language": "go", "generation": generation, "duration_s": command.Duration, "elapsed_s": time.Since(started).Seconds(),
		"log_marker": marker, "window_id": command.WindowID, "error_samples": stats.errorSamples,
		"completed": stats.completed, "attempted": stats.attempted, "unary": stats.unary, "stream": stats.stream, "cancel": stats.cancel, "unexpected_errors": stats.unexpected, "integrity_failures": stats.integrity,
		"active_calls": stats.active.Load(), "queue_scope": "adapter_admission", "queue_capacity": 128, "queue_high_water": stats.high.Load(), "queue_depth": len(jobs), "queue_discarded": discarded,
		"latency_histogram": map[string]any{"unit": "us", "upper_bounds_powers_of_two": true, "counts": stats.hist}, "error_classes": stats.errorClasses,
		"first_success_elapsed_ms": stats.firstSuccess, "last_success_elapsed_ms": stats.lastSuccess, "app_log_emitted": stats.emitted, "app_log_emit_errors": stats.emitErrors, "collector_flush_errors": flushErrors,
		"flush_completed": emit && flushErrors == 0, "flush_elapsed_ms": time.Since(flushStarted).Milliseconds(), "flush_error_class": flushClass, "flush_error_message": flushMessage}, nil
}

func campaignClient(svc *aster.Service, ticket string) error {
	collector := os.Getenv("ASTER_CAMPAIGN_COLLECTOR")
	if collector != "" {
		if err := aster.InitObservability(aster.ObservabilityConfig{ServiceName: "go-campaign", OTLPEndpoint: collector, OTLPLogs: true, NoEnv: true}); err != nil {
			return err
		}
		defer aster.ShutdownObservability(2 * time.Second)
	}
	cl, err := aster.StartClient(context.Background(), config())
	if err != nil {
		return err
	}
	defer cl.Close()
	ownerIdentity, err := cl.Node().NodeID()
	if err != nil {
		return err
	}
	if err := cl.RegisterService(svc); err != nil {
		return err
	}
	connect := func(ticket string) (*aster.RpcConnection, error) {
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		defer cancel()
		return cl.ConnectTicket(ctx, ticket)
	}
	conn, err := connect(ticket)
	if err != nil {
		return err
	}
	peer, err := aster.NodeAddrFromTicket(ticket)
	if err != nil {
		return err
	}
	defer func() { _ = conn.Close() }()
	encode := json.NewEncoder(os.Stdout)
	if err := encode.Encode(map[string]any{"phase": "campaign_ready", "language": "go", "pid": os.Getpid(), "owner_identity": ownerIdentity, "generation": 0, "active_calls": 0, "collector_enabled": collector != ""}); err != nil {
		return err
	}
	generation := 0
	decode := json.NewDecoder(os.Stdin)
	for {
		var command campaignCommand
		if err := decode.Decode(&command); err != nil {
			if errors.Is(err, io.EOF) {
				return nil
			}
			return err
		}
		if command.Cmd == "" {
			command.Cmd = command.Command
		}
		if command.Cmd == "" {
			command.Cmd = command.Op
		}
		switch command.Cmd {
		case "baseline", "run":
			if command.Duration <= 0 || command.Duration > 3600 || command.SlowMS < 0 || command.SlowMS > 300 || len(command.WindowID) > 128 {
				return fmt.Errorf("invalid campaign duration/slow_ms")
			}
			result, err := campaignWindow(conn, command, generation, collector != "")
			if err != nil {
				return err
			}
			result["pid"], result["owner_identity"] = os.Getpid(), ownerIdentity
			if err := encode.Encode(result); err != nil {
				return err
			}
		case "pause":
			if err := encode.Encode(map[string]any{"phase": "campaign_idle", "generation": generation, "active_calls": 0, "queue_depth": 0, "unexpected_errors": 0, "pid": os.Getpid(), "owner_identity": ownerIdentity}); err != nil {
				return err
			}
		case "replace_peer":
			replacement, err := aster.NodeAddrFromTicket(command.Ticket)
			if err != nil {
				return err
			}
			if replacement.EndpointID == peer.EndpointID {
				return fmt.Errorf("replace_peer requires fresh peer identity")
			}
			if err := conn.Close(); err != nil {
				return err
			}
			if conn, err = connect(command.Ticket); err != nil {
				return err
			}
			generation = command.Generation
			peer = replacement
			if err := encode.Encode(map[string]any{"phase": "campaign_peer", "generation": generation, "active_calls": 0, "pid": os.Getpid(), "owner_identity": ownerIdentity}); err != nil {
				return err
			}
		case "stop":
			return encode.Encode(map[string]any{"phase": "campaign_stopped", "generation": generation, "active_calls": 0, "unexpected_errors": 0, "pid": os.Getpid(), "owner_identity": ownerIdentity})
		default:
			return fmt.Errorf("unknown campaign command %q", command.Cmd)
		}
	}
}

func chainContext(ctx context.Context, request string, budget time.Duration) (context.Context, context.CancelFunc, error) {
	tctx, err := aster.WithTraceContext(ctx, "00-"+chainTraceID+"-00f067aa0ba902b7-01",
		map[string]string{"tenant": chainTenant, "request": request})
	if err != nil {
		return nil, nil, err
	}
	if aster.CurrentTraceID(tctx) != chainTraceID || aster.CurrentBaggage(tctx)["tenant"] != chainTenant || aster.CurrentBaggage(tctx)["request"] != request {
		return nil, nil, fmt.Errorf("Go origin trace/baggage mismatch")
	}
	dctx, cancel := context.WithTimeout(tctx, budget)
	return dctx, cancel, nil
}

// Chain receipts travel in existing EchoReply fields, only in opt-in fixture modes.
func checkChainReceipt(receipt, request string, budget time.Duration) error {
	rows := strings.Split(receipt, "\n")
	if len(rows) != 2 {
		return fmt.Errorf("chain receipt has %d hops: %q", len(rows), receipt)
	}
	// Native deadline encoding rounds up to whole seconds. The first hop can
	// therefore observe almost one second more than the source duration.
	previous := budget.Milliseconds() + 1020
	for i, lang := range []string{"java", "dotnet"} {
		fields := strings.Split(rows[i], "|")
		if len(fields) != 5 || fields[0] != lang || fields[1] != chainTraceID || fields[2] != chainTenant || fields[3] != request {
			return fmt.Errorf("%s trace/baggage receipt mismatch: %q", lang, rows[i])
		}
		remaining, err := strconv.ParseInt(fields[4], 10, 64)
		if err != nil || remaining <= 0 || remaining > previous {
			return fmt.Errorf("%s remaining deadline %q outside (0, %d] ms", lang, fields[4], previous)
		}
		if i == 1 && previous-remaining < 800 {
			return fmt.Errorf("forwarding refreshed deadline: java=%d dotnet=%d ms", previous, remaining)
		}
		previous = remaining
		fmt.Printf("CHAIN_RECEIPT %s %s\n", request, rows[i])
	}
	fmt.Printf("CHAIN_RECEIPT %s go|%s|%s|%s|%d\n", request, chainTraceID, chainTenant, request, budget.Milliseconds())
	return nil
}

func chainCancelled(ctx context.Context, conn *aster.RpcConnection) (*EchoReply, error) {
	pctx, cancel := context.WithTimeout(ctx, 2*time.Second)
	defer cancel()
	return aster.Call[EchoRequest, EchoReply](pctx, conn, "InteropEcho", "cancelled", &EchoRequest{Message: "probe"})
}

func chainClient(svc *aster.Service, ticket string) bool {
	ctx := context.Background()
	cl, err := aster.StartClient(ctx, config())
	if err != nil {
		fmt.Printf("CASE connect FAIL %v\n", err)
		return false
	}
	defer cl.Close()
	if err := cl.RegisterService(svc); err != nil {
		fmt.Printf("CASE connect FAIL %v\n", err)
		return false
	}
	cctx, cancel := context.WithTimeout(ctx, 15*time.Second)
	conn, err := cl.ConnectTicket(cctx, ticket)
	cancel()
	if err != nil {
		fmt.Printf("CASE connect FAIL %v\n", err)
		return false
	}
	ok := runCase("chain_trace_baggage", func(ctx context.Context) (string, error) {
		const budget = 6 * time.Second
		tctx, cancel, err := chainContext(ctx, "trace", budget)
		if err != nil {
			return "", err
		}
		defer cancel()
		r, err := aster.Call[EchoRequest, EchoReply](tctx, conn, "InteropEcho", "traceEcho", &EchoRequest{Message: "probe"})
		if err != nil {
			return "", err
		}
		if r.ServerLang != "dotnet" {
			return "", fmt.Errorf("chain leaf %q != dotnet", r.ServerLang)
		}
		return "go->java->dotnet", checkChainReceipt(r.Message, "trace", budget)
	})
	ok = runCase("chain_remaining_deadline", func(ctx context.Context) (string, error) {
		before, err := chainCancelled(ctx, conn)
		if err != nil {
			return "", err
		}
		const budget = 3 * time.Second
		dctx, cancel, err := chainContext(ctx, "deadline", budget)
		if err != nil {
			return "", err
		}
		defer cancel()
		started := time.Now()
		_, err = aster.Call[EchoRequest, EchoReply](dctx, conn, "InteropEcho", "hold", &EchoRequest{Message: "hold"})
		var rpcErr *aster.RpcError
		if !errors.As(err, &rpcErr) || rpcErr.Code != aster.StatusDeadlineExceeded {
			return "", fmt.Errorf("chain hold wanted DEADLINE_EXCEEDED, got %v", err)
		}
		if time.Since(started) > budget+time.Second {
			return "", fmt.Errorf("chain deadline did not bound the call")
		}
		if err := expectServerCancel(ctx, conn, before.Count+1); err != nil {
			return "", err
		}
		after, err := chainCancelled(ctx, conn)
		if err != nil {
			return "", err
		}
		if after.Count != before.Count+1 {
			return "", fmt.Errorf("leaf deadline count %d != %d", after.Count, before.Count+1)
		}
		fmt.Printf("CHAIN_CANCEL_COUNT deadline before=%d after=%d\n", before.Count, after.Count)
		return "go->java->dotnet", checkChainReceipt(string(after.Data), "deadline", budget)
	}) && ok
	ok = runCase("chain_cancel", func(ctx context.Context) (string, error) {
		before, err := chainCancelled(ctx, conn)
		if err != nil {
			return "", err
		}
		const budget = 8 * time.Second
		started := time.Now()
		dctx, timeout, err := chainContext(ctx, "cancel", budget)
		if err != nil {
			return "", err
		}
		defer timeout()
		cctx, cancel := context.WithCancel(dctx)
		defer cancel()
		done := make(chan error, 1)
		go func() {
			_, err := aster.Call[EchoRequest, EchoReply](cctx, conn, "InteropEcho", "hold", &EchoRequest{Message: "hold"})
			done <- err
		}()
		// Confirm this exact traced hold reached the leaf before explicitly cancelling it.
		var receipt *EchoReply
		for until := time.Now().Add(2 * time.Second); time.Now().Before(until); {
			receipt, err = chainCancelled(ctx, conn)
			if err != nil {
				return "", err
			}
			if strings.Contains(string(receipt.Data), "dotnet|"+chainTraceID+"|"+chainTenant+"|cancel|") {
				break
			}
			time.Sleep(20 * time.Millisecond)
		}
		if receipt == nil {
			return "", fmt.Errorf("leaf never started hold")
		}
		if err := checkChainReceipt(string(receipt.Data), "cancel", budget); err != nil {
			return "", err
		}
		if receipt.Count != before.Count {
			return "", fmt.Errorf("leaf hold ended before explicit cancel")
		}
		leafFields := strings.Split(strings.Split(string(receipt.Data), "\n")[1], "|")
		leafRemaining, _ := strconv.ParseInt(leafFields[4], 10, 64)
		if leafRemaining < 6000 {
			return "", fmt.Errorf("leaf cancel test needs at least 6 s remaining, saw %d ms", leafRemaining)
		}
		cancel()
		select {
		case err := <-done:
			if !errors.Is(err, context.Canceled) && !errors.Is(err, aster.ErrCancelled) {
				return "", fmt.Errorf("chain explicit cancel wanted CANCELLED, got %v", err)
			}
		case <-time.After(time.Second):
			return "", fmt.Errorf("chain explicit cancel did not return promptly")
		}
		// This bound finishes well before the 8 s deadline: only explicit cancel can pass.
		for until := time.Now().Add(2 * time.Second); time.Now().Before(until); {
			after, err := chainCancelled(ctx, conn)
			if err != nil {
				return "", err
			}
			if after.Count == before.Count+1 {
				if time.Since(started) >= 4*time.Second {
					return "", fmt.Errorf("explicit cancel observation was not safely before the 8 s deadline")
				}
				fmt.Printf("CHAIN_CANCEL_COUNT cancel before=%d after=%d elapsed_ms=%d\n", before.Count, after.Count, time.Since(started).Milliseconds())
				return "go->java->dotnet", nil
			}
			time.Sleep(20 * time.Millisecond)
		}
		return "", fmt.Errorf("explicit cancellation did not reach leaf before its deadline")
	}) && ok
	return ok
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

	ok = runCase("trace", func(ctx context.Context) (string, error) {
		// traceEcho under a known, sampled traceparent: the call injects the
		// context's traceparent and the server must report the same trace id.
		const traceID = "4bf92f3577b34da6a3ce929d0e0e4736"
		var span [8]byte
		if _, err := rand.Read(span[:]); err != nil {
			return "", err
		}
		span[0] |= 1
		tctx, err := aster.WithTraceContext(ctx, fmt.Sprintf("00-%s-%x-01", traceID, span), nil)
		if err != nil {
			return "", err
		}
		r, err := aster.Call[EchoRequest, EchoReply](tctx, conn, "InteropEcho", "traceEcho", &EchoRequest{Message: "probe"})
		if err != nil {
			return "", fmt.Errorf("traceEcho(): %w", err)
		}
		if r.Message != traceID {
			return "", fmt.Errorf("server saw trace id %q, want %s", r.Message, traceID)
		}
		return r.ServerLang, nil
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
