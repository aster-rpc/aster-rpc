//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include "iroh_ffi.h"
*/
import "C"

import (
	"context"
	"sync/atomic"
	"time"
)

// ─── Metrics (shared with soak_test.go) ────────────────────────────────────

type soakMetrics struct {
	cyclesCompleted atomic.Int64
	opsSubmitted  atomic.Int64
	opsCompleted atomic.Int64
	opsCancelled atomic.Int64
	opsErrored  atomic.Int64
	maxPendingOps  atomic.Int64
	currentPending atomic.Int64
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

// soakPollForEvent polls for a specific event kind within a timeout.
// Returns true if the event was observed, false on timeout.
func soakPollForEvent(runtime *Runtime, kind uint32, timeoutMs int) bool {
	deadline := time.Now().Add(time.Duration(timeoutMs) * time.Millisecond)
	for time.Now().Before(deadline) {
		var events [4]C.iroh_event_t
		n := C.iroh_poll_events(C.uint64_t(runtime.handle), &events[0], 4, 100)
		for i := 0; i < int(n); i++ {
			if uint32(events[i].kind) == kind {
				return true
			}
		}
	}
	return false
}

// soakDrainAllEvents drains all pending events from the runtime.
func soakDrainAllEvents(runtime *Runtime) {
	for {
		var events [16]C.iroh_event_t
		n := C.iroh_poll_events(C.uint64_t(runtime.handle), &events[0], 16, 0)
		if int(n) == 0 {
			break
		}
	}
}

// runSoakCycleOnRuntime performs one iteration of the churn pattern on an existing runtime.
// Pattern: create node → submit accept → optionally cancel → drain → close node.
//
// Without a peer, accepts don't complete — this exercises the node lifecycle
// and validates that create/close cycles don't leak resources. The CQ remains
// bounded because each cycle is short-lived.
//
// Returns true on cycle completion (even if no op completed), false on error.
func runSoakCycleOnRuntime(runtime *Runtime, metrics *soakMetrics) bool {
	soakDrainAllEvents(runtime)

	ctx := context.Background()

	// Create an in-memory node.
	node, err := MemoryWithAlpns(ctx, []string{"aster"})
	if err != nil {
		return false
	}

	// Wait for node creation event (with a short timeout).
	if !soakPollForEvent(runtime, uint32(IROH_EVENT_NODE_CREATED), 2000) {
		node.Close()
		return false
	}

	// Submit an accept operation on the node.
	var acceptOp C.iroh_operation_t
	r := C.iroh_node_accept_aster(
		C.uint64_t(runtime.handle),
		C.uint64_t(node.handle),
		0,
		&acceptOp,
	)
	if r != 0 {
		node.Close()
		return false
	}
	metrics.recordSubmit()

	// Randomly cancel ~25% of accepts to exercise cancellation paths.
	if time.Now().UnixNano()&0x3F < 25 {
		time.Sleep(5 * time.Millisecond)
		runtime.Cancel(uint64(acceptOp))
		metrics.opsCancelled.Add(1)
	}

	// Drain accept events.
	for {
		var events [8]C.iroh_event_t
		n := C.iroh_poll_events(C.uint64_t(runtime.handle), &events[0], 8, 100)
		if int(n) == 0 {
			break
		}
		for i := 0; i < int(n); i++ {
			ev := events[i]
			if uint64(ev.operation) == uint64(acceptOp) {
				if ev.status == 0 {
					metrics.recordComplete()
				} else {
					metrics.recordError()
				}
			}
		}
	}

	node.Close()
	metrics.recordCycle()
	return true
}
