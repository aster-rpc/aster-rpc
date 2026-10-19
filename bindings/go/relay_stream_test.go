//go:build cgo

package aster

import (
	"bytes"
	"context"
	"errors"
	"io"
	"net/http"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// C ABI 4.3 streaming consumer relay, ported from ffi/tests/expose.rs
// (relay_stream_*): full duplex, early answers, abort before the head, close
// before the end, clean end, refusal codes, cancellation.

func (r *exposeRig) stream(req RelayStreamRequest) (*RelayResponse, error) {
	r.t.Helper()
	req.ServiceID = exposeSvc
	if req.Method == "" {
		req.Method = "POST"
	}
	if req.URI == "" {
		req.URI = "/"
	}
	return r.conn.RelayStream(r.ctx, req)
}

// A reader fed by the test, with a hook for its close.
type feed struct {
	pr *io.PipeReader
	pw *io.PipeWriter
}

func newFeed() *feed { pr, pw := io.Pipe(); return &feed{pr, pw} }

func (f *feed) Read(p []byte) (int, error) { return f.pr.Read(p) }
func (f *feed) Close() error               { return f.pr.Close() } // unblocks a pump parked in Read

func TestRelayStream_FullDuplexPingPongAndCleanEnd(t *testing.T) {
	rig := newExposeRig(t, true)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		w.(http.Flusher).Flush()
		buf := make([]byte, 64)
		for {
			n, err := r.Body.Read(buf)
			if n > 0 {
				w.Write(buf[:n])
				w.(http.Flusher).Flush()
			}
			if err != nil {
				return
			}
		}
	}, nil)
	up := newFeed()
	resp, err := rig.stream(RelayStreamRequest{Body: up})
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.Status != 200 {
		t.Fatalf("status %d", resp.Status)
	}
	// The head came back while the upload is still open: ping-pong.
	for i := 0; i < 20; i++ {
		msg := bytes.Repeat([]byte{byte('a' + i)}, 10+i)
		if _, err := up.pw.Write(msg); err != nil {
			t.Fatal(err)
		}
		got := make([]byte, len(msg))
		if _, err := io.ReadFull(resp.Body, got); err != nil || !bytes.Equal(got, msg) {
			t.Fatalf("round %d: %q, %v", i, got, err)
		}
	}
	// A clean end of the upload ends the handler's body and so the response.
	up.pw.Close()
	if rest, err := io.ReadAll(resp.Body); err != nil || len(rest) != 0 {
		t.Fatalf("after the upload ended: %q, %v", rest, err)
	}
	if err := resp.Body.Close(); err != nil {
		t.Fatal(err)
	}
	rig.waitClosed()
}

func TestRelayStream_LargeUploadAndDownloadWithBackpressure(t *testing.T) {
	rig := newExposeRig(t, true)
	const size = 3 << 20
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		w.(http.Flusher).Flush()
		io.Copy(w, r.Body)
	}, &ExposeHTTPOptions{ResponseBuffer: 2, MaxChunk: 64 << 10})
	data := pattern(size)
	resp, err := rig.stream(RelayStreamRequest{Body: bytes.NewReader(data), UploadCapacity: 1, MaxChunk: 16 << 10})
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	var got bytes.Buffer
	buf := make([]byte, 7000) // slow, odd-sized reader
	for {
		n, err := resp.Body.Read(buf)
		got.Write(buf[:n])
		if err == io.EOF {
			break
		}
		if err != nil {
			t.Fatal(err)
		}
	}
	if !bytes.Equal(got.Bytes(), data) {
		t.Fatalf("echo mismatch: %d bytes", got.Len())
	}
}

// An early answer arrives while the upload is still being produced; closing
// the response abandons the exchange and stops the pump.
func TestRelayStream_EarlyAnswerWhileUploading(t *testing.T) {
	rig := newExposeRig(t, true)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		buf := make([]byte, 4096)
		for {
			if _, err := r.Body.Read(buf); err != nil {
				if errors.Is(err, ErrBodyTooLarge) {
					w.WriteHeader(http.StatusRequestEntityTooLarge)
				}
				return
			}
		}
	}, &ExposeHTTPOptions{MaxRequestBody: 20000})
	var produced atomic.Int64
	endless := readerFunc(func(p []byte) (int, error) {
		time.Sleep(time.Millisecond)
		produced.Add(int64(len(p)))
		return len(p), nil
	})
	resp, err := rig.stream(RelayStreamRequest{Body: endless})
	if err != nil {
		t.Fatal(err)
	}
	if resp.Status != 413 {
		t.Fatalf("status %d, want 413", resp.Status)
	}
	if err := resp.Body.Close(); err != nil {
		t.Fatal(err)
	}
	// The pump stopped: production no longer advances.
	eventually(t, "the upload pump to stop", func() bool {
		a := produced.Load()
		time.Sleep(50 * time.Millisecond)
		return produced.Load() == a
	})
	rig.waitClosed()
}

type readerFunc func([]byte) (int, error)

func (f readerFunc) Read(p []byte) (int, error) { return f(p) }

// A blocked upload must be interrupted through its actual io.Closer, not
// merely forgotten when the response/native handles are released.
type closeObservedFeed struct {
	*feed
	closed       chan struct{}
	readingAgain chan struct{}
	reads        atomic.Int32
	once         sync.Once
}

func (f *closeObservedFeed) Read(p []byte) (int, error) {
	if f.reads.Add(1) == 2 {
		close(f.readingAgain)
	}
	return f.feed.Read(p)
}

func (f *closeObservedFeed) Close() error {
	err := f.feed.Close()
	f.once.Do(func() { close(f.closed) })
	return err
}

func waitRelayDone(t *testing.T, description string, done <-chan struct{}) {
	t.Helper()
	select {
	case <-done:
	case <-time.After(waitBudget()):
		t.Fatalf("%s did not finish", description)
	}
}

func TestRelayStream_SourceFailureBeforeTheHeadFailsTheCall(t *testing.T) {
	rig := newExposeRig(t, true)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		io.Copy(io.Discard, r.Body) // no head until the body ends
		io.WriteString(w, "never")
	}, nil)
	boom := errors.New("disk fell over")
	sent := false
	src := readerFunc(func(p []byte) (int, error) {
		if !sent {
			sent = true
			return copy(p, "partial"), nil
		}
		time.Sleep(50 * time.Millisecond)
		return 0, boom
	})
	_, err := rig.stream(RelayStreamRequest{Body: src})
	if !errors.Is(err, boom) {
		t.Fatalf("err = %v, want the source's error", err)
	}
	rig.waitClosed()
}

func TestRelayStream_SourceFailureAfterTheHeadIsReportedNotReturned(t *testing.T) {
	rig := newExposeRig(t, true)
	handlerErr := make(chan error, 1)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		w.(http.Flusher).Flush()
		_, err := io.Copy(io.Discard, r.Body)
		handlerErr <- err
	}, nil)
	boom := errors.New("source broke")
	reported := make(chan error, 2)
	up := newFeed()
	resp, err := rig.stream(RelayStreamRequest{Body: up, OnUploadError: func(e error) { reported <- e }})
	if err != nil {
		t.Fatalf("a post-head failure must not fail the call: %v", err)
	}
	defer resp.Body.Close()
	up.pw.Write([]byte("some"))
	up.pw.CloseWithError(boom)
	select {
	case e := <-reported:
		if !errors.Is(e, boom) {
			t.Fatalf("reported %v", e)
		}
	case <-rig.ctx.Done():
		t.Fatal("OnUploadError never called")
	}
	// The handler sees a failed body, never a truncated one.
	if err := <-handlerErr; err == nil {
		t.Fatal("handler read the aborted upload as a clean end")
	}
	// The response read fails or ends; it must not hang.
	io.Copy(io.Discard, resp.Body)
}

func TestRelayStream_CloseBeforeTheEndResetsTheUpload(t *testing.T) {
	// The shared OTLP collector may accept an unrelated keepalive connection
	// during this test. Verify this exchange's resources instead of process counts.
	rig := newExposeRigWithLeakCheck(t, true, false)
	t.Cleanup(func() {
		// Run before the fixture's idempotent fallback cleanups so native Close
		// errors are observed on their first call, including each owned runtime.
		closed := make(chan []string, 1)
		go func() {
			var failures []string
			for _, step := range []struct {
				name  string
				close func() error
			}{
				{"connection", func() error { return rig.conn.Close("done") }},
				{"expose", rig.ex.Close}, {"bodies", rig.bodies.Close}, {"upcalls", rig.up.Close},
				{"client node and runtime", rig.client.Close}, {"server node and runtime", rig.server.Close},
			} {
				if err := step.close(); err != nil {
					failures = append(failures, step.name+": "+err.Error())
				}
			}
			for name, rt := range map[string]*Runtime{"server": rig.server.Runtime(), "client": rig.client.Runtime()} {
				if !rt.closed.Load() || rt.refs.Load() != 0 {
					failures = append(failures, name+" fixture runtime retained after node close")
				}
				if err := rt.Close(); err != nil {
					failures = append(failures, name+" exact runtime: "+err.Error())
				}
				rt.wg.Wait() // The exact CQ poller must have exited, not just been marked closed.
			}
			closed <- failures
		}()
		select {
		case failures := <-closed:
			for _, failure := range failures {
				t.Errorf("relay fixture cleanup: %s", failure)
			}
		case <-time.After(waitBudget()):
			t.Error("relay fixture and its exact runtimes did not stop within the wait budget")
		}
	})
	handlerErr := make(chan error, 1)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		w.(http.Flusher).Flush()
		w.Write([]byte("hello"))
		w.(http.Flusher).Flush()
		_, err := io.Copy(io.Discard, r.Body)
		handlerErr <- err
	}, nil)
	var uploadErrs atomic.Int64
	up := &closeObservedFeed{feed: newFeed(), closed: make(chan struct{}), readingAgain: make(chan struct{})}
	t.Cleanup(func() { _ = up.Close(); _ = up.pw.Close() })
	resp, err := rig.stream(RelayStreamRequest{Body: up, OnUploadError: func(error) { uploadErrs.Add(1) }})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = resp.Body.Close() })
	exchange := resp.Body.(*relayBody).x
	got := make([]byte, 5)
	if _, err := io.ReadFull(resp.Body, got); err != nil || string(got) != "hello" {
		t.Fatalf("%q, %v", got, err)
	}
	if _, err := up.pw.Write([]byte("in flight")); err != nil {
		t.Fatal(err)
	}
	// No second chunk is supplied: the pump must leave this source Read when closed.
	waitRelayDone(t, "upload pump requesting another source chunk", up.readingAgain)
	if err := resp.Body.Close(); err != nil { // before the end of the response
		t.Fatal(err)
	}
	waitRelayDone(t, "upload source close", up.closed)
	waitRelayDone(t, "upload pump and native upload release", exchange.pumpDone)
	waitRelayDone(t, "exchange abandonment", exchange.done)
	select {
	case err := <-handlerErr:
		if err == nil {
			t.Fatal("the handler read a clean end from an abandoned upload")
		}
	case <-rig.ctx.Done():
		t.Fatal("the upload was not reset")
	}
	if _, err := resp.Body.Read(make([]byte, 1)); err == nil {
		t.Fatal("read on a closed body succeeded")
	}
	if uploadErrs.Load() != 0 {
		t.Fatal("closing the response is not an upload failure")
	}
	rig.waitClosed()
}

func TestRelayStream_BodyReadToItsEndLeavesTheUploadToFinish(t *testing.T) {
	rig := newExposeRig(t, true)
	got := make(chan []byte, 1)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		// 202-then-read: the whole response ends first, the request body is
		// consumed afterwards.
		w.WriteHeader(http.StatusAccepted)
		w.(http.Flusher).Flush()
		io.WriteString(w, "accepted")
		w.(http.Flusher).Flush()
		b, _ := io.ReadAll(r.Body)
		got <- b
	}, nil)
	up := newFeed()
	var uploadErrs atomic.Int64
	resp, err := rig.stream(RelayStreamRequest{Body: up, OnUploadError: func(error) { uploadErrs.Add(1) }})
	if err != nil || resp.Status != 202 {
		t.Fatalf("%+v, %v", resp, err)
	}
	// The response only ends when the handler returns, which needs the whole
	// body: feed it, then finish.
	go func() {
		up.pw.Write([]byte("late body"))
		up.pw.Close()
	}()
	body, err := io.ReadAll(resp.Body)
	if err != nil || string(body) != "accepted" {
		t.Fatalf("%q, %v", body, err)
	}
	resp.Body.Close()
	if b := <-got; string(b) != "late body" {
		t.Fatalf("handler got %q", b)
	}
	if uploadErrs.Load() != 0 {
		t.Fatal("clean upload reported an error")
	}
}

func TestRelayStream_RefusalsHaveStableCodes(t *testing.T) {
	rig := newExposeRig(t, true)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) { io.WriteString(w, "in") }, nil)

	// Unknown service.
	_, err := rig.conn.RelayStream(rig.ctx, RelayStreamRequest{ServiceID: "nope", Method: "GET", URI: "/"})
	if !errors.Is(err, ErrRelayUnknownService) || !errors.Is(err, ErrNotFound) {
		t.Fatalf("unknown service: %v", err)
	}
	var be *BodyError
	if !errors.As(err, &be) || be.Code != 10 {
		t.Fatalf("unknown service code: %v", err)
	}

	// Denied: matches ErrPermissionDenied for the streaming and the buffered relay.
	rig.allow.Store(false)
	_, err = rig.stream(RelayStreamRequest{Method: "GET"})
	if !errors.Is(err, ErrRelayDenied) || !errors.Is(err, ErrPermissionDenied) || errors.Is(err, ErrNotFound) {
		t.Fatalf("denied stream: %v", err)
	}
	_, err = rig.relay(HTTPRelayRequest{})
	if !errors.Is(err, ErrPermissionDenied) {
		t.Fatalf("denied buffered relay: %v", err)
	}
	_, err = rig.conn.RelayRequest(rig.ctx, HTTPRelayRequest{ServiceID: "nope", Method: "GET", URI: "/"})
	if !errors.Is(err, ErrNotFound) {
		t.Fatalf("unknown service, buffered relay: %v", err)
	}
	rig.allow.Store(true)
	resp, err := rig.stream(RelayStreamRequest{Method: "GET"})
	if err != nil {
		t.Fatal(err)
	}
	b, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	if string(b) != "in" {
		t.Fatalf("allowed: %q", b)
	}

	// Invalid arguments are refused before anything is sent.
	if _, err := rig.conn.RelayStream(rig.ctx, RelayStreamRequest{Method: "GET", URI: "/"}); !errors.Is(err, ErrInvalidArgument) {
		t.Errorf("empty service id: %v", err)
	}
	if _, err := rig.conn.RelayStream(rig.ctx, RelayStreamRequest{ServiceID: "x", URI: "/"}); !errors.Is(err, ErrInvalidArgument) {
		t.Errorf("empty method: %v", err)
	}
}

func TestRelayStream_ConnectionClosedAndContextCancel(t *testing.T) {
	rig := newExposeRig(t, true)
	entered := make(chan struct{}, 4)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		entered <- struct{}{}
		<-r.Context().Done()
	}, nil)

	// ctx cancelled before the head.
	ctx, cancel := context.WithCancel(rig.ctx)
	errc := make(chan error, 1)
	go func() {
		_, err := rig.conn.RelayStream(ctx, RelayStreamRequest{ServiceID: exposeSvc, Method: "GET", URI: "/"})
		errc <- err
	}()
	<-entered
	cancel()
	if err := <-errc; !errors.Is(err, context.Canceled) {
		t.Fatalf("cancelled: %v", err)
	}

	// The connection ends while the head is pending.
	go func() {
		_, err := rig.conn.RelayStream(rig.ctx, RelayStreamRequest{ServiceID: exposeSvc, Method: "GET", URI: "/"})
		errc <- err
	}()
	<-entered
	rig.conn.Close("bye")
	if err := <-errc; err == nil {
		t.Fatal("relay on a closed connection succeeded")
	}
	// And a new stream on the closed connection fails.
	if _, err := rig.conn.RelayStream(rig.ctx, RelayStreamRequest{ServiceID: exposeSvc, Method: "GET", URI: "/"}); err == nil {
		t.Fatal("RelayStream on a closed connection succeeded")
	}
	rig.waitClosed()
}

func TestRelayStream_ContextCancelAfterTheHeadFailsReads(t *testing.T) {
	rig := newExposeRig(t, true)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		w.(http.Flusher).Flush()
		<-r.Context().Done()
	}, nil)
	ctx, cancel := context.WithCancel(rig.ctx)
	resp, err := rig.conn.RelayStream(ctx, RelayStreamRequest{ServiceID: exposeSvc, Method: "GET", URI: "/"})
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	go func() { time.Sleep(100 * time.Millisecond); cancel() }()
	if _, err := resp.Body.Read(make([]byte, 8)); err == nil {
		t.Fatal("read survived a cancelled context")
	}
}

// Stress: concurrent full-duplex exchanges (-race, -count=20).
func TestRelayStream_ConcurrentExchanges(t *testing.T) {
	rig := newExposeRig(t, true)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		w.(http.Flusher).Flush()
		io.Copy(w, r.Body)
	}, &ExposeHTTPOptions{ResponseBuffer: 2, MaxChunk: 32 << 10})
	done := make(chan struct{})
	const n = 16
	for i := 0; i < n; i++ {
		go func(i int) {
			defer func() { done <- struct{}{} }()
			data := pattern(1000 + i*40000)
			resp, err := rig.stream(RelayStreamRequest{Body: bytes.NewReader(data)})
			if err != nil {
				t.Errorf("exchange %d: %v", i, err)
				return
			}
			defer resp.Body.Close()
			got, err := io.ReadAll(resp.Body)
			if err != nil || !bytes.Equal(got, data) {
				t.Errorf("exchange %d: %d bytes, %v", i, len(got), err)
			}
		}(i)
	}
	for i := 0; i < n; i++ {
		<-done
	}
	rig.waitClosed()
}
