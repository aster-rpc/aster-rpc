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
	"os"
	"testing"
	"time"
)

// ─── Configuration ─────────────────────────────────────────────────────────

const defaultDurationSecs = 1800 // 30 minutes

// ─── Soak Test ──────────────────────────────────────────────────────────────

// TestSoak_Runs executes the soak test for the configured duration.
func TestSoak_Runs(t *testing.T) {
	if os.Getenv("ASTER_GO_SOAK") == "" {
		t.Skip("30-minute soak; set ASTER_GO_SOAK=1 to run")
	}
	durationSecs := defaultDurationSecs
	flag.Parse()
	if flag.NArg() > 0 {
		fmt.Sscanf(flag.Arg(0), "%d", &durationSecs)
	}

	duration := time.Duration(durationSecs) * time.Second
	deadline := time.Now().Add(duration)

	t.Logf("Starting soak test for %s (%d seconds)", duration, durationSecs)
	t.Logf("Churn pattern: loopback node_start → accept → cancel(~25%%) or node_close → terminal event")
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

	nodeCfg := soakNodeConfig()
	for time.Now().Before(deadline) {
		if err := runSoakCycleOnRuntime(ctx, runtime, nodeCfg, metrics); err != nil {
			t.Fatalf("cycle %d: %v", cycleCount, err)
		}
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
	assertSoakBacklogDrains(t, runtime)

	t.Logf("Soak test complete")
}

// soakNodeConfig is a loopback-only node config (relay disabled, 127.0.0.1:0):
// with the default relay every node start waits for the public n0 relay.
func soakNodeConfig() AsterConfig {
	cfg := testCfg()
	cfg.ALPNs = []string{"aster"}
	return cfg
}

// assertSoakBacklogDrains checks the runtime ends with no undelivered or
// abandoned completions (cancelled accepts complete within a short bound).
func assertSoakBacklogDrains(t *testing.T, runtime *Runtime) {
	t.Helper()
	deadline := time.Now().Add(waitBudget())
	for {
		pending, abandoned := soakRuntimeBacklog(runtime)
		if pending == 0 && abandoned == 0 {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("runtime backlog after soak: pending=%d abandoned=%d", pending, abandoned)
		}
		time.Sleep(20 * time.Millisecond)
	}
}
