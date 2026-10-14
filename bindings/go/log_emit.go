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
	"log/slog"
	"unsafe"
)

// Path B of the observability design (3.4): the application's own log
// records go into the Rust OpenTelemetry logs SDK, so they are exported over
// OTLP next to the Rust records (when OTLPLogs / OTEL_LOGS_EXPORTER asks for
// export; otherwise the call is a quiet no-op). It is opt-in: wrap your
// handler in NewEmitHandler.

// EmitHandler tees slog records into aster_log_emit. Records that came from
// the Rust log bridge (attribute aster_origin=rust) are never re-emitted, so
// the bridge and the emitter cannot feed each other.
type EmitHandler struct {
	inner  slog.Handler
	name   string
	attrs  []slog.Attr // already group-qualified
	groups []string
	rust   bool // a WithAttrs marked every record as a bridged one
}

// NewEmitHandler wraps inner (nil = emit only; records at Info and above are
// accepted then). The logger name on exported records is "app"; use
// NewEmitHandlerNamed to choose it.
//
//	logger := slog.New(aster.NewEmitHandler(aster.NewTraceHandler(slog.NewJSONHandler(os.Stderr, nil))))
//	logger.InfoContext(ctx, "charging card") // also exported over OTLP, with ctx's trace ids
func NewEmitHandler(inner slog.Handler) *EmitHandler { return NewEmitHandlerNamed(inner, "app") }

// NewEmitHandlerNamed is NewEmitHandler with an explicit logger name.
func NewEmitHandlerNamed(inner slog.Handler, name string) *EmitHandler {
	return &EmitHandler{inner: inner, name: name}
}

// Enabled implements slog.Handler.
func (h *EmitHandler) Enabled(ctx context.Context, l slog.Level) bool {
	if h.inner == nil {
		return l >= slog.LevelInfo
	}
	return h.inner.Enabled(ctx, l)
}

// Handle implements slog.Handler: the inner handler first, then the export.
// An export failure never fails the record.
func (h *EmitHandler) Handle(ctx context.Context, r slog.Record) error {
	var err error
	if h.inner != nil {
		err = h.inner.Handle(ctx, r)
	}
	if !h.rust && !isBridged(r) {
		fields := make([]Attr, 0, len(h.attrs)+r.NumAttrs())
		for _, a := range h.attrs {
			fields = flattenAttr(fields, "", a)
		}
		prefix := groupPrefix(h.groups)
		r.Attrs(func(a slog.Attr) bool {
			fields = flattenAttr(fields, prefix, a)
			return true
		})
		var sc *SpanContext
		if c, ok := SpanContextFromContext(ctx); ok {
			sc = &c
		}
		_ = emitLog(emitLevel(r.Level), h.name, r.Message, fields, r.Time.UnixNano(), sc)
	}
	return err
}

// WithAttrs implements slog.Handler.
func (h *EmitHandler) WithAttrs(attrs []slog.Attr) slog.Handler {
	n := *h
	if h.inner != nil {
		n.inner = h.inner.WithAttrs(attrs)
	}
	prefix := groupPrefix(h.groups)
	n.attrs = append([]slog.Attr(nil), h.attrs...)
	for _, a := range attrs {
		if prefix != "" {
			a.Key = prefix + a.Key
		}
		if a.Key == "aster_origin" && a.Value.String() == "rust" {
			n.rust = true
		}
		n.attrs = append(n.attrs, a)
	}
	return &n
}

// WithGroup implements slog.Handler.
func (h *EmitHandler) WithGroup(name string) slog.Handler {
	n := *h
	if h.inner != nil {
		n.inner = h.inner.WithGroup(name)
	}
	if name != "" {
		n.groups = append(append([]string(nil), h.groups...), name)
	}
	return &n
}

func isBridged(r slog.Record) bool {
	bridged := false
	r.Attrs(func(a slog.Attr) bool {
		if a.Key == "aster_origin" && a.Value.String() == "rust" {
			bridged = true
			return false
		}
		return true
	})
	return bridged
}

func groupPrefix(groups []string) string {
	p := ""
	for _, g := range groups {
		p += g + "."
	}
	return p
}

// flattenAttr appends a (resolving groups into dotted keys) as string fields.
func flattenAttr(out []Attr, prefix string, a slog.Attr) []Attr {
	v := a.Value.Resolve()
	if v.Kind() == slog.KindGroup {
		p := prefix
		if a.Key != "" {
			p += a.Key + "."
		}
		for _, g := range v.Group() {
			out = flattenAttr(out, p, g)
		}
		return out
	}
	if a.Key == "" {
		return out
	}
	return append(out, Attr{prefix + a.Key, v.String()})
}

// emitLevel maps slog to aster_log_level_t (1 error .. 5 trace).
func emitLevel(l slog.Level) uint8 {
	switch {
	case l >= slog.LevelError:
		return 1
	case l >= slog.LevelWarn:
		return 2
	case l >= slog.LevelInfo:
		return 3
	case l >= slog.LevelDebug:
		return 4
	default:
		return 5
	}
}

// EmitLog writes one application log record into the Rust OTel logs SDK
// (aster_log_emit). level is slog's; sc nil = no trace context. It never
// blocks, emits nothing the log bridge could read back, and is a no-op when
// no OTLP logs exporter is running.
func EmitLog(level slog.Level, logger, message string, fields []Attr, sc *SpanContext) error {
	return emitLog(emitLevel(level), logger, message, fields, 0, sc)
}

func emitLog(level uint8, logger, message string, fields []Attr, tsNanos int64, sc *SpanContext) error {
	var a cArena
	defer a.free()
	rec := (*C.struct_aster_log_emit_t)(a.alloc(unsafe.Sizeof(C.struct_aster_log_emit_t{})))
	rec.struct_size = C.uint32_t(unsafe.Sizeof(*rec))
	rec.level = C.uint8_t(level)
	rec.timestamp_unix_ns = C.int64_t(tsNanos)
	rec.logger = a.str(logger)
	rec.message = a.str(message)
	rec.fields = a.bytes(encodeAttrs(fields))
	if sc != nil && sc.IsValid() {
		c := (*C.struct_aster_span_context_t)(a.alloc(unsafe.Sizeof(C.struct_aster_span_context_t{})))
		spanContextToC(*sc, c)
		rec.context = c
	}
	if err := ffiStatus(func() C.int32_t { return C.aster_log_emit(rec) }); err != nil {
		return fmt.Errorf("aster_log_emit: %w", err)
	}
	return nil
}
