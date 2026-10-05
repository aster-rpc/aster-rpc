//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"fmt"
	"sync"
)

// SpanKind is the OpenTelemetry span kind (aster_span_kind_t).
type SpanKind uint32

// Span kinds.
const (
	SpanKindInternal SpanKind = C.ASTER_SPAN_INTERNAL
	SpanKindServer   SpanKind = C.ASTER_SPAN_SERVER
	SpanKindClient   SpanKind = C.ASTER_SPAN_CLIENT
	SpanKindProducer SpanKind = C.ASTER_SPAN_PRODUCER
	SpanKindConsumer SpanKind = C.ASTER_SPAN_CONSUMER
)

type spanConfig struct {
	kind   SpanKind
	attrs  []Attr
	parent string
}

// SpanOption configures StartSpan.
type SpanOption func(*spanConfig)

// WithSpanKind sets the span kind (default internal).
func WithSpanKind(k SpanKind) SpanOption { return func(c *spanConfig) { c.kind = k } }

// WithSpanAttributes sets the initial attributes.
func WithSpanAttributes(attrs ...Attr) SpanOption {
	return func(c *spanConfig) { c.attrs = append(c.attrs, attrs...) }
}

// WithParentTraceparent parents the span under a W3C traceparent instead of
// the context's current span.
func WithParentTraceparent(traceparent string) SpanOption {
	return func(c *spanConfig) { c.parent = traceparent }
}

// Span is a span in the Rust tracer (scope aster.app): it is exported with
// Aster's own RPC spans, under the same resource and sampling decision.
// Without a tracer provider (InitObservability with Traces, or OTEL_*) a span
// is a cheap no-op. A Span is safe for concurrent use.
type Span struct {
	mu     sync.Mutex
	handle uint64
	sc     SpanContext
}

// StartSpan starts a span under ctx's current span (an RPC handler's context
// holds the call's server span, so your spans nest under it) and returns a
// context in which it is current: Aster calls made with that context continue
// the trace, and TraceHandler adds its ids to your logs. End the span.
//
//	ctx, span := aster.StartSpan(ctx, "db.query", aster.WithSpanAttributes(aster.StringAttr("db.system", "postgresql")))
//	defer span.End()
func StartSpan(ctx context.Context, name string, opts ...SpanOption) (context.Context, *Span) {
	var cfg spanConfig
	for _, o := range opts {
		o(&cfg)
	}
	var parent SpanContext
	var baggage map[string]string
	if c := carrierFrom(ctx); c != nil {
		parent, baggage = c.sc, c.baggage
	}
	if cfg.parent != "" {
		if sc, err := ParseTraceparent(cfg.parent); err == nil {
			parent = sc
		}
	}
	var pc C.struct_aster_span_context_t
	var pcp *C.struct_aster_span_context_t
	if parent.IsValid() {
		spanContextToC(parent, &pc)
		pcp = &pc
	}
	kv, kvn := cBytesArg(encodeAttrs(cfg.attrs))
	var h C.aster_span_t
	if C.aster_span_start(strArg(name), C.uint32_t(cfg.kind), pcp, kv, kvn, &h) != C.ASTER_STATUS_OK {
		return ctx, &Span{} // the live-span table is full: degrade to a no-op
	}
	var out C.struct_aster_span_context_t
	if C.aster_span_context(h, &out) != C.ASTER_STATUS_OK {
		C.aster_span_end(h)
		return ctx, &Span{}
	}
	sc := spanContextFromC(&out)
	if !sc.IsValid() { // no pipeline: a no-op span
		C.aster_span_end(h)
		return ctx, &Span{}
	}
	sc.Remote = false
	return contextWithCarrier(ctx, sc, baggage), &Span{handle: uint64(h), sc: sc}
}

// WithSpan runs fn inside a span; an error fn returns is recorded on it.
func WithSpan(ctx context.Context, name string, fn func(context.Context) error, opts ...SpanOption) error {
	ctx, span := StartSpan(ctx, name, opts...)
	defer span.End()
	err := fn(ctx)
	if err != nil {
		span.RecordError(err)
	}
	return err
}

// SpanContext is the span's context (invalid for a no-op span).
func (s *Span) SpanContext() SpanContext { return s.sc }

// TraceID is the span's trace id (32 hex characters), or "" for a no-op span.
func (s *Span) TraceID() string {
	if s.sc.IsValid() {
		return s.sc.TraceIDString()
	}
	return ""
}

// SpanID is the span's id (16 hex characters), or "" for a no-op span.
func (s *Span) SpanID() string {
	if s.sc.IsValid() {
		return s.sc.SpanIDString()
	}
	return ""
}

// Traceparent is the span's W3C traceparent, or "" for a no-op span.
func (s *Span) Traceparent() string { return s.sc.Traceparent() }

// live runs fn with the span's handle; a handle the library no longer knows
// (ended, or the pipeline was shut down) turns the span into a no-op.
func (s *Span) live(fn func(h C.aster_span_t) C.int32_t) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.handle == 0 {
		return
	}
	if fn(C.aster_span_t(s.handle)) == C.ASTER_STATUS_NOT_FOUND {
		s.handle = 0
	}
}

// SetAttributes adds attributes.
func (s *Span) SetAttributes(attrs ...Attr) {
	kv, n := cBytesArg(encodeAttrs(attrs))
	s.live(func(h C.aster_span_t) C.int32_t { return C.aster_span_set_attributes(h, kv, n) })
}

// AddEvent adds a timestamped event.
func (s *Span) AddEvent(name string, attrs ...Attr) {
	kv, n := cBytesArg(encodeAttrs(attrs))
	s.live(func(h C.aster_span_t) C.int32_t { return C.aster_span_add_event(h, strArg(name), kv, n) })
}

// SetStatus sets the span status: ok, or an error with message.
func (s *Span) SetStatus(ok bool, message string) {
	code := C.uint32_t(2)
	if ok {
		code = 1
	}
	s.live(func(h C.aster_span_t) C.int32_t { return C.aster_span_set_status(h, code, strArg(message)) })
}

// RecordError records err as an exception event and sets the status to error.
func (s *Span) RecordError(err error) {
	if err == nil {
		return
	}
	typ, msg := fmt.Sprintf("%T", err), err.Error()
	s.live(func(h C.aster_span_t) C.int32_t { return C.aster_span_record_error(h, strArg(typ), strArg(msg)) })
}

// End ends the span (idempotent).
func (s *Span) End() {
	s.mu.Lock()
	h := s.handle
	s.handle = 0
	s.mu.Unlock()
	if h != 0 {
		C.aster_span_end(C.aster_span_t(h))
	}
}
