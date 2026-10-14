//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"fmt"
	"sync"
	"unsafe"
)

// HealthConfig says where the ops listener (/healthz, /readyz, /metrics)
// listens.
type HealthConfig struct {
	// Host to bind; "" = 127.0.0.1. Pods need "0.0.0.0" for probes.
	Host string
	// Port to bind; 0 = an ephemeral port (read it with Addr).
	Port uint16
	// FromNodeConfig ignores Host and Port and uses the node's AsterConfig
	// (ASTER_HEALTH_PORT / _HOST / _ENABLED, [health]); an error matching
	// ErrInvalidArgument when that config has health disabled.
	FromNodeConfig bool
}

// HealthServer is a running ops listener over plain HTTP:
//
//	GET /healthz   liveness: 200 {"status":"ok",...} or 503
//	GET /readyz    readiness: 200 or 503 with {"status","services","checks"}
//	GET /metrics   Prometheus text (built-in aster.* metrics and your own)
//
// It starts healthy and not ready. Attach it to a server with
// ServerOptions.Health (ready once the server serves, not ready first on
// shutdown) or drive readiness with SetReady. Readiness requires the ready
// flag, liveness and every check set with SetCheck. Methods are safe for
// concurrent use.
type HealthServer struct {
	rt     C.uint64_t
	handle uint64
	mu     sync.Mutex
	closed bool
}

// StartHealth starts an ops listener for node, joining the observability
// pipeline (or starting an environment-only one) with the Prometheus pull
// reader. cfg nil = the node's AsterConfig.
func StartHealth(node *Node, cfg *HealthConfig) (*HealthServer, error) {
	if (cfg == nil || cfg.FromNodeConfig) && node.healthSettings != nil {
		settings := node.healthSettings
		if !settings.enabled {
			return nil, fmt.Errorf("health is disabled in the node's config: %w", ErrInvalidArgument)
		}
		cfg = &HealthConfig{Host: settings.host, Port: settings.port}
	}
	EnsureObservability()
	var a cArena
	defer a.free()
	var cc *C.aster_health_config_t
	if cfg != nil && !cfg.FromNodeConfig {
		cc = (*C.aster_health_config_t)(a.alloc(unsafe.Sizeof(C.aster_health_config_t{})))
		cc.struct_size = C.uint32_t(unsafe.Sizeof(*cc))
		cc.port = C.uint16_t(cfg.Port)
		cc.host = a.str(cfg.Host)
	}
	rt := C.uint64_t(node.runtime.handle)
	var h C.aster_health_t
	if err := ffiStatus(func() C.int32_t { return C.aster_health_start(rt, C.uint64_t(node.handle), cc, &h) }); err != nil {
		return nil, fmt.Errorf("aster_health_start: %w", err)
	}
	return &HealthServer{rt: rt, handle: uint64(h)}, nil
}

func (h *HealthServer) call(what string, fn func() C.int32_t) error {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.closed {
		return fmt.Errorf("%s: %w", what, ErrAlreadyClosed)
	}
	if err := ffiStatus(func() C.int32_t { return fn() }); err != nil {
		return fmt.Errorf("%s: %w", what, err)
	}
	return nil
}

// SetReady sets the ready flag (/readyz).
func (h *HealthServer) SetReady(ready bool) error {
	return h.call("aster_health_set_ready", func() C.int32_t {
		return C.aster_health_set_ready(C.aster_health_t(h.handle), C.bool(ready))
	})
}

// SetHealthy sets liveness (/healthz); unhealthy also fails /readyz.
func (h *HealthServer) SetHealthy(healthy bool) error {
	return h.call("aster_health_set_healthy", func() C.int32_t {
		return C.aster_health_set_healthy(C.aster_health_t(h.handle), C.bool(healthy))
	})
}

// SetCheck sets the named readiness check. A check is created failing on
// first use (a declared dependency is not ready until it says so); every
// check must pass for /readyz, which lists them under "checks".
func (h *HealthServer) SetCheck(name string, ok bool) error {
	if name == "" {
		return fmt.Errorf("health check needs a name: %w", ErrInvalidArgument)
	}
	return h.call("aster_health_check_set", func() C.int32_t {
		return C.aster_health_check_set(C.aster_health_t(h.handle), strArg(name), C.bool(ok))
	})
}

// Addr is the bound address as "ip:port" (the real port after Port 0).
func (h *HealthServer) Addr() (string, error) {
	var out C.struct_aster_bytes_t
	if err := h.call("aster_health_addr", func() C.int32_t {
		return C.aster_health_addr(C.aster_health_t(h.handle), &out)
	}); err != nil {
		return "", err
	}
	return string(takeRustBytes(out)), nil
}

// Stop stops the listener and releases its observability reference; the port
// is free on return. Idempotent.
func (h *HealthServer) Stop() error {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.closed {
		return nil
	}
	h.closed = true
	if err := ffiStatus(func() C.int32_t { return C.aster_health_stop(h.rt, C.aster_health_t(h.handle)) }); err != nil {
		return fmt.Errorf("aster_health_stop: %w", err)
	}
	return nil
}

// Close is Stop.
func (h *HealthServer) Close() error { return h.Stop() }
