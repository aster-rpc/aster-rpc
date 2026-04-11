//go:build !cgo

package aster

import "sync"

// FakeCQ is a pure-Go completion queue simulator for unit testing.
// It implements the same event dispatch semantics as the real Runtime
// without requiring the native FFI library.
// It is safe for concurrent use from multiple goroutines.
type FakeCQ struct {
	mu    sync.Mutex
	ops   map[uint64]chan *Event
	closed bool
	nextOp uint64
}

// NewFakeCQ creates a new fake completion queue.
func NewFakeCQ() *FakeCQ {
	return &FakeCQ{
		ops: make(map[uint64]chan *Event),
	}
}

// Submit registers a new operation and returns an operation ID.
func (cq *FakeCQ) Submit() uint64 {
	cq.mu.Lock()
	defer cq.mu.Unlock()

	opID := cq.nextOp
	cq.nextOp++
	cq.ops[opID] = make(chan *Event, 1)
	return opID
}

// Complete emits a terminal event for the given operation.
func (cq *FakeCQ) Complete(opID uint64, ev *Event) {
	cq.mu.Lock()
	ch, ok := cq.ops[opID]
	if ok {
		delete(cq.ops, opID)
	}
	cq.mu.Unlock()

	if !ok {
		return
	}
	select {
	case ch <- ev:
	default:
	}
}

// Drain returns all pending operation IDs without emitting events.
// Caller must not call Submit/Complete concurrently with Drain.
func (cq *FakeCQ) Drain() []uint64 {
	cq.mu.Lock()
	var ops []uint64
	for id := range cq.ops {
		ops = append(ops, id)
	}
	cq.mu.Unlock()
	return ops
}

// Close closes all pending operation channels.
func (cq *FakeCQ) Close() {
	cq.mu.Lock()
	cq.closed = true
	for opID, ch := range cq.ops {
		delete(cq.ops, opID)
		close(ch)
	}
	cq.mu.Unlock()
}

// IsClosed returns whether the CQ has been closed.
func (cq *FakeCQ) IsClosed() bool {
	cq.mu.Lock()
	defer cq.mu.Unlock()
	return cq.closed
}

// Pending returns the number of pending operations.
func (cq *FakeCQ) Pending() int {
	cq.mu.Lock()
	defer cq.mu.Unlock()
	return len(cq.ops)
}

// Register associates a channel with an operation ID.
func (cq *FakeCQ) Register(opID uint64, ch chan *Event) {
	cq.mu.Lock()
	defer cq.mu.Unlock()
	cq.ops[opID] = ch
}

// Unregister removes the channel for an operation ID.
func (cq *FakeCQ) Unregister(opID uint64) {
	cq.mu.Lock()
	defer cq.mu.Unlock()
	delete(cq.ops, opID)
}
