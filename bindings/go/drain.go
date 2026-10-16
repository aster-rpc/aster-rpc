//go:build cgo

package aster

/*
#include <stdint.h>
#include <stdlib.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"errors"
	"fmt"
	"time"
)

// DrainCloseCode is the QUIC application close code a draining server
// closes connections with: "this server is going away; reconnect to another
// replica" (ASTER_DRAIN_CLOSE_CODE).
const DrainCloseCode = uint64(C.ASTER_DRAIN_CLOSE_CODE)

// DrainReport is what AsterServer.Drain did. Calls count those admitted to a
// handler, not those refused at routing.
type DrainReport struct {
	// CallsInFlight were running when the drain started.
	CallsInFlight uint64
	// CallsCompleted finished before the deadline (those in flight at the
	// start, and calls on existing sessions admitted during the drain).
	CallsCompleted uint64
	// CallsCancelled were still running at the deadline and were cancelled
	// by closing their connection.
	CallsCancelled uint64
	// CallsRefused are new calls answered UNAVAILABLE (draining).
	CallsRefused uint64
	// ConnectionsClosed were closed by the drain with DrainCloseCode.
	ConnectionsClosed uint64
	// ConnectionsRefused are new connections refused at accept.
	ConnectionsRefused uint64
	// DeadlineReached is true when the grace ended with work outstanding.
	DeadlineReached bool
	// Elapsed is how long the drain took.
	Elapsed time.Duration
}

// Drain takes the server out of rotation, then closes it (Close):
//
//  1. an attached health listener reports not ready;
//  2. new connections are closed right after their handshake with
//     DrainCloseCode, and new calls answer UNAVAILABLE with the
//     aster.draining detail (IsDraining); calls on a session that already
//     exists still run;
//  3. each connection is closed with DrainCloseCode once it has no call in
//     flight and no live session;
//  4. in-flight calls (every pattern) run until grace; then every remaining
//     connection is closed, which cancels their handlers' contexts, and the
//     drain waits up to 2 s for them to end.
//
// The grace is cut short by ctx: at ctx's deadline, or as soon as ctx is
// cancelled, the remaining calls are cancelled as at the grace deadline. The
// server is closed whatever happens; the report is returned with Close's
// error, if any. A server already closed returns ErrAlreadyClosed.
func (s *AsterServer) Drain(ctx context.Context, grace time.Duration) (DrainReport, error) {
	if grace < 0 {
		return DrainReport{}, fmt.Errorf("drain grace must be nonnegative: %w", ErrInvalidArgument)
	}
	if s.closing.Load() {
		return DrainReport{}, fmt.Errorf("drain: %w", ErrAlreadyClosed)
	}
	if dl, ok := ctx.Deadline(); ok {
		grace = min(grace, max(time.Until(dl), 0))
	}
	type result struct {
		report DrainReport
		err    error
	}
	done := make(chan result, 1)
	go func() {
		r, err := s.ds.drain(drainMillis(grace))
		done <- result{r, err}
	}()
	var res result
	select {
	case res = <-done:
	case <-ctx.Done():
		// A second drain with no grace moves the deadline to now; it returns
		// once the first has finished.
		_, _ = s.ds.drain(0)
		res = <-done
	}
	if res.err != nil {
		return res.report, errors.Join(res.err, s.Close())
	}
	return res.report, s.Close()
}

func drainMillis(d time.Duration) uint32 {
	ms := uint64(d / time.Millisecond)
	if d%time.Millisecond != 0 {
		ms++
	}
	return uint32(min(ms, uint64(1<<32-1)))
}

func (d *dispatchServer) drain(graceMs uint32) (DrainReport, error) {
	var r C.aster_drain_report_t
	status, msg := ffiCall(func() C.int32_t {
		return C.aster_server_drain(d.rt, d.handle, C.uint32_t(graceMs), &r)
	})
	if err := wrapStatusMsg("aster_server_drain", status, msg); err != nil {
		return DrainReport{}, err
	}
	return DrainReport{
		CallsInFlight:      uint64(r.calls_in_flight),
		CallsCompleted:     uint64(r.calls_completed),
		CallsCancelled:     uint64(r.calls_cancelled),
		CallsRefused:       uint64(r.calls_refused),
		ConnectionsClosed:  uint64(r.connections_closed),
		ConnectionsRefused: uint64(r.connections_refused),
		DeadlineReached:    r.deadline_reached != 0,
		Elapsed:            time.Duration(r.elapsed_ms) * time.Millisecond,
	}, nil
}
