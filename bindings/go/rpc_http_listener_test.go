//go:build cgo

package aster

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"errors"
	"io"
	"iter"
	"math/big"
	"net"
	"net/http"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// Cancel at the post-start check, after the native listener has been created.
// This exercises cleanup deterministically without depending on bind timing.
type cancelAfterHTTPStart struct {
	context.Context
	cancel        context.CancelFunc
	checks        atomic.Int32
	boundAddr     string
	observedBound atomic.Bool
}

func (c *cancelAfterHTTPStart) Err() error {
	if c.checks.Add(1) == 3 {
		if socket, err := net.Listen("tcp", c.boundAddr); err != nil {
			c.observedBound.Store(true)
		} else {
			_ = socket.Close()
		}
		c.cancel()
	}
	return c.Context.Err()
}

func httpListenerPEM(t *testing.T) HTTPListenerConfig {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	cert := &x509.Certificate{SerialNumber: big.NewInt(1), Subject: pkix.Name{CommonName: "localhost"},
		NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(24 * time.Hour),
		KeyUsage: x509.KeyUsageDigitalSignature, ExtKeyUsage: []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
		IPAddresses: []net.IP{net.ParseIP("127.0.0.1")}}
	der, err := x509.CreateCertificate(rand.Reader, cert, cert, &key.PublicKey, key)
	if err != nil {
		t.Fatal(err)
	}
	secret, err := x509.MarshalPKCS8PrivateKey(key)
	if err != nil {
		t.Fatal(err)
	}
	return HTTPListenerConfig{BindAddr: "127.0.0.1:0", CertificatePEM: pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}), PrivateKeyPEM: pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: secret})}
}

func httpListenerFixture(t *testing.T, handler func(context.Context, *testEchoRequest) (*testEchoReply, error)) (*AsterServer, *Upcalls) {
	return httpListenerServiceFixture(t, NewService("HTTPTest", 1, Unary("echo", handler, Requires(RequireRole("admin")))))
}

func httpListenerServiceFixture(t *testing.T, service *Service) (*AsterServer, *Upcalls) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	srv, err := StartServer(ctx, ServerOptions{Config: testCfg(), Services: []*Service{service}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = srv.Close() })
	up, err := NewUpcalls(srv.Node().Runtime(), 8)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = up.Close() })
	return srv, up
}

func httpListenerClient(t *testing.T) *http.Client {
	t.Helper()
	transport := &http.Transport{TLSClientConfig: &tls.Config{InsecureSkipVerify: true}}
	t.Cleanup(transport.CloseIdleConnections)
	return &http.Client{Transport: transport, Timeout: 5 * time.Second}
}

func httpListenerCall(client *http.Client, addr, token string, payload []byte) ([]byte, RpcStatus, error) {
	frames, status, err := httpListenerFramesCall(client, addr, token, "echo", payload)
	return bytes.Join(frames, nil), status, err
}

func httpListenerFramesCall(client *http.Client, addr, token, method string, payloads ...[]byte) ([][]byte, RpcStatus, error) {
	return httpListenerFramesCallHeaders(client, addr, token, method, nil, payloads...)
}

func httpListenerFramesCallHeaders(client *http.Client, addr, token, method string, headers map[string]string, payloads ...[]byte) ([][]byte, RpcStatus, error) {
	var requestBody bytes.Buffer
	for i, payload := range payloads {
		var flags byte
		if i == len(payloads)-1 {
			flags = FlagEndStream
		}
		requestBody.Write(EncodeFrame(payload, flags))
	}
	req, err := http.NewRequest(http.MethodPost, "https://"+addr+"/aster/HTTPTest/"+method, &requestBody)
	if err != nil {
		return nil, RpcStatus{}, err
	}
	if token != "" {
		req.Header.Set("Authorization", token)
	}
	req.Header.Set("Content-Type", "application/aster-frames")
	for key, value := range headers {
		req.Header.Set(key, value)
	}
	resp, err := client.Do(req)
	if err != nil {
		return nil, RpcStatus{}, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, RpcStatus{}, errors.New("unexpected HTTP status: " + resp.Status)
	}
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, RpcStatus{}, err
	}
	var result [][]byte
	for len(body) > 0 {
		payload, flags, rest, err := DecodeFrame(body)
		if err != nil {
			return nil, RpcStatus{}, err
		}
		body = rest
		if flags&FlagTrailer != 0 {
			if len(body) != 0 {
				return nil, RpcStatus{}, errors.New("unexpected data after RPC trailer")
			}
			status, err := DecodeRpcStatus(payload)
			return result, status, err
		}
		payload, err = PayloadOf(payload, flags)
		if err != nil {
			return nil, RpcStatus{}, err
		}
		result = append(result, payload)
	}
	return nil, RpcStatus{}, errors.New("HTTP RPC reply missing trailer")
}

func TestHTTPListener_AllRPCPatterns(t *testing.T) {
	for _, public := range []bool{false, true} {
		name := "authenticated"
		if public {
			name = "public"
		}
		t.Run(name, func(t *testing.T) { httpListenerAllRPCPatterns(t, public) })
	}
}

func httpListenerAllRPCPatterns(t *testing.T, public bool) {
	principal := func(ctx context.Context) string {
		info, ok := CallInfoFromContext(ctx)
		role := ""
		if ok {
			for _, attribute := range info.Attributes {
				if attribute.Key == "aster.role" {
					role = attribute.Value
				}
			}
		}
		if !ok || info.IrohAuthenticated || (!public && (info.Principal != "alice" || info.PeerID == info.Principal || role != "admin")) || (public && info.Principal != info.PeerID) || (public && len(info.Attributes) != 0) {
			t.Errorf("HTTP auth context: %+v", info)
			return "invalid"
		}
		if public {
			return "public"
		}
		return info.Principal
	}
	service := NewService("HTTPTest", 1,
		Unary("echo", func(ctx context.Context, r *testEchoRequest) (*testEchoReply, error) {
			return &testEchoReply{Message: r.Message, Count: r.Count + 1, Data: r.Data, ServerLang: principal(ctx)}, nil
		}),
		ServerStream("generate", func(ctx context.Context, r *testEchoRequest, send func(*testEchoReply) error) error {
			for i := int32(0); i < r.Count; i++ {
				if err := send(&testEchoReply{Count: i, Message: r.Message, Data: []byte{}, ServerLang: principal(ctx)}); err != nil {
					return err
				}
			}
			return nil
		}),
		ClientStream("ingest", func(ctx context.Context, requests iter.Seq2[*testEchoRequest, error]) (*testEchoReply, error) {
			var sum int32
			for r, err := range requests {
				if err != nil {
					return nil, err
				}
				sum += r.Count
			}
			return &testEchoReply{Count: sum, Data: []byte{}, ServerLang: principal(ctx)}, nil
		}),
		BidiStream("chat", func(ctx context.Context, requests iter.Seq2[*testEchoRequest, error], send func(*testEchoReply) error) error {
			for r, err := range requests {
				if err != nil {
					return err
				}
				if err := send(&testEchoReply{Message: r.Message + r.Message, Count: r.Count * 2, Data: []byte{}, ServerLang: principal(ctx)}); err != nil {
					return err
				}
			}
			return nil
		}),
		Unary("restricted", func(context.Context, *testEchoRequest) (*testEchoReply, error) {
			t.Error("public call passed Gate 3")
			return nil, nil
		}, Requires(RequireRole("admin"))),
	)
	if !public {
		service.WithRequires(RequireRole("admin"))
	}
	srv, up := httpListenerServiceFixture(t, service)
	var authCalls atomic.Int32
	if err := up.ServeRPCAuthenticator(RPCAuthenticatorFunc(func(_ context.Context, req RPCAuthRequest) (RPCAuthResponse, error) {
		authCalls.Add(1)
		if req.Metadata["authorization"] != "Bearer valid" {
			return RPCAuthResponse{}, nil
		}
		return RPCAuthResponse{Accepted: true, Principal: "alice", Attributes: map[string]string{"aster.role": "admin"}}, nil
	})); err != nil {
		t.Fatal(err)
	}
	generated, err := GenerateWebTransportCertificate("http-test", []string{"127.0.0.1"}, nil)
	if err != nil {
		t.Fatal(err)
	}
	cfg := HTTPListenerConfig{CertificatePEM: generated.CertificatePEM, PrivateKeyPEM: generated.PrivateKeyPEM, WebTransport: true}
	var l *HTTPListener
	if public {
		l, err = srv.StartPublicHTTP(context.Background(), cfg)
	} else {
		l, err = srv.StartHTTP(context.Background(), up, cfg)
	}
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = l.Close() })
	client := httpListenerClient(t)
	expectedPrincipal := "alice"
	token := "Bearer valid"
	if public {
		expectedPrincipal = "public"
		token = ""
		payload, err := srv.codec.Encode(&testEchoRequest{Data: []byte{}})
		if err != nil {
			t.Fatal(err)
		}
		_, status, err := httpListenerFramesCall(client, l.TCPAddr(), "Bearer valid", "restricted", payload)
		if err != nil || status.Code != StatusPermissionDenied {
			t.Fatalf("public Gate3: %+v %v", status, err)
		}
	}
	for _, tc := range []struct {
		method   string
		requests []*testEchoRequest
		counts   []int32
		messages []string
	}{
		{"echo", []*testEchoRequest{{Message: "hello", Count: 41, Data: []byte{1, 2, 255}}}, []int32{42}, []string{"hello"}},
		{"generate", []*testEchoRequest{{Message: "stream", Count: 3, Data: []byte{}}}, []int32{0, 1, 2}, []string{"stream", "stream", "stream"}},
		{"ingest", []*testEchoRequest{{Count: 4, Data: []byte{}}, {Count: 5, Data: []byte{}}}, []int32{9}, []string{""}},
		{"chat", []*testEchoRequest{{Message: "first", Count: 2, Data: []byte{}}, {Message: "second", Count: 3, Data: []byte{}}}, []int32{4, 6}, []string{"firstfirst", "secondsecond"}},
	} {
		t.Run(tc.method, func(t *testing.T) {
			var payloads [][]byte
			for _, request := range tc.requests {
				payload, err := srv.codec.Encode(request)
				if err != nil {
					t.Fatal(err)
				}
				payloads = append(payloads, payload)
			}
			frames, status, err := httpListenerFramesCall(client, l.TCPAddr(), token, tc.method, payloads...)
			if err != nil || status.Code != StatusOK || len(frames) != len(tc.counts) {
				t.Fatalf("frames=%d status=%+v err=%v", len(frames), status, err)
			}
			for i, payload := range frames {
				var reply testEchoReply
				if err := srv.codec.Decode(payload, &reply); err != nil {
					t.Fatal(err)
				}
				if reply.Count != tc.counts[i] || reply.Message != tc.messages[i] || reply.ServerLang != expectedPrincipal {
					t.Fatalf("reply %d: %+v", i, reply)
				}
				if tc.method == "echo" && !bytes.Equal(reply.Data, []byte{1, 2, 255}) {
					t.Fatalf("binary data: %v", reply.Data)
				}
			}
		})
	}
	if public && authCalls.Load() != 0 {
		t.Fatal("public mode invoked an authenticator")
	}
	if err := l.Close(); err != nil {
		t.Fatal(err)
	}
	assertHTTPPortsReleased(t, l.TCPAddr(), true)
}

func assertHTTPPortsReleased(t *testing.T, addr string, quic bool) {
	t.Helper()
	tcp, err := net.Listen("tcp", addr)
	if err != nil {
		t.Fatalf("TCP still owned: %v", err)
	}
	defer tcp.Close()
	if quic {
		udp, err := net.ListenPacket("udp", addr)
		if err != nil {
			t.Fatalf("UDP still owned: %v", err)
		}
		defer udp.Close()
	}
}

func hasEntry(entries []MetadataEntry, key, value string) bool {
	for _, e := range entries {
		if e.Key == key && e.Value == value {
			return true
		}
	}
	return false
}

func TestHTTPListener_AuthenticatedHTTPSAndOwnership(t *testing.T) {
	seen := make(chan *CallInfo, 1)
	srv, up := httpListenerFixture(t, func(ctx context.Context, req *testEchoRequest) (*testEchoReply, error) {
		info, _ := CallInfoFromContext(ctx)
		seen <- info
		return &testEchoReply{Message: req.Message, Count: req.Count + 1, Data: req.Data, ServerLang: "go"}, nil
	})
	if err := up.ServeRPCAuthenticator(RPCAuthenticatorFunc(func(_ context.Context, req RPCAuthRequest) (RPCAuthResponse, error) {
		if req.Service != "HTTPTest" || req.Method != "echo" {
			return RPCAuthResponse{}, errors.New("wrong routed auth context")
		}
		switch req.Metadata["authorization"] {
		case "Bearer valid":
			return RPCAuthResponse{Accepted: true, Principal: "alice", Attributes: map[string]string{"aster.role": "admin"}}, nil
		case "Bearer bob":
			return RPCAuthResponse{Accepted: true, Principal: "bob", Attributes: map[string]string{"aster.role": "admin", "call": "bob"}}, nil
		case "Bearer low":
			return RPCAuthResponse{Accepted: true, Principal: "alice"}, nil
		case "Bearer backend":
			return RPCAuthResponse{}, errors.New("secret backend credential")
		default:
			return RPCAuthResponse{}, nil
		}
	})); err != nil {
		t.Fatal(err)
	}
	if err := up.SetPolicy(UpcallRpcAuthenticate, time.Second, FailureModeOpen); err != nil {
		t.Fatal(err)
	}
	cfg := httpListenerPEM(t)
	cfg.WebTransport = true
	l, err := srv.StartHTTP(context.Background(), up, cfg)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = l.Close() })
	if l.TCPAddr() == "" || l.QUICAddr() != l.TCPAddr() {
		t.Fatalf("readiness addresses TCP=%q QUIC=%q", l.TCPAddr(), l.QUICAddr())
	}
	if udp, err := net.ListenPacket("udp", l.QUICAddr()); err == nil {
		udp.Close()
		t.Fatal("QUIC port was not bound at readiness")
	}
	client := httpListenerClient(t)
	payload, err := srv.codec.Encode(&testEchoRequest{Message: "https", Count: 41, Data: []byte{1, 2, 255}})
	if err != nil {
		t.Fatal(err)
	}
	for _, tc := range []struct {
		token string
		code  StatusCode
	}{{"", StatusUnauthenticated}, {"Bearer low", StatusPermissionDenied}, {"Bearer backend", StatusUnavailable}} {
		_, status, err := httpListenerCall(client, l.TCPAddr(), tc.token, payload)
		if err != nil || status.Code != tc.code {
			t.Fatalf("credential %q: status=%+v err=%v", tc.token, status, err)
		}
		select {
		case info := <-seen:
			t.Fatalf("denied call reached handler: %+v", info)
		default:
		}
	}
	frames, status, err := httpListenerFramesCallHeaders(client, l.TCPAddr(), "Bearer valid", "echo", map[string]string{"x-request-tag": "t1"}, payload)
	if err != nil || status.Code != StatusOK {
		t.Fatalf("accepted: status=%+v err=%v", status, err)
	}
	response := bytes.Join(frames, nil)
	var reply testEchoReply
	if err := srv.codec.Decode(response, &reply); err != nil || reply.Message != "https" || reply.Count != 42 || !bytes.Equal(reply.Data, []byte{1, 2, 255}) {
		t.Fatalf("response %+v, %v", reply, err)
	}
	info := <-seen
	if info.Principal != "alice" || info.IrohAuthenticated {
		t.Fatalf("resolved principal/provenance: %+v", info)
	}
	// ABI 7.5: the transport peer (the client's address over HTTP) stays
	// apart from the Authenticator's principal.
	if !strings.Contains(info.PeerID, "127.0.0.1:") {
		t.Fatalf("transport peer %q (principal %q)", info.PeerID, info.Principal)
	}
	if info.CallID == 0 || info.Service != "HTTPTest" || info.Method != "echo" || info.Deadline.IsZero() {
		t.Fatalf("call identity/deadline: %+v", info)
	}
	if !hasEntry(info.Attributes, "aster.role", "admin") || !hasEntry(info.Metadata, "x-request-tag", "t1") {
		t.Fatalf("attributes/metadata: %+v", info)
	}
	_, status, err = httpListenerCall(client, l.TCPAddr(), "Bearer bob", payload)
	if err != nil || status.Code != StatusOK {
		t.Fatalf("second principal: status=%+v err=%v", status, err)
	}
	info = <-seen
	if info.Principal != "bob" || info.PeerID == "bob" || info.IrohAuthenticated {
		t.Fatalf("HTTP connection reused another principal: %+v", info)
	}
	if err := l.Stop(10 * time.Millisecond); err != nil {
		t.Fatal(err)
	}
	if err := l.Close(); err != nil {
		t.Fatal(err)
	}
	assertHTTPPortsReleased(t, l.TCPAddr(), true)
}

func TestHTTPListener_StartupValidationAndPartialBindCleanup(t *testing.T) {
	srv, up := httpListenerFixture(t, func(context.Context, *testEchoRequest) (*testEchoReply, error) {
		t.Error("unexpected handler")
		return nil, nil
	})
	cfg := httpListenerPEM(t)
	if _, err := srv.StartHTTP(context.Background(), up, cfg); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("missing auth: %v", err)
	}
	// Public mode refuses authentication limits and a cancelled start before
	// anything binds. (Its gates and session refusal are ffi/tests/http_server.rs
	// public_start_is_explicit_and_does_not_bypass_method_permissions and
	// aster-transport-salvo/tests/public_listener.rs.)
	for _, limits := range []HTTPListenerConfig{{MaxPendingAuth: 1}, {MaxAuthPayloadBytes: 1}} {
		limits.CertificatePEM, limits.PrivateKeyPEM = cfg.CertificatePEM, cfg.PrivateKeyPEM
		if l, err := srv.StartPublicHTTP(context.Background(), limits); l != nil || !errors.Is(err, ErrInvalidArgument) {
			t.Fatalf("public auth limits: listener=%v err=%v", l, err)
		}
	}
	if err := up.ServeRPCAuthenticator(RPCAuthenticatorFunc(func(context.Context, RPCAuthRequest) (RPCAuthResponse, error) { return RPCAuthResponse{}, nil })); err != nil {
		t.Fatal(err)
	}
	canceled, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := srv.StartHTTP(canceled, up, cfg); !errors.Is(err, context.Canceled) {
		t.Fatalf("canceled startup: %v", err)
	}
	if l, err := srv.StartPublicHTTP(canceled, cfg); l != nil || !errors.Is(err, context.Canceled) {
		t.Fatalf("public canceled start: %v %v", l, err)
	}
	bad := cfg
	bad.PrivateKeyPEM = []byte("invalid key")
	if l, err := srv.StartHTTP(context.Background(), up, bad); err == nil {
		_ = l.Close()
		t.Fatal("invalid PEM accepted")
	}
	udp, err := net.ListenPacket("udp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer udp.Close()
	cfg.BindAddr = udp.LocalAddr().String()
	cfg.WebTransport = true
	if l, err := srv.StartHTTP(context.Background(), up, cfg); err == nil {
		_ = l.Close()
		t.Fatal("occupied UDP accepted")
	}
	assertHTTPPortsReleased(t, cfg.BindAddr, false)
	if err := udp.Close(); err != nil {
		t.Fatal(err)
	}
	parent, cancel := context.WithCancel(context.Background())
	defer cancel()
	late := &cancelAfterHTTPStart{Context: parent, cancel: cancel, boundAddr: cfg.BindAddr}
	if l, err := srv.StartHTTP(late, up, cfg); !errors.Is(err, context.Canceled) || l != nil {
		if l != nil {
			_ = l.Close()
		}
		t.Fatalf("post-start cancellation owner=%v err=%v", l, err)
	}
	if !late.observedBound.Load() {
		t.Fatal("cancellation did not race a bound native listener")
	}
	assertHTTPPortsReleased(t, cfg.BindAddr, true)
}

func TestHTTPListener_StopCancelsPendingAuthentication(t *testing.T) {
	srv, up := httpListenerFixture(t, func(context.Context, *testEchoRequest) (*testEchoReply, error) {
		t.Error("unauthenticated handler")
		return nil, nil
	})
	entered := make(chan struct{})
	canceled := make(chan struct{})
	if err := up.ServeRPCAuthenticator(RPCAuthenticatorFunc(func(ctx context.Context, _ RPCAuthRequest) (RPCAuthResponse, error) {
		close(entered)
		<-ctx.Done()
		close(canceled)
		return RPCAuthResponse{}, ctx.Err()
	})); err != nil {
		t.Fatal(err)
	}
	cfg := httpListenerPEM(t)
	cfg.MaxPendingAuth = 1
	l, err := srv.StartHTTP(context.Background(), up, cfg)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = l.Close() })
	client := httpListenerClient(t)
	done := make(chan struct{})
	go func() {
		defer close(done)
		_, _, _ = httpListenerCall(client, l.TCPAddr(), "Bearer pending", []byte{1})
	}()
	select {
	case <-entered:
	case <-time.After(waitBudget()):
		t.Fatal("authentication never started")
	}
	if _, status, err := httpListenerCall(client, l.TCPAddr(), "Bearer overflow", []byte{1}); err != nil || status.Code != StatusResourceExhausted {
		t.Fatalf("pending-auth limit: status=%+v err=%v", status, err)
	}
	if err := l.Close(); err != nil {
		t.Fatal(err)
	}
	select {
	case <-canceled:
	case <-time.After(waitBudget()):
		t.Fatal("stop did not cancel authentication")
	}
	select {
	case <-done:
	case <-time.After(waitBudget()):
		t.Fatal("stopped request did not finish")
	}
	assertHTTPPortsReleased(t, l.TCPAddr(), false)
}

func TestHTTPListener_AuthPayloadLimitBeforeCallback(t *testing.T) {
	srv, up := httpListenerFixture(t, func(context.Context, *testEchoRequest) (*testEchoReply, error) {
		t.Error("oversized authentication reached handler")
		return nil, nil
	})
	if err := up.ServeRPCAuthenticator(RPCAuthenticatorFunc(func(context.Context, RPCAuthRequest) (RPCAuthResponse, error) {
		t.Error("oversized authentication reached callback")
		return RPCAuthResponse{}, nil
	})); err != nil {
		t.Fatal(err)
	}
	cfg := httpListenerPEM(t)
	cfg.MaxAuthPayloadBytes = 1
	l, err := srv.StartHTTP(context.Background(), up, cfg)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = l.Close() })
	if l.QUICAddr() != "" {
		t.Fatalf("disabled QUIC address %q", l.QUICAddr())
	}
	if _, status, err := httpListenerCall(httpListenerClient(t), l.TCPAddr(), "Bearer oversized", []byte{1}); err != nil || status.Code != StatusResourceExhausted {
		t.Fatalf("auth payload limit: status=%+v err=%v", status, err)
	}
}

func TestHTTPListener_GraceThenCancelsGoHandler(t *testing.T) {
	entered := make(chan struct{})
	canceled := make(chan struct{})
	srv, up := httpListenerFixture(t, func(ctx context.Context, _ *testEchoRequest) (*testEchoReply, error) {
		close(entered)
		<-ctx.Done()
		close(canceled)
		return nil, ctx.Err()
	})
	if err := up.ServeRPCAuthenticator(RPCAuthenticatorFunc(func(context.Context, RPCAuthRequest) (RPCAuthResponse, error) {
		return RPCAuthResponse{Accepted: true, Principal: "alice", Attributes: map[string]string{"aster.role": "admin"}}, nil
	})); err != nil {
		t.Fatal(err)
	}
	l, err := srv.StartHTTP(context.Background(), up, httpListenerPEM(t))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = l.Close() })
	payload, err := srv.codec.Encode(&testEchoRequest{Data: []byte{}})
	if err != nil {
		t.Fatal(err)
	}
	client := httpListenerClient(t)
	done := make(chan struct{})
	go func() { defer close(done); _, _, _ = httpListenerCall(client, l.TCPAddr(), "Bearer valid", payload) }()
	select {
	case <-entered:
	case <-time.After(waitBudget()):
		t.Fatal("Go handler never started")
	}
	begin := time.Now()
	if err := l.Stop(30 * time.Millisecond); err != nil {
		t.Fatal(err)
	}
	if time.Since(begin) < 20*time.Millisecond {
		t.Fatal("listener skipped the requested drain grace")
	}
	select {
	case <-canceled:
	case <-time.After(waitBudget()):
		t.Fatal("listener stop did not cancel Go handler context")
	}
	select {
	case <-done:
	case <-time.After(waitBudget()):
		t.Fatal("stopped handler request did not finish")
	}
	if calls := srv.LiveCalls(); calls != 0 {
		t.Fatalf("native call owner remained after stop: %d", calls)
	}
	assertHTTPPortsReleased(t, l.TCPAddr(), false)
}
