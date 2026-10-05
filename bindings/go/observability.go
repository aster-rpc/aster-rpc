//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"fmt"
	"log/slog"
	"os"
	"runtime"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"
)

// Observability (ABI 4.6): the whole pipeline lives in Rust (OpenTelemetry
// SDK, exporters, the tracing subscriber, the Prometheus renderer, the log
// bridge). This file starts and stops it and delivers Rust log records to
// log/slog; metrics.go, tracing.go and health.go wrap the rest.
//
// Without any call, StartServer and StartHealth run EnsureObservability: the
// log bridge opens (Rust logs appear in slog) and exporters start only when
// OTEL_* asks for them. To configure the pipeline yourself, call
// InitObservability before that.

// ObservabilityConfig configures the pipeline. The zero value is the implicit
// start: environment (OTEL_* / ASTER_*) applied, log bridge on. Precedence,
// highest first: these fields, the environment, ConfigTOML, defaults.
type ObservabilityConfig struct {
	// ServiceName is the service.name resource attribute.
	ServiceName string
	// OTLPEndpoint is the OTLP/HTTP collector base URL.
	OTLPEndpoint string
	// OTLPHeaders are request headers (often a bearer token). Never logged.
	OTLPHeaders map[string]string
	// ResourceAttributes are extra resource attributes.
	ResourceAttributes map[string]string
	// Traces, Metrics and OTLPLogs export that signal over OTLP when true;
	// false lets OTEL_* decide (traces and logs default off without an
	// endpoint).
	Traces, Metrics, OTLPLogs bool
	// SampleRatio is a parent-based trace-id ratio sampler in [0, 1]; nil
	// lets OTEL_TRACES_SAMPLER decide.
	SampleRatio *float64
	// LogFilter is EnvFilter directives for Rust events (ASTER_LOG), e.g.
	// "warn,aster=debug". It pins the Rust filter; without it Rust follows
	// the slog logger's level (SetLogger / SyncLogLevel).
	LogFilter string
	// ConfigTOML is the [observability] table (header optional), used below
	// the environment: the place for attribute_extractors and
	// propagate_as_baggage.
	ConfigTOML string
	// LogQueueCapacity bounds the Rust log queue (0 = 8192 records).
	LogQueueCapacity uint32
	// NoEnv ignores the OTEL_* / ASTER_* environment.
	NoEnv bool
	// NoLogBridge keeps Rust log records out of slog.
	NoLogBridge bool
}

// logPollMs bounds each aster_log_poll wait so the loop sees stop requests
// and the level re-check; levelRecheck is the design's 5 s re-sync period.
const (
	logPollMs    = 250
	logBatch     = 256
	levelRecheck = 5 * time.Second
)

var obs struct {
	mu     sync.Mutex
	refs   int
	auto   bool
	poller *logPoller
	logger atomic.Pointer[slog.Logger]
}

// currentLogger is the logger Rust log records and the binding's own messages
// go to: SetLogger's, else slog.Default().
func currentLogger() *slog.Logger {
	if l := obs.logger.Load(); l != nil {
		return l
	}
	return slog.Default()
}

// SetLogger sets the slog logger Rust log records (and the binding's own
// messages) are delivered to; nil restores slog.Default(). The Rust filter
// follows the logger's level (see SyncLogLevel). Wrap the logger's handler in
// NewTraceHandler to add trace ids to your own records.
func SetLogger(l *slog.Logger) {
	obs.logger.Store(l)
	_, _ = SyncLogLevel()
}

// SyncLogLevel makes Rust filter its events by the logger's level now: it
// sends "warn,aster=<level>" to the log bridge. Raising verbosity takes
// effect within 5 s on its own, or at once through this call. It does nothing
// (and returns "") while an explicit LogFilter or ASTER_LOG pins the filter,
// and returns ErrNotFound when no bridge is open.
func SyncLogLevel() (string, error) {
	obs.mu.Lock()
	p := obs.poller
	obs.mu.Unlock()
	if p == nil || p.pinned {
		return "", nil
	}
	return p.sync()
}

func envPinsLogFilter() bool {
	return os.Getenv("ASTER_LOG") != "" || os.Getenv("ASTER_LOG_LEVEL") != ""
}

// bytesArg views b as an aster_bytes_t without copying (Go memory with no Go
// pointers in it; Rust copies what it keeps before the call returns).
func bytesArg(b []byte) C.struct_aster_bytes_t {
	if len(b) == 0 {
		return C.struct_aster_bytes_t{}
	}
	return C.struct_aster_bytes_t{ptr: (*C.uint8_t)(unsafe.Pointer(&b[0])), len: C.uintptr_t(len(b))}
}

func strArg(s string) C.struct_aster_bytes_t {
	if s == "" {
		return C.struct_aster_bytes_t{}
	}
	return C.struct_aster_bytes_t{ptr: (*C.uint8_t)(unsafe.Pointer(unsafe.StringData(s))), len: C.uintptr_t(len(s))}
}

func mapAttrs(m map[string]string) []Attr {
	if len(m) == 0 {
		return nil
	}
	out := make([]Attr, 0, len(m))
	for k, v := range m {
		out = append(out, Attr{k, v})
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Key < out[j].Key })
	return out
}

// InitObservability starts the observability pipeline, or joins the running
// one. A configuration that differs from the running pipeline's returns an
// error matching ErrAlreadyExists that names the differing settings (the
// SDK's globals are per process): call it before starting a server. Pair it
// with ShutdownObservability.
func InitObservability(cfg ObservabilityConfig) error {
	obs.mu.Lock()
	defer obs.mu.Unlock()
	return initLocked(cfg)
}

func initLocked(cfg ObservabilityConfig) error {
	bridge := !cfg.NoLogBridge
	pinned := cfg.LogFilter != ""
	filter := cfg.LogFilter
	if bridge && !pinned {
		if envPinsLogFilter() {
			pinned = true
		} else {
			filter = levelDirective(handlerLevel(currentLogger().Handler()))
		}
	}

	var a cArena
	defer a.free()
	c := (*C.aster_observability_config_t)(a.alloc(unsafe.Sizeof(C.aster_observability_config_t{})))
	c.struct_size = C.uint32_t(unsafe.Sizeof(*c))
	if !cfg.NoEnv {
		c.flags |= C.ASTER_OBSERVABILITY_FROM_ENV
	}
	if cfg.Traces {
		c.flags |= C.ASTER_OBSERVABILITY_TRACES
	}
	if cfg.Metrics {
		c.flags |= C.ASTER_OBSERVABILITY_METRICS
	}
	if cfg.OTLPLogs {
		c.flags |= C.ASTER_OBSERVABILITY_OTLP_LOGS
	}
	if bridge {
		c.flags |= C.ASTER_OBSERVABILITY_LOG_BRIDGE
	}
	c.service_name = a.str(cfg.ServiceName)
	c.otlp_endpoint = a.str(cfg.OTLPEndpoint)
	c.otlp_headers = a.bytes(encodeAttrs(mapAttrs(cfg.OTLPHeaders)))
	c.resource_attrs = a.bytes(encodeAttrs(mapAttrs(cfg.ResourceAttributes)))
	c.log_filter = a.str(filter)
	c.extra_toml = a.str(cfg.ConfigTOML)
	c.trace_sample_ratio = -1
	if cfg.SampleRatio != nil {
		c.trace_sample_ratio = C.double(*cfg.SampleRatio)
	}
	c.log_queue_capacity = C.uint32_t(cfg.LogQueueCapacity)
	c.binding = C.ASTER_BINDING_GO

	if err := statusError(C.aster_observability_init(c)); err != nil {
		return fmt.Errorf("aster_observability_init: %w", err)
	}
	obs.refs++
	if bridge && obs.poller == nil {
		obs.poller = startLogPoller(pinned)
	}
	return nil
}

// EnsureObservability is the implicit start (StartServer and StartHealth call
// it): the log bridge opens with an environment-only configuration, and
// exporters start only when OTEL_* asks for them. A no-op after any
// InitObservability. Failures are logged, never returned: observability must
// not stop a server.
func EnsureObservability() {
	obs.mu.Lock()
	defer obs.mu.Unlock()
	if obs.auto || obs.refs > 0 {
		return
	}
	obs.auto = true
	if err := initLocked(ObservabilityConfig{}); err != nil {
		currentLogger().Warn("aster: observability could not start", "error", err)
	}
}

// FlushObservability flushes every signal, waiting at most timeout.
func FlushObservability(timeout time.Duration) error {
	if err := statusError(C.aster_observability_flush(C.uint32_t(timeout.Milliseconds()))); err != nil {
		return fmt.Errorf("aster_observability_flush: %w", err)
	}
	return nil
}

// ShutdownObservability releases every reference this process holds on the
// pipeline: the log poller stops, providers flush and stop (unless an ops
// listener still holds the pipeline), spans end and metric instruments are
// freed (the Counter, Histogram... values declare themselves again on next
// use). It reports whether anything was held. A later InitObservability
// starts a new pipeline.
func ShutdownObservability(timeout time.Duration) (bool, error) {
	obs.mu.Lock()
	p := obs.poller
	obs.poller = nil
	obs.mu.Unlock()
	if p != nil {
		p.stop()
	}
	obs.mu.Lock()
	defer obs.mu.Unlock()
	held := false
	var err error
	for obs.refs > 0 {
		s := C.aster_observability_shutdown(C.uint32_t(timeout.Milliseconds()))
		if s == C.ASTER_STATUS_ALREADY_CLOSED {
			break
		}
		if s != C.ASTER_STATUS_OK {
			err = fmt.Errorf("aster_observability_shutdown: %w", statusError(s))
			break
		}
		held = true
		obs.refs--
	}
	obs.refs = 0
	obs.auto = false
	return held, err
}

// LogsDropped is the number of Rust log records dropped because the bridge
// queue was full (also the aster.log.records_dropped metric).
func LogsDropped() uint64 {
	var n C.uint64_t
	if C.aster_log_dropped(&n) != C.ASTER_STATUS_OK {
		return 0
	}
	return uint64(n)
}

// MetricsPrometheus returns the running pipeline's metrics as Prometheus text
// (format 0.0.4): the built-in aster.* instruments, your own, process uptime
// and target_info -- what a HealthServer's /metrics serves. It needs the
// Prometheus pull reader: a running HealthServer, or OTEL_METRICS_EXPORTER=
// prometheus. Without one it returns an error matching ErrNotFound.
func MetricsPrometheus() (string, error) {
	var out C.struct_aster_bytes_t
	if err := statusError(C.aster_metrics_prometheus(&out)); err != nil {
		return "", fmt.Errorf("aster_metrics_prometheus: %w", err)
	}
	return string(takeRustBytes(out)), nil
}

// ─── The log poller ────────────────────────────────────────────────────────

// logPoller is the single goroutine that polls the Rust log queue, on a
// locked OS thread: aster_log_poll marks its calling thread as a log drain
// (events emitted while delivering never reach the bridge again), and the
// mark has to stay on the thread that delivers to slog.
type logPoller struct {
	pinned   bool
	closing  atomic.Bool
	done     chan struct{}
	lastSync slog.Level
	synced   bool
	mu       sync.Mutex // guards lastSync / synced
}

func startLogPoller(pinned bool) *logPoller {
	p := &logPoller{pinned: pinned, done: make(chan struct{})}
	if !pinned {
		p.mu.Lock()
		p.lastSync, p.synced = handlerLevel(currentLogger().Handler()), true
		p.mu.Unlock()
	}
	go p.run()
	return p
}

// sync sends the filter for the logger's current level to Rust.
func (p *logPoller) sync() (string, error) {
	level := handlerLevel(currentLogger().Handler())
	directive := levelDirective(level)
	d := []byte(directive)
	pp, n := cBytesArg(d)
	if s := C.aster_log_set_filter(pp, n); s != C.ASTER_STATUS_OK {
		return "", fmt.Errorf("aster_log_set_filter: %w", statusError(s))
	}
	p.mu.Lock()
	p.lastSync, p.synced = level, true
	p.mu.Unlock()
	return directive, nil
}

// recheck re-syncs only when the logger's level changed since the last sync.
func (p *logPoller) recheck() {
	if p.pinned {
		return
	}
	level := handlerLevel(currentLogger().Handler())
	p.mu.Lock()
	changed := !p.synced || level != p.lastSync
	p.mu.Unlock()
	if changed {
		_, _ = p.sync()
	}
}

// stop ends the poller: the queue closes (which wakes a poll in flight) and
// the goroutine is joined.
func (p *logPoller) stop() {
	p.closing.Store(true)
	C.aster_log_close()
	select {
	case <-p.done:
	case <-time.After(2 * time.Second):
	}
}

func (p *logPoller) run() {
	defer close(p.done)
	runtime.LockOSThread()
	// Not unlocked: a goroutine that exits locked takes its thread with it,
	// so the drain mark never lands on a thread another goroutine reuses.
	size := unsafe.Sizeof(C.aster_log_record_t{})
	buf := C.calloc(logBatch, C.size_t(size))
	if buf == nil {
		panic("aster: calloc failed")
	}
	defer C.free(buf)
	recs := unsafe.Slice((*C.aster_log_record_t)(buf), logBatch)
	lastCheck := time.Now()
	for !p.closing.Load() {
		start := time.Now()
		n := int(C.aster_log_poll(&recs[0], C.uint32_t(size), logBatch, logPollMs))
		for i := 0; i < n; i++ {
			rec := readLogRecord(&recs[i])
			deliverLog(currentLogger(), &rec)
		}
		if n == 0 && time.Since(start) < logPollMs*time.Millisecond/2 {
			// An early empty return is a refusal or a closed bridge, not a
			// timeout: stop for the fatal ones, never spin on the rest.
			msg := lastErrorMessage()
			if strings.HasPrefix(msg, "ASTER_STATUS_NOT_FOUND") || strings.HasPrefix(msg, "ASTER_STATUS_ALREADY_EXISTS") ||
				strings.HasPrefix(msg, "ASTER_STATUS_INVALID_ARGUMENT") || strings.HasPrefix(msg, "ASTER_STATUS_UNSUPPORTED") {
				if !p.closing.Load() && !strings.HasPrefix(msg, "ASTER_STATUS_NOT_FOUND") {
					currentLogger().Warn("aster: log poller stopped", "reason", msg)
				}
				return
			}
			time.Sleep(10 * time.Millisecond)
		}
		if time.Since(lastCheck) >= levelRecheck {
			lastCheck = time.Now()
			p.recheck()
		}
	}
}

// readLogRecord copies one record out of the poll arena (valid until the next
// poll).
func readLogRecord(r *C.aster_log_record_t) logRecord {
	str := func(p *C.uint8_t, n C.uint32_t) string {
		if p == nil || n == 0 {
			return ""
		}
		return C.GoStringN((*C.char)(unsafe.Pointer(p)), C.int(n))
	}
	rec := logRecord{
		level:     uint8(r.level),
		flags:     uint8(r.flags),
		time:      time.Unix(0, int64(r.timestamp_unix_ns)),
		target:    str(r.target_ptr, r.target_len),
		message:   str(r.message_ptr, r.message_len),
		callID:    uint64(r.call_id),
		hasTrace:  r.flags&C.ASTER_LOG_FLAG_HAS_TRACE != 0,
		inCall:    r.flags&C.ASTER_LOG_FLAG_IN_CALL != 0,
		synthetic: r.flags&C.ASTER_LOG_FLAG_SYNTHETIC != 0,
		audit:     r.flags&C.ASTER_LOG_FLAG_AUDIT != 0,
	}
	rec.traceID = *(*[16]byte)(unsafe.Pointer(&r.trace_id))
	rec.spanID = *(*[8]byte)(unsafe.Pointer(&r.span_id))
	if r.fields_ptr != nil && r.fields_len > 0 {
		raw := C.GoBytes(unsafe.Pointer(r.fields_ptr), C.int(r.fields_len))
		rec.fields, _ = decodeAttrs(raw)
	}
	return rec
}

// ─── Call-context carrier, server side ─────────────────────────────────────

// callTraceContext reads the dispatch trace context (server span) and the
// inbound baggage / tracestate of a live call and returns ctx carrying them.
// Without an exported span and without baggage ctx is returned as it is.
func (d *dispatchServer) callTraceContext(ctx context.Context, callID uint64, flags uint32, metadata []MetadataEntry) context.Context {
	var sc SpanContext
	if flags&dispatchFlagHasTrace != 0 {
		var c C.struct_aster_span_context_t
		if C.aster_dispatch_trace_context(d.rt, d.handle, C.uint64_t(callID), &c) == C.ASTER_STATUS_OK {
			sc = spanContextFromC(&c)
			sc.Remote = false
		}
	}
	var baggage map[string]string
	if flags&dispatchFlagHasInboundTrace != 0 {
		for _, kv := range d.inboundTrace(callID) {
			if kv.Key == "baggage" {
				baggage = ParseBaggage(kv.Value)
			}
		}
	} else if !sc.IsValid() {
		// No tracing pipeline: the trace keys pass through as metadata.
		for _, e := range metadata {
			if e.Key == "baggage" {
				baggage = ParseBaggage(e.Value)
				break
			}
		}
	}
	if !sc.IsValid() && len(baggage) == 0 {
		return ctx
	}
	return contextWithCarrier(ctx, sc, baggage)
}

// inboundTrace reads the inbound baggage / tracestate k/v list of callID.
func (d *dispatchServer) inboundTrace(callID uint64) []Attr {
	var ptr *C.uint8_t
	var n C.uint32_t
	var buf C.uint64_t
	if C.aster_dispatch_inbound_trace(d.rt, d.handle, C.uint64_t(callID), &ptr, &n, &buf) != C.ASTER_STATUS_OK {
		return nil
	}
	var raw []byte
	if n > 0 {
		raw = C.GoBytes(unsafe.Pointer(ptr), C.int(n))
	}
	d.releaseBuffer(buf)
	kv, _ := decodeAttrs(raw)
	return kv
}

func spanContextFromC(c *C.struct_aster_span_context_t) SpanContext {
	return SpanContext{
		TraceID: *(*[16]byte)(unsafe.Pointer(&c.trace_id)),
		SpanID:  *(*[8]byte)(unsafe.Pointer(&c.span_id)),
		Flags:   byte(c.trace_flags),
		Remote:  c.is_remote != 0,
	}
}

func spanContextToC(sc SpanContext, c *C.struct_aster_span_context_t) {
	*(*[16]byte)(unsafe.Pointer(&c.trace_id)) = sc.TraceID
	*(*[8]byte)(unsafe.Pointer(&c.span_id)) = sc.SpanID
	c.trace_flags = C.uint8_t(sc.Flags)
	if sc.Remote {
		c.is_remote = 1
	}
}

// injectTrace returns md plus the current traceparent / baggage of ctx (the
// same slice when ctx has none or the caller set its own traceparent).
func injectTrace(ctx context.Context, md []MetadataEntry) []MetadataEntry {
	tp, bg := traceHeaders(ctx)
	if tp == "" && bg == "" {
		return md
	}
	has := func(key string) bool {
		for _, e := range md {
			if e.Key == key {
				return true
			}
		}
		return false
	}
	if has("traceparent") {
		return md
	}
	out := append([]MetadataEntry(nil), md...)
	if tp != "" {
		out = append(out, MetadataEntry{"traceparent", tp})
	}
	if bg != "" && !has("baggage") {
		out = append(out, MetadataEntry{"baggage", bg})
	}
	return out
}
