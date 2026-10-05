//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>

#include "aster_ffi.h"
*/
import "C"

import "unsafe"

// cTraceparentFormat is aster_trace_context_format. The binding formats
// traceparent in Go (SpanContext.Traceparent); abi_contract_test.go and
// observability_test.go pin the two to each other.
func cTraceparentFormat(sc SpanContext) (string, error) {
	var c C.struct_aster_span_context_t
	spanContextToC(sc, &c)
	var buf [55]C.uint8_t
	var n C.uint32_t
	if err := statusError(C.aster_trace_context_format(&c, &buf[0], 55, &n)); err != nil {
		return "", err
	}
	return string(unsafe.Slice((*byte)(unsafe.Pointer(&buf[0])), int(n))), nil
}

// cTraceparentParse is aster_trace_context_parse.
func cTraceparentParse(s string) (SpanContext, error) {
	var c C.struct_aster_span_context_t
	p, n := cBytesArg([]byte(s))
	if err := statusError(C.aster_trace_context_parse(p, n, &c)); err != nil {
		return SpanContext{}, err
	}
	return spanContextFromC(&c), nil
}
