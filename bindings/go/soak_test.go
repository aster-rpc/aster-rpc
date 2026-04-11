//go:build cgo

package aster

// Long-Run Soak / Leak Tests (5b.9)
//
// Multi-hour churn test to catch resource leaks.
//
// The cgo FFI helpers are in soak_cgo.go. This test file uses those
// helpers without directly using cgo, since _test.go files cannot import "C".
//
// Build and run (30-minute soak):
//
//	cd bindings/go
//	CGO_CFLAGS="-I$(pwd)/../../ffi" CGO_LDFLAGS="-L$(pwd)/../../target/release/deps -laster_transport_ffi" go test -v -run "Soak" -timeout 35m ./...
//
// Or set a custom duration in seconds:
//
//	CGO_CFLAGS="..." go test -v -run "TestSoak" -timeout 35m -args 1800 ./...
//
// Assertions:
//   - final_pending == 0  (no leaked ops)
//   - max_pending < 100   (CQ depth bounded)
//
// Requires: Go 1.23+, cgo enabled, native library built

import (
	"context"
	"flag"
	"fmt"
	"testing"
	"time"
)

// ─── Configuration ─────────────────────────────────────────────────────────

const defaultDurationSecs = 1800 // 30 minutes

// ─── Soak Test ──────────────────────────────────────────────────────────────

// TestSoak_Runs executes the soak test for the configured duration.
func TestSoak_Runs(t *testing.T) {
	durationSecs := defaultDurationSecs
	flag.Parse()
	if flag.NArg() > 0 {
		fmt.Sscanf(flag.Arg(0), "%d", &durationSecs)
	}

	duration := time.Duration(durationSecs) * time.Second
	deadline := time.Now().Add(duration)

	t.Logf("Starting soak test for %s (%d seconds)", duration, durationSecs)
	t.Logf("Churn pattern: node_create → accept → cancel(~25%%) → drain → node_close")
	t.Logf("Assertions: final_pending == 0, max_pending < 100")
	t.Logf("")

	ctx := context.Background()
	cfg := DefaultRuntimeConfig()
	runtime, err := NewRuntime(ctx, cfg)
	if err != nil {
		t.Fatalf("NewRuntime: %v", err)
	}
	defer runtime.Close()

	metrics := &soakMetrics{}
	cycleCount := 0
	printInterval := 100
	start := time.Now()

	for time.Now().Before(deadline) {
		_ = runSoakCycleOnRuntime(runtime, metrics)
		cycleCount++

		elapsed := time.Since(start)

		if cycleCount%printInterval == 0 {
			sub, comp, canc, errCount, maxPend, currPend := metrics.snapshot()
			t.Logf("[%s] cycle %d — submitted=%d completed=%d cancelled=%d errored=%d max_pending=%d current_pending=%d",
				elapsed.Round(time.Second),
				cycleCount,
				sub, comp, canc, errCount,
				maxPend, currPend,
			)
		}

		time.Sleep(10 * time.Millisecond)
	}

	elapsed := time.Since(start)

	sub, comp, canc, errCount, maxPend, currPend := metrics.snapshot()

	t.Logf("")
	t.Logf("=== Soak Test Results ===")
	t.Logf("Duration: %s", elapsed.Round(time.Second))
	t.Logf("Cycles: %d", cycleCount)
	t.Logf("Ops submitted: %d", sub)
	t.Logf("Ops completed: %d", comp)
	t.Logf("Ops cancelled: %d", canc)
	t.Logf("Ops errored: %d", errCount)
	t.Logf("Max pending ops: %d", maxPend)
	t.Logf("Final pending ops: %d", currPend)
	t.Logf("")

	if currPend != 0 {
		t.Errorf("FAIL: final pending ops = %d (expected 0 — leaked ops detected)", currPend)
	} else {
		t.Logf("PASS: final pending ops = 0")
	}

	if maxPend >= 100 {
		t.Errorf("FAIL: max pending ops = %d (expected < 100)", maxPend)
	} else {
		t.Logf("PASS: max pending ops = %d (< 100)", maxPend)
	}

	t.Logf("Soak test complete")
}

// ─── Short sanity test ─────────────────────────────────────────────────────

func TestSoak_Short(t *testing.T) {
	durationSecs := 5

	ctx := context.Background()
	cfg := DefaultRuntimeConfig()
	runtime, err := NewRuntime(ctx, cfg)
	if err != nil {
		t.Fatalf("NewRuntime: %v", err)
	}
	defer runtime.Close()

	metrics := &soakMetrics{}
	deadline := time.Now().Add(time.Duration(durationSecs) * time.Second)
	cycleCount := 0

	for time.Now().Before(deadline) {
		_ = runSoakCycleOnRuntime(runtime, metrics)
		cycleCount++
		time.Sleep(10 * time.Millisecond)
	}

	sub, comp, _, errCount, maxPend, currPend := metrics.snapshot()
	t.Logf("Cycles: %d, submitted: %d, completed: %d, errored: %d, max_pending: %d, current_pending: %d",
		cycleCount, sub, comp, errCount, maxPend, currPend)

	if currPend != 0 {
		t.Errorf("final pending ops = %d (expected 0)", currPend)
	}
	if maxPend >= 100 {
		t.Errorf("max pending ops = %d (expected < 100)", maxPend)
	}
}
