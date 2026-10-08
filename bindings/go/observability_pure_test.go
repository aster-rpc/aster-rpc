package aster

// Pure-Go tests of the observability codecs, the context carrier, the slog
// handler wrapper and the Rust-record-to-slog mapping. No cgo: these also run
// under CGO_ENABLED=0.

import (
	"context"
	"errors"
	"log/slog"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"
)

const (
	pureTraceID = "4bf92f3577b34da6a3ce929d0e0e4736"
	pureSpanID  = "00f067aa0ba902b7"
	pureTP      = "00-" + pureTraceID + "-" + pureSpanID + "-01"
)

func TestObsPure_TraceparentRoundTrip(t *testing.T) {
	sc, err := ParseTraceparent(pureTP)
	if err != nil {
		t.Fatal(err)
	}
	if sc.TraceIDString() != pureTraceID || sc.SpanIDString() != pureSpanID || !sc.Sampled() || !sc.Remote || !sc.IsValid() {
		t.Fatalf("parsed %+v", sc)
	}
	if got := sc.Traceparent(); got != pureTP {
		t.Fatalf("Traceparent() = %q", got)
	}
	// Unsampled flags survive; a future version with extra fields parses.
	sc.Flags = 0
	if got := sc.Traceparent(); !strings.HasSuffix(got, "-00") {
		t.Fatalf("flags: %q", got)
	}
	if _, err := ParseTraceparent("01-" + pureTraceID + "-" + pureSpanID + "-01-extra"); err != nil {
		t.Fatalf("future version: %v", err)
	}
	if (SpanContext{}).Traceparent() != "" {
		t.Fatal("an invalid context formats to the empty string")
	}
}

func TestObsPure_TraceparentRejectsMalformed(t *testing.T) {
	for name, s := range map[string]string{
		"empty":        "",
		"short":        "00-abc",
		"version ff":   "ff-" + pureTraceID + "-" + pureSpanID + "-01",
		"uppercase":    "00-" + strings.ToUpper(pureTraceID) + "-" + pureSpanID + "-01",
		"zero trace":   "00-" + strings.Repeat("0", 32) + "-" + pureSpanID + "-01",
		"zero span":    "00-" + pureTraceID + "-" + strings.Repeat("0", 16) + "-01",
		"trailing v00": pureTP + "-x",
		"bad dash":     "00_" + pureTraceID + "-" + pureSpanID + "-01",
		"non hex":      "00-" + strings.Repeat("g", 32) + "-" + pureSpanID + "-01",
	} {
		if _, err := ParseTraceparent(s); !errors.Is(err, ErrInvalidTraceContext) {
			t.Errorf("%s: err = %v", name, err)
		}
	}
}

func TestObsPure_Baggage(t *testing.T) {
	in := map[string]string{"tenant": "acme", "k": "v w", "uni": "é,;="}
	header := FormatBaggage(in)
	if header != "k=v%20w,tenant=acme,uni=%C3%A9%2C%3B%3D" {
		t.Fatalf("format: %q", header)
	}
	if got := ParseBaggage(header); !reflect.DeepEqual(got, in) {
		t.Fatalf("round trip: %v", got)
	}
	// Properties after ';' are dropped, malformed members skipped.
	if got := ParseBaggage("a=1;prop=x, b = 2 ,junk,=novalue"); !reflect.DeepEqual(got, map[string]string{"a": "1", "b": "2"}) {
		t.Fatalf("lenient parse: %v", got)
	}
	// W3C limits: the whole header is dropped.
	if ParseBaggage(strings.Repeat("a=b,", 64)+"c=d") != nil {
		t.Fatal("65 members must be dropped")
	}
	if ParseBaggage("k="+strings.Repeat("x", baggageMaxBytes)) != nil {
		t.Fatal("an oversized header must be dropped")
	}
	if FormatBaggage(nil) != "" || ParseBaggage("") != nil {
		t.Fatal("empty baggage")
	}
}

func TestObsPure_AttrCodec(t *testing.T) {
	attrs := []Attr{StringAttr("a", "1"), IntAttr("n", -7), BoolAttr("ok", true), FloatAttr("f", 0.5), StringAttr("empty", "")}
	enc := encodeAttrs(attrs)
	// u32 LE count, then u32 LE len + key, u32 LE len + value.
	if enc[0] != 5 || enc[1] != 0 || enc[4] != 1 || enc[8] != 'a' {
		t.Fatalf("layout: % x", enc[:12])
	}
	got, err := decodeAttrs(enc)
	if err != nil || !reflect.DeepEqual(got, attrs) {
		t.Fatalf("round trip: %v %v", got, err)
	}
	if got[1].Value != "-7" || got[2].Value != "true" || got[3].Value != "0.5" {
		t.Fatalf("typed constructors stringify: %+v", got)
	}
	if encodeAttrs(nil) != nil {
		t.Fatal("no attributes encode to nil")
	}
	if _, err := decodeAttrs(enc[:len(enc)-1]); err == nil {
		t.Fatal("a truncated list must fail")
	}
	if _, err := decodeAttrs([]byte{1, 0}); err == nil {
		t.Fatal("a short header must fail")
	}
	if kl := encodeKeyList([]string{"ab", "c"}); !reflect.DeepEqual(kl, []byte{2, 0, 0, 0, 2, 0, 0, 0, 'a', 'b', 1, 0, 0, 0, 'c'}) {
		t.Fatalf("key list: % x", kl)
	}
}

func TestObsPure_ContextCarrier(t *testing.T) {
	ctx := context.Background()
	if CurrentTraceID(ctx) != "" || CurrentSpanID(ctx) != "" || CurrentTraceparent(ctx) != "" || len(CurrentBaggage(ctx)) != 0 {
		t.Fatal("an empty context has no trace")
	}
	if _, err := WithTraceContext(ctx, "nonsense", nil); !errors.Is(err, ErrInvalidTraceContext) {
		t.Fatalf("invalid traceparent: %v", err)
	}
	bag := map[string]string{"tenant": "acme"}
	ctx2, err := WithTraceContext(ctx, pureTP, bag)
	if err != nil {
		t.Fatal(err)
	}
	bag["tenant"] = "mutated" // the carrier holds a copy
	if CurrentTraceID(ctx2) != pureTraceID || CurrentSpanID(ctx2) != pureSpanID || CurrentTraceparent(ctx2) != pureTP {
		t.Fatal("ids")
	}
	got := CurrentBaggage(ctx2)
	if got["tenant"] != "acme" {
		t.Fatalf("baggage %v", got)
	}
	got["tenant"] = "x" // and hands out copies
	if CurrentBaggage(ctx2)["tenant"] != "acme" {
		t.Fatal("CurrentBaggage must return a copy")
	}
	tp, bg := traceHeaders(ctx2)
	if tp != pureTP || bg != "tenant=acme" {
		t.Fatalf("headers %q %q", tp, bg)
	}
	// Baggage without a span context (a call whose span is not exported).
	ctx3 := WithBaggage(ctx, map[string]string{"k": "v"})
	tp, bg = traceHeaders(ctx3)
	if CurrentTraceID(ctx3) != "" || tp != "" || bg != "k=v" {
		t.Fatalf("baggage only: %q %q", tp, bg)
	}
	// WithBaggage keeps the trace context.
	if CurrentTraceID(WithBaggage(ctx2, nil)) != pureTraceID {
		t.Fatal("WithBaggage dropped the trace context")
	}
}

// capture is an slog.Handler that records what it is given.
type capture struct {
	mu      sync.Mutex
	level   slog.Level
	records []map[string]any
}

func (c *capture) Enabled(_ context.Context, l slog.Level) bool { return l >= c.level }
func (c *capture) Handle(_ context.Context, r slog.Record) error {
	m := map[string]any{"msg": r.Message, "level": r.Level, "time": r.Time}
	r.Attrs(func(a slog.Attr) bool { m[a.Key] = a.Value.Any(); return true })
	c.mu.Lock()
	c.records = append(c.records, m)
	c.mu.Unlock()
	return nil
}
func (c *capture) WithAttrs([]slog.Attr) slog.Handler { return c }
func (c *capture) WithGroup(string) slog.Handler      { return c }
func (c *capture) all() []map[string]any {
	c.mu.Lock()
	defer c.mu.Unlock()
	return append([]map[string]any(nil), c.records...)
}

func TestObsPure_TraceHandlerAddsIDs(t *testing.T) {
	cap := &capture{level: slog.LevelDebug}
	l := slog.New(NewTraceHandler(cap))
	ctx, err := WithTraceContext(context.Background(), pureTP, nil)
	if err != nil {
		t.Fatal(err)
	}
	l.InfoContext(ctx, "inside")
	l.Info("outside")
	l.DebugContext(context.Background(), "no trace")
	recs := cap.all()
	if len(recs) != 3 {
		t.Fatalf("%d records", len(recs))
	}
	if recs[0]["trace_id"] != pureTraceID || recs[0]["span_id"] != pureSpanID {
		t.Fatalf("with trace: %v", recs[0])
	}
	if _, ok := recs[1]["trace_id"]; ok {
		t.Fatalf("a record without a trace gets no ids: %v", recs[1])
	}
	if l.Enabled(ctx, slog.LevelDebug-1) {
		t.Fatal("Enabled must follow the wrapped handler")
	}
	if _, ok := l.With("k", "v").Handler().(*TraceHandler); !ok {
		t.Fatal("WithAttrs must keep the wrapper")
	}
	if _, ok := l.WithGroup("g").Handler().(*TraceHandler); !ok {
		t.Fatal("WithGroup must keep the wrapper")
	}
}

func TestObsPure_RustRecordsReachSlog(t *testing.T) {
	cap := &capture{level: LevelTrace}
	l := slog.New(cap)
	when := time.Unix(1_700_000_000, 123_000_000)
	rec := &logRecord{
		level: 5, time: when, target: "aster::rpc::dispatch", message: "rpc completed",
		fields:  []Attr{{"rpc.service", "Echo"}, {"rpc.method", "echo"}, {"thread.name", "tokio"}},
		traceID: [16]byte{0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e, 0x47, 0x36},
		spanID:  [8]byte{0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7},
		callID:  42, hasTrace: true, inCall: true,
	}
	deliverLog(l, rec)
	rec2 := &logRecord{level: 2, time: when, target: "aster::audit", message: "denied", audit: true, synthetic: true}
	deliverLog(l, rec2)
	recs := cap.all()
	if len(recs) != 2 {
		t.Fatalf("%d records", len(recs))
	}
	r := recs[0]
	if r["level"] != LevelTrace || r["msg"] != "rpc completed" || r["logger"] != "aster.rpc.dispatch" || r["aster_origin"] != "rust" ||
		r["trace_id"] != pureTraceID || r["span_id"] != pureSpanID || r["aster_call_id"] != uint64(42) ||
		r["rpc.service"] != "Echo" || r["rpc.method"] != "echo" || r["thread.name"] != "tokio" || !r["time"].(time.Time).Equal(when) {
		t.Fatalf("record %v", r)
	}
	r = recs[1]
	if r["level"] != slog.LevelWarn || r["aster_audit"] != true || r["aster_synthetic"] != true {
		t.Fatalf("audit record %v", r)
	}
	if _, ok := r["trace_id"]; ok {
		t.Fatalf("no ids outside a span: %v", r)
	}
	// A record the logger has disabled is dropped before any work.
	quiet := slog.New(&capture{level: slog.LevelWarn})
	quiet.Handler().(*capture).level = slog.LevelError
	deliverLog(quiet, &logRecord{level: 3, target: "a::b", message: "info"})
	if len(quiet.Handler().(*capture).all()) != 0 {
		t.Fatal("a disabled level must not be delivered")
	}
	// The records carry their ids as attributes, so a TraceHandler around the
	// logger's handler (with a plain context) does not add them twice.
	cap2 := &capture{level: LevelTrace}
	deliverLog(slog.New(NewTraceHandler(cap2)), rec)
	if got := cap2.all()[0]["trace_id"]; got != pureTraceID {
		t.Fatalf("trace_id = %v", got)
	}
}

func TestObsPure_LevelMapping(t *testing.T) {
	for in, want := range map[uint8]slog.Level{1: slog.LevelError, 2: slog.LevelWarn, 3: slog.LevelInfo, 4: slog.LevelDebug, 5: LevelTrace} {
		if got := slogLevel(in); got != want {
			t.Errorf("slogLevel(%d) = %v, want %v", in, got, want)
		}
	}
	if loggerName("aster::rpc::dispatch") != "aster.rpc.dispatch" {
		t.Error("logger name")
	}
	for l, want := range map[slog.Level]string{
		LevelTrace: "warn,aster=trace", slog.LevelDebug: "warn,aster=debug", slog.LevelInfo: "warn,aster=info",
		slog.LevelWarn: "warn,aster=warn", slog.LevelError: "warn,aster=error", slog.LevelError + 4: "warn,aster=error",
	} {
		if got := levelDirective(l); got != want {
			t.Errorf("levelDirective(%v) = %q, want %q", l, got, want)
		}
	}
	for _, l := range []slog.Level{LevelTrace, slog.LevelDebug, slog.LevelInfo, slog.LevelWarn, slog.LevelError} {
		if got := handlerLevel(&capture{level: l}); got != l {
			t.Errorf("handlerLevel(min %v) = %v", l, got)
		}
	}
	if got := handlerLevel(&capture{level: slog.LevelError + 8}); got != slog.LevelError {
		t.Errorf("above error: %v", got)
	}
}

func TestConcurrentTraceCarriersRemainIndependent(t *testing.T) {
	contexts := make([]context.Context, 2)
	traces := []string{strings.Repeat("1", 32), strings.Repeat("2", 32)}
	for i, trace := range traces {
		var err error
		contexts[i], err = WithTraceContext(context.Background(), "00-"+trace+"-00f067aa0ba902b7-01", map[string]string{"tenant": trace})
		if err != nil {
			t.Fatal(err)
		}
	}
	entered := make(chan struct{}, 2)
	release := make(chan struct{})
	results := make(chan bool, 2)
	for i, ctx := range contexts {
		go func(i int, ctx context.Context) {
			entered <- struct{}{}
			<-release
			results <- CurrentTraceID(ctx) == traces[i] && CurrentBaggage(ctx)["tenant"] == traces[i]
		}(i, ctx)
	}
	for range contexts {
		<-entered
	}
	close(release)
	for range contexts {
		if !<-results {
			t.Fatal("concurrent carrier crossed contexts")
		}
	}
	if CurrentTraceID(context.Background()) != "" {
		t.Fatal("background context inherited a trace")
	}
}
