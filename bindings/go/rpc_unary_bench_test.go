//go:build cgo

package aster

import (
	"context"
	"fmt"
	"os"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// TestRPC_UnaryCancelBench is a closed-loop A/B of the unary client paths
// (skipped unless ASTER_GO_UNARY_BENCH is set; ASTER_GO_UNARY_BENCH_SECS sets
// the seconds per cell, default 5; ASTER_GO_UNARY_BENCH_CONC the
// concurrency list, default 1,16,100). Modes, interleaved per run:
//
//   - fast:   Call with context.Background() (aster_rpc_unary, no token);
//
//   - cancel: Call with a cancellable ctx (ABI 4.5: aster_rpc_unary_cancellable
//
//   - a cancel token, the default for any ctx that can end);
//
//   - open:   the pre-4.5 cancellable path, aster_rpc_open (server-stream) +
//     send + read to the trailer on a dedicated stream.
//
//     ASTER_GO_UNARY_BENCH=1 go test -run TestRPC_UnaryCancelBench -v -count=1 .
func TestRPC_UnaryCancelBench(t *testing.T) {
	if os.Getenv("ASTER_GO_UNARY_BENCH") == "" {
		t.Skip("set ASTER_GO_UNARY_BENCH=1 to run")
	}
	secs := 5.0
	if s := os.Getenv("ASTER_GO_UNARY_BENCH_SECS"); s != "" {
		secs, _ = strconv.ParseFloat(s, 64)
	}
	concs := []int{1, 16, 100}
	if s := os.Getenv("ASTER_GO_UNARY_BENCH_CONC"); s != "" {
		concs = nil
		for _, f := range strings.Split(s, ",") {
			n, _ := strconv.Atoi(f)
			concs = append(concs, n)
		}
	}
	svc := NewService("Bench", 1,
		Unary("echo", func(_ context.Context, r *testEchoRequest) (*testEchoReply, error) {
			return &testEchoReply{Message: r.Message, Count: r.Count + 1, Data: r.Data}, nil
		}),
	)
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
	req := &testEchoRequest{Message: "bench", Count: 1, Data: make([]byte, 64)}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	payload := mustEncode(t, f.conn, req)
	// wctx: the worker's own cancellable context (one per goroutine, as a
	// request context would be).
	call := map[string]func(wctx context.Context) error{
		"fast": func(context.Context) error {
			_, err := Call[testEchoRequest, testEchoReply](context.Background(), f.conn, "Bench", "echo", req)
			return err
		},
		"cancel": func(wctx context.Context) error {
			_, err := Call[testEchoRequest, testEchoReply](wctx, f.conn, "Bench", "echo", req)
			return err
		},
		// Every worker on ONE shared context: AfterFunc registrations
		// contend on its mutex.
		"cancel-shared": func(context.Context) error {
			_, err := Call[testEchoRequest, testEchoReply](ctx, f.conn, "Bench", "echo", req)
			return err
		},
		"open": func(wctx context.Context) error {
			c, err := openCall(wctx, f.conn, PatternServerStream, "Bench", "echo", buildOptions(nil))
			if err != nil {
				return err
			}
			defer c.release()
			if err := c.send(payload, true); err != nil {
				return err
			}
			_, err = c.readSingle("unary")
			return err
		},
	}
	modes := []string{"fast", "cancel", "open"}
	if s := os.Getenv("ASTER_GO_UNARY_BENCH_MODES"); s != "" {
		modes = strings.Split(s, ",")
	}
	type cell struct{ rps, p50, p99 []float64 }
	cells := map[string]*cell{}
	for _, m := range modes {
		for i := 0; i < 200; i++ { // warm up
			if err := call[m](ctx); err != nil {
				t.Fatalf("%s: %v", m, err)
			}
		}
		// Client + server allocations per call (one process).
		var before, after runtime.MemStats
		runtime.ReadMemStats(&before)
		for i := 0; i < 1000; i++ {
			_ = call[m](ctx)
		}
		runtime.ReadMemStats(&after)
		t.Logf("ALLOC mode=%s allocs/call=%.1f bytes/call=%.0f", m,
			float64(after.Mallocs-before.Mallocs)/1000, float64(after.TotalAlloc-before.TotalAlloc)/1000)
	}
	runs := 3
	if s := os.Getenv("ASTER_GO_UNARY_BENCH_RUNS"); s != "" {
		runs, _ = strconv.Atoi(s)
	}
	for run := 0; run < runs; run++ {
		for _, conc := range concs {
			for _, m := range modes {
				var stop atomic.Bool
				var mu sync.Mutex
				var lats []time.Duration
				var wg sync.WaitGroup
				start := time.Now()
				for range conc {
					wg.Add(1)
					go func() {
						defer wg.Done()
						wctx, wcancel := context.WithCancel(ctx)
						defer wcancel()
						var mine []time.Duration
						for !stop.Load() {
							t0 := time.Now()
							if err := call[m](wctx); err != nil {
								t.Errorf("%s: %v", m, err)
								return
							}
							mine = append(mine, time.Since(t0))
						}
						mu.Lock()
						lats = append(lats, mine...)
						mu.Unlock()
					}()
				}
				time.Sleep(time.Duration(secs * float64(time.Second)))
				stop.Store(true)
				wg.Wait()
				el := time.Since(start).Seconds()
				sort.Slice(lats, func(i, j int) bool { return lats[i] < lats[j] })
				pct := func(p float64) float64 {
					if len(lats) == 0 {
						return 0
					}
					return float64(lats[min(int(float64(len(lats))*p), len(lats)-1)]) / 1e6
				}
				rps := float64(len(lats)) / el
				t.Logf("RESULT run=%d mode=%s conc=%d rps=%.1f p50_ms=%.3f p99_ms=%.3f p999_ms=%.3f max_ms=%.3f", run, m, conc, rps, pct(0.5), pct(0.99), pct(0.999), pct(1))
				k := fmt.Sprintf("%s/%d", m, conc)
				if cells[k] == nil {
					cells[k] = &cell{}
				}
				cells[k].rps = append(cells[k].rps, rps)
				cells[k].p50 = append(cells[k].p50, pct(0.5))
				cells[k].p99 = append(cells[k].p99, pct(0.99))
			}
		}
	}
	med := func(v []float64) float64 {
		s := append([]float64(nil), v...)
		sort.Float64s(s)
		return s[len(s)/2]
	}
	t.Log("| mode | conc | rps (median) | p50 ms | p99 ms |")
	for _, conc := range concs {
		for _, m := range modes {
			c := cells[fmt.Sprintf("%s/%d", m, conc)]
			t.Logf("| %s | %d | %.0f | %.3f | %.3f |", m, conc, med(c.rps), med(c.p50), med(c.p99))
		}
	}
}
