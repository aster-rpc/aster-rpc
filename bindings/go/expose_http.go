//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/textproto"
	"net/url"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
)

// HTTP handlers in Go behind expose (C ABI 4.2, kinds 48/49): a net/http
// Handler serves the requests peers relay to a service. One kind-49 upcall
// carries each request head; bodies never travel over upcalls: a request
// body is inline (small, declared length) or pulled through HTTPBodies.Read,
// a response body is inline (small, finished before any Flush) or streamed
// with HTTPBodies.Write after the head is answered. Design:
// docs/_internal/bindings/upcall-design.md section 6.

const (
	// inlineResponseMax is the largest response answered inline in the C5
	// head; a bigger or flushed response streams.
	inlineResponseMax = 64 << 10
	// bodyPollSlice is how long one blocking body call waits before it
	// re-checks the request context.
	bodyPollSlice = 100 * time.Millisecond
	defaultChunk  = 1 << 20
)

// ExposeInfo describes the exchange a handler is serving; see ExposeInfoFrom.
type ExposeInfo struct {
	ServiceID string
	// PeerID is the transport-authenticated peer (hex node id); HasPeer is
	// false when the request did not arrive over the relay.
	PeerID   string
	HasPeer  bool
	StreamID uint64
}

type exposeInfoKey struct{}

// ExposeInfoFrom returns the exchange a handler's request context belongs to.
func ExposeInfoFrom(ctx context.Context) (ExposeInfo, bool) {
	i, ok := ctx.Value(exposeInfoKey{}).(ExposeInfo)
	return i, ok
}

// ExposeHTTP exposes serviceID with both halves in Go: admission is an
// expose-authorize decision on up (serve it with Upcalls.ServeExposeAuthorize;
// without a handler every peer is refused: fail-closed) and each request is
// one kind-49 upcall served by h. The request is a net/http Request:
// Body streams from the peer (inline when small), RemoteAddr is the peer id,
// the context is cancelled when the peer or Rust gives up before the
// response head. The ResponseWriter buffers a small response into one
// round trip; Flush, a large body or a declared Trailer switches to a
// streamed body with backpressure. A panic answers 500 (before the head) or
// aborts the stream (after it); http.ErrAbortHandler aborts quietly.
//
// The kind-49 timeout (Upcalls.SetPolicy, default 5 s) bounds the time to the
// response head only; long-polling handlers that answer late need a longer
// one. opts nil = defaults. Re-registering an id replaces it. up, bodies and
// the registry must share one runtime.
func (e *Expose) ExposeHTTP(up *Upcalls, bodies *HTTPBodies, serviceID string, h http.Handler, opts *ExposeHTTPOptions) error {
	if up == nil || bodies == nil || h == nil {
		return fmt.Errorf("expose http: nil upcalls, bodies or handler: %w", ErrInvalidArgument)
	}
	if up.rt != e.rt || bodies.rt != e.rt {
		return fmt.Errorf("expose http: upcalls, bodies and registry must share a runtime: %w", ErrInvalidArgument)
	}
	var o C.aster_http_foreign_options_t
	if err := wrapCall("aster_http_foreign_options_init", func() C.int32_t { return C.aster_http_foreign_options_init(&o) }); err != nil {
		return err
	}
	svc := &httpService{bodies: bodies, handler: h, maxChunk: defaultChunk}
	if opts != nil {
		o.inline_request_max = C.uint64_t(opts.InlineRequestMax)
		o.max_request_body = C.uint64_t(opts.MaxRequestBody)
		o.max_response_body = C.uint64_t(opts.MaxResponseBody)
		o.max_chunk = C.uint64_t(opts.MaxChunk)
		o.response_buffer = C.uint32_t(opts.ResponseBuffer)
		if opts.MaxChunk != 0 {
			svc.maxChunk = int(opts.MaxChunk)
		}
	}
	// Go side first, so no request can arrive unhandled.
	if err := up.addHTTPService(serviceID, svc); err != nil {
		return err
	}
	var a cArena
	defer a.free()
	if err := wrapCall("aster_expose_http_foreign", func() C.int32_t {
		return C.aster_expose_http_foreign(e.rtH(), C.uint64_t(e.handle),
			C.uint64_t(up.handle), C.uint64_t(bodies.handle), a.str(serviceID), &o)
	}); err != nil {
		up.removeHTTPService(serviceID)
		return err
	}
	e.mu.Lock()
	if old := e.http[serviceID]; old != nil && old != up {
		old.removeHTTPService(serviceID)
	}
	e.http[serviceID] = up
	e.mu.Unlock()
	return nil
}

// httpService is the Go half of one exposed HTTP service.
type httpService struct {
	bodies   *HTTPBodies
	handler  http.Handler
	maxChunk int
}

type httpRegistry struct {
	mu       sync.RWMutex
	services map[string]*httpService
}

func (u *Upcalls) addHTTPService(id string, svc *httpService) error {
	u.http.mu.Lock()
	first := len(u.http.services) == 0
	if u.http.services == nil {
		u.http.services = map[string]*httpService{}
	}
	u.http.services[id] = svc
	u.http.mu.Unlock()
	if first {
		return u.HandleKind(UpcallExposeHTTP, u.serveHTTPUpcall)
	}
	return nil
}

func (u *Upcalls) removeHTTPService(id string) {
	u.http.mu.Lock()
	delete(u.http.services, id)
	u.http.mu.Unlock()
}

func (u *Upcalls) httpService(id string) *httpService {
	u.http.mu.RLock()
	defer u.http.mu.RUnlock()
	return u.http.services[id]
}

// serveHTTPUpcall is the kind-49 handler of the table: it routes by service id.
func (u *Upcalls) serveHTTPUpcall(ctx context.Context, req *UpcallRequest) ([]byte, error) {
	r, err := DecodeExposeHTTPRequest(req.Payload)
	if err != nil {
		return nil, err
	}
	svc := u.httpService(r.ServiceID)
	if svc == nil {
		return nil, ErrUpcallUnimplemented
	}
	return nil, svc.serve(ctx, req, r)
}

var httpVersions = [...]struct {
	proto        string
	major, minor int
}{{"HTTP/0.9", 0, 9}, {"HTTP/1.0", 1, 0}, {"HTTP/1.1", 1, 1}, {"HTTP/2.0", 2, 0}, {"HTTP/3.0", 3, 0}}

// serve runs the handler for one request. It answers the upcall itself (the
// head, inline or streamed); a returned error is for the generic ERROR path
// and only used when nothing was answered.
func (s *httpService) serve(ctx context.Context, req *UpcallRequest, r ExposeHTTPRequest) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	ctx = context.WithValue(ctx, exposeInfoKey{}, ExposeInfo{
		ServiceID: r.ServiceID, PeerID: r.PeerID, HasPeer: r.HasPeer, StreamID: r.StreamID,
	})
	w := &exposeWriter{svc: s, req: req, ctx: ctx, cancel: cancel, streamID: r.StreamID,
		head: r.Method == http.MethodHead, hdr: http.Header{}}

	u, err := url.ParseRequestURI(r.URI)
	if err != nil {
		w.WriteHeader(http.StatusBadRequest)
		return w.finish()
	}
	v := httpVersions[r.Version]
	hr := &http.Request{
		Method: r.Method, URL: u, RequestURI: r.URI, Proto: v.proto, ProtoMajor: v.major, ProtoMinor: v.minor,
		Header: http.Header{}, ContentLength: -1, RemoteAddr: r.PeerID,
	}
	for _, h := range r.Headers {
		if h.Name == "host" {
			hr.Host = string(h.Value)
			continue
		}
		key := textproto.CanonicalMIMEHeaderKey(h.Name)
		hr.Header[key] = append(hr.Header[key], string(h.Value))
	}
	if cl := hr.Header.Get("Content-Length"); cl != "" {
		if n, err := strconv.ParseInt(cl, 10, 64); err == nil && n >= 0 {
			hr.ContentLength = n
		}
	}
	if r.Body.Stream {
		hr.Body = &exposeBody{bodies: s.bodies, id: r.StreamID, ctx: ctx, cancel: cancel}
	} else {
		hr.ContentLength = int64(len(r.Body.Inline))
		hr.Body = io.NopCloser(bytes.NewReader(r.Body.Inline))
	}
	hr = hr.WithContext(ctx)

	var pv any
	func() {
		defer func() { pv = recover() }()
		s.handler.ServeHTTP(w, hr)
	}()
	if pv != nil {
		return w.fail(pv)
	}
	return w.finish()
}

// exposeBody is the streamed request body: pull-based reads through the
// body table, cancelled with the request.
type exposeBody struct {
	bodies *HTTPBodies
	id     uint64
	ctx    context.Context
	cancel context.CancelFunc
	err    error
}

func (b *exposeBody) Read(p []byte) (int, error) {
	if b.err != nil {
		return 0, b.err
	}
	if len(p) == 0 {
		return 0, nil
	}
	for {
		if err := b.ctx.Err(); err != nil {
			b.err = err
			return 0, err
		}
		n, eof, err := b.bodies.Read(b.id, p, bodyPollSlice)
		switch {
		case errors.Is(err, ErrBodyTimeout):
			continue
		case err != nil:
			if errors.Is(err, ErrBodyCancelled) {
				b.cancel() // the peer is gone: cancel the request context too
			}
			b.err = err
			return 0, err
		case n > 0:
			return n, nil
		case eof:
			b.err = io.EOF
			return 0, io.EOF
		}
	}
}

func (b *exposeBody) Close() error { return nil }

// errHeadNotAccepted: Rust no longer waited for the response head (timeout
// or the request was abandoned).
var errHeadNotAccepted = errors.New("expose http: response head not accepted (timed out or abandoned)")

// exposeWriter is the http.ResponseWriter of one exchange.
type exposeWriter struct {
	svc      *httpService
	req      *UpcallRequest
	ctx      context.Context
	cancel   context.CancelFunc
	streamID uint64
	head     bool // HEAD request: no body

	mu        sync.Mutex
	hdr       http.Header
	status    int
	wrote     bool        // WriteHeader called
	snapshot  http.Header // header at WriteHeader time
	buf       bytes.Buffer
	answered  bool // head answered (inline or streamed)
	streaming bool
	err       error
	done      bool
}

func (w *exposeWriter) Header() http.Header { return w.hdr }

func bodyAllowed(status int) bool {
	return !(status >= 100 && status <= 199) && status != 204 && status != 304
}

func (w *exposeWriter) WriteHeader(code int) {
	w.mu.Lock()
	defer w.mu.Unlock()
	w.writeHeaderLocked(code)
}

func (w *exposeWriter) writeHeaderLocked(code int) {
	if w.wrote || w.done {
		return
	}
	if code >= 100 && code <= 199 {
		return // interim responses are not carried over the relay
	}
	if code < 100 || code > 999 {
		panic(fmt.Sprintf("invalid WriteHeader code %v", code))
	}
	w.wrote, w.status = true, code
	w.snapshot = w.hdr.Clone()
}

func (w *exposeWriter) Write(p []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.done {
		return 0, http.ErrHandlerTimeout
	}
	w.writeHeaderLocked(http.StatusOK)
	if w.err != nil {
		return 0, w.err
	}
	if !bodyAllowed(w.status) {
		return 0, http.ErrBodyNotAllowed
	}
	if w.head {
		return len(p), nil
	}
	if !w.streaming {
		if w.buf.Len()+len(p) <= inlineResponseMax {
			w.buf.Write(p)
			return len(p), nil
		}
		if err := w.startStream(p); err != nil {
			return 0, err
		}
	}
	if err := w.writeChunks(p); err != nil {
		return 0, err
	}
	return len(p), nil
}

// Flush starts streaming the response: the head goes out now and later
// writes are chunks with backpressure.
func (w *exposeWriter) Flush() { _ = w.FlushError() }

// FlushError is Flush with an error (http.ResponseController).
func (w *exposeWriter) FlushError() error {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.done {
		return http.ErrHandlerTimeout
	}
	w.writeHeaderLocked(http.StatusOK)
	if w.err != nil {
		return w.err
	}
	if w.head || !bodyAllowed(w.status) || w.streaming {
		return nil
	}
	return w.startStream(nil)
}

// startStream answers the head with a streamed body and queues what is
// buffered. next is data about to be written (for content-type sniffing).
func (w *exposeWriter) startStream(next []byte) error {
	h := w.snapshot
	w.sniff(h, append(append([]byte(nil), w.buf.Bytes()...), next...))
	if !w.req.Respond(UpcallStatusOK, EncodeExposeHTTPResponse(ExposeHTTPResponse{
		Status: uint16(w.status), Headers: headerList(h), Body: HTTPBody{Stream: true},
	})) {
		w.err = errHeadNotAccepted
		w.cancel()
		return w.err
	}
	w.answered, w.streaming = true, true
	if w.buf.Len() > 0 {
		pending := append([]byte(nil), w.buf.Bytes()...)
		w.buf.Reset()
		return w.writeChunks(pending)
	}
	return nil
}

// sniff sets Content-Type from the first bytes when the handler set none.
func (w *exposeWriter) sniff(h http.Header, data []byte) {
	if len(data) == 0 || !bodyAllowed(w.status) || h.Get("Content-Type") != "" || h.Get("Content-Encoding") != "" {
		return
	}
	if _, set := h["Content-Type"]; set {
		return
	}
	if len(data) > 512 {
		data = data[:512]
	}
	h.Set("Content-Type", http.DetectContentType(data))
}

// writeChunks queues p as response chunks of at most max_chunk, waiting for
// room (backpressure) and for the peer: ErrBodyCancelled means it is gone.
func (w *exposeWriter) writeChunks(p []byte) error {
	for len(p) > 0 {
		n := len(p)
		if n > w.svc.maxChunk {
			n = w.svc.maxChunk
		}
		for {
			if err := w.ctx.Err(); err != nil {
				w.err = err
				return err
			}
			err := w.svc.bodies.Write(w.streamID, p[:n], bodyPollSlice)
			if err == nil {
				break
			}
			if errors.Is(err, ErrBodyTimeout) {
				continue
			}
			w.err = err
			if errors.Is(err, ErrBodyCancelled) {
				w.cancel()
			}
			return err
		}
		p = p[n:]
	}
	return nil
}

func headerList(h http.Header) []HTTPHeader {
	keys := make([]string, 0, len(h))
	for k := range h {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	var out []HTTPHeader
	for _, k := range keys {
		name := lowerASCII(k)
		for _, v := range h[k] {
			out = append(out, HTTPHeader{Name: name, Value: []byte(v)})
		}
	}
	return out
}

// trailers collects declared trailers (the Trailer header) and
// http.TrailerPrefix keys from the live header map.
func (w *exposeWriter) trailers() []HTTPHeader {
	var out []HTTPHeader
	for _, decl := range w.snapshot["Trailer"] {
		for _, name := range strings.Split(decl, ",") {
			name = textproto.CanonicalMIMEHeaderKey(strings.TrimSpace(name))
			if name == "" {
				continue
			}
			for _, v := range w.hdr[name] {
				out = append(out, HTTPHeader{Name: lowerASCII(name), Value: []byte(v)})
			}
		}
	}
	for k, vs := range w.hdr {
		if strings.HasPrefix(k, http.TrailerPrefix) {
			name := lowerASCII(strings.TrimPrefix(k, http.TrailerPrefix))
			if name == "" {
				continue
			}
			for _, v := range vs {
				out = append(out, HTTPHeader{Name: name, Value: []byte(v)})
			}
		}
	}
	return out
}

// finish completes the response after the handler returned.
func (w *exposeWriter) finish() error {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.done {
		return nil
	}
	w.done = true
	w.writeHeaderLocked(http.StatusOK)
	if w.err != nil && !w.answered {
		return w.err
	}
	if w.streaming {
		if w.err != nil {
			_ = w.svc.bodies.Abort(w.streamID, w.err.Error())
			return nil
		}
		_ = w.svc.bodies.Finish(w.streamID, w.trailers())
		return nil
	}
	if tr := w.trailers(); len(tr) > 0 && !w.head && bodyAllowed(w.status) {
		// Trailers need a streamed body.
		if err := w.startStream(nil); err != nil {
			return nil // head not accepted: nobody is waiting
		}
		_ = w.svc.bodies.Finish(w.streamID, tr)
		return nil
	}
	h := w.snapshot
	body := w.buf.Bytes()
	if w.head || !bodyAllowed(w.status) {
		body = nil
	} else {
		w.sniff(h, body)
		if h.Get("Content-Length") == "" && h.Get("Transfer-Encoding") == "" {
			h.Set("Content-Length", strconv.Itoa(len(body)))
		}
	}
	w.req.Respond(UpcallStatusOK, EncodeExposeHTTPResponse(ExposeHTTPResponse{
		Status: uint16(w.status), Headers: headerList(h), Body: HTTPBody{Inline: append([]byte(nil), body...)},
	}))
	w.answered = true
	return nil
}

// fail handles a handler panic: before the head the upcall answers ERROR
// (Rust maps it to 500 and never sends the message to the peer); after it
// the stream is aborted so the consumer sees a failed response, not a
// truncated one.
func (w *exposeWriter) fail(pv any) error {
	w.mu.Lock()
	defer w.mu.Unlock()
	w.done = true
	if w.streaming {
		_ = w.svc.bodies.Abort(w.streamID, fmt.Sprintf("handler panic: %v", pv))
		return nil
	}
	if pv == http.ErrAbortHandler {
		return errors.New("handler aborted")
	}
	return fmt.Errorf("handler panic: %v", pv)
}

// ─── consumer helpers ────────────────────────────────────────────────────

func (r *HTTPRelayResponse) toHTTP(req *http.Request) *http.Response {
	h := http.Header{}
	for _, hd := range r.Headers {
		key := textproto.CanonicalMIMEHeaderKey(hd.Name)
		h[key] = append(h[key], string(hd.Value))
	}
	return &http.Response{
		Status:     fmt.Sprintf("%d %s", r.Status, http.StatusText(int(r.Status))),
		StatusCode: int(r.Status), Proto: "HTTP/1.1", ProtoMajor: 1, ProtoMinor: 1,
		Header: h, Body: io.NopCloser(bytes.NewReader(r.Body)), ContentLength: int64(len(r.Body)), Request: req,
	}
}

func readAllLimited(r io.Reader, max int64) ([]byte, error) {
	b, err := io.ReadAll(io.LimitReader(r, max+1))
	if err != nil {
		return nil, err
	}
	if int64(len(b)) > max {
		return nil, fmt.Errorf("request body over %d bytes: %w", max, ErrInvalidArgument)
	}
	return b, nil
}
