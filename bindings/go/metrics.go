//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"fmt"
	"sync"
	"sync/atomic"
	"unsafe"
)

// Application metrics over the Rust OpenTelemetry pipeline. Each instrument
// is a handle into aster-otel (meter scope aster.app), recorded through the
// same MeterProvider as Aster's built-in RPC metrics (aster.rpc.server.*,
// aster.admission.*, ...), so one exporter and one /metrics endpoint serve
// both. Without a pipeline (InitObservability) recording is a no-op.
//
//	orders, err := aster.NewCounter("orders.placed", "Orders placed", "")
//	latency, err := aster.NewHistogram("orders.latency", "Order latency", "s")
//	orders.Add(ctx, 1, aster.StringAttr("region", "eu"))
//	latency.Record(ctx, 0.042, aster.StringAttr("region", "eu"))
//
// Attribute values are strings. Keep their cardinality bounded: the SDK folds
// series beyond 2000 per instrument into otel.metric.overflow=true. Names
// follow the OpenTelemetry rules (a letter, then letters, digits, _ . - /); a
// name can have only one kind. Prometheus text appends the unit to the name
// (unit "s" renders ..._seconds) and _total to counters.
//
// A histogram in seconds (unit "s") gets the RPC duration buckets (0.005 ..
// 10 s); pass WithBoundaries for others. Any other unit without boundaries
// gets the SDK default, which is millisecond-scaled. WithAutoAttrs and
// WithCardinalityLimit are accepted and recorded but have no effect yet.
//
// Instruments survive ShutdownObservability: a freed handle is declared again
// on next use.

// InstrumentOption configures an instrument.
type InstrumentOption func(*instrumentConfig)

type instrumentConfig struct {
	autoAttrs   []string
	cardinality uint32
	boundaries  []float64
}

// WithAutoAttrs lists attribute keys merged in from the call being handled
// (reserved: accepted, no effect yet).
func WithAutoAttrs(keys ...string) InstrumentOption {
	return func(c *instrumentConfig) { c.autoAttrs = append(c.autoAttrs, keys...) }
}

// WithCardinalityLimit sets a per-instrument series limit (reserved: the SDK
// default of 2000 applies).
func WithCardinalityLimit(n uint32) InstrumentOption {
	return func(c *instrumentConfig) { c.cardinality = n }
}

// WithBoundaries sets histogram bucket upper bounds: finite and strictly
// increasing.
func WithBoundaries(bounds ...float64) InstrumentOption {
	return func(c *instrumentConfig) { c.boundaries = append(c.boundaries, bounds...) }
}

type instrument struct {
	kind                    C.uint32_t
	name, description, unit string
	cfg                     instrumentConfig
	handle                  atomic.Uint64
	mu                      sync.Mutex // serialises (re)declaration
}

func newInstrument(kind C.uint32_t, name, description, unit string, opts []InstrumentOption) (*instrument, error) {
	in := &instrument{kind: kind, name: name, description: description, unit: unit}
	for _, o := range opts {
		o(&in.cfg)
	}
	if err := in.declare(); err != nil {
		return nil, err
	}
	return in, nil
}

func (in *instrument) declare() error {
	var a cArena
	defer a.free()
	d := (*C.aster_instrument_desc_t)(a.alloc(unsafe.Sizeof(C.aster_instrument_desc_t{})))
	d.struct_size = C.uint32_t(unsafe.Sizeof(*d))
	d.kind = in.kind
	d.name = a.str(in.name)
	d.description = a.str(in.description)
	d.unit = a.str(in.unit)
	d.auto_attrs = a.bytes(encodeKeyList(in.cfg.autoAttrs))
	d.cardinality_limit = C.uint32_t(in.cfg.cardinality)
	if n := len(in.cfg.boundaries); n > 0 {
		// The boundaries live in C memory: the descriptor is C memory too and
		// must not hold a Go pointer.
		p := a.alloc(uintptr(n) * 8)
		copy(unsafe.Slice((*float64)(p), n), in.cfg.boundaries)
		d.boundaries = (*C.double)(p)
		d.boundaries_len = C.uint32_t(n)
	}
	var h C.aster_instrument_t
	if err := ffiStatus(func() C.int32_t { return C.aster_instrument_new(d, &h) }); err != nil {
		return fmt.Errorf("aster_instrument_new(%q): %w", in.name, err)
	}
	in.handle.Store(uint64(h))
	return nil
}

// redeclare declares the instrument again if its handle is still stale (the
// pipeline was shut down and freed it).
func (in *instrument) redeclare(stale uint64) {
	in.mu.Lock()
	defer in.mu.Unlock()
	if in.handle.Load() == stale {
		_ = in.declare()
	}
}

// Close releases the instrument's handle (optional: shutdown frees them all).
func (in *instrument) Close() error {
	if h := in.handle.Swap(0); h != 0 {
		return ffiStatus(func() C.int32_t { return C.aster_instrument_free(C.aster_instrument_t(h)) })
	}
	return nil
}

// AttrSet is an interned attribute set: build it once, record many times
// without re-encoding the attributes.
type AttrSet struct {
	attrs  []Attr
	handle atomic.Uint64
	mu     sync.Mutex
}

// NewAttrSet interns attrs.
func NewAttrSet(attrs ...Attr) (*AttrSet, error) {
	s := &AttrSet{attrs: append([]Attr(nil), attrs...)}
	if err := s.intern(); err != nil {
		return nil, err
	}
	return s, nil
}

func (s *AttrSet) intern() error {
	kv, n := cBytesArg(encodeAttrs(s.attrs))
	var h C.aster_attrset_t
	if err := ffiStatus(func() C.int32_t { return C.aster_attrset_new(kv, n, &h) }); err != nil {
		return fmt.Errorf("aster_attrset_new: %w", err)
	}
	s.handle.Store(uint64(h))
	return nil
}

func (s *AttrSet) reintern(stale uint64) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.handle.Load() == stale {
		_ = s.intern()
	}
}

// Close releases the set's handle.
func (s *AttrSet) Close() error {
	if h := s.handle.Swap(0); h != 0 {
		return ffiStatus(func() C.int32_t { return C.aster_attrset_free(C.aster_attrset_t(h)) })
	}
	return nil
}

// record calls fn(instrument, set, kv, kvLen, callID); a NOT_FOUND (handles
// freed by a pipeline shutdown) declares again and retries once. Other
// failures are dropped: recording a metric never fails the caller.
func (in *instrument) record(ctx context.Context, set *AttrSet, attrs []Attr,
	fn func(h C.aster_instrument_t, set C.aster_attrset_t, kv *C.uint8_t, n C.uint32_t, call C.uint64_t) C.int32_t) {
	kv, n := cBytesArg(encodeAttrs(attrs))
	var call C.uint64_t
	if ci, ok := CallInfoFromContext(ctx); ok {
		call = C.uint64_t(ci.CallID)
	}
	for attempt := 0; attempt < 2; attempt++ {
		h := in.handle.Load()
		var sh uint64
		if set != nil {
			sh = set.handle.Load()
		}
		if h == 0 {
			return // closed
		}
		if fn(C.aster_instrument_t(h), C.aster_attrset_t(sh), kv, n, call) != C.ASTER_STATUS_NOT_FOUND {
			return
		}
		in.redeclare(h)
		if set != nil {
			set.reintern(sh)
		}
	}
}

// Counter is a monotonic sum.
type Counter struct{ in *instrument }

// NewCounter declares a counter.
func NewCounter(name, description, unit string, opts ...InstrumentOption) (*Counter, error) {
	in, err := newInstrument(C.ASTER_INSTRUMENT_COUNTER_U64, name, description, unit, opts)
	if err != nil {
		return nil, err
	}
	return &Counter{in}, nil
}

// Add adds v.
func (c *Counter) Add(ctx context.Context, v uint64, attrs ...Attr) { c.AddWith(ctx, v, nil, attrs...) }

// AddWith adds v with the attributes of set (may be nil) followed by attrs.
func (c *Counter) AddWith(ctx context.Context, v uint64, set *AttrSet, attrs ...Attr) {
	c.in.record(ctx, set, attrs, func(h C.aster_instrument_t, s C.aster_attrset_t, kv *C.uint8_t, n C.uint32_t, call C.uint64_t) C.int32_t {
		return C.aster_metric_add_u64(h, C.uint64_t(v), s, kv, n, call)
	})
}

// Close releases the instrument's handle.
func (c *Counter) Close() error { return c.in.Close() }

// UpDownCounter is a sum that may go down (in-flight work, queue depth).
type UpDownCounter struct{ in *instrument }

// NewUpDownCounter declares an up-down counter.
func NewUpDownCounter(name, description, unit string, opts ...InstrumentOption) (*UpDownCounter, error) {
	in, err := newInstrument(C.ASTER_INSTRUMENT_UP_DOWN_I64, name, description, unit, opts)
	if err != nil {
		return nil, err
	}
	return &UpDownCounter{in}, nil
}

// Add adds v (which may be negative).
func (c *UpDownCounter) Add(ctx context.Context, v int64, attrs ...Attr) {
	c.AddWith(ctx, v, nil, attrs...)
}

// AddWith adds v with the attributes of set (may be nil) followed by attrs.
func (c *UpDownCounter) AddWith(ctx context.Context, v int64, set *AttrSet, attrs ...Attr) {
	c.in.record(ctx, set, attrs, func(h C.aster_instrument_t, s C.aster_attrset_t, kv *C.uint8_t, n C.uint32_t, call C.uint64_t) C.int32_t {
		return C.aster_metric_add_i64(h, C.int64_t(v), s, kv, n, call)
	})
}

// Close releases the instrument's handle.
func (c *UpDownCounter) Close() error { return c.in.Close() }

// Histogram is a distribution of values (durations, sizes).
type Histogram struct{ in *instrument }

// NewHistogram declares a histogram.
func NewHistogram(name, description, unit string, opts ...InstrumentOption) (*Histogram, error) {
	in, err := newInstrument(C.ASTER_INSTRUMENT_HISTOGRAM_F64, name, description, unit, opts)
	if err != nil {
		return nil, err
	}
	return &Histogram{in}, nil
}

// Record records v.
func (h *Histogram) Record(ctx context.Context, v float64, attrs ...Attr) {
	h.RecordWith(ctx, v, nil, attrs...)
}

// RecordWith records v with the attributes of set (may be nil) followed by attrs.
func (h *Histogram) RecordWith(ctx context.Context, v float64, set *AttrSet, attrs ...Attr) {
	h.in.record(ctx, set, attrs, recordF64(v))
}

// Close releases the instrument's handle.
func (h *Histogram) Close() error { return h.in.Close() }

// Gauge keeps the last recorded value.
type Gauge struct{ in *instrument }

// NewGauge declares a gauge.
func NewGauge(name, description, unit string, opts ...InstrumentOption) (*Gauge, error) {
	in, err := newInstrument(C.ASTER_INSTRUMENT_GAUGE_F64, name, description, unit, opts)
	if err != nil {
		return nil, err
	}
	return &Gauge{in}, nil
}

// Record records v.
func (g *Gauge) Record(ctx context.Context, v float64, attrs ...Attr) {
	g.RecordWith(ctx, v, nil, attrs...)
}

// RecordWith records v with the attributes of set (may be nil) followed by attrs.
func (g *Gauge) RecordWith(ctx context.Context, v float64, set *AttrSet, attrs ...Attr) {
	g.in.record(ctx, set, attrs, recordF64(v))
}

// Close releases the instrument's handle.
func (g *Gauge) Close() error { return g.in.Close() }

func recordF64(v float64) func(C.aster_instrument_t, C.aster_attrset_t, *C.uint8_t, C.uint32_t, C.uint64_t) C.int32_t {
	return func(h C.aster_instrument_t, s C.aster_attrset_t, kv *C.uint8_t, n C.uint32_t, call C.uint64_t) C.int32_t {
		return C.aster_metric_record_f64(h, C.double(v), s, kv, n, call)
	}
}
