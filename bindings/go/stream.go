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
	"errors"
	"fmt"
	"io"
	"unsafe"
)

// SendStream is the sending side of a stream.
// It is safe to use from multiple goroutines.
type SendStream struct {
	handle  uint64
	runtime *Runtime
	// isSender is true for send streams opened via OpenBi/OpenUni.
	// It is false for send streams obtained via AcceptBi (where we are the receiver
	// of the remote's send side).
	isSender bool
}

// Write sends data on the stream.
func (s *SendStream) Write(ctx context.Context, data []byte) error {
	bytes := C.struct_aster_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&data[0])),
		len: C.uintptr_t(len(data)),
	}
	var opID C.aster_operation_t
	r := C.aster_stream_write(
		C.uint64_t(s.runtime.handle),
		C.uint64_t(s.handle),
		bytes,
		0,
		&opID,
	)
	if r != 0 {
		return fmt.Errorf("aster_stream_write: %w", Error(r))
	}

	ev, err := s.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return fmt.Errorf("write poll: %w", err)
	}
	if ev.Kind != ASTER_EVENT_SEND_COMPLETED {
		return fmt.Errorf("write: unexpected event %d", ev.Kind)
	}
	return nil
}

// Finish signals that no more data will be written on this stream.
func (s *SendStream) Finish(ctx context.Context) error {
	var opID C.aster_operation_t
	r := C.aster_stream_finish(
		C.uint64_t(s.runtime.handle),
		C.uint64_t(s.handle),
		0,
		&opID,
	)
	if r != 0 {
		return fmt.Errorf("aster_stream_finish: %w", Error(r))
	}

	ev, err := s.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return fmt.Errorf("finish poll: %w", err)
	}
	if ev.Kind != ASTER_EVENT_STREAM_FINISHED {
		return fmt.Errorf("finish: unexpected event %d", ev.Kind)
	}
	return nil
}

// Stopped returns whether the receive side has stopped.
func (s *SendStream) Stopped(ctx context.Context) (bool, error) {
	var opID C.aster_operation_t
	r := C.aster_stream_stopped(
		C.uint64_t(s.runtime.handle),
		C.uint64_t(s.handle),
		0,
		&opID,
	)
	if r != 0 {
		return false, fmt.Errorf("aster_stream_stopped: %w", Error(r))
	}

	ev, err := s.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return false, fmt.Errorf("stopped poll: %w", err)
	}
	return ev.Kind == ASTER_EVENT_STREAM_RESET, nil
}

// Close releases resources held by the send stream.
func (s *SendStream) Close() error {
	r := C.aster_send_stream_free(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle))
	if r != 0 {
		return fmt.Errorf("aster_send_stream_free: %w", Error(r))
	}
	return nil
}

// RecvStream is the receiving side of a stream.
type RecvStream struct {
	handle  uint64
	runtime *Runtime
}

// Read reads data from the stream.
// It returns io.EOF when the stream has finished.
func (r *RecvStream) Read(ctx context.Context) ([]byte, error) {
	var opID C.aster_operation_t
	maxLen := C.uintptr_t(64 * 1024) // 64 KiB

	sr := C.aster_stream_read(
		C.uint64_t(r.runtime.handle),
		C.uint64_t(r.handle),
		maxLen,
		0,
		&opID,
	)
	if sr != 0 {
		return nil, fmt.Errorf("aster_stream_read: %w", Error(sr))
	}

	ev, err := r.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("read poll: %w", err)
	}
	if ev.Kind == ASTER_EVENT_STREAM_FINISHED {
		return nil, io.EOF
	}
	if ev.Kind == ASTER_EVENT_STREAM_RESET {
		return nil, errors.New("stream reset")
	}
	if ev.Kind != ASTER_EVENT_FRAME_RECEIVED {
		return nil, fmt.Errorf("read: unexpected event %d", ev.Kind)
	}

	data := cDataToGoBytes(ev.DataPtr, ev.DataLen)

	if ev.Buffer != 0 {
		r.runtime.ReleaseBuffer(ev.Buffer)
	}

	return data, nil
}

// ReadAll reads all remaining data from the stream until EOF or error.
func (r *RecvStream) ReadAll(ctx context.Context) ([]byte, error) {
	var out []byte
	for {
		data, err := r.Read(ctx)
		if err != nil {
			if errors.Is(err, io.EOF) {
				return out, nil
			}
			return out, err
		}
		out = append(out, data...)
	}
}

// ReadExact reads exactly n bytes from the stream.
func (r *RecvStream) ReadExact(ctx context.Context, n uintptr) ([]byte, error) {
	var opID C.aster_operation_t
	sr := C.aster_stream_read_exact(
		C.uint64_t(r.runtime.handle),
		C.uint64_t(r.handle),
		C.uintptr_t(n),
		0,
		&opID,
	)
	if sr != 0 {
		return nil, fmt.Errorf("aster_stream_read_exact: %w", Error(sr))
	}

	ev, err := r.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("read exact poll: %w", err)
	}
	if ev.Kind == ASTER_EVENT_STREAM_FINISHED {
		return nil, io.EOF
	}
	if ev.Kind == ASTER_EVENT_STREAM_RESET {
		return nil, errors.New("stream reset")
	}
	if ev.Kind != ASTER_EVENT_FRAME_RECEIVED {
		return nil, fmt.Errorf("read exact: unexpected event %d", ev.Kind)
	}

	data := cDataToGoBytes(ev.DataPtr, ev.DataLen)
	if uintptr(len(data)) != n {
		return data, fmt.Errorf("read exact: got %d bytes, expected %d", len(data), n)
	}

	if ev.Buffer != 0 {
		r.runtime.ReleaseBuffer(ev.Buffer)
	}

	return data, nil
}

// Stop stops the receive stream, signaling an error to the writer.
func (r *RecvStream) Stop() error {
	sr := C.aster_stream_stop(
		C.uint64_t(r.runtime.handle),
		C.uint64_t(r.handle),
		0,
	)
	if sr != 0 {
		return fmt.Errorf("aster_stream_stop: %w", Error(sr))
	}
	return nil
}

// Close releases resources held by the receive stream.
func (r *RecvStream) Close() error {
	sr := C.aster_recv_stream_free(C.uint64_t(r.runtime.handle), C.uint64_t(r.handle))
	if sr != 0 {
		return fmt.Errorf("aster_recv_stream_free: %w", Error(sr))
	}
	return nil
}

// Writer adapts the stream to io.WriteCloser: Write blocks until the data is
// written (bounded by ctx), Close finishes the stream.
func (s *SendStream) Writer(ctx context.Context) io.WriteCloser {
	return &streamWriter{ctx: ctx, s: s}
}

type streamWriter struct {
	ctx context.Context
	s   *SendStream
}

func (w *streamWriter) Write(p []byte) (int, error) {
	if len(p) == 0 {
		return 0, nil
	}
	if err := w.s.Write(w.ctx, p); err != nil {
		return 0, err
	}
	return len(p), nil
}

func (w *streamWriter) Close() error { return w.s.Finish(w.ctx) }

// Reader adapts the stream to io.Reader (io.EOF once the peer finishes).
func (r *RecvStream) Reader(ctx context.Context) io.Reader {
	return &streamReader{ctx: ctx, r: r}
}

type streamReader struct {
	ctx  context.Context
	r    *RecvStream
	buf  []byte
	done bool
}

func (sr *streamReader) Read(p []byte) (int, error) {
	for len(sr.buf) == 0 {
		if sr.done {
			return 0, io.EOF
		}
		data, err := sr.r.Read(sr.ctx)
		if errors.Is(err, io.EOF) {
			sr.done = true
			continue
		}
		if err != nil {
			return 0, err
		}
		sr.buf = data
	}
	n := copy(p, sr.buf)
	sr.buf = sr.buf[n:]
	return n, nil
}
