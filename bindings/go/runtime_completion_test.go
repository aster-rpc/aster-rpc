//go:build cgo

package aster

import (
	"sync"
	"testing"
)

func completionTestRuntime() *Runtime {
	return &Runtime{ops: make(map[uint64]chan<- *Event), pending: make(map[uint64]*Event), abandoned: make(map[uint64]struct{})}
}

// Model the deadline winning select after dispatch has already filled the
// completion channel. Detaching must return ownership, not strand the event.
func TestDetachCompletionAfterDelivery(t *testing.T) {
	r := completionTestRuntime()
	ch := make(chan *Event, 1)
	r.Register(1, ch)
	want := &Event{Operation: 1, Handle: 42}
	r.dispatchCompletion(want)
	if got := r.detachCompletion(1, ch); got != want {
		t.Fatalf("lost buffered completion: %v", got)
	}
	if len(ch) != 0 || len(r.ops) != 0 || len(r.pending) != 0 || len(r.abandoned) != 0 {
		t.Fatal("completion ownership retained after detach")
	}
}

func TestDetachCompletionBeforeDelivery(t *testing.T) {
	r := completionTestRuntime()
	ch := make(chan *Event, 1)
	r.Register(1, ch)
	if got := r.detachCompletion(1, ch); got != nil {
		t.Fatalf("unexpected event: %v", got)
	}
	r.dispatchCompletion(&Event{Operation: 1})
	if len(ch) != 0 || len(r.pending) != 0 || len(r.abandoned) != 0 {
		t.Fatal("late completion retained after cancellation")
	}
}

func TestDetachCompletionBeforeRegistration(t *testing.T) {
	r := completionTestRuntime()
	want := &Event{Operation: 1}
	r.dispatchCompletion(want)
	if got := r.detachCompletion(1, nil); got != want {
		t.Fatal("lost pending completion")
	}
}

func TestDetachCompletionConcurrentDispatch(t *testing.T) {
	for i := 0; i < 1000; i++ {
		r := completionTestRuntime()
		ch := make(chan *Event, 1)
		r.Register(1, ch)
		want := &Event{Operation: 1}
		var wg sync.WaitGroup
		wg.Add(1)
		go func() { defer wg.Done(); r.dispatchCompletion(want) }()
		got := r.detachCompletion(1, ch)
		wg.Wait()
		if got != nil && got != want {
			t.Fatal("unexpected completion")
		}
		if len(ch) != 0 || len(r.pending) != 0 || len(r.ops) != 0 || len(r.abandoned) != 0 {
			t.Fatal("completion stranded between dispatch and detach")
		}
	}
}

// The timeout path must collect a successful completion even if native
// completion committed before cancel but dispatch has not delivered it yet.
func TestOwnedTimeoutCollectsCommittedCompletion(t *testing.T) {
	for _, buffered := range []bool{false, true} {
		r := completionTestRuntime()
		ch := make(chan *Event, 1)
		r.Register(1, ch)
		want := &Event{Operation: 1, Handle: 42}
		if buffered {
			r.dispatchCompletion(want)
		}
		done := make(chan *Event, 1)
		go func() { done <- r.cancelAndCollect(1, ch) }()
		if !buffered {
			r.dispatchCompletion(want)
		}
		if got := <-done; got != want {
			t.Fatalf("lost owned completion: %v", got)
		}
		if len(ch) != 0 || len(r.ops) != 0 || len(r.pending) != 0 {
			t.Fatal("retained completion")
		}
	}
}

func TestRegisterDuringShutdownRejectsPendingSuccess(t *testing.T) {
	r := completionTestRuntime()
	r.pending[1] = &Event{Operation: 1, Handle: 42}
	r.closed.Store(true)
	ch := make(chan *Event, 1)
	r.Register(1, ch)
	if ev, ok := <-ch; ok || ev != nil {
		t.Fatal("delivered a native owner after shutdown began")
	}
	if len(r.pending) != 0 || len(r.ops) != 0 {
		t.Fatal("retained closed registration")
	}
}
