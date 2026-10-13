//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"fmt"
	"sync"
)

// TrustMode is the Gate-0 trust posture (aster_trust_mode_t).
type TrustMode uint32

const (
	// TrustProtected: unknown peers may use only the admission ALPNs (default).
	TrustProtected TrustMode = C.ASTER_TRUST_MODE_PROTECTED
	// TrustOpenDev: local/dev; unknown peers may use any ALPN.
	TrustOpenDev TrustMode = C.ASTER_TRUST_MODE_OPEN_DEV
)

// Gate is a Gate-0 connection admission policy decided in Rust
// (aster::GatePolicy, C ABI aster_gate_*). Pass it in NodeOptions.Gate (or to
// Node.SetGate): admission ALPNs stay open to everyone, every other ALPN needs
// an admitted peer (or TrustOpenDev), and rejected peers are closed with the
// reject code and reason.
//
// A gate belongs to one Runtime; nodes gated by it run on that runtime.
// Peers are 64-hex node ids (case-insensitive). Close frees the handle (nodes
// using it keep it until they close).
type Gate struct {
	runtime *Runtime
	handle  uint64
	once    sync.Once
}

// NewGate creates a PROTECTED gate (admission ALPNs open, other peers
// rejected with 403 "peer not admitted"). With a nil runtime the gate creates
// one, shared with the nodes it gates.
func NewGate(rt *Runtime) (*Gate, error) {
	if rt == nil {
		var err error
		if rt, err = newDefaultRuntime(); err != nil {
			return nil, err
		}
	} else {
		rt.retain()
	}
	var h C.aster_gate_t
	if err := statusError(C.aster_gate_new(C.uint64_t(rt.handle), &h)); err != nil {
		_ = rt.release()
		return nil, fmt.Errorf("aster_gate_new: %w", err)
	}
	return &Gate{runtime: rt, handle: uint64(h)}, nil
}

// Runtime is the runtime this gate (and every node it gates) lives on.
func (g *Gate) Runtime() *Runtime { return g.runtime }

func (g *Gate) rt() C.uint64_t { return C.uint64_t(g.runtime.handle) }

// SetTrustMode sets the trust posture.
func (g *Gate) SetTrustMode(mode TrustMode) error {
	return wrapStatus("aster_gate_set_trust_mode",
		C.aster_gate_set_trust_mode(g.rt(), C.aster_gate_t(g.handle), C.uint32_t(mode)))
}

// SetAdmissionALPNs replaces the ALPNs that stay open to peers that are not
// admitted (may be empty).
func (g *Gate) SetAdmissionALPNs(alpns ...string) error {
	var a cArena
	defer a.free()
	return wrapStatus("aster_gate_set_admission_alpns",
		C.aster_gate_set_admission_alpns(g.rt(), C.aster_gate_t(g.handle), a.list(alpns)))
}

// SetReject sets the QUIC close code and reason sent to rejected peers.
func (g *Gate) SetReject(code uint32, reason string) error {
	var a cArena
	defer a.free()
	return wrapStatus("aster_gate_set_reject",
		C.aster_gate_set_reject(g.rt(), C.aster_gate_t(g.handle), C.uint32_t(code), a.str(reason)))
}

// Admit admits peer (64-hex node id, no expiry).
func (g *Gate) Admit(peer string) error {
	var a cArena
	defer a.free()
	return wrapStatus("aster_gate_admit", C.aster_gate_admit(g.rt(), C.aster_gate_t(g.handle), a.str(peer)))
}

// Revoke revokes peer's admission; new connections are gated again.
func (g *Gate) Revoke(peer string) error {
	var a cArena
	defer a.free()
	return wrapStatus("aster_gate_revoke", C.aster_gate_revoke(g.rt(), C.aster_gate_t(g.handle), a.str(peer)))
}

// IsAdmitted reports whether peer holds an unexpired admission.
func (g *Gate) IsAdmitted(peer string) (bool, error) {
	var a cArena
	defer a.free()
	var out C.uint32_t
	if err := wrapStatus("aster_gate_is_admitted",
		C.aster_gate_is_admitted(g.rt(), C.aster_gate_t(g.handle), a.str(peer), &out)); err != nil {
		return false, err
	}
	return out != 0, nil
}

// ShouldAllow reports whether a connection from peer on alpn would be accepted.
func (g *Gate) ShouldAllow(peer, alpn string) (bool, error) {
	var a cArena
	defer a.free()
	var out C.uint32_t
	if err := wrapStatus("aster_gate_should_allow",
		C.aster_gate_should_allow(g.rt(), C.aster_gate_t(g.handle), a.str(peer), a.str(alpn), &out)); err != nil {
		return false, err
	}
	return HookDecision(out) == HookAllow, nil
}

// Close frees the gate handle and releases its runtime reference.
func (g *Gate) Close() error {
	var err error
	g.once.Do(func() {
		err = wrapStatus("aster_gate_free", C.aster_gate_free(g.rt(), C.aster_gate_t(g.handle)))
		if rerr := g.runtime.release(); err == nil {
			err = rerr
		}
	})
	return err
}

func wrapStatus(what string, r C.int32_t) error {
	if err := statusError(r); err != nil {
		return fmt.Errorf("%s: %w", what, err)
	}
	return nil
}
