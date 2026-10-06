//go:build cgo

package aster

import (
	"bytes"
	"compress/gzip"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"runtime/pprof"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
)

// These tests share one process-wide pipeline (the SDK's globals are per
// process): traces export to a local OTLP receiver, Rust logs go to obsLogs
// at trace level. TestObs_ZShutdownAndReinit runs last and restarts it.

var (
	obsLogs = &capture{level: LevelTrace}

	obsRecvOnce     sync.Once
	obsRecv         *httptest.Server
	obsRecvClosed   bool
	obsBodiesMu     sync.Mutex
	obsBodies       [][]byte
	obsLogBodies    [][]byte
	obsLogRequests  int
	obsRejectLogs   bool
	obsRejectedLogs int
	obsStallLogs    bool
	obsStalledLogs  int
	obsStallGate    chan struct{}
	obsOurs         bool // this file's pipeline is the running one; guarded by obs.mu
)

func obsReceiverHandler() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		if r.Header.Get("Content-Encoding") == "gzip" {
			if zr, err := gzip.NewReader(bytes.NewReader(body)); err == nil {
				body, _ = io.ReadAll(zr)
			}
		}
		if strings.HasSuffix(r.URL.Path, "/v1/logs") {
			obsBodiesMu.Lock()
			obsLogRequests++
			if obsStallLogs {
				obsStalledLogs++
				gate := obsStallGate
				obsBodiesMu.Unlock()
				if gate != nil {
					<-gate
				}
				obsBodiesMu.Lock()
			}
			if obsRejectLogs {
				obsRejectedLogs++
				obsBodiesMu.Unlock()
				w.WriteHeader(http.StatusServiceUnavailable)
				return
			}
			obsLogBodies = append(obsLogBodies, body)
			obsBodiesMu.Unlock()
		}
		if strings.HasSuffix(r.URL.Path, "/v1/traces") {
			obsBodiesMu.Lock()
			obsBodies = append(obsBodies, body)
			obsBodiesMu.Unlock()
		}
		w.WriteHeader(http.StatusOK)
	})
}

func obsReceiver() *httptest.Server {
	obsRecvOnce.Do(func() {
		obsRecv = httptest.NewServer(obsReceiverHandler())
	})
	return obsRecv
}

func obsRestartReceiver(t *testing.T, addr string) {
	t.Helper()
	listener, err := net.Listen("tcp", addr)
	if err != nil {
		t.Fatalf("rebind collector at same endpoint %s: %v", addr, err)
	}
	obsRecv = httptest.NewUnstartedServer(obsReceiverHandler())
	obsRecv.Listener = listener
	obsRecv.Start()
}

// obsSetup (re)starts the shared pipeline when none is held.
func obsSetup(t *testing.T) {
	t.Helper()
	obs.mu.Lock()
	ours := obsOurs && obs.refs > 0
	obs.mu.Unlock()
	if ours {
		return
	}
	// Another test's StartServer may have started the implicit,
	// environment-only pipeline, which has no tracer provider: replace it.
	if _, err := ShutdownObservability(5 * time.Second); err != nil {
		t.Fatalf("ShutdownObservability: %v", err)
	}
	SetLogger(slog.New(obsLogs))
	err := InitObservability(ObservabilityConfig{
		ServiceName: "go-o10-test", OTLPEndpoint: obsReceiver().URL, Traces: true, OTLPLogs: true, NoEnv: true,
	})
	if err != nil {
		t.Fatalf("InitObservability: %v", err)
	}
	obs.mu.Lock()
	obsOurs = true
	obs.mu.Unlock()
}

// parentOf finds the exported span (trace, span) and returns its parent span
// id: the protobuf Span encodes trace_id (1), span_id (2) and parent_span_id
// (4) as length-delimited bytes; a span without trace_state has
// 12 08 <span> 22 08 <parent> back to back.
func parentOf(trace [16]byte, span [8]byte) (parent [8]byte, ok bool) {
	pat := append([]byte{0x12, 8}, span[:]...)
	obsBodiesMu.Lock()
	defer obsBodiesMu.Unlock()
	for _, b := range obsBodies {
		if !bytes.Contains(b, trace[:]) {
			continue
		}
		if i := bytes.Index(b, pat); i >= 0 && len(b) >= i+len(pat)+10 && b[i+len(pat)] == 0x22 && b[i+len(pat)+1] == 8 {
			copy(parent[:], b[i+len(pat)+2:])
			return parent, true
		}
	}
	return parent, false
}

// spanExported reports whether a span with this id was exported.
func spanExported(span [8]byte) bool {
	pat := append([]byte{0x12, 8}, span[:]...)
	obsBodiesMu.Lock()
	defer obsBodiesMu.Unlock()
	for _, b := range obsBodies {
		if bytes.Contains(b, pat) {
			return true
		}
	}
	return false
}

func waitFor(t *testing.T, what string, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		if cond() {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatalf("timed out waiting for %s", what)
}

func httpGet(t *testing.T, addr, path string) (int, string) {
	t.Helper()
	c := &http.Client{Timeout: 5 * time.Second}
	resp, err := c.Get("http://" + addr + path)
	if err != nil {
		t.Fatalf("GET %s: %v", path, err)
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, string(b)
}

type probeState struct {
	mu                      sync.Mutex
	serverTrace, serverSpan string
	childTrace, childSpan   string
	baggage                 map[string]string
	serverSC, childSC       SpanContext
}

func probeService(p *probeState) *Service {
	return NewService("O10Probe", 1,
		Unary("probe", func(ctx context.Context, r *testEchoRequest) (*testEchoReply, error) {
			cctx, span := StartSpan(ctx, "o10.child", WithSpanAttributes(StringAttr("k", "v")))
			span.AddEvent("tick", IntAttr("n", 2))
			// The application's own record carries the child span's ids.
			slog.New(NewTraceHandler(obsLogs)).InfoContext(cctx, "inside the handler")
			p.mu.Lock()
			p.serverTrace, p.serverSpan = CurrentTraceID(ctx), CurrentSpanID(ctx)
			p.serverSC, _ = SpanContextFromContext(ctx)
			p.childTrace, p.childSpan = span.TraceID(), span.SpanID()
			p.childSC = span.SpanContext()
			p.baggage = CurrentBaggage(ctx)
			p.mu.Unlock()
			span.End()
			return &testEchoReply{Message: r.Message, Data: []byte{}, ServerLang: "go"}, nil
		}),
	)
}

func rustRecordsWithTrace(trace string) []map[string]any {
	var out []map[string]any
	for _, r := range obsLogs.all() {
		if r["aster_origin"] == "rust" && r["trace_id"] == trace {
			out = append(out, r)
		}
	}
	return out
}

// Rust log records reach slog with the server span's ids; the handler sees the
// client's trace; spans started in the handler parent under the server span
// and are exported with their parent ids.
func TestObs_LogsAndSpansFollowTheTrace(t *testing.T) {
	obsSetup(t)
	p := &probeState{}
	f := newRPCFixture(t, ServerOptions{Services: []*Service{probeService(p)}})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	cctx, client := StartSpan(ctx, "client.work", WithSpanKind(SpanKindClient))
	if !client.SpanContext().IsValid() || CurrentTraceID(cctx) != client.TraceID() {
		t.Fatalf("StartSpan must make the span current (trace %q, span %q)", CurrentTraceID(cctx), client.TraceID())
	}
	reply, err := Call[testEchoRequest, testEchoReply](cctx, f.conn, "O10Probe", "probe", &testEchoRequest{Message: "hi", Data: []byte{}})
	client.End()
	if err != nil || reply.Message != "hi" {
		t.Fatalf("call: %v %+v", err, reply)
	}

	p.mu.Lock()
	defer p.mu.Unlock()
	trace := client.TraceID()
	if p.serverTrace != trace {
		t.Fatalf("handler trace %q, want the client's %q (traceparent must cross the wire)", p.serverTrace, trace)
	}
	if p.serverSpan == "" || p.serverSpan == client.SpanID() {
		t.Fatalf("handler span %q: the server span is a new span under the client's", p.serverSpan)
	}
	if p.childTrace != trace || p.childSpan == "" || p.childSpan == p.serverSpan {
		t.Fatalf("child span %q/%q, server %q: must be a new span in the same trace", p.childTrace, p.childSpan, p.serverSpan)
	}

	// Rust events inside the server span carry its ids, plus the call fields.
	waitFor(t, "a Rust record inside the server span", func() bool { return len(rustRecordsWithTrace(trace)) > 0 })
	recs := rustRecordsWithTrace(trace)
	var sawService bool
	for _, r := range recs {
		if r["span_id"] != p.serverSpan {
			continue
		}
		if r["rpc.service"] == "O10Probe" && r["rpc.method"] == "probe" {
			sawService = true
		}
		if l, _ := r["logger"].(string); !strings.HasPrefix(l, "aster.") {
			t.Errorf("logger name %q not mapped from the Rust target", l)
		}
	}
	if !sawService {
		t.Fatalf("no Rust record with the server span id and rpc.service/rpc.method: %v", recs)
	}

	// The application's own record, through TraceHandler, has the child's ids.
	var app map[string]any
	for _, r := range obsLogs.all() {
		if r["msg"] == "inside the handler" {
			app = r
		}
	}
	if app == nil || app["trace_id"] != trace || app["span_id"] != p.childSpan {
		t.Fatalf("application record %v, want trace %s span %s", app, trace, p.childSpan)
	}

	// Parenting reaches the exporter: the child span names the server span as
	// its parent, and the server span names the client's.
	if err := FlushObservability(5 * time.Second); err != nil {
		t.Fatal(err)
	}
	waitFor(t, "the exported spans", func() bool {
		_ = FlushObservability(5 * time.Second)
		pc, okc := parentOf(p.childSC.TraceID, p.childSC.SpanID)
		ps, oks := parentOf(p.serverSC.TraceID, p.serverSC.SpanID)
		// child -> server span -> (the client call's span) -> client.work
		return okc && pc == p.serverSC.SpanID && oks && ps != p.serverSC.SpanID && ps != [8]byte{} && spanExported(ps)
	})
}

// Inbound W3C baggage reaches the handler through the context, a traceparent
// the caller continues becomes the trace, and metadata does not carry them.
func TestObs_BaggageAndContinuedTrace(t *testing.T) {
	obsSetup(t)
	p := &probeState{}
	f := newRPCFixture(t, ServerOptions{Services: []*Service{probeService(p)}})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	bctx, err := WithTraceContext(ctx, pureTP, map[string]string{"tenant": "acme", "k": "v w"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := Call[testEchoRequest, testEchoReply](bctx, f.conn, "O10Probe", "probe", &testEchoRequest{Data: []byte{}}); err != nil {
		t.Fatal(err)
	}
	p.mu.Lock()
	if p.serverTrace != pureTraceID || p.baggage["tenant"] != "acme" || p.baggage["k"] != "v w" {
		t.Fatalf("trace %q baggage %v", p.serverTrace, p.baggage)
	}
	p.mu.Unlock()

	// No context: nothing inherited from the previous call.
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "O10Probe", "probe", &testEchoRequest{Data: []byte{}}); err != nil {
		t.Fatal(err)
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	if len(p.baggage) != 0 || p.serverTrace == pureTraceID {
		t.Fatalf("a call without context must not inherit: trace %q baggage %v", p.serverTrace, p.baggage)
	}
}

func TestObs_InjectTrace(t *testing.T) {
	ctx, err := WithTraceContext(context.Background(), pureTP, map[string]string{"k": "v"})
	if err != nil {
		t.Fatal(err)
	}
	got := injectTrace(ctx, []MetadataEntry{{"x", "1"}})
	want := []MetadataEntry{{"x", "1"}, {"traceparent", pureTP}, {"baggage", "k=v"}}
	if fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("inject: %v", got)
	}
	// The caller's own traceparent wins, untouched.
	own := []MetadataEntry{{"traceparent", "00-" + strings.Repeat("a", 32) + "-" + strings.Repeat("b", 16) + "-00"}}
	if got := injectTrace(ctx, own); fmt.Sprint(got) != fmt.Sprint(own) {
		t.Fatalf("own traceparent: %v", got)
	}
	// An explicit baggage entry is kept, not duplicated.
	got = injectTrace(ctx, []MetadataEntry{{"baggage", "mine=1"}})
	if len(got) != 2 || got[0].Value != "mine=1" || got[1].Key != "traceparent" {
		t.Fatalf("own baggage: %v", got)
	}
	if md := []MetadataEntry{{"x", "1"}}; fmt.Sprint(injectTrace(context.Background(), md)) != fmt.Sprint(md) {
		t.Fatal("no context, no change")
	}
}

// The Go traceparent codec and the library's agree.
func TestObs_TraceparentMatchesTheLibrary(t *testing.T) {
	sc, err := ParseTraceparent(pureTP)
	if err != nil {
		t.Fatal(err)
	}
	c, err := cTraceparentFormat(sc)
	if err != nil || c != sc.Traceparent() || c != pureTP {
		t.Fatalf("format: %q %v", c, err)
	}
	back, err := cTraceparentParse(pureTP)
	if err != nil || back.TraceID != sc.TraceID || back.SpanID != sc.SpanID || back.Flags != sc.Flags || !back.Remote {
		t.Fatalf("parse: %+v %v", back, err)
	}
	for _, bad := range []string{"", "00-abc", "ff-" + pureTraceID + "-" + pureSpanID + "-01", "00-" + strings.Repeat("0", 32) + "-" + pureSpanID + "-01"} {
		_, goErr := ParseTraceparent(bad)
		_, cErr := cTraceparentParse(bad)
		if (goErr == nil) != (cErr == nil) {
			t.Errorf("%q: Go err %v, library err %v", bad, goErr, cErr)
		}
	}
}

// promSeries parses Prometheus text (every non-comment line must be
// `name{labels} value`) into series -> value.
func promSeries(t *testing.T, text string) map[string]float64 {
	t.Helper()
	out := map[string]float64{}
	for _, line := range strings.Split(text, "\n") {
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		i := strings.LastIndexByte(line, ' ')
		if i < 0 {
			t.Fatalf("unparsable Prometheus line %q", line)
		}
		v, err := strconv.ParseFloat(line[i+1:], 64)
		if err != nil {
			t.Fatalf("unparsable Prometheus value in %q: %v", line, err)
		}
		out[line[:i]] = v
	}
	return out
}

func findSeries(series map[string]float64, name string, frags ...string) (float64, bool) {
	for k, v := range series {
		if !strings.HasPrefix(k, name+"{") && k != name {
			continue
		}
		ok := true
		for _, f := range frags {
			ok = ok && strings.Contains(k, f)
		}
		if ok {
			return v, true
		}
	}
	return 0, false
}

func wantSeries(t *testing.T, text string, series map[string]float64, want float64, name string, frags ...string) {
	t.Helper()
	got, ok := findSeries(series, name, frags...)
	if !ok || got != want {
		t.Fatalf("series %s%v = %v (found %v), want %v in:\n%s", name, frags, got, ok, want, text)
	}
}

// Application instruments render as Prometheus text next to the built-ins,
// both through the library accessor and the HTTP /metrics endpoint.
func TestObs_MetricsInPrometheusText(t *testing.T) {
	obsSetup(t)
	p := &probeState{}
	f := newRPCFixture(t, ServerOptions{Services: []*Service{probeService(p)}, Health: &HealthConfig{}})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	if _, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "O10Probe", "probe", &testEchoRequest{Data: []byte{}}); err != nil {
		t.Fatal(err)
	}

	orders, err := NewCounter("go.o10.orders.placed", "Orders placed", "")
	if err != nil {
		t.Fatal(err)
	}
	inflight, _ := NewUpDownCounter("go.o10.inflight", "In flight", "")
	latency, _ := NewHistogram("go.o10.latency", "Latency", "s")
	sizes, err := NewHistogram("go.o10.sizes", "Sizes", "By", WithBoundaries(10, 100, 1000))
	if err != nil {
		t.Fatal(err)
	}
	depth, _ := NewGauge("go.o10.queue.depth", "Depth", "")
	set, err := NewAttrSet(StringAttr("region", "eu"))
	if err != nil {
		t.Fatal(err)
	}
	defer set.Close()

	orders.Add(ctx, 1, StringAttr("region", "eu"))
	orders.AddWith(ctx, 2, set)
	orders.Add(ctx, 5, StringAttr("region", "us"), BoolAttr("vip", true))
	inflight.Add(ctx, 3)
	inflight.Add(ctx, -1)
	latency.Record(ctx, 0.042, StringAttr("op", "price"))
	sizes.Record(ctx, 50)
	sizes.RecordWith(ctx, 5000, nil)
	depth.Record(ctx, 7)
	depth.Record(ctx, 4)

	if _, err := NewHistogram("go.o10.orders.placed", "", ""); err == nil || !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("a name has one kind: %v", err)
	}
	if _, err := NewCounter("1 bad name", "", ""); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("OTel name rules: %v", err)
	}

	text, err := MetricsPrometheus()
	if err != nil {
		t.Fatal(err)
	}
	s := promSeries(t, text)
	wantSeries(t, text, s, 3, "go_o10_orders_placed_total", `region="eu"`)
	wantSeries(t, text, s, 5, "go_o10_orders_placed_total", `region="us"`, `vip="true"`)
	wantSeries(t, text, s, 2, "go_o10_inflight")
	wantSeries(t, text, s, 4, "go_o10_queue_depth")
	// Seconds histograms get the RPC duration buckets; explicit boundaries win.
	wantSeries(t, text, s, 1, "go_o10_latency_seconds_bucket", `le="0.05"`)
	wantSeries(t, text, s, 0, "go_o10_latency_seconds_bucket", `le="0.025"`)
	wantSeries(t, text, s, 1, "go_o10_sizes_bytes_bucket", `le="100"`)
	wantSeries(t, text, s, 2, "go_o10_sizes_bytes_bucket", `le="+Inf"`)
	wantSeries(t, text, s, 2, "go_o10_sizes_bytes_count")
	// Built-ins record next to them.
	if _, ok := findSeries(s, "aster_rpc_server_requests_completed_total", `rpc_service="O10Probe"`); !ok {
		t.Fatalf("no built-in server request counter in:\n%s", text)
	}

	// The listener serves the same text.
	addr, err := f.srv.Health().Addr()
	if err != nil {
		t.Fatal(err)
	}
	code, body := httpGet(t, addr, "/metrics")
	if code != 200 {
		t.Fatalf("/metrics: %d", code)
	}
	hs := promSeries(t, body)
	wantSeries(t, body, hs, 3, "go_o10_orders_placed_total", `region="eu"`)
}

// /healthz, /readyz, checks and port release, with the listener on its own
// and attached to a server.
func TestObs_HealthEndpoints(t *testing.T) {
	obsSetup(t)
	f := newRPCFixture(t, ServerOptions{Services: []*Service{probeService(&probeState{})}, Health: &HealthConfig{}})
	attached := f.srv.Health()
	addr, err := attached.Addr()
	if err != nil || !strings.HasPrefix(addr, "127.0.0.1:") {
		t.Fatalf("addr %q %v (loopback by default)", addr, err)
	}
	// Attached: ready as soon as the server serves.
	if code, body := httpGet(t, addr, "/healthz"); code != 200 || !strings.Contains(body, `"ok"`) {
		t.Fatalf("/healthz: %d %s", code, body)
	}
	code, body := httpGet(t, addr, "/readyz")
	if code != 200 {
		t.Fatalf("/readyz: %d %s", code, body)
	}
	var ready struct {
		Status string          `json:"status"`
		Checks map[string]bool `json:"checks"`
	}
	if err := json.Unmarshal([]byte(body), &ready); err != nil {
		t.Fatalf("/readyz body %q: %v", body, err)
	}
	// A named check fails readiness until it passes.
	if err := attached.SetCheck("db", false); err != nil {
		t.Fatal(err)
	}
	if code, body = httpGet(t, addr, "/readyz"); code != 503 || !strings.Contains(body, `"db":false`) {
		t.Fatalf("failing check: %d %s", code, body)
	}
	if err := attached.SetCheck("db", true); err != nil {
		t.Fatal(err)
	}
	if code, _ = httpGet(t, addr, "/readyz"); code != 200 {
		t.Fatalf("passing check: %d", code)
	}
	if err := attached.SetReady(false); err != nil {
		t.Fatal(err)
	}
	if code, _ = httpGet(t, addr, "/readyz"); code != 503 {
		t.Fatalf("not ready: %d", code)
	}
	if code, _ = httpGet(t, addr, "/healthz"); code != 200 {
		t.Fatalf("not ready is still alive: %d", code)
	}
	if err := attached.SetReady(true); err != nil {
		t.Fatal(err)
	}
	if err := attached.SetHealthy(false); err != nil {
		t.Fatal(err)
	}
	if code, _ = httpGet(t, addr, "/healthz"); code != 503 {
		t.Fatalf("unhealthy: %d", code)
	}
	if code, _ = httpGet(t, addr, "/readyz"); code != 503 {
		t.Fatalf("unhealthy fails readiness: %d", code)
	}
	if err := attached.SetCheck("", true); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("empty check name: %v", err)
	}

	// A standalone listener starts not ready.
	h, err := StartHealth(f.srv.Node(), &HealthConfig{Host: "127.0.0.1"})
	if err != nil {
		t.Fatal(err)
	}
	haddr, _ := h.Addr()
	if code, _ = httpGet(t, haddr, "/readyz"); code != 503 {
		t.Fatalf("standalone listener starts not ready: %d", code)
	}
	if code, _ = httpGet(t, haddr, "/healthz"); code != 200 {
		t.Fatalf("standalone liveness: %d", code)
	}
	_ = h.SetReady(true)
	if code, _ = httpGet(t, haddr, "/readyz"); code != 200 {
		t.Fatalf("standalone ready: %d", code)
	}
	if err := h.Stop(); err != nil {
		t.Fatal(err)
	}
	if _, err := (&http.Client{Timeout: time.Second}).Get("http://" + haddr + "/healthz"); err == nil {
		t.Fatal("the port must be free after Stop")
	}
	if err := h.SetReady(true); !errors.Is(err, ErrAlreadyClosed) {
		t.Fatalf("use after Stop: %v", err)
	}
	if err := h.Stop(); err != nil {
		t.Fatalf("Stop is idempotent: %v", err)
	}

	// Closing the server stops its listener.
	if err := f.srv.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := (&http.Client{Timeout: time.Second}).Get("http://" + addr + "/healthz"); err == nil {
		t.Fatal("the server's listener must be down after Close")
	}
}

// The slog level drives the Rust filter: SyncLogLevel sends it at once, and a
// pinned filter is left alone.
func TestObs_LogLevelFollowsTheLogger(t *testing.T) {
	obsSetup(t)
	defer SetLogger(slog.New(obsLogs))
	SetLogger(slog.New(&capture{level: slog.LevelInfo}))
	d, err := SyncLogLevel()
	if err != nil || d != "warn,aster=info" {
		t.Fatalf("directive %q, %v", d, err)
	}
	SetLogger(slog.New(&capture{level: LevelTrace}))
	if d, err = SyncLogLevel(); err != nil || d != "warn,aster=trace" {
		t.Fatalf("directive %q, %v", d, err)
	}

	obs.mu.Lock()
	p := obs.poller
	obs.mu.Unlock()
	if p == nil {
		t.Fatal("no log poller")
	}
	p.pinned = true
	defer func() { p.pinned = false }()
	if d, err = SyncLogLevel(); err != nil || d != "" {
		t.Fatalf("a pinned filter must not be rewritten: %q %v", d, err)
	}
}

func TestObs_InitWithADifferentConfigIsRefused(t *testing.T) {
	obsSetup(t)
	err := InitObservability(ObservabilityConfig{ServiceName: "someone-else", OTLPEndpoint: obsReceiver().URL, Traces: true, NoEnv: true})
	if !errors.Is(err, ErrAlreadyExists) {
		t.Fatalf("err = %v, want ALREADY_EXISTS", err)
	}
}

func pollerRunning() bool {
	var b strings.Builder
	_ = pprof.Lookup("goroutine").WriteTo(&b, 1)
	return strings.Contains(b.String(), "(*logPoller).run")
}

// Shutdown stops the poller and frees every handle; instruments declare
// themselves again, a new Init starts a new pipeline and poller.
func TestObs_ZShutdownAndReinit(t *testing.T) {
	obsSetup(t)
	// The last of this file's tests: leave the process as the others expect.
	t.Cleanup(func() {
		_, _ = ShutdownObservability(5 * time.Second)
		SetLogger(nil)
		obs.mu.Lock()
		obsOurs = false
		obs.mu.Unlock()
	})
	if !pollerRunning() {
		t.Fatal("the log poller goroutine should be running")
	}
	c, err := NewCounter("go.o10.reinit", "Survives shutdown", "")
	if err != nil {
		t.Fatal(err)
	}
	set, _ := NewAttrSet(StringAttr("a", "b"))
	ctx := context.Background()
	c.Add(ctx, 1)

	held, err := ShutdownObservability(5 * time.Second)
	obs.mu.Lock()
	obsOurs = false
	obs.mu.Unlock()
	if err != nil || !held {
		t.Fatalf("shutdown: held=%v err=%v", held, err)
	}
	waitFor(t, "the poller to exit", func() bool { return !pollerRunning() })
	if held, _ = ShutdownObservability(time.Second); held {
		t.Fatal("nothing left to release")
	}
	c.Add(ctx, 1) // after shutdown: dropped, never a failure

	obsSetup(t)
	waitFor(t, "a new poller after re-init", pollerRunning)
	// The listener first: it gives the new pipeline its Prometheus reader.
	f := newRPCFixture(t, ServerOptions{Services: []*Service{probeService(&probeState{})}, Health: &HealthConfig{}})
	c.AddWith(ctx, 4, set) // both handles were freed: declared again
	text, err := MetricsPrometheus()
	if err != nil {
		t.Fatal(err)
	}
	wantSeries(t, text, promSeries(t, text), 4, "go_o10_reinit_total", `a="b"`)
	_ = f.srv.Close()
}

func TestObs_StatusAndABIFloor(t *testing.T) {
	if err := CheckABI(); err != nil {
		t.Fatal(err)
	}
	if ASTER_STATUS_ALREADY_EXISTS != 17 || StatusName(17) != "ASTER_STATUS_ALREADY_EXISTS" ||
		!errors.Is(wrapError(ASTER_STATUS_ALREADY_EXISTS), ErrAlreadyExists) {
		t.Fatal("ALREADY_EXISTS (17)")
	}
	if ABIMinorRequired != 6 {
		t.Fatalf("floor %d", ABIMinorRequired)
	}
}

// Path B: an application record emitted through EmitHandler reaches the OTLP
// logs exporter with the context's trace ids, and bridged Rust records are
// never re-emitted.
func TestObs_EmitHandlerExportsAppLogs(t *testing.T) {
	obsSetup(t)
	ctx, err := WithTraceContext(context.Background(), pureTP, nil)
	if err != nil {
		t.Fatal(err)
	}
	sc, _ := SpanContextFromContext(ctx)
	l := slog.New(NewEmitHandlerNamed(nil, "app.pathb").WithAttrs([]slog.Attr{slog.String("k0", "v0")}))
	l.InfoContext(ctx, "pathb-marker-app", "order", "o-1")
	l.Info("pathb-marker-rust", "aster_origin", "rust")
	if err := FlushObservability(5 * time.Second); err != nil {
		t.Fatal(err)
	}
	has := func(needle []byte) bool {
		obsBodiesMu.Lock()
		defer obsBodiesMu.Unlock()
		for _, b := range obsLogBodies {
			if bytes.Contains(b, needle) {
				return true
			}
		}
		return false
	}
	waitFor(t, "the app log record at the OTLP receiver", func() bool { return has([]byte("pathb-marker-app")) })
	for _, want := range []string{"app.pathb", "order", "o-1", "k0"} {
		if !has([]byte(want)) {
			t.Errorf("exported logs lack %q", want)
		}
	}
	if !has(sc.TraceID[:]) || !has(sc.SpanID[:]) {
		t.Error("exported record lacks the context's trace/span ids")
	}
	if has([]byte("pathb-marker-rust")) {
		t.Error("a record marked aster_origin=rust was re-emitted")
	}
}

func TestObs_ApplicationLogsRecoverAfterExporterHTTP503(t *testing.T) {
	exerciseApplicationLogOutage(t, "http503")
}

func TestObs_ApplicationLogsRecoverAfterConnectionRefusal(t *testing.T) {
	exerciseApplicationLogOutage(t, "refusal")
}

func TestObs_ApplicationLogsRecoverAfterStalledResponse(t *testing.T) {
	exerciseApplicationLogOutage(t, "stall")
}

func exerciseApplicationLogOutage(t *testing.T, mode string) {
	obsSetup(t)
	if err := FlushObservability(5 * time.Second); err != nil {
		t.Fatalf("flush before exporter outage: %v", err)
	}
	node, err := Memory(context.Background())
	if err != nil {
		t.Fatalf("start audit health node: %v", err)
	}
	health, err := StartHealth(node, &HealthConfig{Host: "127.0.0.1"})
	if err != nil {
		_ = node.Close()
		t.Fatalf("start audit health listener: %v", err)
	}
	healthAddr, err := health.Addr()
	if err != nil {
		_ = health.Close()
		_ = node.Close()
		t.Fatalf("audit health address: %v", err)
	}
	if err := health.SetReady(true); err != nil {
		t.Fatal(err)
	}
	assertAuditReady := func(stage string) {
		t.Helper()
		if code, body := httpGet(t, healthAddr, "/healthz"); code != http.StatusOK {
			t.Fatalf("%s healthz: %d %s", stage, code, body)
		}
		if code, body := httpGet(t, healthAddr, "/readyz"); code != http.StatusOK || !strings.Contains(body, `"audit":true`) {
			t.Fatalf("%s readyz audit check: %d %s", stage, code, body)
		}
	}
	defer func() {
		_ = health.Close()
		_ = node.Close()
	}()
	assertAuditReady("before exporter outage")
	setOutage := func(value bool) {
		obsBodiesMu.Lock()
		obsRejectLogs = value
		obsBodiesMu.Unlock()
	}
	endpoint := ""
	t.Cleanup(func() {
		setOutage(false)
		obsBodiesMu.Lock()
		obsStallLogs = false
		if obsStallGate != nil {
			close(obsStallGate)
			obsStallGate = nil
		}
		obsBodiesMu.Unlock()
		if obsRecvClosed && endpoint != "" {
			obsRestartReceiver(t, endpoint)
			obsRecvClosed = false
		}
		obsSetup(t) // Restore the healthy shared pipeline after outage shutdown.
	})
	marker := "go-" + mode + "-"
	obsBodiesMu.Lock()
	rejectedBefore := obsRejectedLogs
	stalledBefore := obsStalledLogs
	requestsBefore := obsLogRequests
	addr, err := url.Parse(obsReceiver().URL)
	if err != nil {
		obsBodiesMu.Unlock()
		t.Fatal(err)
	}
	endpoint = addr.Host
	obsBodiesMu.Unlock()
	if mode == "refusal" {
		obsRecv.Close()
		obsRecvClosed = true
		probe, err := net.Listen("tcp", endpoint)
		if err != nil {
			t.Fatalf("collector endpoint did not become unbound: %v", err)
		}
		probe.Close()
	} else {
		obsBodiesMu.Lock()
		obsRejectLogs = true
		if mode == "stall" {
			obsStallLogs = true
			obsStallGate = make(chan struct{})
		}
		obsBodiesMu.Unlock()
	}
	start := time.Now()
	for i := 0; i < 1000; i++ {
		if err := EmitLog(slog.LevelWarn, "app.outage", fmt.Sprintf("%soutage-%d", marker, i), nil, nil); err != nil {
			t.Fatal(err)
		}
	}
	if elapsed := time.Since(start); elapsed >= time.Second {
		t.Fatalf("application emission blocked for %s", elapsed)
	}
	start = time.Now()
	if err := FlushObservability(time.Second); err == nil {
		t.Fatal("flush should report exporter failure")
	}
	if elapsed := time.Since(start); elapsed >= 3*time.Second {
		t.Fatalf("failed flush exceeded bound: %s", elapsed)
	}
	obsBodiesMu.Lock()
	rejected := obsRejectedLogs - rejectedBefore
	stalled := obsStalledLogs - stalledBefore
	requests := obsLogRequests
	for _, body := range obsLogBodies {
		if bytes.Contains(body, []byte(marker+"outage-")) {
			obsBodiesMu.Unlock()
			t.Fatal("collector accepted an outage record while rejecting logs")
		}
	}
	obsBodiesMu.Unlock()
	assertAuditReady("during " + mode + " exporter outage")
	if mode == "http503" && rejected == 0 {
		t.Fatal("collector did not observe a refused logs request")
	}
	if mode == "http503" && requests <= requestsBefore {
		t.Fatal("collector did not receive a logs request")
	}
	if mode == "refusal" && rejected != 0 {
		t.Fatalf("closed collector unexpectedly returned HTTP responses: %d", rejected)
	}
	if mode == "refusal" && requests != requestsBefore {
		t.Fatalf("closed collector received logs requests: before=%d after=%d", requestsBefore, requests)
	}
	if mode == "stall" && stalled == 0 {
		deadline := time.Now().Add(5 * time.Second)
		for stalled == 0 && time.Now().Before(deadline) {
			time.Sleep(25 * time.Millisecond)
			obsBodiesMu.Lock()
			stalled = obsStalledLogs - stalledBefore
			obsBodiesMu.Unlock()
		}
		if stalled == 0 {
			t.Fatal("collector did not read a logs request and withhold its response")
		}
		obsBodiesMu.Lock()
		requests := obsLogRequests
		obsBodiesMu.Unlock()
		if requests <= requestsBefore {
			t.Fatal("collector did not receive a logs request")
		}
	}
	if mode == "stall" {
		obsBodiesMu.Lock()
		close(obsStallGate)
		obsStallGate = nil
		obsStallLogs = false
		obsBodiesMu.Unlock()
	}
	if mode == "refusal" {
		obsRestartReceiver(t, endpoint)
		obsRecvClosed = false
	}
	setOutage(false)
	sc, err := ParseTraceparent(pureTP)
	if err != nil {
		t.Fatal(err)
	}
	if err := EmitLog(slog.LevelWarn, "app.outage", marker+"recovered", []Attr{StringAttr("phase", "recovered")}, &sc); err != nil {
		t.Fatal(err)
	}
	var recovered []byte
	waitFor(t, "recovered application record", func() bool {
		_ = FlushObservability(time.Second)
		obsBodiesMu.Lock()
		defer obsBodiesMu.Unlock()
		for _, body := range obsLogBodies {
			if bytes.Contains(body, []byte(marker+"recovered")) {
				recovered = append([]byte(nil), body...)
				return true
			}
		}
		return false
	})
	for _, expected := range [][]byte{[]byte("app.outage"), []byte("phase"), sc.TraceID[:], sc.SpanID[:]} {
		if !bytes.Contains(recovered, expected) {
			t.Fatalf("recovered payload lacks %x", expected)
		}
	}
	assertAuditReady("after " + mode + " exporter recovery")
	for _, record := range obsLogs.all() {
		message, _ := record["msg"].(string)
		if record["aster_origin"] == "rust" && (strings.Contains(message, marker+"outage-") || strings.Contains(message, marker+"recovered")) {
			t.Fatal("application record fed back into Rust log bridge")
		}
	}
	if err := FlushObservability(5 * time.Second); err != nil {
		t.Fatalf("flush recovery before shutdown outage: %v", err)
	}
	obsBodiesMu.Lock()
	shutdownRequestsBefore := obsLogRequests
	shutdownRejectedBefore := obsRejectedLogs
	shutdownStalledBefore := obsStalledLogs
	if mode != "refusal" {
		obsRejectLogs = true
	}
	if mode == "stall" {
		obsStallLogs = true
		obsStallGate = make(chan struct{})
	}
	obsBodiesMu.Unlock()
	if mode == "refusal" {
		obsRecv.Close()
		obsRecvClosed = true
		probe, err := net.Listen("tcp", endpoint)
		if err != nil {
			t.Fatalf("collector endpoint did not become unbound for shutdown: %v", err)
		}
		probe.Close()
	}
	if err := EmitLog(slog.LevelWarn, "app.outage", marker+"shutdown-outage", nil, nil); err != nil {
		t.Fatal(err)
	}
	if err := health.Close(); err != nil {
		t.Fatalf("close audit listener before shutdown measurement: %v", err)
	}
	if err := node.Close(); err != nil {
		t.Fatalf("close audit node before shutdown measurement: %v", err)
	}
	start = time.Now()
	held, err := ShutdownObservability(time.Second)
	obs.mu.Lock()
	obsOurs = false
	obs.mu.Unlock()
	if (err != nil && !errors.Is(err, ErrTimeout)) || !held {
		t.Fatalf("outage shutdown: held=%v err=%v", held, err)
	}
	if elapsed := time.Since(start); elapsed >= 3*time.Second {
		t.Fatalf("outage shutdown exceeded bound: %s", elapsed)
	}
	obsBodiesMu.Lock()
	shutdownRequests := obsLogRequests
	shutdownRejected := obsRejectedLogs
	shutdownStalled := obsStalledLogs
	obsBodiesMu.Unlock()
	if mode == "refusal" {
		if shutdownRequests != shutdownRequestsBefore || shutdownRejected != shutdownRejectedBefore {
			t.Fatalf("shutdown logs reached closed collector: requests %d->%d, responses %d->%d", shutdownRequestsBefore, shutdownRequests, shutdownRejectedBefore, shutdownRejected)
		}
	} else if mode == "stall" {
		if shutdownStalled <= shutdownStalledBefore || shutdownRequests <= shutdownRequestsBefore {
			t.Fatalf("shutdown did not reach a held collector request: stalled %d->%d, requests %d->%d", shutdownStalledBefore, shutdownStalled, shutdownRequestsBefore, shutdownRequests)
		}
		obsBodiesMu.Lock()
		close(obsStallGate)
		obsStallGate = nil
		obsStallLogs = false
		obsBodiesMu.Unlock()
	} else if shutdownRejected <= shutdownRejectedBefore || shutdownRequests <= shutdownRequestsBefore {
		t.Fatalf("shutdown exporter request was not rejected: requests %d->%d, responses %d->%d", shutdownRequestsBefore, shutdownRequests, shutdownRejectedBefore, shutdownRejected)
	}
	if held, err := ShutdownObservability(time.Second); err != nil || held {
		t.Fatalf("shutdown left a pipeline reference: held=%v err=%v", held, err)
	}
}
