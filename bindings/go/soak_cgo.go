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
	"sync/atomic"
	"time"
)

// ─── Metrics (shared with soak_test.go) ────────────────────────────────────

type soakMetrics struct {
	cyclesCompleted atomic.Int64
	opsSubmitted    atomic.Int64
	opsCompleted    atomic.Int64
	opsCancelled    atomic.Int64
	opsErrored      atomic.Int64
	maxPendingOps   atomic.Int64
	currentPending  atomic.Int64
}

func (m *soakMetrics) recordSubmit() {
	m.opsSubmitted.Add(1)
	pending := m.currentPending.Add(1)
	for {
		currentMax := m.maxPendingOps.Load()
		if pending <= currentMax {
			break
		}
		if m.maxPendingOps.CompareAndSwap(currentMax, pending) {
			break
		}
	}
}

func (m *soakMetrics) recordComplete() {
	m.opsCompleted.Add(1)
	m.currentPending.Add(-1)
}

func (m *soakMetrics) recordError() {
	m.opsErrored.Add(1)
	m.currentPending.Add(-1)
}

func (m *soakMetrics) recordCycle() {
	m.cyclesCompleted.Add(1)
}

func (m *soakMetrics) snapshot() (submitted, completed, cancelled, errored, maxPending, currentPending int64) {
	return m.opsSubmitted.Load(),
		m.opsCompleted.Load(),
		m.opsCancelled.Load(),
		m.opsErrored.Load(),
		m.maxPendingOps.Load(),
		m.currentPending.Load()
}

// ─── Helpers ────────────────────────────────────────────────────────────────

// runSoakCycleOnRuntime performs one iteration of the churn pattern on an
// existing runtime. Pattern: start a loopback node on runtime → submit accept →
// cancel it (after a short delay ~25% of the time) → close the node. A cancelled
// operation still completes in Rust; the runtime drops that completion, and the
// caller checks the runtime's backlog drains (soakRuntimeBacklog).
//
// cfg must be loopback-only (relay disabled, 127.0.0.1:0): with the default
// relay every node start waits for the public n0 relay. Without a peer, accepts
// don't complete — this exercises the node lifecycle and validates that
// create/close cycles don't leak operations. Any failure is returned.
func runSoakCycleOnRuntime(ctx context.Context, runtime *Runtime, cfg AsterConfig, metrics *soakMetrics) error {
	node, err := startNodeOn(ctx, runtime, cfg, NodeOptions{}, -1)
	if err != nil {
		return fmt.Errorf("start node: %w", err)
	}
	// The runtime belongs to the caller; Close must not release it.
	node.ownsRuntime = false

	acceptOp, err := submitAccept(runtime, node)
	if err != nil {
		node.Close()
		return fmt.Errorf("aster_node_accept_aster: %w", err)
	}
	metrics.recordSubmit()

	if time.Now().UnixNano()&0x3F < 16 {
		time.Sleep(5 * time.Millisecond)
	}
	if err := runtime.Cancel(acceptOp); err != nil {
		node.Close()
		return fmt.Errorf("cancel accept %d: %w", acceptOp, err)
	}
	metrics.opsCancelled.Add(1)
	metrics.currentPending.Add(-1)
	if err := node.Close(); err != nil {
		return fmt.Errorf("close node: %w", err)
	}
	metrics.recordCycle()
	return nil
}

// soakRuntimeBacklog reports the completions runtime still holds for nobody
// (arrived before Register, or cancelled and not yet completed).
func soakRuntimeBacklog(runtime *Runtime) (pending, abandoned int) {
	runtime.mu.Lock()
	defer runtime.mu.Unlock()
	return len(runtime.pending), len(runtime.abandoned)
}
