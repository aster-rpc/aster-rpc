package aster

// Observability, pure-Go half (no cgo): attributes and their wire encoding,
// W3C trace context and baggage, the context.Context carrier, the slog
// handler wrapper that adds trace ids, and the mapping of Rust log records to
// slog. Everything that talks to the native library lives in observability.go,
// metrics.go, tracing.go and health.go; this file builds with CGO_ENABLED=0
// so the codecs are testable without the library.

import (
	"context"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"log/slog"
	"net/url"
	"sort"
	"strconv"
	"strings"
	"time"
)

// ─── Attributes ────────────────────────────────────────────────────────────

// Attr is a string-valued attribute for metrics, spans and span events. ABI
// 4.6 carries string values only; the typed constructors stringify.
type Attr struct {
	Key   string
	Value string
}

// StringAttr is an attribute with a string value.
func StringAttr(key, value string) Attr { return Attr{key, value} }

// IntAttr is an attribute with a decimal integer value.
func IntAttr(key string, value int64) Attr { return Attr{key, strconv.FormatInt(value, 10)} }

// BoolAttr is an attribute with the value "true" or "false".
func BoolAttr(key string, value bool) Attr { return Attr{key, strconv.FormatBool(value)} }

// FloatAttr is an attribute with a floating-point value.
func FloatAttr(key string, value float64) Attr {
	return Attr{key, strconv.FormatFloat(value, 'g', -1, 64)}
}

// encodeAttrs encodes the C ABI k/v list: u32 LE count, then per entry u32 LE
// key length + key, u32 LE value length + value. No attributes = nil.
func encodeAttrs(attrs []Attr) []byte {
	if len(attrs) == 0 {
		return nil
	}
	size := 4
	for _, a := range attrs {
		size += 8 + len(a.Key) + len(a.Value)
	}
	b := make([]byte, 0, size)
	b = binary.LittleEndian.AppendUint32(b, uint32(len(attrs)))
	for _, a := range attrs {
		b = binary.LittleEndian.AppendUint32(b, uint32(len(a.Key)))
		b = append(b, a.Key...)
		b = binary.LittleEndian.AppendUint32(b, uint32(len(a.Value)))
		b = append(b, a.Value...)
	}
	return b
}

// decodeAttrs decodes a k/v list (order kept). Empty input = no entries.
func decodeAttrs(b []byte) ([]Attr, error) {
	if len(b) == 0 {
		return nil, nil
	}
	if len(b) < 4 {
		return nil, fmt.Errorf("k/v list: %w", errMalformed)
	}
	n := binary.LittleEndian.Uint32(b)
	b = b[4:]
	out := make([]Attr, 0, min(int(n), 64))
	next := func() (string, bool) {
		if len(b) < 4 {
			return "", false
		}
		l := binary.LittleEndian.Uint32(b)
		if uint64(len(b)-4) < uint64(l) {
			return "", false
		}
		s := string(b[4 : 4+l])
		b = b[4+l:]
		return s, true
	}
	for i := uint32(0); i < n; i++ {
		k, ok1 := next()
		v, ok2 := next()
		if !ok1 || !ok2 {
			return out, fmt.Errorf("k/v list truncated: %w", errMalformed)
		}
		out = append(out, Attr{k, v})
	}
	return out, nil
}

// encodeKeyList encodes a list of keys (u32 LE count, then u32 LE len + key).
func encodeKeyList(keys []string) []byte {
	if len(keys) == 0 {
		return nil
	}
	b := binary.LittleEndian.AppendUint32(nil, uint32(len(keys)))
	for _, k := range keys {
		b = binary.LittleEndian.AppendUint32(b, uint32(len(k)))
		b = append(b, k...)
	}
	return b
}

// ─── W3C trace context ─────────────────────────────────────────────────────

// SpanContext identifies a span: trace id, span id and W3C trace flags.
type SpanContext struct {
	TraceID [16]byte
	SpanID  [8]byte
	// Flags are the W3C trace flags; bit 0 is "sampled".
	Flags byte
	// Remote is set when the context came from another process.
	Remote bool
}

// IsValid reports whether both ids are non-zero.
func (sc SpanContext) IsValid() bool {
	return sc.TraceID != [16]byte{} && sc.SpanID != [8]byte{}
}

// Sampled reports the W3C sampled flag.
func (sc SpanContext) Sampled() bool { return sc.Flags&1 != 0 }

// TraceIDString is the trace id as 32 lowercase hex characters.
func (sc SpanContext) TraceIDString() string { return hex.EncodeToString(sc.TraceID[:]) }

// SpanIDString is the span id as 16 lowercase hex characters.
func (sc SpanContext) SpanIDString() string { return hex.EncodeToString(sc.SpanID[:]) }

// Traceparent is the W3C traceparent header value
// ("00-<trace id>-<span id>-<flags>"), or "" for an invalid context.
func (sc SpanContext) Traceparent() string {
	if !sc.IsValid() {
		return ""
	}
	var b [55]byte
	b[0], b[1], b[2] = '0', '0', '-'
	hex.Encode(b[3:35], sc.TraceID[:])
	b[35] = '-'
	hex.Encode(b[36:52], sc.SpanID[:])
	b[52] = '-'
	hex.Encode(b[53:55], []byte{sc.Flags})
	return string(b[:])
}

// ParseTraceparent parses a W3C traceparent header value. The result is
// marked remote.
func ParseTraceparent(s string) (SpanContext, error) {
	bad := func(why string) (SpanContext, error) {
		return SpanContext{}, fmt.Errorf("traceparent %q: %s: %w", s, why, ErrInvalidTraceContext)
	}
	if len(s) < 55 || s[2] != '-' || s[35] != '-' || s[52] != '-' {
		return bad("malformed")
	}
	if s[:2] == "ff" {
		return bad("version ff")
	}
	if s[:2] == "00" && len(s) != 55 {
		return bad("trailing data")
	}
	if len(s) > 55 && s[55] != '-' {
		return bad("malformed")
	}
	lower := func(part string) bool {
		for i := 0; i < len(part); i++ {
			c := part[i]
			if (c < '0' || c > '9') && (c < 'a' || c > 'f') {
				return false
			}
		}
		return true
	}
	if !lower(s[:2]) || !lower(s[3:35]) || !lower(s[36:52]) || !lower(s[53:55]) {
		return bad("not lowercase hex")
	}
	var sc SpanContext
	_, _ = hex.Decode(sc.TraceID[:], []byte(s[3:35]))
	_, _ = hex.Decode(sc.SpanID[:], []byte(s[36:52]))
	var fl [1]byte
	_, _ = hex.Decode(fl[:], []byte(s[53:55]))
	sc.Flags, sc.Remote = fl[0], true
	if !sc.IsValid() {
		return bad("all-zero id")
	}
	return sc, nil
}

// W3C baggage limits.
const (
	baggageMaxBytes   = 8192
	baggageMaxMembers = 64
)

// FormatBaggage renders baggage as a W3C baggage header value (members sorted
// by key, keys and values percent-encoded).
func FormatBaggage(b map[string]string) string {
	if len(b) == 0 {
		return ""
	}
	keys := make([]string, 0, len(b))
	for k := range b {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	var sb strings.Builder
	for i, k := range keys {
		if i > 0 {
			sb.WriteByte(',')
		}
		sb.WriteString(percentEncode(k))
		sb.WriteByte('=')
		sb.WriteString(percentEncode(b[k]))
	}
	return sb.String()
}

func percentEncode(s string) string {
	const upper = "0123456789ABCDEF"
	var sb strings.Builder
	for i := 0; i < len(s); i++ {
		c := s[i]
		if c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '-' || c == '_' || c == '.' || c == '~' {
			sb.WriteByte(c)
			continue
		}
		sb.WriteByte('%')
		sb.WriteByte(upper[c>>4])
		sb.WriteByte(upper[c&15])
	}
	return sb.String()
}

// ParseBaggage parses a W3C baggage header value (member properties after
// ';' are dropped). A header over the W3C limits (8192 bytes, 64 members)
// yields nil, as Rust drops it.
func ParseBaggage(header string) map[string]string {
	if header == "" || len(header) > baggageMaxBytes || strings.Count(header, ",") >= baggageMaxMembers {
		return nil
	}
	out := map[string]string{}
	for _, member := range strings.Split(header, ",") {
		pair, _, _ := strings.Cut(member, ";")
		k, v, ok := strings.Cut(pair, "=")
		k = strings.TrimSpace(k)
		if !ok || k == "" {
			continue
		}
		dk, err1 := url.PathUnescape(k)
		dv, err2 := url.PathUnescape(strings.TrimSpace(v))
		if err1 != nil || err2 != nil {
			continue
		}
		out[dk] = dv
	}
	if len(out) == 0 {
		return nil
	}
	return out
}

// ─── The context.Context carrier ───────────────────────────────────────────

// traceCarrier is what rides in a context.Context. A call with inbound
// baggage but no exported server span has baggage and no valid span context.
type traceCarrier struct {
	sc      SpanContext
	baggage map[string]string
}

type traceCarrierKey struct{}

func carrierFrom(ctx context.Context) *traceCarrier {
	if ctx == nil {
		return nil
	}
	c, _ := ctx.Value(traceCarrierKey{}).(*traceCarrier)
	return c
}

func contextWithCarrier(ctx context.Context, sc SpanContext, baggage map[string]string) context.Context {
	return context.WithValue(ctx, traceCarrierKey{}, &traceCarrier{sc: sc, baggage: baggage})
}

// WithTraceContext returns a context whose current trace context is
// traceparent (and baggage, which may be nil). Use it to continue a trace
// that arrived by other means (an HTTP header, a message queue); outbound
// Aster calls made with the context continue it. An invalid traceparent is
// an error wrapping ErrInvalidTraceContext.
func WithTraceContext(ctx context.Context, traceparent string, baggage map[string]string) (context.Context, error) {
	sc, err := ParseTraceparent(traceparent)
	if err != nil {
		return ctx, err
	}
	return contextWithCarrier(ctx, sc, copyBaggage(baggage)), nil
}

// WithBaggage returns a context with the given baggage in place of the current
// one (the trace context is kept).
func WithBaggage(ctx context.Context, baggage map[string]string) context.Context {
	var sc SpanContext
	if c := carrierFrom(ctx); c != nil {
		sc = c.sc
	}
	return contextWithCarrier(ctx, sc, copyBaggage(baggage))
}

func copyBaggage(b map[string]string) map[string]string {
	if len(b) == 0 {
		return nil
	}
	out := make(map[string]string, len(b))
	for k, v := range b {
		out[k] = v
	}
	return out
}

// SpanContextFromContext is the current span context (inside an RPC handler:
// the call's server span; inside StartSpan: that span).
func SpanContextFromContext(ctx context.Context) (SpanContext, bool) {
	if c := carrierFrom(ctx); c != nil && c.sc.IsValid() {
		return c.sc, true
	}
	return SpanContext{}, false
}

// CurrentTraceID is the current trace id (32 lowercase hex characters), or "".
func CurrentTraceID(ctx context.Context) string {
	if sc, ok := SpanContextFromContext(ctx); ok {
		return sc.TraceIDString()
	}
	return ""
}

// CurrentSpanID is the current span id (16 lowercase hex characters), or "".
func CurrentSpanID(ctx context.Context) string {
	if sc, ok := SpanContextFromContext(ctx); ok {
		return sc.SpanIDString()
	}
	return ""
}

// CurrentTraceparent is the current W3C traceparent, or "".
func CurrentTraceparent(ctx context.Context) string {
	if sc, ok := SpanContextFromContext(ctx); ok {
		return sc.Traceparent()
	}
	return ""
}

// CurrentBaggage is a copy of the current W3C baggage (inside an RPC handler:
// what the caller sent). Baggage values end up wherever you log them: never
// put secrets in baggage.
func CurrentBaggage(ctx context.Context) map[string]string {
	if c := carrierFrom(ctx); c != nil {
		return copyBaggage(c.baggage)
	}
	return map[string]string{}
}

// traceHeaders are the values an outbound call writes into its metadata:
// the current traceparent and the formatted baggage ("" = nothing to send).
func traceHeaders(ctx context.Context) (traceparent, baggage string) {
	c := carrierFrom(ctx)
	if c == nil {
		return "", ""
	}
	return c.sc.Traceparent(), FormatBaggage(c.baggage)
}

// ─── slog: trace ids on the application's own records ──────────────────────

// LevelTrace is the slog level of Rust TRACE events (-8, below Debug).
const LevelTrace = slog.Level(-8)

// TraceHandler wraps an slog.Handler and adds "trace_id" and "span_id" from
// the record's context.Context (Logger.InfoContext and friends). Records
// logged without a trace context pass through unchanged.
type TraceHandler struct {
	inner slog.Handler
}

// NewTraceHandler wraps inner.
//
//	logger := slog.New(aster.NewTraceHandler(slog.NewJSONHandler(os.Stderr, nil)))
//	logger.InfoContext(ctx, "charging card") // trace_id and span_id when ctx has a trace
func NewTraceHandler(inner slog.Handler) *TraceHandler { return &TraceHandler{inner: inner} }

// Enabled implements slog.Handler.
func (h *TraceHandler) Enabled(ctx context.Context, l slog.Level) bool {
	return h.inner.Enabled(ctx, l)
}

// Handle implements slog.Handler.
func (h *TraceHandler) Handle(ctx context.Context, r slog.Record) error {
	if sc, ok := SpanContextFromContext(ctx); ok {
		r.AddAttrs(slog.String("trace_id", sc.TraceIDString()), slog.String("span_id", sc.SpanIDString()))
	}
	return h.inner.Handle(ctx, r)
}

// WithAttrs implements slog.Handler.
func (h *TraceHandler) WithAttrs(attrs []slog.Attr) slog.Handler {
	return &TraceHandler{inner: h.inner.WithAttrs(attrs)}
}

// WithGroup implements slog.Handler.
func (h *TraceHandler) WithGroup(name string) slog.Handler {
	return &TraceHandler{inner: h.inner.WithGroup(name)}
}

// ─── Rust log records -> slog ──────────────────────────────────────────────

// logRecord is one Rust log record copied out of the poll arena.
type logRecord struct {
	level     uint8 // 1 error .. 5 trace
	flags     uint8
	time      time.Time
	target    string
	message   string
	fields    []Attr
	traceID   [16]byte
	spanID    [8]byte
	callID    uint64
	hasTrace  bool
	inCall    bool
	synthetic bool
	audit     bool
}

// aster_log_record_t.flags.
const (
	logFlagHasTrace  = 1
	logFlagInCall    = 2
	logFlagSynthetic = 4
	logFlagAudit     = 8
)

// slogLevel maps aster_log_level_t to slog (TRACE is LevelTrace).
func slogLevel(l uint8) slog.Level {
	switch l {
	case 1:
		return slog.LevelError
	case 2:
		return slog.LevelWarn
	case 3:
		return slog.LevelInfo
	case 4:
		return slog.LevelDebug
	default:
		return LevelTrace
	}
}

// loggerName maps a Rust target "a::b::c" to the logger name "a.b.c".
func loggerName(target string) string { return strings.ReplaceAll(target, "::", ".") }

// deliverLog hands rec to l after an Enabled check. The record's own ids ride
// as attributes (the context stays plain, so a TraceHandler in l does not add
// them twice). Attributes: logger, aster_origin=rust, trace_id/span_id (inside
// an exported span), aster_call_id, aster_audit, then the event's fields.
func deliverLog(l *slog.Logger, rec *logRecord) {
	level := slogLevel(rec.level)
	ctx := context.Background()
	if !l.Enabled(ctx, level) {
		return
	}
	r := slog.NewRecord(rec.time, level, rec.message, 0)
	r.AddAttrs(slog.String("logger", loggerName(rec.target)), slog.String("aster_origin", "rust"))
	if rec.hasTrace {
		r.AddAttrs(slog.String("trace_id", hex.EncodeToString(rec.traceID[:])),
			slog.String("span_id", hex.EncodeToString(rec.spanID[:])))
	}
	if rec.inCall {
		r.AddAttrs(slog.Uint64("aster_call_id", rec.callID))
	}
	if rec.audit {
		r.AddAttrs(slog.Bool("aster_audit", true))
	}
	if rec.synthetic {
		r.AddAttrs(slog.Bool("aster_synthetic", true))
	}
	for _, f := range rec.fields {
		r.AddAttrs(slog.String(f.Key, f.Value))
	}
	_ = l.Handler().Handle(ctx, r)
}

// handlerLevel is the lowest slog level h has enabled, probing the levels the
// bridge can deliver (trace, debug, info, warn, error); above error is error.
func handlerLevel(h slog.Handler) slog.Level {
	ctx := context.Background()
	for _, l := range []slog.Level{LevelTrace, slog.LevelDebug, slog.LevelInfo, slog.LevelWarn} {
		if h.Enabled(ctx, l) {
			return l
		}
	}
	return slog.LevelError
}

// levelDirective is the Rust filter that matches an slog level: everything at
// warn, the aster crates at the logger's level (the Python binding's rule).
func levelDirective(l slog.Level) string {
	name := "error"
	switch {
	case l <= LevelTrace:
		name = "trace"
	case l <= slog.LevelDebug:
		name = "debug"
	case l <= slog.LevelInfo:
		name = "info"
	case l <= slog.LevelWarn:
		name = "warn"
	}
	return "warn,aster=" + name
}

// ErrInvalidTraceContext: a malformed W3C traceparent.
var ErrInvalidTraceContext = errors.New("invalid traceparent")

var errMalformed = errors.New("malformed")

// errNoPipeline is returned where a function needs a running pipeline.
var errNoPipeline = errors.New("aster: observability is not initialised")
