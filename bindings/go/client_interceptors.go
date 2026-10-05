//go:build cgo

package aster

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"iter"
	"maps"
	"slices"
	"strconv"
	"sync"
	"time"
)

// Client interceptors.
//
// An AsterClient (WithClientInterceptors) or a single call (WithInterceptors)
// takes the same Interceptor values a server does, with the semantics of the
// Python client (bindings/python/aster/client.py):
//
//   - OnRequest runs in order on each request message, OnResponse in order on
//     each response message, OnError in reverse order on a failed call (a nil
//     return swallows the error: a unary call then returns a zero response
//     and a nil error, a stream just ends). The CallContext carries service,
//     method, pattern, metadata (hooks may add to Metadata: the call is sent
//     with it), the ctx deadline, Idempotent and the attempt number.
//   - Unary and client-streaming calls run through the retry loop: per
//     attempt each CircuitBreakerInterceptor is consulted first (an open
//     breaker fails the call with UNAVAILABLE, never retried), the request
//     chain runs, the call is made, the response chain runs, and the breaker
//     records the outcome (only UNAVAILABLE / INTERNAL / UNKNOWN count as
//     failures). A failed attempt goes through the OnError chain; then a
//     RetryInterceptor retries it if the call is idempotent, the code is
//     retryable (default UNAVAILABLE) and attempts remain, after its backoff
//     (bounded by the context). Several retry interceptors retry if any
//     does, with the longest delay and each one's own attempt cap.
//   - Server-streaming and bidi calls get the OnRequest / OnResponse /
//     OnError hooks only: no retry and no breaker (nothing is replayable once
//     messages flowed), as in Python. A CircuitBreakerInterceptor is not a
//     hook on any call: the loop above drives it.
//   - A client-streaming retry calls the request iterator again, so it must
//     be replayable (an iter.Seq over a slice is); only retry idempotent
//     methods that are safe to replay.
//   - The deadline is the context's: it is sent natively and enforced by
//     Rust (DEADLINE_EXCEEDED) on every attempt, and a DeadlineInterceptor
//     additionally refuses a call whose deadline has passed before it is
//     sent. A retry never outlives the context.
//
// The frozen AuthInterceptor / CapabilityInterceptor work here too (they are
// ordinary Interceptors) but are not extended (plan D3).

// callChain is the interceptors of one call, sorted by role.
type callChain struct {
	rest     []Interceptor // the chain run for every message (breakers excluded)
	retries  []*RetryInterceptor
	breakers []*CircuitBreakerInterceptor
}

// chain builds the call's chain, nil when there are no interceptors (the
// call then takes the plain path).
func (rc *RpcConnection) chain(o *callOptions) *callChain {
	if len(rc.interceptors) == 0 && len(o.interceptors) == 0 {
		return nil
	}
	ch := &callChain{}
	for _, ic := range slices.Concat(rc.interceptors, o.interceptors) {
		switch v := ic.(type) {
		case nil:
		case *CircuitBreakerInterceptor:
			ch.breakers = append(ch.breakers, v)
		case *RetryInterceptor:
			ch.retries = append(ch.retries, v)
			ch.rest = append(ch.rest, ic)
		default:
			ch.rest = append(ch.rest, ic)
		}
	}
	return ch
}

func (rc *RpcConnection) methodIdempotent(service, method string) bool {
	c := rc.client
	if c == nil {
		return false
	}
	c.mu.Lock()
	svc := c.services[service]
	c.mu.Unlock()
	if svc == nil {
		return false
	}
	m := svc.methods[method]
	return m != nil && m.Idempotent
}

func (ch *callChain) newContext(ctx context.Context, rc *RpcConnection, service, method string, p MethodPattern, o *callOptions) *CallContext {
	var id [8]byte
	_, _ = rand.Read(id[:])
	cc := &CallContext{
		Service: service, Method: method, CallID: hex.EncodeToString(id[:]),
		Metadata: metaMap(o.metadata), Attributes: map[string]string{},
		IsStreaming: p != PatternUnary, Pattern: p.String(), Attempt: 1,
	}
	if o.idempotent != nil {
		cc.Idempotent = *o.idempotent
	} else {
		cc.Idempotent = rc.methodIdempotent(service, method)
	}
	if o.sessionID != 0 {
		cc.SessionID = strconv.FormatUint(uint64(o.sessionID), 10)
	}
	if d, ok := ctx.Deadline(); ok {
		cc.Deadline = &d
	}
	if peer, err := rc.conn.RemoteID(); err == nil {
		cc.Peer = peer
	}
	return cc
}

func metaMap(entries []MetadataEntry) map[string]string {
	m := make(map[string]string, len(entries))
	for _, e := range entries {
		m[e.Key] = e.Value
	}
	return m
}

// metadataFor is the metadata to send after the request hooks ran: orig
// unchanged when they did not touch the map, else the original keys (with
// their current values, dropped ones omitted) then new keys in sorted order.
func metadataFor(orig []MetadataEntry, m map[string]string) []MetadataEntry {
	if maps.Equal(metaMap(orig), m) {
		return orig
	}
	out := make([]MetadataEntry, 0, len(m))
	seen := map[string]bool{}
	for _, e := range orig {
		if v, ok := m[e.Key]; ok && !seen[e.Key] {
			out = append(out, MetadataEntry{e.Key, v})
			seen[e.Key] = true
		}
	}
	for _, k := range slices.Sorted(maps.Keys(m)) {
		if !seen[k] {
			out = append(out, MetadataEntry{k, m[k]})
		}
	}
	return out
}

// asRpcError is the *RpcError view of an error (Python's normalize_error).
func asRpcError(err error) *RpcError {
	var re *RpcError
	if errors.As(err, &re) {
		return re
	}
	switch {
	case errors.Is(err, context.DeadlineExceeded):
		return &RpcError{Code: StatusDeadlineExceeded, Message: "deadline exceeded"}
	case errors.Is(err, context.Canceled):
		return &RpcError{Code: StatusCancelled, Message: "call cancelled"}
	}
	return &RpcError{Code: StatusUnknown, Message: err.Error()}
}

// failedCall records a failed attempt with the breakers and runs the OnError
// chain over err. It returns the error the call fails with (err itself,
// keeping wrapped context errors, when the chain left the *RpcError alone) and
// the *RpcError the retry policy sees, or swallowed == true.
func (ch *callChain) failedCall(cc *CallContext, err error) (out error, rpcErr *RpcError, swallowed bool) {
	for _, b := range ch.breakers {
		b.RecordFailure(asRpcError(err))
	}
	return ch.runErrorChain(cc, err)
}

// runErrorChain runs the OnError chain over err.
func (ch *callChain) runErrorChain(cc *CallContext, err error) (out error, rpcErr *RpcError, swallowed bool) {
	re := asRpcError(err)
	res := ApplyErrorInterceptors(ch.rest, cc, re)
	switch {
	case res == nil:
		return nil, nil, true
	case res == re:
		return err, re, false
	}
	return res, res, false
}

// retryDelay decides whether a failed attempt is retried, and after how long.
func (ch *callChain) retryDelay(cc *CallContext, err *RpcError, attempt int) (time.Duration, bool) {
	var delay float64
	retry := false
	for _, r := range ch.retries {
		if r.ShouldRetry(cc, err) && attempt < r.Policy.MaxAttempts {
			retry = true
			delay = max(delay, r.BackoffSeconds(attempt))
		}
	}
	return time.Duration(delay * float64(time.Second)), retry
}

// sleepCtx waits d, or fails with the context's failure once it ends.
func sleepCtx(ctx context.Context, d time.Duration) error {
	if d <= 0 {
		return ctx.Err() // nil unless ended
	}
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-t.C:
		return nil
	case <-ctx.Done():
		return ctxFailure(ctx)
	}
}

// retryLoop runs attempt (request hooks, the call, response hooks) under the
// breakers, the OnError chain and the retry policy.
func retryLoop[T any](ctx context.Context, ch *callChain, cc *CallContext, o *callOptions, attempt func() (*T, error)) (*T, error) {
	for n := 1; ; n++ {
		cc.Attempt = n
		cc.Metadata = metaMap(o.metadata)
		for i, b := range ch.breakers {
			if err := b.BeforeCall(); err != nil {
				// The call never leaves: give back the slots the earlier
				// breakers took.
				for _, a := range ch.breakers[:i] {
					a.ReleaseProbe()
				}
				return nil, err
			}
		}
		resp, err := attempt()
		if err == nil {
			for _, b := range ch.breakers {
				b.RecordSuccess()
			}
			return resp, nil
		}
		out, re, swallowed := ch.failedCall(cc, err)
		if swallowed {
			return new(T), nil
		}
		delay, retry := ch.retryDelay(cc, re, n)
		if !retry || ctx.Err() != nil {
			return nil, out
		}
		if err := sleepCtx(ctx, delay); err != nil {
			return nil, err
		}
	}
}

// typed converts a hook's output back to the call's message type.
func typed[T any](v any, what string) (*T, error) {
	switch x := v.(type) {
	case *T:
		if x != nil {
			return x, nil
		}
	case T:
		return &x, nil
	}
	return nil, &RpcError{Code: StatusInternal, Message: fmt.Sprintf("interceptor returned %T as the %s, want %T", v, what, new(T))}
}

// hookError makes a hook's error an *RpcError (Python's normalize_error),
// leaving errors that already wrap one alone.
func hookError(err error) error {
	var re *RpcError
	if errors.As(err, &re) {
		return err
	}
	return asRpcError(err)
}

func runRequestChain[Req any](ch *callChain, cc *CallContext, req *Req) (*Req, error) {
	cur, err := ApplyRequestInterceptors(ch.rest, cc, req)
	if err != nil {
		return nil, hookError(err)
	}
	return typed[Req](cur, "request")
}

func runResponseChain[Resp any](ch *callChain, cc *CallContext, resp *Resp) (*Resp, error) {
	cur, err := ApplyResponseInterceptors(ch.rest, cc, resp)
	if err != nil {
		return nil, hookError(err)
	}
	return typed[Resp](cur, "response")
}

// ─── Unary ──────────────────────────────────────────────────────────────────

func callUnaryIntercepted[Req, Resp any](ctx context.Context, rc *RpcConnection, ch *callChain, service, method string, req *Req, o *callOptions) (*Resp, error) {
	cc := ch.newContext(ctx, rc, service, method, PatternUnary, o)
	return retryLoop(ctx, ch, cc, o, func() (*Resp, error) {
		r, err := runRequestChain(ch, cc, req)
		if err != nil {
			return nil, err
		}
		ao := *o
		ao.metadata = metadataFor(o.metadata, cc.Metadata)
		resp, err := callUnary[Req, Resp](ctx, rc, service, method, r, &ao)
		if err != nil {
			return nil, err
		}
		return runResponseChain(ch, cc, resp)
	})
}

// ─── Client streaming ───────────────────────────────────────────────────────

// callClientStreamIntercepted runs the request hooks on every request message
// as it is sent. The first message is pulled and hooked before the call is
// opened, so a hook that adds metadata or rejects the call acts on the header
// (Python runs the chain once with no request at open; Go has no such nil
// call, the first real message does the job).
func callClientStreamIntercepted[Req, Resp any](ctx context.Context, rc *RpcConnection, ch *callChain, service, method string, reqs iter.Seq[*Req], o *callOptions) (*Resp, error) {
	cc := ch.newContext(ctx, rc, service, method, PatternClientStream, o)
	return retryLoop(ctx, ch, cc, o, func() (*Resp, error) {
		next, stop := iter.Pull(reqs)
		defer stop()
		first, haveFirst := next()
		if haveFirst {
			var err error
			if first, err = runRequestChain(ch, cc, first); err != nil {
				return nil, err
			}
		}
		actx, cancel := context.WithCancel(ctx)
		defer cancel()
		var reqErr error
		wrapped := func(yield func(*Req) bool) {
			if !haveFirst {
				return
			}
			if !yield(first) {
				return
			}
			for {
				r, ok := next()
				if !ok {
					return
				}
				r, err := runRequestChain(ch, cc, r)
				if err != nil {
					reqErr = err
					cancel() // the server sees a cancel, not a clean end
					return
				}
				if !yield(r) {
					return
				}
			}
		}
		ao := *o
		ao.metadata = metadataFor(o.metadata, cc.Metadata)
		resp, err := callClientStream[Req, Resp](actx, rc, service, method, wrapped, &ao)
		if reqErr != nil {
			return nil, reqErr
		}
		if err != nil {
			if ctx.Err() == nil && errors.Is(err, context.Canceled) {
				err = &RpcError{Code: StatusCancelled, Message: "call cancelled"}
			}
			return nil, err
		}
		return runResponseChain(ch, cc, resp)
	})
}

// ─── Server streaming ───────────────────────────────────────────────────────

func callServerStreamIntercepted[Req, Resp any](ctx context.Context, rc *RpcConnection, ch *callChain, service, method string, req *Req, o *callOptions) iter.Seq2[*Resp, error] {
	return func(yield func(*Resp, error) bool) {
		cc := ch.newContext(ctx, rc, service, method, PatternServerStream, o)
		fail := func(err error) {
			if out, _, swallowed := ch.runErrorChain(cc, err); !swallowed {
				yield(nil, out)
			}
		}
		r, err := runRequestChain(ch, cc, req)
		if err != nil {
			fail(err)
			return
		}
		ao := *o
		ao.metadata = metadataFor(o.metadata, cc.Metadata)
		for resp, err := range callServerStream[Req, Resp](ctx, rc, service, method, r, &ao) {
			if err == nil {
				resp, err = runResponseChain(ch, cc, resp)
			}
			if err != nil {
				fail(err)
				return
			}
			if !yield(resp, nil) {
				return
			}
		}
	}
}

// ─── Bidi ───────────────────────────────────────────────────────────────────

// bidiIntercept serialises the hooks of one bidi call: Send and Recv run on
// different goroutines but share its CallContext.
type bidiIntercept struct {
	mu sync.Mutex
	ch *callChain
	cc *CallContext
}

func (ch *callChain) newBidi(ctx context.Context, rc *RpcConnection, service, method string, o *callOptions) *bidiIntercept {
	return &bidiIntercept{ch: ch, cc: ch.newContext(ctx, rc, service, method, PatternBidiStream, o)}
}

// failed runs the OnError chain over a failed call; a swallowed error ends
// the stream like a clean end (io.EOF).
func (b *bidiIntercept) failed(err error) error {
	b.mu.Lock()
	defer b.mu.Unlock()
	out, _, swallowed := b.ch.runErrorChain(b.cc, err)
	if swallowed {
		return io.EOF
	}
	return out
}

func interceptRequest[Req any](b *bidiIntercept, req *Req) (*Req, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return runRequestChain(b.ch, b.cc, req)
}

func interceptResponse[Resp any](b *bidiIntercept, resp *Resp, err error) (*Resp, error) {
	if err == nil {
		b.mu.Lock()
		resp, err = runResponseChain(b.ch, b.cc, resp)
		b.mu.Unlock()
		if err == nil {
			return resp, nil
		}
	}
	return nil, b.failed(err)
}
