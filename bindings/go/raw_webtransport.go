//go:build cgo

package aster

/*
#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"sync"
	"time"
	"unsafe"
)

// Raw WebTransport completion kinds (ABI 4.17).
const (
	ASTER_EVENT_RAW_WT_SESSION  = C.ASTER_EVENT_RAW_WT_SESSION
	ASTER_EVENT_RAW_WT_STREAM   = C.ASTER_EVENT_RAW_WT_STREAM
	ASTER_EVENT_RAW_WT_READ     = C.ASTER_EVENT_RAW_WT_READ
	ASTER_EVENT_RAW_WT_DATAGRAM = C.ASTER_EVENT_RAW_WT_DATAGRAM
	ASTER_EVENT_RAW_WT_CLOSED   = C.ASTER_EVENT_RAW_WT_CLOSED
	ASTER_EVENT_RAW_WT_ACKED    = C.ASTER_EVENT_RAW_WT_ACKED
)

// RawWTConfig configures a separate, exact-route raw WebTransport listener.
// Bytes have no RPC framing. Zero limits select native bounded defaults.
type RawWTConfig struct {
	BindAddr                                                              string
	CertificatePEM, PrivateKeyPEM                                         []byte
	Path                                                                  string
	MaxSessions, MaxStreams, MaxDatagrams, MaxDatagramBytes, MaxReadBytes uint32
	MaxPendingAuth, MaxAuthPayloadBytes                                   uint32
}

// RawWTListener owns TCP and QUIC sockets. Close joins native listener tasks.
// Authenticated listeners borrow their upcall table until Close returns.
type RawWTListener struct {
	runtime           *Runtime
	upcalls           *Upcalls
	handle            uint64
	tcpAddr, quicAddr string
	mu                sync.Mutex
	closed            bool
}

// StartRawWT runs the registered typed authenticator before accepting CONNECT.
// Metadata includes aster.webtransport.path and the unnormalized query. Peer
// addresses are transport hints, not authenticated Iroh identities.
func (r *Runtime) StartRawWT(ctx context.Context, up *Upcalls, cfg RawWTConfig) (*RawWTListener, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if up == nil || up.rt != r || up.closing.Load() || up.handler(UpcallRpcAuthenticate) == nil {
		return nil, fmt.Errorf("raw WT needs a live authenticator on its runtime: %w", ErrInvalidArgument)
	}
	return r.startRawWT(ctx, up, cfg)
}

// StartPublicRawWT explicitly accepts unauthenticated sessions. Applications
// needing a first-stream proof must gate all media until they verify it.
func (r *Runtime) StartPublicRawWT(ctx context.Context, cfg RawWTConfig) (*RawWTListener, error) {
	if cfg.MaxPendingAuth != 0 || cfg.MaxAuthPayloadBytes != 0 {
		return nil, fmt.Errorf("public raw WT does not accept auth limits: %w", ErrInvalidArgument)
	}
	return r.startRawWT(ctx, nil, cfg)
}
func (r *Runtime) startRawWT(ctx context.Context, up *Upcalls, cfg RawWTConfig) (*RawWTListener, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if cfg.BindAddr == "" {
		cfg.BindAddr = "127.0.0.1:0"
	}
	var a cArena
	defer a.free()
	c := C.aster_raw_wt_config_t{struct_size: C.uint32_t(unsafe.Sizeof(C.aster_raw_wt_config_t{})), bind_addr: a.str(cfg.BindAddr), certificate_pem: a.bytes(cfg.CertificatePEM), private_key_pem: a.bytes(cfg.PrivateKeyPEM), path: a.str(cfg.Path), max_sessions: C.uint32_t(cfg.MaxSessions), max_streams: C.uint32_t(cfg.MaxStreams), max_datagrams: C.uint32_t(cfg.MaxDatagrams), max_datagram_bytes: C.uint32_t(cfg.MaxDatagramBytes), max_read_bytes: C.uint32_t(cfg.MaxReadBytes), max_pending_auth: C.uint32_t(cfg.MaxPendingAuth), max_auth_payload_bytes: C.uint32_t(cfg.MaxAuthPayloadBytes)}
	var h C.uint64_t
	var status C.int32_t
	var statusMsg string
	if up == nil {
		status, statusMsg = ffiCall(func() C.int32_t { return C.aster_raw_wt_listener_start_public(C.uint64_t(r.handle), &c, &h) })
	} else {
		status, statusMsg = ffiCall(func() C.int32_t {
			return C.aster_raw_wt_listener_start(C.uint64_t(r.handle), C.uint64_t(up.handle), &c, &h)
		})
	}
	if err := wrapStatusMsg("raw WT start", status, statusMsg); err != nil {
		return nil, err
	}
	l := &RawWTListener{runtime: r.retain(), upcalls: up, handle: uint64(h)}
	fail := func(err error) (*RawWTListener, error) {
		if closeErr := l.Close(); closeErr != nil {
			return l, errors.Join(err, closeErr)
		}
		return nil, err
	}
	if err := ctx.Err(); err != nil {
		return fail(err)
	}
	var span C.aster_bytes_t
	if err := wrapCall("raw WT address", func() C.int32_t { return C.aster_raw_wt_listener_addr(C.uint64_t(r.handle), h, &span) }); err != nil {
		return fail(err)
	}
	defer C.aster_string_release(span.ptr, span.len)
	var addr struct {
		Version int    `json:"version"`
		TCP     string `json:"tcp_addr"`
		QUIC    string `json:"quic_addr"`
	}
	if err := json.Unmarshal(cDataToGoBytes(unsafe.Pointer(span.ptr), uintptr(span.len)), &addr); err != nil {
		return fail(err)
	}
	if addr.Version != 1 || addr.TCP == "" || addr.QUIC == "" {
		return fail(fmt.Errorf("invalid raw WT address snapshot"))
	}
	l.tcpAddr, l.quicAddr = addr.TCP, addr.QUIC
	return l, nil
}
func (l *RawWTListener) TCPAddr() string  { return l.tcpAddr }
func (l *RawWTListener) QUICAddr() string { return l.quicAddr }

// Stop seals admission and joins sessions and protocol/socket cleanup. Grace is
// an application drain budget, not a deadline on the complete join.
func (l *RawWTListener) Stop(grace time.Duration) error {
	if grace < 0 {
		return ErrInvalidArgument
	}
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.closed {
		return nil
	}
	ms := uint64(grace / time.Millisecond)
	if grace%time.Millisecond != 0 {
		ms++
	}
	ms = min(ms, uint64(1<<32-1))
	status, statusMsg := ffiCall(func() C.int32_t {
		return C.aster_raw_wt_listener_stop(C.uint64_t(l.runtime.handle), C.uint64_t(l.handle), C.uint32_t(ms))
	})
	if status != C.ASTER_STATUS_NOT_FOUND && status != C.ASTER_STATUS_ALREADY_CLOSED {
		if err := wrapStatusMsg("raw WT stop", status, statusMsg); err != nil {
			return err
		}
	}
	l.closed = true
	return l.runtime.release()
}
func (l *RawWTListener) Close() error { return l.Stop(0) }

// rawWTOp immediately cancels native work on context cancellation and collects
// its sole terminal event. A success already committed wins cancellation, so
// returned owners and consumed bytes are never silently discarded.
func rawWTOp(ctx context.Context, rt *Runtime, what string, call func(*C.aster_operation_t) C.int32_t) (*Event, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	rt.retain()
	defer rt.release()
	var op C.aster_operation_t
	if err := wrapCall(what, func() C.int32_t { return call(&op) }); err != nil {
		return nil, err
	}
	ch := make(chan *Event, 1)
	rt.Register(uint64(op), ch)
	var ev *Event
	select {
	case ev = <-ch:
	case <-ctx.Done():
		ev = rt.cancelAndCollect(uint64(op), ch)
	}
	if ev == nil {
		return nil, ErrAlreadyClosed
	}
	if ev.Kind == ASTER_EVENT_OPERATION_CANCELLED {
		rt.releaseEventBuffer(ev)
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		return nil, ErrCancelled
	}
	return ev, nil
}
func (l *RawWTListener) Accept(ctx context.Context) (*RawWTSession, error) {
	l.runtime.retain()
	defer l.runtime.release()
	ev, err := rawWTOp(ctx, l.runtime, "raw WT accept", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_listener_accept(C.uint64_t(l.runtime.handle), C.uint64_t(l.handle), 0, op)
	})
	if err != nil {
		return nil, err
	}
	if _, err = expect(l.runtime, ev, C.ASTER_EVENT_RAW_WT_SESSION, "raw WT accept"); err != nil {
		return nil, err
	}
	return &RawWTSession{runtime: l.runtime.retain(), handle: ev.Handle}, nil
}

// RawWTSession owns a raw browser session. CloseConnection actively closes the
// underlying QUIC connection (including any other sessions on that connection).
// Close additionally frees this native owner. Close all stream owners as well.
type RawWTSession struct {
	runtime *Runtime
	handle  uint64
	mu      sync.Mutex
	closed  bool
}
type RawWTSessionInfo struct {
	Version      int               `json:"version"`
	ConnectionID uint64            `json:"connection_id"`
	SessionID    uint64            `json:"session_id"`
	PeerAddress  string            `json:"peer_address"`
	Closed       bool              `json:"closed"`
	Principal    *string           `json:"principal"`
	Attributes   map[string]string `json:"attributes"`
}
type RawWTCloseInfo struct {
	Version int     `json:"version"`
	Kind    string  `json:"kind"`
	Code    *uint64 `json:"code"`
	Reason  string  `json:"reason"`
}
type RawWTStats struct {
	Version          int    `json:"version"`
	RTTMicros        uint64 `json:"rtt_micros"`
	CongestionWindow uint64 `json:"cwnd"`
	MTU              uint16 `json:"mtu"`
	SentPackets      uint64 `json:"sent_packets"`
	LostPackets      uint64 `json:"lost_packets"`
	LostBytes        uint64 `json:"lost_bytes"`
	CongestionEvents uint64 `json:"congestion_events"`
	UDPTxBytes       uint64 `json:"udp_tx_bytes"`
	UDPTxPackets     uint64 `json:"udp_tx_packets"`
	UDPRxBytes       uint64 `json:"udp_rx_bytes"`
	UDPRxPackets     uint64 `json:"udp_rx_packets"`
	PingTx           uint64 `json:"ping_tx"`
	AckRx            uint64 `json:"ack_rx"`
}

func rawWTJSON(what string, call func(*C.aster_bytes_t) C.int32_t, out any) error {
	var span C.aster_bytes_t
	status, statusMsg := ffiCall(func() C.int32_t { return call(&span) })
	defer C.aster_string_release(span.ptr, span.len)
	if err := wrapStatusMsg(what, status, statusMsg); err != nil {
		return err
	}
	data := cDataToGoBytes(unsafe.Pointer(span.ptr), uintptr(span.len))
	var version struct {
		Version int `json:"version"`
	}
	if err := json.Unmarshal(data, &version); err != nil {
		return err
	}
	if version.Version != 1 {
		return fmt.Errorf("%s: unsupported snapshot version %d", what, version.Version)
	}
	return json.Unmarshal(data, out)
}
func (s *RawWTSession) Info() (RawWTSessionInfo, error) {
	var out RawWTSessionInfo
	err := rawWTJSON("raw WT info", func(p *C.aster_bytes_t) C.int32_t {
		return C.aster_raw_wt_session_info(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), p)
	}, &out)
	return out, err
}
func (s *RawWTSession) Stats() (RawWTStats, error) {
	var out RawWTStats
	err := rawWTJSON("raw WT stats", func(p *C.aster_bytes_t) C.int32_t {
		return C.aster_raw_wt_session_stats(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), p)
	}, &out)
	return out, err
}
func (s *RawWTSession) CloseConnection(code uint32, reason []byte) error {
	var a cArena
	defer a.free()
	return wrapCall("raw WT close", func() C.int32_t {
		return C.aster_raw_wt_session_close(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), C.uint32_t(code), a.bytes(reason))
	})
}
func (s *RawWTSession) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return nil
	}
	status, statusMsg := ffiCall(func() C.int32_t {
		return C.aster_raw_wt_session_free(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle))
	})
	if status != C.ASTER_STATUS_NOT_FOUND && status != C.ASTER_STATUS_ALREADY_CLOSED {
		if err := wrapStatusMsg("raw WT free", status, statusMsg); err != nil {
			return err
		}
	}
	s.closed = true
	return s.runtime.release()
}
func (s *RawWTSession) Closed(ctx context.Context) (RawWTCloseInfo, error) {
	var out RawWTCloseInfo
	ev, err := rawWTOp(ctx, s.runtime, "raw WT closed", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_session_closed(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), 0, op)
	})
	if err != nil {
		return out, err
	}
	data, err := expect(s.runtime, ev, C.ASTER_EVENT_RAW_WT_CLOSED, "raw WT closed")
	if err != nil {
		return out, err
	}
	if err = json.Unmarshal(data, &out); err == nil && out.Version != 1 {
		err = fmt.Errorf("unsupported raw WT close snapshot")
	}
	return out, err
}
func (s *RawWTSession) MaxDatagramSize() (size uint64, available bool, err error) {
	var n C.uint64_t
	var some C.uint32_t
	err = wrapCall("raw WT datagram size", func() C.int32_t {
		return C.aster_raw_wt_session_max_datagram_size(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), &n, &some)
	})
	return uint64(n), some != 0, err
}
func (s *RawWTSession) SendDatagram(data []byte) error {
	var a cArena
	defer a.free()
	return wrapCall("raw WT send datagram", func() C.int32_t {
		return C.aster_raw_wt_session_send_datagram(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), a.bytes(data))
	})
}
func (s *RawWTSession) ReceiveDatagram(ctx context.Context) ([]byte, error) {
	ev, err := rawWTOp(ctx, s.runtime, "raw WT receive datagram", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_session_receive_datagram(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), 0, op)
	})
	if err != nil {
		return nil, err
	}
	return expect(s.runtime, ev, C.ASTER_EVENT_RAW_WT_DATAGRAM, "raw WT receive datagram")
}
func (s *RawWTSession) stream(ctx context.Context, what string, call func(*C.aster_operation_t) C.int32_t) (*RawWTStream, error) {
	s.runtime.retain()
	defer s.runtime.release()
	ev, err := rawWTOp(ctx, s.runtime, what, call)
	if err != nil {
		return nil, err
	}
	if _, err = expect(s.runtime, ev, C.ASTER_EVENT_RAW_WT_STREAM, what); err != nil {
		return nil, err
	}
	if ev.Flags == 0 || ev.Flags&^uint32(3) != 0 {
		C.aster_raw_wt_stream_free(C.uint64_t(s.runtime.handle), C.uint64_t(ev.Handle))
		return nil, fmt.Errorf("invalid raw WT stream directions")
	}
	return &RawWTStream{runtime: s.runtime.retain(), handle: ev.Handle, CanSend: ev.Flags&1 != 0, CanReceive: ev.Flags&2 != 0}, nil
}
func (s *RawWTSession) OpenBi(ctx context.Context) (*RawWTStream, error) {
	return s.stream(ctx, "raw WT open bi", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_session_open_bi(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), 0, op)
	})
}
func (s *RawWTSession) OpenUni(ctx context.Context) (*RawWTStream, error) {
	return s.stream(ctx, "raw WT open uni", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_session_open_uni(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), 0, op)
	})
}
func (s *RawWTSession) AcceptBi(ctx context.Context) (*RawWTStream, error) {
	return s.stream(ctx, "raw WT accept bi", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_session_accept_bi(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), 0, op)
	})
}
func (s *RawWTSession) AcceptUni(ctx context.Context) (*RawWTStream, error) {
	return s.stream(ctx, "raw WT accept uni", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_session_accept_uni(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), 0, op)
	})
}

// RawWTStream is one unframed uni/bidirectional stream owner. Native operations
// serialize each direction. Cancellation of Write can leave a transmitted prefix;
// callers must not retry the complete payload automatically.
type RawWTStream struct {
	runtime             *Runtime
	handle              uint64
	CanSend, CanReceive bool
	mu                  sync.Mutex
	closed              bool
}

func (s *RawWTStream) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return nil
	}
	status, statusMsg := ffiCall(func() C.int32_t {
		return C.aster_raw_wt_stream_free(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle))
	})
	if status != C.ASTER_STATUS_NOT_FOUND && status != C.ASTER_STATUS_ALREADY_CLOSED {
		if err := wrapStatusMsg("raw WT stream free", status, statusMsg); err != nil {
			return err
		}
	}
	s.closed = true
	return s.runtime.release()
}

// Read returns up to maxBytes, or io.EOF. No unbounded read-to-end allocation.
func (s *RawWTStream) Read(ctx context.Context, maxBytes uint32) ([]byte, error) {
	ev, err := rawWTOp(ctx, s.runtime, "raw WT read", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_stream_read(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), C.uint32_t(maxBytes), 0, op)
	})
	if err != nil {
		return nil, err
	}
	data, err := expect(s.runtime, ev, C.ASTER_EVENT_RAW_WT_READ, "raw WT read")
	if err == nil && ev.Flags&1 != 0 {
		return data, io.EOF
	}
	return data, err
}
func (s *RawWTStream) unit(ctx context.Context, what string, call func(*C.aster_operation_t) C.int32_t) error {
	ev, err := rawWTOp(ctx, s.runtime, what, call)
	if err != nil {
		return err
	}
	_, err = expect(s.runtime, ev, C.ASTER_EVENT_UNIT_RESULT, what)
	return err
}
func (s *RawWTStream) Write(ctx context.Context, data []byte) error {
	if len(data) > 16<<20 {
		return fmt.Errorf("raw WT write exceeds 16 MiB ABI limit: %w", ErrInvalidArgument)
	}
	var a cArena
	defer a.free()
	return s.unit(ctx, "raw WT write", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_stream_write(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), a.bytes(data), 0, op)
	})
}

// Finish queues FIN; it neither releases the send owner nor proves receipt.
func (s *RawWTStream) Finish(ctx context.Context) error {
	return s.unit(ctx, "raw WT finish", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_stream_finish(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), 0, op)
	})
}
func (s *RawWTStream) Reset(ctx context.Context, code uint32) error {
	return s.unit(ctx, "raw WT reset", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_stream_reset(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), C.uint32_t(code), 0, op)
	})
}
func (s *RawWTStream) Stop(ctx context.Context, code uint32) error {
	return s.unit(ctx, "raw WT stop receive", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_stream_stop(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), C.uint32_t(code), 0, op)
	})
}

// SetPriority uses signed Quinn priority: larger values are scheduled sooner.
func (s *RawWTStream) SetPriority(ctx context.Context, priority int32) error {
	return s.unit(ctx, "raw WT priority", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_stream_set_priority(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), C.int32_t(priority), 0, op)
	})
}
func (s *RawWTStream) Priority(ctx context.Context) (value int32, available bool, err error) {
	ev, err := rawWTOp(ctx, s.runtime, "raw WT priority", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_stream_priority(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), 0, op)
	})
	if err != nil {
		return 0, false, err
	}
	data, err := expect(s.runtime, ev, C.ASTER_EVENT_BYTES_RESULT, "raw WT priority")
	if err != nil {
		return 0, false, err
	}
	return decodeRawWTPriority(ev.Flags, data)
}

// TransportAcked waits for all bytes and FIN to be acknowledged. False means
// reset or connection loss; a context timeout is an error, never a false ACK.
func (s *RawWTStream) TransportAcked(ctx context.Context) (bool, error) {
	ev, err := rawWTOp(ctx, s.runtime, "raw WT ACK", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_raw_wt_stream_transport_acked(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), 0, op)
	})
	if err != nil {
		return false, err
	}
	if _, err = expect(s.runtime, ev, C.ASTER_EVENT_RAW_WT_ACKED, "raw WT ACK"); err != nil {
		return false, err
	}
	return ev.Flags&1 != 0, nil
}

func decodeRawWTPriority(flags uint32, data []byte) (int32, bool, error) {
	if flags == 0 && len(data) == 0 {
		return 0, false, nil
	}
	if flags != 1 || len(data) != 4 {
		return 0, false, fmt.Errorf("invalid raw WT priority payload")
	}
	return int32(binary.LittleEndian.Uint32(data)), true, nil
}
