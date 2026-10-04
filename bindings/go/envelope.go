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
	"encoding/binary"
	"errors"
	"fmt"
	"unsafe"
)

// SerializationMode is the payload serialization carried in a StreamHeader
// (Aster-SPEC §5.1). Bindings speak XlangMode only; JSON mode 3 is retired.
type SerializationMode int8

const (
	XlangMode  SerializationMode = 0
	NativeMode SerializationMode = 1
	RowMode    SerializationMode = 2
)

// MetadataEntry is one request-metadata / status-detail pair. Order and
// duplicates are preserved on the wire.
type MetadataEntry struct {
	Key   string
	Value string
}

// StreamHeader is the AEv1 HEADER frame that opens every RPC stream
// (Aster-SPEC §6.9). Deadline is relative, in whole seconds (0 = none).
type StreamHeader struct {
	Service           string
	Method            string
	Version           int32
	CallID            int32
	Deadline          int16
	SerializationMode SerializationMode
	Metadata          []MetadataEntry
	SessionID         int32
}

// CallHeader is the AEv1 per-call header inside a session stream.
type CallHeader struct {
	Method   string
	CallID   int32
	Deadline int16
	Metadata []MetadataEntry
}

// RpcStatus is the AEv1 TRAILER payload that ends every call.
type RpcStatus struct {
	Code    StatusCode
	Message string
	Details []MetadataEntry
}

// Err returns nil for OK, else the status as an *RpcError.
func (s RpcStatus) Err() error {
	if s.Code == StatusOK {
		return nil
	}
	details := make(map[string]string, len(s.Details))
	for _, d := range s.Details {
		details[d.Key] = d.Value
	}
	return &RpcError{Code: s.Code, Message: s.Message, Details: details}
}

// StatusFromError maps a handler error to the trailer status: an *RpcError
// keeps its code, context cancellation becomes CANCELLED / DEADLINE_EXCEEDED,
// anything else INTERNAL.
func StatusFromError(err error) RpcStatus {
	if err == nil {
		return RpcStatus{Code: StatusOK}
	}
	var rpcErr *RpcError
	if errors.As(err, &rpcErr) {
		st := RpcStatus{Code: rpcErr.Code, Message: rpcErr.Message}
		for k, v := range rpcErr.Details {
			st.Details = append(st.Details, MetadataEntry{k, v})
		}
		return st
	}
	switch {
	case errors.Is(err, context.DeadlineExceeded):
		return RpcStatus{Code: StatusDeadlineExceeded, Message: "deadline exceeded"}
	case errors.Is(err, context.Canceled):
		return RpcStatus{Code: StatusCancelled, Message: err.Error()}
	}
	return RpcStatus{Code: StatusInternal, Message: err.Error()}
}

func metaLists(a *cArena, entries []MetadataEntry) (C.struct_aster_bytes_list_t, C.struct_aster_bytes_list_t) {
	keys := make([]string, len(entries))
	values := make([]string, len(entries))
	for i, e := range entries {
		keys[i], values[i] = e.Key, e.Value
	}
	return a.list(keys), a.list(values)
}

func readList(l C.struct_aster_bytes_list_t) []string {
	if l.items == nil || l.len == 0 {
		return nil
	}
	items := unsafe.Slice(l.items, int(l.len))
	out := make([]string, len(items))
	for i, it := range items {
		out[i] = string(viewBytes(it))
	}
	return out
}

func readPairs(keys, values C.struct_aster_bytes_list_t) []MetadataEntry {
	k, v := readList(keys), readList(values)
	n := min(len(k), len(v))
	if n == 0 {
		return nil
	}
	out := make([]MetadataEntry, n)
	for i := 0; i < n; i++ {
		out[i] = MetadataEntry{k[i], v[i]}
	}
	return out
}

func inputPtr(b []byte) (*C.uint8_t, C.uintptr_t, func()) {
	if len(b) == 0 {
		return nil, 0, func() {}
	}
	p := C.CBytes(b)
	return (*C.uint8_t)(p), C.uintptr_t(len(b)), func() { C.free(p) }
}

// EncodeStreamHeader encodes h as an AEv1 StreamHeader payload.
func EncodeStreamHeader(h StreamHeader) ([]byte, error) {
	var a cArena
	defer a.free()
	keys, values := metaLists(&a, h.Metadata)
	n := C.aster_stream_header_t{
		struct_size:        C.uint32_t(unsafe.Sizeof(C.aster_stream_header_t{})),
		version:            C.int32_t(h.Version),
		call_id:            C.int32_t(h.CallID),
		session_id:         C.int32_t(h.SessionID),
		deadline:           C.int16_t(h.Deadline),
		serialization_mode: C.int8_t(h.SerializationMode),
		service:            a.str(h.Service),
		method:             a.str(h.Method),
		metadata_keys:      keys,
		metadata_values:    values,
	}
	var out C.struct_aster_bytes_t
	if err := statusError(C.aster_stream_header_encode(&n, &out)); err != nil {
		return nil, fmt.Errorf("aster_stream_header_encode: %w", err)
	}
	return takeRustBytes(out), nil
}

// DecodeStreamHeader decodes an AEv1 StreamHeader payload.
func DecodeStreamHeader(b []byte) (StreamHeader, error) {
	p, n, free := inputPtr(b)
	defer free()
	var h C.aster_stream_header_t
	if err := statusError(C.aster_stream_header_decode(p, n, &h)); err != nil {
		return StreamHeader{}, fmt.Errorf("aster_stream_header_decode: %w", err)
	}
	defer C.aster_stream_header_release(&h)
	return StreamHeader{
		Service:           string(viewBytes(h.service)),
		Method:            string(viewBytes(h.method)),
		Version:           int32(h.version),
		CallID:            int32(h.call_id),
		Deadline:          int16(h.deadline),
		SerializationMode: SerializationMode(h.serialization_mode),
		Metadata:          readPairs(h.metadata_keys, h.metadata_values),
		SessionID:         int32(h.session_id),
	}, nil
}

// EncodeCallHeader encodes h as an AEv1 CallHeader payload.
func EncodeCallHeader(h CallHeader) ([]byte, error) {
	var a cArena
	defer a.free()
	keys, values := metaLists(&a, h.Metadata)
	n := C.aster_call_header_t{
		struct_size:     C.uint32_t(unsafe.Sizeof(C.aster_call_header_t{})),
		call_id:         C.int32_t(h.CallID),
		deadline:        C.int16_t(h.Deadline),
		method:          a.str(h.Method),
		metadata_keys:   keys,
		metadata_values: values,
	}
	var out C.struct_aster_bytes_t
	if err := statusError(C.aster_call_header_encode(&n, &out)); err != nil {
		return nil, fmt.Errorf("aster_call_header_encode: %w", err)
	}
	return takeRustBytes(out), nil
}

// DecodeCallHeader decodes an AEv1 CallHeader payload.
func DecodeCallHeader(b []byte) (CallHeader, error) {
	p, n, free := inputPtr(b)
	defer free()
	var h C.aster_call_header_t
	if err := statusError(C.aster_call_header_decode(p, n, &h)); err != nil {
		return CallHeader{}, fmt.Errorf("aster_call_header_decode: %w", err)
	}
	defer C.aster_call_header_release(&h)
	return CallHeader{
		Method:   string(viewBytes(h.method)),
		CallID:   int32(h.call_id),
		Deadline: int16(h.deadline),
		Metadata: readPairs(h.metadata_keys, h.metadata_values),
	}, nil
}

// EncodeRpcStatus encodes s as an AEv1 RpcStatus payload.
func EncodeRpcStatus(s RpcStatus) ([]byte, error) {
	var a cArena
	defer a.free()
	keys, values := metaLists(&a, s.Details)
	n := C.aster_rpc_status_t{
		struct_size:   C.uint32_t(unsafe.Sizeof(C.aster_rpc_status_t{})),
		code:          C.int32_t(s.Code),
		message:       a.str(s.Message),
		detail_keys:   keys,
		detail_values: values,
	}
	var out C.struct_aster_bytes_t
	if err := statusError(C.aster_rpc_status_encode(&n, &out)); err != nil {
		return nil, fmt.Errorf("aster_rpc_status_encode: %w", err)
	}
	return takeRustBytes(out), nil
}

// DecodeRpcStatus decodes an AEv1 RpcStatus payload.
func DecodeRpcStatus(b []byte) (RpcStatus, error) {
	p, n, free := inputPtr(b)
	defer free()
	var s C.aster_rpc_status_t
	if err := statusError(C.aster_rpc_status_decode(p, n, &s)); err != nil {
		return RpcStatus{}, fmt.Errorf("aster_rpc_status_decode: %w", err)
	}
	defer C.aster_rpc_status_release(&s)
	return RpcStatus{
		Code:    StatusCode(s.code),
		Message: string(viewBytes(s.message)),
		Details: readPairs(s.detail_keys, s.detail_values),
	}, nil
}

// ─── zstd (Aster-SPEC §5.6) ─────────────────────────────────────────────────

const (
	// CompressionThreshold: payloads strictly larger are zstd-compressed when
	// that makes them smaller.
	CompressionThreshold = int(C.ASTER_COMPRESSION_THRESHOLD)
	// MaxDecompressedSize: decompression refuses more output than this.
	MaxDecompressedSize = int(C.ASTER_MAX_DECOMPRESSED_SIZE)
)

// ZstdMaybeCompress returns the zstd-compressed payload, or nil when the
// payload should go uncompressed (at or under threshold, or no gain).
func ZstdMaybeCompress(payload []byte, threshold int) ([]byte, error) {
	p, n, free := inputPtr(payload)
	defer free()
	var out C.struct_aster_bytes_t
	if err := statusError(C.aster_zstd_maybe_compress(p, n, C.uintptr_t(threshold), &out)); err != nil {
		return nil, fmt.Errorf("aster_zstd_maybe_compress: %w", err)
	}
	if out.ptr == nil {
		return nil, nil
	}
	return takeRustBytes(out), nil
}

// ZstdDecompress decompresses a zstd frame (16 MiB cap). Corrupt or
// over-limit input returns an error matching ErrInvalidArgument.
func ZstdDecompress(data []byte) ([]byte, error) {
	p, n, free := inputPtr(data)
	defer free()
	var out C.struct_aster_bytes_t
	if err := statusError(C.aster_zstd_decompress(p, n, &out)); err != nil {
		return nil, fmt.Errorf("aster_zstd_decompress: %w", err)
	}
	b := takeRustBytes(out)
	if b == nil {
		b = []byte{}
	}
	return b, nil
}

// ─── Framing (Aster-SPEC §6.1) ─────────────────────────────────────────────

// FlagEndStream marks the last request frame of a client-stream / bidi call.
const FlagEndStream = 0x40

// EncodePayloadFrame frames a data payload, zstd-compressing it (and setting
// FlagCompressed) when it exceeds CompressionThreshold and compression helps.
func EncodePayloadFrame(payload []byte, flags byte) ([]byte, error) {
	compressed, err := ZstdMaybeCompress(payload, CompressionThreshold)
	if err != nil {
		return nil, err
	}
	if compressed != nil {
		return EncodeFrame(compressed, flags|FlagCompressed), nil
	}
	return EncodeFrame(payload, flags&^FlagCompressed), nil
}

// PayloadOf returns the logical payload of a received data frame,
// decompressing it when FlagCompressed is set.
func PayloadOf(payload []byte, flags byte) ([]byte, error) {
	if flags&FlagCompressed != 0 {
		return ZstdDecompress(payload)
	}
	return payload, nil
}

// DecodeFrame splits one whole frame into (payload, flags, rest).
func DecodeFrame(frame []byte) (payload []byte, flags byte, rest []byte, err error) {
	if len(frame) < 5 {
		return nil, 0, nil, fmt.Errorf("frame shorter than 5 bytes: %w", ErrInvalidArgument)
	}
	n := binary.LittleEndian.Uint32(frame[:4])
	if n == 0 || uint64(len(frame)-4) < uint64(n) {
		return nil, 0, nil, fmt.Errorf("frame length %d exceeds buffer: %w", n, ErrInvalidArgument)
	}
	return frame[5 : 4+n], frame[4], frame[4+n:], nil
}

func trailerFrame(st RpcStatus) ([]byte, error) {
	b, err := EncodeRpcStatus(st)
	if err != nil {
		return nil, err
	}
	return EncodeFrame(b, FlagTrailer), nil
}
