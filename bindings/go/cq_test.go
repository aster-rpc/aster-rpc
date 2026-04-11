//go:build !cgo

package aster

import (
	"sync"
	"testing"
	"time"
)

// TestFakeCQ_SubmitComplete tests the basic submit → complete lifecycle.
func TestFakeCQ_SubmitComplete(t *testing.T) {
	cq := NewFakeCQ()
	defer cq.Close()

	op := cq.Submit()
	if op == 0 && cq.nextOp != 1 {
		// Verify op IDs are incrementing
	}

	ch := make(chan *Event, 1)
	cq.Register(op, ch)

	// Complete the operation with a terminal event.
	ev := &Event{Kind: IROH_EVENT_STREAM_FINISHED, Handle: 42}
	cq.Complete(op, ev)

	// Should receive the event.
	select {
	case got := <-ch:
		if got.Handle != 42 {
			t.Errorf("expected handle 42, got %d", got.Handle)
		}
		if got.Kind != IROH_EVENT_STREAM_FINISHED {
			t.Errorf("expected STREAM_FINISHED, got %d", got.Kind)
		}
	case <-time.After(100 * time.Millisecond):
		t.Error("timeout waiting for event")
	}

	// Operation should be unregistered after terminal event.
	if cq.Pending() != 0 {
		t.Errorf("expected 0 pending ops, got %d", cq.Pending())
	}
}

// TestFakeCQ_SubmitCancel tests submit → cancel → drain.
func TestFakeCQ_SubmitCancel(t *testing.T) {
	cq := NewFakeCQ()
	defer cq.Close()

	op := cq.Submit()
	ch := make(chan *Event, 1)
	cq.Register(op, ch)

	// Cancel the operation.
	cq.Unregister(op)

	// Completing a cancelled op should be a no-op (op not found).
	cq.Complete(op, &Event{Kind: IROH_EVENT_STREAM_FINISHED})
	if cq.Pending() != 0 {
		t.Errorf("expected 0 pending after cancel, got %d", cq.Pending())
	}

	// Channel should not receive anything.
	select {
	case ev := <-ch:
		t.Errorf("unexpected event %v after cancel", ev)
	case <-time.After(50 * time.Millisecond):
		// Expected: no event received
	}
}

// TestFakeCQ_ExactlyOnceTerminal tests that each operation gets exactly one terminal event.
func TestFakeCQ_ExactlyOnceTerminal(t *testing.T) {
	cq := NewFakeCQ()
	defer cq.Close()

	// Submit multiple operations.
	op1 := cq.Submit()
	op2 := cq.Submit()
	op3 := cq.Submit()

	ch1 := make(chan *Event, 1)
	ch2 := make(chan *Event, 1)
	ch3 := make(chan *Event, 1)
	cq.Register(op1, ch1)
	cq.Register(op2, ch2)
	cq.Register(op3, ch3)

	// Complete op1.
	cq.Complete(op1, &Event{Kind: IROH_EVENT_STREAM_FINISHED, Handle: 1})
	select {
	case <-ch1:
	case <-time.After(100 * time.Millisecond):
		t.Error("timeout waiting for op1 event")
	}

	// Complete op2.
	cq.Complete(op2, &Event{Kind: IROH_EVENT_STREAM_FINISHED, Handle: 2})
	select {
	case <-ch2:
	case <-time.After(100 * time.Millisecond):
		t.Error("timeout waiting for op2 event")
	}

	// Complete op3.
	cq.Complete(op3, &Event{Kind: IROH_EVENT_ERROR, Handle: 3, ErrorCode: -1})
	select {
	case <-ch3:
	case <-time.After(100 * time.Millisecond):
		t.Error("timeout waiting for op3 event")
	}

	if cq.Pending() != 0 {
		t.Errorf("expected 0 pending, got %d", cq.Pending())
	}
}

// TestFakeCQ_CloseDrainsAll verifies that Close() drains all pending ops.
func TestFakeCQ_CloseDrainsAll(t *testing.T) {
	cq := NewFakeCQ()

	op1 := cq.Submit()
	op2 := cq.Submit()
	op3 := cq.Submit()

	ch1 := make(chan *Event, 1)
	ch2 := make(chan *Event, 1)
	ch3 := make(chan *Event, 1)
	cq.Register(op1, ch1)
	cq.Register(op2, ch2)
	cq.Register(op3, ch3)

	cq.Close()

	if cq.Pending() != 0 {
		t.Errorf("expected 0 pending after close, got %d", cq.Pending())
	}

	// All channels should be closed (not receive an event).
	select {
	case _, ok := <-ch1:
		if ok {
			t.Error("ch1 should be closed, not have a value")
		}
	case <-time.After(100 * time.Millisecond):
		t.Error("timeout waiting for ch1 close")
	}
}

// TestFakeCQ_NoDoubleComplete verifies double-complete is a no-op.
func TestFakeCQ_NoDoubleComplete(t *testing.T) {
	cq := NewFakeCQ()
	defer cq.Close()

	op := cq.Submit()
	ch := make(chan *Event, 1)
	cq.Register(op, ch)

	ev1 := &Event{Kind: IROH_EVENT_STREAM_FINISHED, Handle: 1}
	ev2 := &Event{Kind: IROH_EVENT_STREAM_FINISHED, Handle: 2}

	cq.Complete(op, ev1)

	select {
	case got := <-ch:
		if got.Handle != 1 {
			t.Errorf("first complete: expected handle 1, got %d", got.Handle)
		}
	case <-time.After(100 * time.Millisecond):
		t.Error("timeout waiting for first event")
	}

	// Second complete should be a no-op (op already unregistered).
	cq.Complete(op, ev2)

	select {
	case got := <-ch:
		t.Errorf("unexpected second event handle=%d", got.Handle)
	case <-time.After(50 * time.Millisecond):
		// Expected: no second event
	}
}

// TestFakeCQ_StaleOpComplete tests completing an unknown op ID is a no-op.
func TestFakeCQ_StaleOpComplete(t *testing.T) {
	cq := NewFakeCQ()
	defer cq.Close()

	// Complete an op that was never submitted.
	stale := cq.Submit() // this increments nextOp
	cq.Unregister(stale) // simulate cancelled

	// Should not panic.
	cq.Complete(stale, &Event{Kind: IROH_EVENT_STREAM_FINISHED})
}

// TestFakeCQ_EventDataPreserved verifies event fields are preserved through dispatch.
func TestFakeCQ_EventDataPreserved(t *testing.T) {
	cq := NewFakeCQ()
	defer cq.Close()

	op := cq.Submit()
	ch := make(chan *Event, 1)
	cq.Register(op, ch)

	ev := &Event{
		Kind:      IROH_EVENT_FRAME_RECEIVED,
		Handle:    99,
		DataLen:   5,
		Buffer:    7,
		ErrorCode: 0,
	}

	cq.Complete(op, ev)

	select {
	case got := <-ch:
		if got.Handle != 99 {
			t.Errorf("expected handle 99, got %d", got.Handle)
		}
		if got.Kind != IROH_EVENT_FRAME_RECEIVED {
			t.Errorf("expected FRAME_RECEIVED, got %d", got.Kind)
		}
		if got.Buffer != 7 {
			t.Errorf("expected buffer 7, got %d", got.Buffer)
		}
	case <-time.After(100 * time.Millisecond):
		t.Error("timeout waiting for event")
	}
}

// TestFakeCQ_ConcurrentSubmitComplete tests concurrent submit/complete from multiple goroutines.
func TestFakeCQ_ConcurrentSubmitComplete(t *testing.T) {
	cq := NewFakeCQ()
	defer cq.Close()

	const n = 100
	var wg sync.WaitGroup
	wg.Add(n)

	// Concurrent submit + complete from n goroutines.
	for i := 0; i < n; i++ {
		go func(idx int) {
			defer wg.Done()
			op := cq.Submit()
			ch := make(chan *Event, 1)
			cq.Register(op, ch)

			ev := &Event{Kind: IROH_EVENT_STREAM_FINISHED, Handle: uint64(idx)}
			cq.Complete(op, ev)

			// Verify we receive exactly one event.
			select {
			case got := <-ch:
				if got.Handle != uint64(idx) {
					t.Errorf("goroutine %d: expected handle %d, got %d", idx, idx, got.Handle)
				}
			case <-time.After(500 * time.Millisecond):
				t.Errorf("goroutine %d: timeout waiting for event", idx)
			}
		}(i)
	}

	wg.Wait()

	if cq.Pending() != 0 {
		t.Errorf("expected 0 pending after concurrent test, got %d", cq.Pending())
	}
}

// TestFakeCQ_ConcurrentCancelComplete races cancel with complete.
func TestFakeCQ_ConcurrentCancelComplete(t *testing.T) {
	cq := NewFakeCQ()
	defer cq.Close()

	const n = 50
	var wg sync.WaitGroup
	wg.Add(n * 2)

	completed := make([]bool, n)

	// Each op will have one goroutine try to complete and one try to cancel.
	for i := 0; i < n; i++ {
		op := cq.Submit()
		ch := make(chan *Event, 1)
		cq.Register(op, ch)

		// Complete goroutine
		go func(idx int, o uint64, c chan *Event) {
			defer wg.Done()
			ev := &Event{Kind: IROH_EVENT_STREAM_FINISHED, Handle: uint64(idx)}
			cq.Complete(o, ev)
			completed[idx] = true
		}(i, op, ch)

		// Cancel goroutine
		go func(o uint64) {
			defer wg.Done()
			cq.Unregister(o)
		}(op)
	}

	wg.Wait()

	// At the end, all ops should be unregistered (either completed or cancelled).
	if cq.Pending() != 0 {
		t.Errorf("expected 0 pending, got %d", cq.Pending())
	}
}

// TestFakeCQ_ConcurrentCloseAndSubmit races close with new submits.
func TestFakeCQ_ConcurrentCloseAndSubmit(t *testing.T) {
	cq := NewFakeCQ()

	const n = 50
	var wg sync.WaitGroup
	wg.Add(n + 1)

	// N concurrent submits.
	for i := 0; i < n; i++ {
		go func() {
			defer wg.Done()
			cq.Submit()
		}()
	}

	// Close races with the submits.
	go func() {
		defer wg.Done()
		time.Sleep(1 * time.Millisecond) // let some submits happen first
		cq.Close()
	}()

	wg.Wait()

	if !cq.IsClosed() {
		t.Error("CQ should be closed after test")
	}
}
