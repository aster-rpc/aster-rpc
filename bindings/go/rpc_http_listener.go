//go:build cgo

package aster

/*
#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"errors"
	"fmt"
	"sync"
	"time"
	"unsafe"
)

// HTTPListenerConfig configures an RPC listener. CertificatePEM
// and PrivateKeyPEM must contain a certificate chain and its private key.
type HTTPListenerConfig struct {
	// BindAddr is an IP socket address; empty means 127.0.0.1:0.
	BindAddr       string
	CertificatePEM []byte
	PrivateKeyPEM  []byte
	// WebTransport also binds QUIC on the TCP listener's port.
	WebTransport bool
	// Zero limits select 256 pending authentications and 64 KiB per payload.
	MaxPendingAuth      uint32
	MaxAuthPayloadBytes uint32
}

// HTTPListener owns an HTTPS listener and its optional WebTransport listener.
// Close it before closing its server and, for authenticated serving, upcall table.
// Server shutdown also stops the native listener; Close still releases this
// wrapper's runtime reference. Authenticated listeners resolve every RPC call.
type HTTPListener struct {
	server   *AsterServer
	upcalls  *Upcalls
	runtime  *Runtime
	handle   uint64
	tcpAddr  string
	quicAddr string
	mu       sync.Mutex
	closed   bool
}

// StartHTTP serves this server's RPC services through authenticated HTTPS and,
// optionally, WebTransport. Register up.ServeRPCAuthenticator first. The table
// must share the server's runtime and remain open while serving.
//
// Startup returns only after all requested sockets are bound. The context is
// checked before and after the synchronous native startup. If it expires during
// startup, the newly created listener is stopped before returning. A nonnil
// listener returned with an error owns cleanup that failed and must be closed.
func (s *AsterServer) StartHTTP(ctx context.Context, up *Upcalls, cfg HTTPListenerConfig) (*HTTPListener, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if up == nil || up.rt != s.node.runtime || up.closing.Load() || up.handler(UpcallRpcAuthenticate) == nil {
		return nil, fmt.Errorf("HTTP listener requires a live RPC authenticator on the server runtime: %w", ErrInvalidArgument)
	}
	return s.startHTTP(ctx, up, cfg, false)
}

// StartPublicHTTP explicitly serves public HTTPS/optional WebTransport without
// an authenticator. Calls have no capability attributes or authenticated Iroh
// provenance; Gate 3 and Iroh-only requirements still apply. Session-scoped
// services are rejected before their factory runs. Authentication limits must
// be zero. Readiness, cancellation and ownership follow StartHTTP.
func (s *AsterServer) StartPublicHTTP(ctx context.Context, cfg HTTPListenerConfig) (*HTTPListener, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if cfg.MaxPendingAuth != 0 || cfg.MaxAuthPayloadBytes != 0 {
		return nil, fmt.Errorf("public HTTP listener does not accept authentication limits: %w", ErrInvalidArgument)
	}
	return s.startHTTP(ctx, nil, cfg, true)
}

func (s *AsterServer) startHTTP(ctx context.Context, up *Upcalls, cfg HTTPListenerConfig, public bool) (*HTTPListener, error) {
	if len(cfg.CertificatePEM) == 0 || len(cfg.PrivateKeyPEM) == 0 {
		return nil, fmt.Errorf("HTTP listener requires certificate and private key PEM: %w", ErrInvalidArgument)
	}
	s.closeMu.Lock()
	defer s.closeMu.Unlock()
	if s.closing.Load() {
		return nil, ErrAlreadyClosed
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if cfg.BindAddr == "" {
		cfg.BindAddr = "127.0.0.1:0"
	}
	var arena cArena
	defer arena.free()
	native := C.aster_http_server_config_t{
		struct_size:            C.uint32_t(unsafe.Sizeof(C.aster_http_server_config_t{})),
		bind_addr:              arena.str(cfg.BindAddr),
		cert_pem:               arena.bytes(cfg.CertificatePEM),
		key_pem:                arena.bytes(cfg.PrivateKeyPEM),
		max_pending_auth:       C.uint32_t(cfg.MaxPendingAuth),
		max_auth_payload_bytes: C.uint32_t(cfg.MaxAuthPayloadBytes),
	}
	if cfg.WebTransport {
		native.flags = C.ASTER_HTTP_SERVER_WEBTRANSPORT
	}
	var handle C.uint64_t
	rt := s.node.runtime
	var status C.int32_t
	operation := "aster_server_http_start"
	if public {
		operation = "aster_server_http_start_public"
		status = C.aster_server_http_start_public(C.uint64_t(rt.handle), s.ds.handle, &native, &handle)
	} else {
		status = C.aster_server_http_start(C.uint64_t(rt.handle), s.ds.handle, C.uint64_t(up.handle), &native, &handle)
	}
	if err := wrapStatus(operation, status); err != nil {
		return nil, err
	}
	listener := &HTTPListener{server: s, upcalls: up, runtime: rt.retain(), handle: uint64(handle)}
	fail := func(err error) (*HTTPListener, error) {
		if stopErr := listener.Close(); stopErr != nil {
			return listener, errors.Join(err, stopErr)
		}
		return nil, err
	}
	if err := ctx.Err(); err != nil {
		return fail(err)
	}
	var err error
	if listener.tcpAddr, err = listener.address(false); err != nil {
		return fail(err)
	}
	if cfg.WebTransport {
		if listener.quicAddr, err = listener.address(true); err != nil {
			return fail(err)
		}
	}
	return listener, nil
}

func (l *HTTPListener) address(quic bool) (string, error) {
	var buffer C.uint64_t
	var ptr *C.uint8_t
	var length C.uint32_t
	var status C.int32_t
	if quic {
		status = C.aster_http_listener_quic_addr(C.uint64_t(l.runtime.handle), C.uint64_t(l.handle), &buffer, &ptr, &length)
	} else {
		status = C.aster_http_listener_tcp_addr(C.uint64_t(l.runtime.handle), C.uint64_t(l.handle), &buffer, &ptr, &length)
	}
	if buffer != 0 {
		defer l.runtime.ReleaseBuffer(uint64(buffer))
	}
	if err := wrapStatus("HTTP listener address", status); err != nil {
		return "", err
	}
	return C.GoStringN((*C.char)(unsafe.Pointer(ptr)), C.int(length)), nil
}

// TCPAddr is the bound HTTPS socket address, including the selected port.
func (l *HTTPListener) TCPAddr() string { return l.tcpAddr }

// QUICAddr is the bound WebTransport socket address, or empty when disabled.
func (l *HTTPListener) QUICAddr() string { return l.quicAddr }

// Stop stops accepting calls, waits for grace, then cancels remaining calls and
// joins the listener tasks. Grace bounds application draining; QUIC protocol and
// socket cleanup can extend beyond it. It is idempotent. A native error preserves
// ownership so the caller can retry. A zero grace cancels application work
// immediately and still joins protocol cleanup.
func (l *HTTPListener) Stop(grace time.Duration) error {
	if grace < 0 {
		return fmt.Errorf("HTTP listener grace must be nonnegative: %w", ErrInvalidArgument)
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
	status := C.aster_http_listener_stop(C.uint64_t(l.runtime.handle), C.uint64_t(l.handle), C.uint32_t(ms))
	if status != C.ASTER_STATUS_NOT_FOUND && status != C.ASTER_STATUS_ALREADY_CLOSED {
		if err := wrapStatus("aster_http_listener_stop", status); err != nil {
			return err
		}
	}
	l.closed = true
	return l.runtime.release()
}

// Close cancels application work immediately, joins protocol cleanup and
// releases the listener's ownership.
func (l *HTTPListener) Close() error { return l.Stop(0) }
