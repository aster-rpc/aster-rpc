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
	"sync"
	"sync/atomic"
	"time"
)

// ─── Event polling helpers ─────────────────────────────────────────────────

// pollForEvent polls the runtime until we see a specific event kind or timeout.
func pollForEvent(runtime *Runtime, kind uint32, timeout time.Duration) bool {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		var events [8]C.iroh_event_t
		n := C.iroh_poll_events(C.uint64_t(runtime.handle), &events[0], 8, 10)
		for i := 0; i < int(n); i++ {
			if uint32(events[i].kind) == kind {
				return true
			}
		}
		time.Sleep(5 * time.Millisecond)
	}
	return false
}

// drainAllEvents drains all pending events from the runtime.
func drainAllEvents(runtime *Runtime) {
	for {
		var events [16]C.iroh_event_t
		n := C.iroh_poll_events(C.uint64_t(runtime.handle), &events[0], 16, 0)
		if int(n) == 0 {
			break
		}
	}
}

// ─── FFI operation helpers ─────────────────────────────────────────────────

// submitAccept submits an accept operation on the given node.
func submitAccept(runtime *Runtime, node *Node) (uint64, error) {
	var acceptOp C.iroh_operation_t
	r := C.iroh_node_accept_aster(
		C.uint64_t(runtime.handle),
		C.uint64_t(node.handle),
		0,
		&acceptOp,
	)
	if r != 0 {
		return 0, Error(r)
	}
	return uint64(acceptOp), nil
}

// closeNode submits a close operation on the given node.
func closeNode(runtime *Runtime, node *Node) (uint64, error) {
	var closeOp C.iroh_operation_t
	r := C.iroh_node_close(
		C.uint64_t(runtime.handle),
		C.uint64_t(node.handle),
		0,
		&closeOp,
	)
	if r != 0 {
		return 0, Error(r)
	}
	return uint64(closeOp), nil
}

// drainAcceptEvents drains events for a specific accept operation.
// Returns (success, hadTerminal).
func drainAcceptEvents(runtime *Runtime, acceptOp uint64) (bool, bool) {
	for i := 0; i < 100; i++ {
		var events [8]C.iroh_event_t
		n := C.iroh_poll_events(C.uint64_t(runtime.handle), &events[0], 8, 50)
		if int(n) == 0 {
			break
		}
		for j := 0; j < int(n); j++ {
			ev := events[j]
			if uint64(ev.operation) == acceptOp {
				if ev.status == 0 && uint32(ev.kind) == uint32(IROH_EVENT_ASTER_ACCEPTED) {
					return true, true
				}
				if uint32(ev.kind) == uint32(IROH_EVENT_OPERATION_CANCELLED) || uint32(ev.kind) == uint32(IROH_EVENT_ERROR) {
					return false, true
				}
			}
		}
	}
	return false, false
}

// drainEventsWithTracking tracks terminal events for acceptOp and closeOp.
func drainEventsWithTracking(runtime *Runtime, acceptOp, closeOp uint64) (acceptTerminal, closeTerminal int32) {
	for i := 0; i < 100; i++ {
		var events [8]C.iroh_event_t
		n := C.iroh_poll_events(C.uint64_t(runtime.handle), &events[0], 8, 50)
		if int(n) == 0 {
			break
		}
		for j := 0; j < int(n); j++ {
			ev := events[j]
			switch uint64(ev.operation) {
			case acceptOp:
				if ev.status == 0 {
					atomic.StoreInt32(&acceptTerminal, 1)
				} else {
					atomic.StoreInt32(&acceptTerminal, 2)
				}
			case closeOp:
				if ev.status == 0 {
					atomic.StoreInt32(&closeTerminal, 1)
				} else {
					atomic.StoreInt32(&closeTerminal, 2)
				}
			}
		}
	}
	return
}

// raceCancelAndDrain races cancel with event draining.
func raceCancelAndDrain(runtime *Runtime, acceptOp uint64) (kind int32, count int32) {
	var wg sync.WaitGroup
	done := make(chan struct{})

	wg.Add(1)
	go func() {
		defer wg.Done()
		time.Sleep(5 * time.Millisecond)
		runtime.Cancel(acceptOp)
	}()

	wg.Add(1)
	go func() {
		defer wg.Done()
		for {
			var events [8]C.iroh_event_t
			n := C.iroh_poll_events(C.uint64_t(runtime.handle), &events[0], 8, 50)
			for i := 0; i < int(n); i++ {
				if uint64(events[i].operation) == acceptOp {
					atomic.AddInt32(&count, 1)
					atomic.StoreInt32(&kind, int32(events[i].kind))
					close(done)
					return
				}
			}
			if atomic.LoadInt32(&count) > 0 {
				return
			}
		}
	}()

	select {
	case <-done:
	case <-time.After(2 * time.Second):
	}
	wg.Wait()
	return
}

// drainAllOpsEvents drains all events for a set of operation IDs.
func drainAllOpsEvents(runtime *Runtime, ops []uint64) int {
	opSet := make(map[uint64]bool)
	for _, op := range ops {
		opSet[op] = true
	}

	completed := 0
	for i := 0; i < 200; i++ {
		var events [8]C.iroh_event_t
		n := C.iroh_poll_events(C.uint64_t(runtime.handle), &events[0], 8, 50)
		if int(n) == 0 {
			break
		}
		for j := 0; j < int(n); j++ {
			ev := events[j]
			if opSet[uint64(ev.operation)] {
				completed++
			}
		}
	}
	return completed
}

// tryPollEvents does a single non-blocking poll and returns the raw event count.
func tryPollEvents(runtime *Runtime) int {
	var events [8]C.iroh_event_t
	n := C.iroh_poll_events(C.uint64_t(runtime.handle), &events[0], 8, 0)
	return int(n)
}
