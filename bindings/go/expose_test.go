//go:build cgo

package aster

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// C ABI 4.2 end to end (ported from ffi/tests/expose.rs): authorize allow /
// deny / timeout, HTTP exchanges with inline and streamed bodies, handler
// failures and limits, abort, consumer cancellation, socket targets, route
// policy, relay_request and request_route.

const exposeALPN = "aster-expose/go-test"
const exposeSvc = "svc"

type exposeRig struct {
	t        *testing.T
	ctx      context.Context
	server   *Node
	client   *Node
	up       *Upcalls
	bodies   *HTTPBodies
	ex       *Expose
	conn     *Connection // the consumer's connection to the server
	clientID string
	allow    atomic.Bool
	authz    atomic.Int64
}

// newExposeRig starts a server (with an expose registry, an upcall table, a
// body table and an allow/deny authorizer) and a client connected to it.
// serve picks the accept path: Expose.Serve (true) or Node.Accept +
// HandleConnection (false).
func newExposeRig(t *testing.T, serve bool) *exposeRig {
	t.Helper()
	leakCheck(t)
	ctx := upCtx(t)
	r := &exposeRig{t: t, ctx: ctx}
	r.server = startTestNode(t, ctx, exposeALPN)
	r.client = startTestNode(t, ctx, exposeALPN)
	r.up = testUpcalls(t, r.server.Runtime())
	var err error
	if r.bodies, err = NewHTTPBodies(r.server.Runtime()); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = r.bodies.Close() })
	if r.ex, err = NewExpose(r.server); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = r.ex.Close() })
	r.clientID, _ = r.client.NodeID()
	r.allow.Store(true)
	if err := r.up.ServeExposeAuthorize(func(_ context.Context, req ExposeAuthorizeRequest) (bool, error) {
		r.authz.Add(1)
		if req.PeerID != r.clientID {
			t.Errorf("authorize saw peer %q, want %q", req.PeerID, r.clientID)
		}
		return r.allow.Load(), nil
	}); err != nil {
		t.Fatal(err)
	}

	var addr *NodeAddr
	for deadline := time.Now().Add(10 * time.Second); ; time.Sleep(50 * time.Millisecond) {
		if addr, err = r.server.NodeAddr(); err != nil {
			t.Fatal(err)
		}
		if len(addr.DirectAddresses) > 0 || time.Now().After(deadline) {
			break
		}
	}
	if serve {
		if err := r.ex.Serve(exposeALPN); err != nil {
			t.Fatal(err)
		}
		if r.conn, err = r.client.Connect(ctx, addr, exposeALPN); err != nil {
			t.Fatal(err)
		}
	} else {
		type acc struct {
			c   *Connection
			err error
		}
		accepted := make(chan acc, 1)
		go func() {
			c, err := r.server.Accept(ctx)
			accepted <- acc{c, err}
		}()
		if r.conn, err = r.client.Connect(ctx, addr, exposeALPN); err != nil {
			t.Fatal(err)
		}
		a := <-accepted
		if a.err != nil {
			t.Fatal(a.err)
		}
		t.Cleanup(func() { _ = a.c.Close("done") })
		if err := r.ex.HandleConnection(a.c); err != nil {
			t.Fatal(err)
		}
	}
	t.Cleanup(func() { _ = r.conn.Close("done") })
	return r
}

func (r *exposeRig) serveHTTP(h http.HandlerFunc, opts *ExposeHTTPOptions) {
	r.t.Helper()
	if err := r.ex.ExposeHTTP(r.up, r.bodies, exposeSvc, h, opts); err != nil {
		r.t.Fatal(err)
	}
}

func (r *exposeRig) relay(req HTTPRelayRequest) (*HTTPRelayResponse, error) {
	r.t.Helper()
	req.ServiceID = exposeSvc
	if req.Method == "" {
		req.Method = "GET"
	}
	if req.URI == "" {
		req.URI = "/"
	}
	return r.conn.RelayRequest(r.ctx, req)
}

func (r *exposeRig) openExchanges() uint64 {
	n, err := r.bodies.OpenCount()
	if err != nil {
		r.t.Fatal(err)
	}
	return n
}

func (r *exposeRig) waitClosed() {
	r.t.Helper()
	eventually(r.t, "exchanges to close", func() bool { return r.openExchanges() == 0 })
}

func headerValues(hs []HTTPHeader, name string) []string {
	var out []string
	for _, h := range hs {
		if h.Name == name {
			out = append(out, string(h.Value))
		}
	}
	return out
}

func pattern(n int) []byte {
	b := make([]byte, n)
	for i := range b {
		b[i] = byte(i * 31 % 251)
	}
	return b
}

func TestExpose_InlineRequestAndResponse(t *testing.T) {
	rig := newExposeRig(t, true)
	type seen struct {
		info          ExposeInfo
		method, uri   string
		host, remote  string
		body          []byte
		contentLength int64
		xbin          string
	}
	got := make(chan seen, 4)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		info, _ := ExposeInfoFrom(r.Context())
		body, _ := io.ReadAll(r.Body)
		got <- seen{info, r.Method, r.RequestURI, r.Host, r.RemoteAddr, body, r.ContentLength, r.Header.Get("X-Bin")}
		if r.Method == http.MethodPost {
			w.Header().Set("Content-Type", "text/plain")
			w.Header().Add("Set-Cookie", "a=1")
			w.Header().Add("Set-Cookie", "b=2")
			w.WriteHeader(http.StatusCreated)
			w.Write(append(body, '!'))
			return
		}
		io.WriteString(w, "ok")
	}, nil)

	resp, err := rig.relay(HTTPRelayRequest{Method: "POST", URI: "/echo?x=1", Body: []byte("hello"),
		Headers: []HTTPHeader{{Name: "Host", Value: []byte("app.local")}, {Name: "X-Bin", Value: []byte("v")}}})
	if err != nil {
		t.Fatal(err)
	}
	if resp.Status != 201 || string(resp.Body) != "hello!" {
		t.Fatalf("resp = %d %q", resp.Status, resp.Body)
	}
	if v := headerValues(resp.Headers, "content-type"); len(v) != 1 || v[0] != "text/plain" {
		t.Errorf("content-type = %v", v)
	}
	if v := headerValues(resp.Headers, "set-cookie"); len(v) != 2 || v[0] != "a=1" || v[1] != "b=2" {
		t.Errorf("repeated header = %v", v)
	}
	if v := headerValues(resp.Headers, "content-length"); len(v) != 1 || v[0] != "6" {
		t.Errorf("content-length = %v", v)
	}
	s := <-got
	if s.info.ServiceID != exposeSvc || !s.info.HasPeer || s.info.PeerID != rig.clientID || s.info.StreamID == 0 {
		t.Errorf("info = %+v", s.info)
	}
	if s.method != "POST" || s.uri != "/echo?x=1" || s.host != "app.local" || s.remote != rig.clientID ||
		string(s.body) != "hello" || s.contentLength != 5 || s.xbin != "v" {
		t.Errorf("request = %+v", s)
	}

	// No body at all: the empty body is streamed and reads as EOF at once.
	resp, err = rig.relay(HTTPRelayRequest{})
	if err != nil || resp.Status != 200 || string(resp.Body) != "ok" {
		t.Fatalf("GET = %+v, %v", resp, err)
	}
	if s := <-got; s.contentLength != -1 || len(s.body) != 0 {
		t.Errorf("bodyless request = %+v", s)
	}
	if rig.authz.Load() < 1 {
		t.Error("the authorizer was never asked")
	}
	rig.waitClosed()

	// RelayHTTP is the same over net/http types; content-type is sniffed.
	req, _ := http.NewRequest("POST", "http://app.local/echo", strings.NewReader("net"))
	hr, err := rig.conn.RelayHTTP(rig.ctx, exposeSvc, req)
	if err != nil {
		t.Fatal(err)
	}
	defer hr.Body.Close()
	b, _ := io.ReadAll(hr.Body)
	if hr.StatusCode != 201 || string(b) != "net!" || hr.Header.Get("Content-Type") != "text/plain" {
		t.Errorf("RelayHTTP = %d %q %v", hr.StatusCode, b, hr.Header)
	}
}

func TestExpose_StreamedRequestAndResponseWithBackpressure(t *testing.T) {
	rig := newExposeRig(t, false) // also covers Accept + HandleConnection
	const size = 3 << 20
	upload := pattern(size)
	var contentLength atomic.Int64
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		contentLength.Store(r.ContentLength)
		w.Header().Set("Content-Type", "application/octet-stream")
		w.(http.Flusher).Flush() // stream the response
		if n, err := io.Copy(w, r.Body); err != nil || n != size {
			t.Errorf("echoed %d bytes: %v", n, err)
		}
	}, &ExposeHTTPOptions{ResponseBuffer: 2, MaxChunk: 64 << 10})

	resp, err := rig.relay(HTTPRelayRequest{Method: "POST", URI: "/upload", Body: upload})
	if err != nil {
		t.Fatal(err)
	}
	if resp.Status != 200 || !bytes.Equal(resp.Body, upload) {
		t.Fatalf("echo mismatch: status %d, %d bytes", resp.Status, len(resp.Body))
	}
	if contentLength.Load() != size {
		t.Errorf("handler saw content-length %d", contentLength.Load())
	}
	rig.waitClosed()
}

// The raw body API: writes before the head queue up to the buffer, TryWrite
// reports "full", trailers, and calls on a finished exchange.
func TestExpose_RawBodyBackpressureAndTrailers(t *testing.T) {
	rig := newExposeRig(t, true)
	rig.serveHTTP(func(http.ResponseWriter, *http.Request) {}, &ExposeHTTPOptions{ResponseBuffer: 2})
	failures := make(chan string, 8)
	done := make(chan struct{})
	rig.up.HandleKind(UpcallExposeHTTP, func(_ context.Context, req *UpcallRequest) ([]byte, error) {
		defer close(done)
		r, err := DecodeExposeHTTPRequest(req.Payload)
		if err != nil {
			return nil, err
		}
		fail := func(format string, a ...any) {
			select {
			case failures <- fmt.Sprintf(format, a...):
			default:
			}
		}
		got, buf := []byte(nil), make([]byte, 4096)
		for {
			n, eof, err := rig.bodies.Read(r.StreamID, buf, 5*time.Second)
			if err != nil {
				fail("read: %v", err)
				break
			}
			got = append(got, buf[:n]...)
			if eof {
				break
			}
		}
		chunks := [][]byte{got[:16<<10], got[16<<10 : 32<<10], got[32<<10 : 48<<10]}
		for i, want := range []bool{true, true, false} {
			if q, err := rig.bodies.TryWrite(r.StreamID, chunks[i]); err != nil || q != want {
				fail("TryWrite %d = %v, %v", i, q, err)
			}
		}
		req.Respond(UpcallStatusOK, EncodeExposeHTTPResponse(ExposeHTTPResponse{
			Status: 200, Body: HTTPBody{Stream: true}}))
		for rest := got[32<<10:]; len(rest) > 0; {
			n := min(len(rest), 16<<10)
			if err := rig.bodies.Write(r.StreamID, rest[:n], 10*time.Second); err != nil {
				fail("Write: %v", err)
				break
			}
			rest = rest[n:]
		}
		if err := rig.bodies.Finish(r.StreamID, []HTTPHeader{{Name: "x-checksum", Value: []byte("abc")}}); err != nil {
			fail("Finish: %v", err)
		}
		err = rig.bodies.Write(r.StreamID, []byte("late"), time.Second)
		if !errors.Is(err, ErrBodyFinished) && !errors.Is(err, ErrBodyUnknownStream) {
			fail("write after finish = %v", err)
		}
		return nil, nil
	})
	upload := pattern(200 << 10)
	resp, err := rig.relay(HTTPRelayRequest{Method: "POST", URI: "/u", Body: upload})
	if err != nil {
		t.Fatal(err)
	}
	if resp.Status != 200 || !bytes.Equal(resp.Body, upload) {
		t.Fatalf("status %d, %d bytes", resp.Status, len(resp.Body))
	}
	<-done
	select {
	case f := <-failures:
		t.Fatal(f)
	default:
	}
	rig.waitClosed()

	// A blocking write beyond the buffer times out (nothing queued) until the
	// head goes out.
	rig2 := newExposeRig(t, true)
	rig2.serveHTTP(func(http.ResponseWriter, *http.Request) {}, &ExposeHTTPOptions{ResponseBuffer: 1})
	rig2.up.HandleKind(UpcallExposeHTTP, func(_ context.Context, req *UpcallRequest) ([]byte, error) {
		r, _ := DecodeExposeHTTPRequest(req.Payload)
		if err := rig2.bodies.Write(r.StreamID, []byte("one"), 2*time.Second); err != nil {
			t.Errorf("first write: %v", err)
		}
		if err := rig2.bodies.Write(r.StreamID, []byte("two"), 100*time.Millisecond); !errors.Is(err, ErrBodyTimeout) || !errors.Is(err, ErrTimeout) {
			t.Errorf("second write = %v, want timeout", err)
		}
		req.Respond(UpcallStatusOK, EncodeExposeHTTPResponse(ExposeHTTPResponse{Status: 200, Body: HTTPBody{Stream: true}}))
		if err := rig2.bodies.Write(r.StreamID, []byte("two"), 10*time.Second); err != nil {
			t.Errorf("second write after head: %v", err)
		}
		if err := rig2.bodies.Finish(r.StreamID, nil); err != nil {
			t.Errorf("finish: %v", err)
		}
		return nil, nil
	})
	resp, err = rig2.relay(HTTPRelayRequest{})
	if err != nil || string(resp.Body) != "onetwo" {
		t.Fatalf("resp = %+v, %v", resp, err)
	}
}

func TestExpose_AuthorizeDenyAndFailureModes(t *testing.T) {
	rig := newExposeRig(t, true)
	var handled atomic.Int64
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		handled.Add(1)
		io.WriteString(w, "in")
	}, nil)

	rig.allow.Store(false)
	if _, err := rig.relay(HTTPRelayRequest{}); err == nil {
		t.Fatal("a denied peer got a response")
	}
	if handled.Load() != 0 {
		t.Fatal("the handler ran for a denied peer")
	}
	// The decision is Go's: flip it and a fresh stream is admitted.
	rig.allow.Store(true)
	resp, err := rig.relay(HTTPRelayRequest{})
	if err != nil || string(resp.Body) != "in" {
		t.Fatalf("allowed: %+v, %v", resp, err)
	}

	// An authorizer that never answers: the failure mode decides. Admission
	// is decided once per (connection, service) and an allowed pair is
	// remembered, so use services not seen on this connection yet.
	var cancelled atomic.Int64
	if err := rig.up.ServeExposeAuthorize(func(ctx context.Context, _ ExposeAuthorizeRequest) (bool, error) {
		<-ctx.Done()
		cancelled.Add(1)
		return true, ctx.Err()
	}); err != nil {
		t.Fatal(err)
	}
	if err := rig.up.SetPolicy(UpcallExposeAuthorize, 200*time.Millisecond, FailureModeClosed); err != nil {
		t.Fatal(err)
	}
	h := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { io.WriteString(w, "in") })
	for _, svc := range []string{"hung-closed", "hung-open"} {
		if err := rig.ex.ExposeHTTP(rig.up, rig.bodies, svc, h, nil); err != nil {
			t.Fatal(err)
		}
	}
	relayTo := func(svc string) (*HTTPRelayResponse, error) {
		return rig.conn.RelayRequest(rig.ctx, HTTPRelayRequest{ServiceID: svc, Method: "GET", URI: "/", Body: []byte{}})
	}
	if _, err := relayTo("hung-closed"); err == nil {
		t.Fatal("fail-closed admitted an unanswered request")
	}
	eventually(t, "the hung authorizer to be cancelled", func() bool { return cancelled.Load() >= 1 })
	if err := rig.up.SetPolicy(UpcallExposeAuthorize, 200*time.Millisecond, FailureModeOpen); err != nil {
		t.Fatal(err)
	}
	resp, err = relayTo("hung-open")
	if err != nil || string(resp.Body) != "in" {
		t.Fatalf("fail-open: %+v, %v", resp, err)
	}
}

func TestExpose_HandlerFailuresBecome5xxStatuses(t *testing.T) {
	rig := newExposeRig(t, true)
	cancelled := make(chan struct{}, 1)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/panic":
			panic("secret detail")
		case "/badstatus":
			w.WriteHeader(1000)
		case "/slow":
			select {
			case <-r.Context().Done(): // Rust gave up on the head
				cancelled <- struct{}{}
			case <-time.After(10 * time.Second):
			}
			io.WriteString(w, "too late")
		}
	}, nil)
	if err := rig.up.SetPolicy(UpcallExposeHTTP, 400*time.Millisecond, FailureModeClosed); err != nil {
		t.Fatal(err)
	}
	for path, want := range map[string]uint16{"/panic": 500, "/badstatus": 500, "/slow": 504} {
		resp, err := rig.relay(HTTPRelayRequest{URI: path})
		if err != nil {
			t.Fatalf("%s: %v", path, err)
		}
		if resp.Status != want {
			t.Errorf("%s = %d, want %d", path, resp.Status, want)
		}
		if len(resp.Body) != 0 || strings.Contains(string(resp.Body), "secret") {
			t.Errorf("%s: failure body %q", path, resp.Body)
		}
		for _, h := range resp.Headers {
			if strings.Contains(string(h.Value), "secret") {
				t.Errorf("%s: the handler's message reached the peer in %s", path, h.Name)
			}
		}
	}
	select {
	case <-cancelled:
	case <-rig.ctx.Done():
		t.Fatal("the slow handler was never cancelled")
	}

	// A service Rust knows but Go does not serve answers UNIMPLEMENTED: 501.
	rig.up.removeHTTPService(exposeSvc)
	if resp, err := rig.relay(HTTPRelayRequest{}); err != nil || resp.Status != 501 {
		t.Fatalf("unserved service = %+v, %v", resp, err)
	}
	rig.waitClosed()
}

func TestExpose_Limits(t *testing.T) {
	rig := newExposeRig(t, true)
	var handled atomic.Int64
	var writeErr atomic.Value
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		handled.Add(1)
		switch r.URL.Path {
		case "/inline-over-cap":
			w.Write(make([]byte, 2000))
		case "/streamed-over-cap":
			w.(http.Flusher).Flush()
			w.Write(make([]byte, 1000))
			if _, err := w.Write(make([]byte, 1000)); err != nil {
				writeErr.Store(err)
			}
		}
	}, &ExposeHTTPOptions{MaxRequestBody: 2000, MaxResponseBody: 1500})

	// A declared length over the cap is a 413 without any upcall.
	resp, err := rig.relay(HTTPRelayRequest{Method: "POST", Body: make([]byte, 5000)})
	if err != nil || resp.Status != 413 || handled.Load() != 0 {
		t.Fatalf("oversized request = %+v, %v (handled %d)", resp, err, handled.Load())
	}
	// An inline body over the response cap becomes a 502.
	resp, err = rig.relay(HTTPRelayRequest{URI: "/inline-over-cap"})
	if err != nil || resp.Status != 502 {
		t.Fatalf("inline over cap = %+v, %v", resp, err)
	}
	// A streamed one is aborted by the write that crosses the cap.
	if _, err = rig.relay(HTTPRelayRequest{URI: "/streamed-over-cap"}); err == nil {
		t.Fatal("an over-cap streamed body must fail the consumer, not truncate")
	}
	eventually(t, "the crossing write to fail", func() bool { return writeErr.Load() != nil })
	if err := writeErr.Load().(error); !errors.Is(err, ErrBodyTooLarge) {
		t.Fatalf("crossing write = %v, want ErrBodyTooLarge", err)
	}
	rig.waitClosed()
}

func TestExpose_AbortedStreamFailsTheConsumer(t *testing.T) {
	rig := newExposeRig(t, true)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		w.(http.Flusher).Flush()
		w.Write(make([]byte, 4096))
		time.Sleep(200 * time.Millisecond)
		panic(http.ErrAbortHandler)
	}, nil)
	if resp, err := rig.relay(HTTPRelayRequest{}); err == nil {
		t.Fatalf("an aborted body must fail, not truncate: %+v", resp)
	}
	rig.waitClosed()
}

func TestExpose_StreamedResponseWithTrailersAndHead(t *testing.T) {
	rig := newExposeRig(t, true)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Trailer", "X-Checksum")
		w.Header().Set("Content-Length", "5")
		io.WriteString(w, "hello")
		w.Header().Set("X-Checksum", "abc")
	}, nil)
	// Trailers force a streamed body; the relay hop does not carry them yet,
	// so the consumer sees the body only.
	resp, err := rig.relay(HTTPRelayRequest{})
	if err != nil || resp.Status != 200 || string(resp.Body) != "hello" {
		t.Fatalf("trailer response = %+v, %v", resp, err)
	}
	// HEAD keeps the headers and drops the body.
	resp, err = rig.relay(HTTPRelayRequest{Method: "HEAD"})
	if err != nil || len(resp.Body) != 0 || headerValues(resp.Headers, "content-length")[0] != "5" {
		t.Fatalf("HEAD = %+v, %v", resp, err)
	}
	rig.waitClosed()
}

func TestExpose_ConsumerGoneBeforeTheHeadCancelsTheHandler(t *testing.T) {
	rig := newExposeRig(t, true)
	entered := make(chan struct{})
	finished := make(chan error, 1)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		close(entered)
		select {
		case <-r.Context().Done():
			finished <- nil
		case <-time.After(10 * time.Second):
			finished <- errors.New("never cancelled")
		}
		// A late answer is ignored.
		io.WriteString(w, "late")
	}, nil)
	relayErr := make(chan error, 1)
	go func() {
		_, err := rig.relay(HTTPRelayRequest{})
		relayErr <- err
	}()
	select {
	case <-entered:
	case <-rig.ctx.Done():
		t.Fatal("the handler never ran")
	}
	if err := rig.conn.Close("cancel"); err != nil {
		t.Fatal(err)
	}
	if err := <-relayErr; err == nil {
		t.Fatal("the abandoned relay completed")
	}
	if err := <-finished; err != nil {
		t.Fatal(err)
	}
	eventually(t, "CANCEL{abandoned}", func() bool { return rig.up.CancelCount() >= 1 })
	rig.waitClosed()
}

func TestExpose_ConsumerGoneMidStreamMakesWritesCancelled(t *testing.T) {
	rig := newExposeRig(t, true)
	started := make(chan struct{})
	var once sync.Once
	outcome := make(chan error, 1)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		w.(http.Flusher).Flush()
		chunk := make([]byte, 256<<10)
		for end := time.Now().Add(upcallWait); time.Now().Before(end); {
			_, err := w.Write(chunk)
			once.Do(func() { close(started) })
			if err != nil {
				outcome <- err
				return
			}
		}
		outcome <- errors.New("writes never failed")
	}, nil)
	relayErr := make(chan error, 1)
	go func() {
		_, err := rig.relay(HTTPRelayRequest{})
		relayErr <- err
	}()
	select {
	case <-started:
	case <-rig.ctx.Done():
		t.Fatal("streaming never started")
	}
	if err := rig.conn.Close("bye"); err != nil {
		t.Fatal(err)
	}
	if err := <-relayErr; err == nil {
		t.Fatal("relay completed although the stream never finished")
	}
	if err := <-outcome; !errors.Is(err, ErrBodyCancelled) {
		t.Fatalf("handler write error = %v, want ErrBodyCancelled", err)
	}
	rig.waitClosed()
}

func TestExpose_SocketTargetWithAndWithoutAnAuthorizer(t *testing.T) {
	rig := newExposeRig(t, true)
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	accepted := make(chan net.Conn, 4)
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			accepted <- c
		}
	}()
	addr := ln.Addr().String()

	// Argument validation.
	if err := rig.ex.ExposeSocket("sock", "not an addr", nil); !errors.Is(err, ErrInvalidArgument) {
		t.Errorf("bad addr: %v", err)
	}
	if err := rig.ex.ExposeSocket("", addr, nil); !errors.Is(err, ErrInvalidArgument) {
		t.Errorf("empty service id: %v", err)
	}

	// Guarded by the authorizer: denied, so nothing is spliced.
	if err := rig.ex.ExposeSocket("sock", addr, rig.up); err != nil {
		t.Fatal(err)
	}
	rig.allow.Store(false)
	if _, err := rig.conn.RelayRequest(rig.ctx, HTTPRelayRequest{ServiceID: "sock", Method: "GET", URI: "/", Body: []byte{}}); err == nil {
		t.Fatal("a denied peer reached the socket")
	}
	select {
	case <-accepted:
		t.Fatal("the target saw a connection while denied")
	case <-time.After(200 * time.Millisecond):
	}

	// Allowed: the stream is spliced to the target. The consumer's framing is
	// not HTTP/1.1, so the relay call ends once the target hangs up.
	rig.allow.Store(true)
	relayDone := make(chan error, 1)
	go func() {
		_, err := rig.conn.RelayRequest(rig.ctx, HTTPRelayRequest{ServiceID: "sock", Method: "GET", URI: "/", Body: []byte{}})
		relayDone <- err
	}()
	select {
	case c := <-accepted:
		_ = c.Close()
	case <-rig.ctx.Done():
		t.Fatal("the target never saw a connection")
	}
	select {
	case <-relayDone:
	case <-rig.ctx.Done():
		t.Fatal("the relay never ended")
	}

	// Without an authorizer every peer is admitted and no authorize upcall is made.
	if err := rig.ex.ExposeSocket("open", addr, nil); err != nil {
		t.Fatal(err)
	}
	before := rig.authz.Load()
	rig.allow.Store(false)
	go func() {
		_, err := rig.conn.RelayRequest(rig.ctx, HTTPRelayRequest{ServiceID: "open", Method: "GET", URI: "/", Body: []byte{}})
		relayDone <- err
	}()
	select {
	case c := <-accepted:
		_ = c.Close()
	case <-rig.ctx.Done():
		t.Fatal("the open service never reached the target")
	}
	<-relayDone
	if rig.authz.Load() != before {
		t.Fatal("an authorizer-less service asked the authorizer")
	}
}

func TestExpose_RoutePolicyGrantDenyAndFailureMode(t *testing.T) {
	rig := newExposeRig(t, true)
	var mu sync.Mutex
	mode := "grant"
	type seen struct {
		peer     string
		metadata []byte
		specs    []RouteSpec
	}
	seenCh := make(chan seen, 8)
	policy := func(ctx context.Context, req ExposeRoutePolicyRequest) ([]uint32, error) {
		seenCh <- seen{req.PeerID, req.Metadata, req.Specs}
		mu.Lock()
		m := mode
		mu.Unlock()
		switch m {
		case "grant":
			return []uint32{0, 2}, nil
		case "deny":
			return []uint32{}, nil
		case "out-of-range":
			return []uint32{1, 9}, nil
		}
		<-ctx.Done() // silent
		return nil, ctx.Err()
	}
	if err := rig.ex.ExposeRoutePolicy(rig.up, policy); err != nil {
		t.Fatal(err)
	}
	specs := []RouteSpec{
		{Host: "a.example", Port: 443, Protocol: RouteProtocolHTTP, ServiceID: "web"},
		{Host: "", Port: 8080, Protocol: RouteProtocolTCP, ServiceID: "db"},
		{Host: "b.example", Port: 0, Protocol: RouteProtocolHTTP, ServiceID: "api"},
	}
	setMode := func(m string) { mu.Lock(); mode = m; mu.Unlock() }
	request := func(meta string) ([]uint32, *RouteRegistration) {
		t.Helper()
		g, reg, err := rig.conn.RequestRoute(rig.ctx, specs, []byte(meta))
		if err != nil {
			t.Fatalf("RequestRoute(%s): %v", mode, err)
		}
		return g, reg
	}
	eq := func(got, want []uint32) bool {
		if len(got) != len(want) {
			return false
		}
		for i := range got {
			if got[i] != want[i] {
				return false
			}
		}
		return true
	}

	// Grant: the subset Go named, as indexes into the request.
	g, reg := request("tok")
	if !eq(g, []uint32{0, 2}) || reg == nil {
		t.Fatalf("granted %v, reg %v", g, reg)
	}
	s := <-seenCh
	if s.peer != rig.clientID || string(s.metadata) != "tok" || len(s.specs) != 3 || s.specs[1] != specs[1] || s.specs[2] != specs[2] {
		t.Fatalf("policy saw %+v", s)
	}
	if err := reg.Close(); err != nil {
		t.Fatal(err)
	}
	if err := reg.Close(); err != nil { // idempotent
		t.Fatal(err)
	}

	// Deny: nothing granted.
	setMode("deny")
	g, reg = request("")
	if len(g) != 0 {
		t.Fatalf("deny granted %v", g)
	}
	_ = reg.Close()

	// A malformed answer is "no usable answer": fail-closed grants nothing.
	setMode("out-of-range")
	g, reg = request("")
	if len(g) != 0 {
		t.Fatalf("out-of-range granted %v", g)
	}
	_ = reg.Close()

	// No answer: the failure mode decides; fail-open grants every route.
	setMode("silent")
	if err := rig.up.SetPolicy(UpcallExposeRoutePolicy, 300*time.Millisecond, FailureModeOpen); err != nil {
		t.Fatal(err)
	}
	g, reg = request("")
	if !eq(g, []uint32{0, 1, 2}) {
		t.Fatalf("fail-open granted %v", g)
	}
	_ = reg.Close()
	if err := rig.up.SetPolicy(UpcallExposeRoutePolicy, 300*time.Millisecond, FailureModeClosed); err != nil {
		t.Fatal(err)
	}
	g, reg = request("")
	if len(g) != 0 {
		t.Fatalf("fail-closed granted %v", g)
	}
	_ = reg.Close()

	// Argument validation happens before anything is sent.
	for _, bad := range []RouteSpec{
		{ServiceID: "x", Port: 1, Protocol: 7},
	} {
		if _, _, err := rig.conn.RequestRoute(rig.ctx, []RouteSpec{bad}, nil); !errors.Is(err, ErrInvalidArgument) {
			t.Errorf("spec %+v: %v", bad, err)
		}
	}
}

func TestExpose_UnexposeStopsTheService(t *testing.T) {
	rig := newExposeRig(t, true)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) { io.WriteString(w, "up") }, nil)
	if resp, err := rig.relay(HTTPRelayRequest{}); err != nil || string(resp.Body) != "up" {
		t.Fatalf("before: %+v, %v", resp, err)
	}
	if err := rig.ex.Unexpose(exposeSvc); err != nil {
		t.Fatal(err)
	}
	if err := rig.ex.Unexpose(exposeSvc); err != nil { // idempotent
		t.Fatal(err)
	}
	if _, err := rig.relay(HTTPRelayRequest{}); err == nil {
		t.Fatal("an unexposed service answered")
	}
	// Registering it again brings it back.
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) { io.WriteString(w, "again") }, nil)
	if resp, err := rig.relay(HTTPRelayRequest{}); err != nil || string(resp.Body) != "again" {
		t.Fatalf("after: %+v, %v", resp, err)
	}
}

func TestExpose_ValidationAndLifecycle(t *testing.T) {
	rig := newExposeRig(t, true)
	h := http.HandlerFunc(func(http.ResponseWriter, *http.Request) {})
	if err := rig.ex.ExposeHTTP(rig.up, rig.bodies, exposeSvc, nil, nil); !errors.Is(err, ErrInvalidArgument) {
		t.Errorf("nil handler: %v", err)
	}
	if err := rig.ex.ExposeHTTP(rig.up, rig.bodies, "", h, nil); !errors.Is(err, ErrInvalidArgument) {
		t.Errorf("empty service id: %v", err)
	}
	if err := rig.ex.ExposeHTTP(rig.up, rig.bodies, exposeSvc, h, &ExposeHTTPOptions{MaxChunk: 1 << 10, ResponseBuffer: 1}); err != nil {
		t.Errorf("valid options: %v", err)
	}
	// A table on another runtime is refused up front.
	other := testRuntime(t)
	if otherBodies, err := NewHTTPBodies(other); err != nil {
		t.Fatal(err)
	} else {
		defer otherBodies.Close()
		if err := rig.ex.ExposeHTTP(rig.up, otherBodies, exposeSvc, h, nil); !errors.Is(err, ErrInvalidArgument) {
			t.Errorf("foreign runtime bodies: %v", err)
		}
	}

	// Body calls on unknown exchanges and unusable arguments.
	buf := make([]byte, 8)
	if _, _, err := rig.bodies.Read(99, buf, 0); !errors.Is(err, ErrBodyUnknownStream) {
		t.Errorf("Read unknown: %v", err)
	}
	var be *BodyError
	if err := rig.bodies.Write(99, []byte("x"), 0); !errors.Is(err, ErrBodyUnknownStream) || !errors.As(err, &be) || be.Code != 1 {
		t.Errorf("Write unknown: %v", err)
	}
	if _, err := rig.bodies.TryWrite(99, []byte("x")); !errors.Is(err, ErrBodyUnknownStream) {
		t.Errorf("TryWrite unknown: %v", err)
	}
	if err := rig.bodies.Finish(99, nil); !errors.Is(err, ErrBodyUnknownStream) {
		t.Errorf("Finish unknown: %v", err)
	}
	if err := rig.bodies.Abort(99, "x"); !errors.Is(err, ErrBodyUnknownStream) {
		t.Errorf("Abort unknown: %v", err)
	}
	if n, eof, err := rig.bodies.Read(99, nil, 0); n != 0 || eof || err != nil {
		t.Errorf("empty read buffer: %d %v %v", n, eof, err)
	}

	// relay_request argument validation.
	if _, err := rig.conn.RelayRequest(rig.ctx, HTTPRelayRequest{Method: "GET", URI: "/"}); !errors.Is(err, ErrInvalidArgument) {
		t.Errorf("relay without a service id: %v", err)
	}
	if _, err := rig.conn.RelayRequest(rig.ctx, HTTPRelayRequest{ServiceID: "x", URI: "/"}); err == nil {
		t.Error("relay without a method succeeded")
	}

	// Serve twice is refused; Close stops everything and is idempotent.
	if err := rig.ex.Serve(exposeALPN); !errors.Is(err, ErrInvalidArgument) {
		t.Errorf("second Serve: %v", err)
	}
	if err := rig.ex.Close(); err != nil {
		t.Fatal(err)
	}
	if err := rig.ex.Close(); err != nil {
		t.Fatal(err)
	}
	select {
	case <-rig.ex.Done():
	case <-time.After(time.Second):
		t.Fatal("Done never closed after Close")
	}
	if err := rig.ex.Serve(exposeALPN); !errors.Is(err, ErrAlreadyClosed) {
		t.Errorf("Serve after Close: %v", err)
	}
}

// Stress: many concurrent exchanges, inline and streamed, all answered
// exactly once with no exchange or goroutine left over (-race, -count=20).
func TestExpose_ConcurrentExchanges(t *testing.T) {
	rig := newExposeRig(t, true)
	rig.serveHTTP(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("stream") == "1" {
			w.(http.Flusher).Flush()
		}
		io.Copy(w, r.Body)
	}, &ExposeHTTPOptions{ResponseBuffer: 2, MaxChunk: 32 << 10})
	var wg sync.WaitGroup
	for i := 0; i < 24; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			size := 100 + i*9000
			body := pattern(size)
			uri := "/e"
			if i%2 == 0 {
				uri = "/e?stream=1"
			}
			resp, err := rig.relay(HTTPRelayRequest{Method: "POST", URI: uri, Body: body})
			if err != nil || resp.Status != 200 || !bytes.Equal(resp.Body, body) {
				t.Errorf("exchange %d: %v (status %v, %d bytes, want %d)", i, err, resp, func() int {
					if resp == nil {
						return 0
					}
					return len(resp.Body)
				}(), size)
			}
		}(i)
	}
	wg.Wait()
	rig.waitClosed()
}
