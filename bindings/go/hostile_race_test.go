//go:build cgo

package aster

// Hostile Race Integration Tests (5b.6)
//
// Go-side wiring under concurrency: the runtime's event poller and handle
// lifetimes. The CQ races themselves (accept then close, cancel racing
// completion, many outstanding ops, many nodes on one CQ) are Rust's:
// ffi/tests/hostile_race_integration_tests.rs.
//
// These tests use the Go-native async model (channels, context deadlines)
// instead of direct FFI polling, to avoid conflicting with the Runtime's
// background event poller goroutine.
//
// Architecture note:
// In-memory nodes have no network presence — they cannot be reached by
// other endpoints for peer connections. Therefore, these tests focus on
// Go FFI correctness patterns that don't require peer connections.
//
// The Java hostile race tests (in the Java repo) test the Rust CQ behavior
// with real peer connections. The Go hostile race tests validate that the
// Go FFI binding correctly exercises the same CQ code paths without peers.
//
// Build and run:
//
//	cd bindings/go
//	CGO_CFLAGS="-I$(pwd)/../../ffi" CGO_LDFLAGS="-L$(pwd)/../../target/release/deps -laster_transport_ffi" go test -v -run "Hostile" ./...
//
// Requires: Go 1.23+, cgo enabled, native library built

import (
	"context"
	"sync"
	"testing"
	"time"
)

// ─── handle_close_after_submit ─────────────────────────────────────────────
//
// Scenario: submit → close runtime → drain
// Expected: no success event for that handle generation

func TestHostile_HandleCloseAfterSubmit(t *testing.T) {
	ctx := context.Background()
	cfg := DefaultRuntimeConfig()

	runtime, err := NewRuntime(ctx, cfg)
	if err != nil {
		t.Fatalf("NewRuntime: %v", err)
	}

	node, err := loopbackMemoryNode(ctx, []string{"aster"})
	if err != nil {
		t.Fatalf("MemoryWithAlpns: %v", err)
	}
	// loopbackMemoryNode creates the node on its own runtime; ops must be
	// submitted against that runtime or they return NOT_FOUND.
	runtime = node.runtime

	// Submit accept.
	acceptOp, err := submitAccept(runtime, node)
	if err != nil {
		t.Fatalf("aster_node_accept_aster: %s", err)
	}
	opID := acceptOp

	ch := make(chan *Event, 1)
	runtime.Register(opID, ch)

	// Close runtime immediately.
	runtime.Close()

	// Channel should be closed (not receive event) when runtime closes.
	select {
	case _, ok := <-ch:
		if ok {
			t.Error("expected channel closed after runtime.Close(), got event")
		}
	case <-time.After(waitBudget()):
		t.Error("timeout waiting for channel close")
	}
}

// ─── concurrent_create_close ───────────────────────────────────────────────
//
// Scenario: concurrent create and close from multiple goroutines
// Expected: no panics, all resources properly released

func TestHostile_ConcurrentCreateClose(t *testing.T) {
	ctx := context.Background()
	cfg := DefaultRuntimeConfig()

	const n = 10
	errCh := make(chan error, n*2) // each goroutine does create + close

	var wg sync.WaitGroup
	wg.Add(n)

	for i := 0; i < n; i++ {
		go func(idx int) {
			defer wg.Done()

			runtime, err := NewRuntime(ctx, cfg)
			if err != nil {
				errCh <- err
				return
			}

			// Create node
			node, err := loopbackMemoryNode(ctx, []string{"aster"})
			if err != nil {
				runtime.Close()
				errCh <- err
				return
			}

			// Close node
			if err := node.Close(); err != nil {
				errCh <- err
			}

			// Close runtime
			if err := runtime.Close(); err != nil {
				errCh <- err
			}
		}(i)
	}

	wg.Wait()
	close(errCh)

	errorCount := 0
	for err := range errCh {
		t.Logf("error: %v", err)
		errorCount++
	}

	if errorCount > 0 {
		t.Errorf("%d errors during concurrent create/close", errorCount)
	}
}

// loopbackMemoryNode is MemoryWithAlpns on a loopback-only config (relay
// disabled, 127.0.0.1:0): with the default relay every node start waits for
// the public n0 relay. Like MemoryWithAlpns, the node runs on its own runtime,
// which its Close leaves running.
func loopbackMemoryNode(ctx context.Context, alpns []string) (*Node, error) {
	rt, err := NewRuntime(ctx, DefaultRuntimeConfig())
	if err != nil {
		return nil, err
	}
	cfg := testCfg()
	cfg.ALPNs = alpns
	node, err := startNodeOn(ctx, rt, cfg, NodeOptions{}, -1)
	if err != nil {
		rt.Close()
		return nil, err
	}
	node.ownsRuntime = false
	return node, nil
}
