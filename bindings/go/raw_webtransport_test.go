//go:build cgo

package aster

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"
)

func rawWTFixture(t *testing.T) (*Runtime, RawWTConfig) {
	t.Helper()
	rt, err := NewRuntime(context.Background(), DefaultRuntimeConfig())
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = rt.Close() })
	cert, err := GenerateWebTransportCertificate("raw-test", []string{"127.0.0.1"}, nil)
	if err != nil {
		t.Fatal(err)
	}
	return rt, RawWTConfig{BindAddr: "127.0.0.1:0", CertificatePEM: cert.CertificatePEM, PrivateKeyPEM: cert.PrivateKeyPEM, Path: "/raw"}
}
func rawWTPeer(t *testing.T, url, scenario string, args ...string) *exec.Cmd {
	t.Helper()
	root, err := filepath.Abs("../..")
	if err != nil {
		t.Fatal(err)
	}
	python := os.Getenv("ASTER_TEST_PYTHON")
	if python == "" {
		python = filepath.Join(root, ".venv", "bin", "python")
	}
	if _, err := os.Stat(python); err != nil && os.Getenv("ASTER_TEST_PYTHON") == "" {
		python, err = exec.LookPath("python3")
		if err != nil {
			t.Fatalf("raw WT peer Python unavailable: %v", err)
		}
	}
	if _, err := exec.LookPath(python); err != nil {
		t.Fatalf("raw WT network test needs Python+aioquic (ASTER_TEST_PYTHON): %v", err)
	}
	argv := append([]string{filepath.Join(root, "tests", "python", "raw_wt_peer.py"), "--url", url, "--scenario", scenario}, args...)
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	t.Cleanup(cancel)
	return exec.CommandContext(ctx, python, argv...)
}
func rawWTReadAll(ctx context.Context, s *RawWTStream) ([]byte, error) {
	var out []byte
	for {
		data, err := s.Read(ctx, 1024)
		out = append(out, data...)
		if errors.Is(err, io.EOF) {
			return out, nil
		}
		if err != nil {
			return out, err
		}
	}
}
func rawWTEcho(ctx context.Context, s *RawWTSession) error {
	bi, err := s.AcceptBi(ctx)
	if err != nil {
		return err
	}
	defer bi.Close()
	if !bi.CanSend || !bi.CanReceive {
		return fmt.Errorf("bidi directions")
	}
	data, err := rawWTReadAll(ctx, bi)
	if err != nil {
		return err
	}
	if !bytes.Equal(data, []byte("control")) {
		return fmt.Errorf("control bytes %q", data)
	}
	if err = bi.SetPriority(ctx, -10); err != nil {
		return err
	}
	p, some, err := bi.Priority(ctx)
	if err != nil {
		return err
	}
	if !some || p != -10 {
		return fmt.Errorf("signed priority %d/%t", p, some)
	}
	if err = bi.Write(ctx, []byte("CONTROL")); err != nil {
		return err
	}
	if err = bi.Finish(ctx); err != nil {
		return err
	}
	ack, err := bi.TransportAcked(ctx)
	if err != nil {
		return err
	}
	if !ack {
		return fmt.Errorf("control not transport ACKed")
	}
	uni, err := s.AcceptUni(ctx)
	if err != nil {
		return err
	}
	defer uni.Close()
	if uni.CanSend || !uni.CanReceive {
		return fmt.Errorf("receive uni directions")
	}
	data, err = rawWTReadAll(ctx, uni)
	if err != nil {
		return err
	}
	if string(data) != "input" {
		return fmt.Errorf("input bytes %q", data)
	}
	frame, err := s.OpenUni(ctx)
	if err != nil {
		return err
	}
	defer frame.Close()
	if !frame.CanSend || frame.CanReceive {
		return fmt.Errorf("send uni directions")
	}
	if err = frame.Write(ctx, []byte("frame")); err != nil {
		return err
	}
	if err = frame.Finish(ctx); err != nil {
		return err
	}
	ack, err = frame.TransportAcked(ctx)
	if err != nil {
		return err
	}
	if !ack {
		return fmt.Errorf("frame not ACKed")
	}
	data, err = s.ReceiveDatagram(ctx)
	if err != nil {
		return err
	}
	if string(data) != "feedback" {
		return fmt.Errorf("datagram %q", data)
	}
	size, some, err := s.MaxDatagramSize()
	if err != nil {
		return err
	}
	if !some || size == 0 {
		return fmt.Errorf("missing datagram size")
	}
	if err = s.SendDatagram([]byte("reply")); err != nil {
		return err
	}
	info, err := s.Info()
	if err != nil {
		return err
	}
	if info.Version != 1 || info.PeerAddress == "" {
		return fmt.Errorf("session info %#v", info)
	}
	stats, err := s.Stats()
	if err != nil {
		return err
	}
	if stats.Version != 1 || stats.UDPRxPackets == 0 || stats.UDPTxPackets == 0 {
		return fmt.Errorf("missing live stats %#v", stats)
	}
	return nil
}
func TestRawWT_PublicRealStreamsDatagramsPriorityAndACK(t *testing.T) {
	rt, cfg := rawWTFixture(t)
	ctx, cancel := context.WithTimeout(context.Background(), 25*time.Second)
	defer cancel()
	l, err := rt.StartPublicRawWT(ctx, cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	cmd := rawWTPeer(t, "https://"+l.QUICAddr()+cfg.Path, "echo")
	var output bytes.Buffer
	cmd.Stdout = &output
	cmd.Stderr = &output
	if err = cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if cmd.Process != nil {
			_ = cmd.Process.Kill()
		}
	})
	s, err := l.Accept(ctx)
	if err != nil {
		t.Fatalf("accept: %v; peer %s", err, &output)
	}
	defer s.Close()
	if err = rawWTEcho(ctx, s); err != nil {
		t.Fatalf("server: %v; peer %s", err, &output)
	}
	if err = cmd.Wait(); err != nil {
		t.Fatalf("peer: %v\n%s", err, &output)
	}
	var receipt struct {
		Result string `json:"result"`
	}
	if err = json.Unmarshal(bytes.TrimSpace(output.Bytes()), &receipt); err != nil || receipt.Result != "PASS" {
		t.Fatalf("peer receipt: %v %s", err, &output)
	}
	closed, err := s.Closed(ctx)
	if err != nil || closed.Version != 1 {
		t.Fatalf("closed: %#v %v", closed, err)
	}
}
func TestRawWT_CancelAcceptDoesNotConsumeNextSessionAndStopReleasesPorts(t *testing.T) {
	rt, cfg := rawWTFixture(t)
	l, err := rt.StartPublicRawWT(context.Background(), cfg)
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	defer cancel()
	if s, err := l.Accept(ctx); s != nil || !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("cancelled accept %v %v", s, err)
	}
	cmd := rawWTPeer(t, "https://"+l.QUICAddr()+cfg.Path, "connect")
	done := make(chan error, 1)
	go func() {
		out, err := cmd.CombinedOutput()
		if err != nil {
			err = fmt.Errorf("%w: %s", err, out)
		}
		done <- err
	}()
	next, cancelNext := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancelNext()
	s, err := l.Accept(next)
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	if err = <-done; err != nil {
		t.Fatal(err)
	}
	addr := l.TCPAddr()
	if err = l.Close(); err != nil {
		t.Fatal(err)
	}
	if err = l.Close(); err != nil {
		t.Fatal(err)
	}
	tcp, err := net.Listen("tcp", addr)
	if err != nil {
		t.Fatalf("TCP retained after stop: %v", err)
	}
	defer tcp.Close()
	udp, err := net.ListenPacket("udp", addr)
	if err != nil {
		t.Fatalf("UDP retained after stop: %v", err)
	}
	defer udp.Close()
}
func TestRawWT_AuthBeforeUpgradeAndExactRoute(t *testing.T) {
	rt, cfg := rawWTFixture(t)
	up, err := NewUpcalls(rt, 8)
	if err != nil {
		t.Fatal(err)
	}
	defer up.Close()
	seen := make(chan RPCAuthRequest, 8)
	if err = up.ServeRPCAuthenticator(RPCAuthenticatorFunc(func(_ context.Context, r RPCAuthRequest) (RPCAuthResponse, error) {
		seen <- r
		return RPCAuthResponse{Accepted: r.Metadata["authorization"] == "Bearer valid", Principal: "browser"}, nil
	})); err != nil {
		t.Fatal(err)
	}
	l, err := rt.StartRawWT(context.Background(), up, cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	for _, tc := range []struct{ path, token, status string }{{"/raw?cap=a&cap=b", "Bearer invalid", "401"}, {"/raw-extra", "Bearer valid", "404"}} {
		cmd := rawWTPeer(t, "https://"+l.QUICAddr()+tc.path, "connect", "--token", tc.token, "--expected-status", tc.status)
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("%s: %v %s", tc.path, err, out)
		}
	}
	select {
	case req := <-seen:
		if req.Metadata["aster.webtransport.path"] != "/raw" || req.Metadata["aster.webtransport.query"] != "cap=a&cap=b" || req.Service != "webtransport" || req.Method != "CONNECT" {
			t.Fatalf("auth metadata %#v", req)
		}
	case <-time.After(time.Second):
		t.Fatal("no auth")
	}
	select {
	case req := <-seen:
		t.Fatalf("wrong route authenticated: %#v", req)
	default:
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	defer cancel()
	if s, err := l.Accept(ctx); s != nil || !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("denied request created session %v %v", s, err)
	}
}
func TestRawWT_ConfigAndPrecancelValidation(t *testing.T) {
	rt, cfg := rawWTFixture(t)
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if l, err := rt.StartPublicRawWT(ctx, cfg); l != nil || !errors.Is(err, context.Canceled) {
		t.Fatalf("pre cancelled %v %v", l, err)
	}
	cfg.Path = "/aster/wt"
	if l, err := rt.StartPublicRawWT(context.Background(), cfg); l != nil || !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("reserved route %v %v", l, err)
	}
	cfg.Path = "/raw"
	cfg.MaxPendingAuth = 1
	if l, err := rt.StartPublicRawWT(context.Background(), cfg); l != nil || !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("public auth limit %v %v", l, err)
	}
	if l, err := rt.StartRawWT(context.Background(), nil, cfg); l != nil || !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("nil auth %v %v", l, err)
	}
}

func TestRawWT_PriorityPayloadStrictSignedOptional(t *testing.T) {
	for _, tc := range []struct {
		flags         uint32
		data          []byte
		value         int32
		some, invalid bool
	}{
		{0, nil, 0, false, false}, {1, []byte{246, 255, 255, 255}, -10, true, false},
		{1, []byte{20, 0, 0, 0}, 20, true, false}, {0, []byte{0, 0, 0, 0}, 0, false, true},
		{1, nil, 0, false, true}, {1, []byte{1, 0, 0}, 0, false, true},
		{1, []byte{1, 0, 0, 0, 0}, 0, false, true}, {2, []byte{1, 0, 0, 0}, 0, false, true},
	} {
		value, some, err := decodeRawWTPriority(tc.flags, tc.data)
		if (err != nil) != tc.invalid || !tc.invalid && (value != tc.value || some != tc.some) {
			t.Fatalf("flags%d data%x: %d %t %v", tc.flags, tc.data, value, some, err)
		}
	}
}

func TestRawWT_ReadCancellationResetServerBiAndCloseWake(t *testing.T) {
	rt, cfg := rawWTFixture(t)
	ctx, cancel := context.WithTimeout(context.Background(), 25*time.Second)
	defer cancel()
	l, err := rt.StartPublicRawWT(ctx, cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	cmd := rawWTPeer(t, "https://"+l.QUICAddr()+cfg.Path, "lifecycle")
	var output bytes.Buffer
	cmd.Stdout = &output
	cmd.Stderr = &output
	if err = cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if cmd.Process != nil {
			_ = cmd.Process.Kill()
		}
	})
	session, err := l.Accept(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer session.Close()
	pending, err := session.AcceptBi(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer pending.Close()
	data, err := pending.Read(ctx, 1024)
	if err != nil || string(data) != "pending" {
		t.Fatalf("initial read %q %v", data, err)
	}
	timeout, stop := context.WithTimeout(ctx, 20*time.Millisecond)
	defer stop()
	if data, err = pending.Read(timeout, 1024); len(data) != 0 || !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("cancelled read %q %v", data, err)
	}
	if err = session.SendDatagram([]byte("continue")); err != nil {
		t.Fatal(err)
	}
	data, err = rawWTReadAll(ctx, pending)
	if err != nil || string(data) != "-tail" {
		t.Fatalf("read after cancellation %q %v", data, err)
	}
	reset, err := session.AcceptBi(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer reset.Close()
	data, err = reset.Read(ctx, 1024)
	if err != nil || string(data) != "reset-me" {
		t.Fatalf("reset stream %q %v", data, err)
	}
	if err = reset.Reset(ctx, 7); err != nil {
		t.Fatal(err)
	}
	if err = reset.Stop(ctx, 9); err != nil {
		t.Fatal(err)
	}
	ack, err := reset.TransportAcked(ctx)
	if err != nil || ack {
		t.Fatalf("reset ACK=%t err=%v", ack, err)
	}
	serverBi, err := session.OpenBi(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer serverBi.Close()
	if err = serverBi.Write(ctx, []byte("server-bi")); err != nil {
		t.Fatal(err)
	}
	if err = serverBi.Finish(ctx); err != nil {
		t.Fatal(err)
	}
	data, err = rawWTReadAll(ctx, serverBi)
	if err != nil || string(data) != "SERVER-BI" {
		t.Fatalf("server bidi %q %v", data, err)
	}
	ack, err = serverBi.TransportAcked(ctx)
	if err != nil || !ack {
		t.Fatalf("server bidi ACK=%t %v", ack, err)
	}
	// Empty datagrams are messages, not stream EOF, and keep queue order.
	data, err = session.ReceiveDatagram(ctx)
	if err != nil || len(data) != 0 {
		t.Fatalf("empty datagram %q %v", data, err)
	}
	data, err = session.ReceiveDatagram(ctx)
	if err != nil || string(data) != "lifecycle" {
		t.Fatalf("next datagram %q %v", data, err)
	}
	max, some, err := session.MaxDatagramSize()
	if err != nil || !some {
		t.Fatalf("datagram limit %v %v", some, err)
	}
	if err = session.SendDatagram(make([]byte, max+1)); err == nil {
		t.Fatal("oversized datagram accepted")
	}
	// Pending native accept wakes on active close, without waiting for ctx expiry.
	wake := make(chan error, 1)
	go func() {
		stream, err := session.AcceptUni(ctx)
		if stream != nil {
			_ = stream.Close()
		}
		wake <- err
	}()
	if err = session.SendDatagram([]byte("done")); err != nil {
		t.Fatal(err)
	}
	data, err = session.ReceiveDatagram(ctx)
	if err != nil || string(data) != "ready-close" {
		t.Fatalf("close readiness %q %v", data, err)
	}
	if err = session.CloseConnection(42, []byte("test complete")); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-wake:
		if err == nil {
			t.Fatal("closed session accept succeeded")
		}
	case <-time.After(2 * time.Second):
		t.Fatal("close did not wake pending accept")
	}
	if _, err = session.Closed(ctx); err != nil {
		t.Fatal(err)
	}
	if err = cmd.Wait(); err != nil {
		t.Fatalf("lifecycle peer: %v\n%s", err, &output)
	}
}

func TestRawWT_AuthenticatedSessionRetainsPrincipalAndAttributes(t *testing.T) {
	rt, cfg := rawWTFixture(t)
	ctx, cancel := context.WithTimeout(context.Background(), 25*time.Second)
	defer cancel()
	up, err := NewUpcalls(rt, 8)
	if err != nil {
		t.Fatal(err)
	}
	defer up.Close()
	if err = up.ServeRPCAuthenticator(RPCAuthenticatorFunc(func(_ context.Context, r RPCAuthRequest) (RPCAuthResponse, error) {
		return RPCAuthResponse{Accepted: r.Metadata["authorization"] == "Bearer valid", Principal: "alice", Attributes: map[string]string{"portal.role": "viewer"}}, nil
	})); err != nil {
		t.Fatal(err)
	}
	l, err := rt.StartRawWT(ctx, up, cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	cmd := rawWTPeer(t, "https://"+l.QUICAddr()+cfg.Path, "echo", "--token", "Bearer valid")
	var output bytes.Buffer
	cmd.Stdout = &output
	cmd.Stderr = &output
	if err = cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if cmd.Process != nil {
			_ = cmd.Process.Kill()
		}
	})
	s, err := l.Accept(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	info, err := s.Info()
	if err != nil || info.Principal == nil || *info.Principal != "alice" || info.Attributes["portal.role"] != "viewer" {
		t.Fatalf("accepted identity %#v %v", info, err)
	}
	if err = rawWTEcho(ctx, s); err != nil {
		t.Fatal(err)
	}
	if err = cmd.Wait(); err != nil {
		t.Fatalf("authenticated peer: %v %s", err, &output)
	}
}

func TestRawWT_ABIEventsAndFloor(t *testing.T) {
	if ASTER_EVENT_RAW_WT_SESSION != 121 || ASTER_EVENT_RAW_WT_STREAM != 122 || ASTER_EVENT_RAW_WT_READ != 123 || ASTER_EVENT_RAW_WT_DATAGRAM != 124 || ASTER_EVENT_RAW_WT_CLOSED != 125 || ASTER_EVENT_RAW_WT_ACKED != 126 || ASTER_EVENT_ENROLLMENT_RESULT != 127 || ABIMinorRequired != 2 {
		t.Fatal("raw WT ABI event/floor drift")
	}
}
