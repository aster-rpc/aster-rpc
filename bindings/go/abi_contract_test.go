//go:build cgo

package aster

import (
	"context"
	"errors"
	"testing"
	"time"
	"unsafe"
)

// Sizes pinned in ffi/src/layout.rs and ffi/tests/abi_contract_test.c (LP64).
func TestABI_StructSizes(t *testing.T) {
	if unsafe.Sizeof(uintptr(0)) != 8 {
		t.Skip("pinned sizes are for LP64")
	}
	want := map[string]uintptr{
		"aster_event_t":              80,
		"aster_runtime_config_t":     16,
		"aster_connect_config_t":     48,
		"aster_node_addr_t":          48,
		"aster_bytes_t":              16,
		"aster_bytes_list_t":         16,
		"aster_remote_info_t":        72,
		"aster_connection_info_t":    56,
		"aster_transport_snapshot_t": 56,
		"aster_transport_metrics_t":  136,
		"aster_dispatch_event_t":     104,
		"aster_call_options_t":       72,
	}
	got := cStructSizes()
	for name, w := range want {
		if got[name] != w {
			t.Errorf("sizeof(%s) = %d, want %d", name, got[name], w)
		}
	}
	wantOff := map[string]uintptr{
		"kind": 0, "flags": 4, "call_id": 8, "session_key": 24, "connection_handle": 40,
		"deadline_unix_ms": 48, "session_id": 60, "request_ptr": 64, "peer_ptr": 80, "buffer": 96,
	}
	for name, off := range dispatchEventOffsets() {
		if off != wantOff[name] {
			t.Errorf("offsetof(aster_dispatch_event_t, %s) = %d, want %d", name, off, wantOff[name])
		}
	}
}

func TestABI_VersionMatchesHeader(t *testing.T) {
	major, minor, patch := Version()
	if got := major<<16 | minor<<8 | patch; got != headerABIVersion {
		t.Errorf("library ABI %#x != header ASTER_ABI_VERSION %#x", got, headerABIVersion)
	}
	if major != 4 || ABIVersionMajor != 4 {
		t.Errorf("expected ABI major 4, lib=%d binding=%d", major, ABIVersionMajor)
	}
	if err := CheckABI(); err != nil {
		t.Fatal(err)
	}
}

func TestABI_StatusName(t *testing.T) {
	if got := StatusName(ASTER_STATUS_ABI_MISMATCH); got != "ASTER_STATUS_ABI_MISMATCH" {
		t.Errorf("StatusName(ABI_MISMATCH) = %q", got)
	}
	if got := StatusName(-12345); got != "ASTER_STATUS_UNKNOWN" {
		t.Errorf("StatusName(unknown) = %q", got)
	}
	if !errors.Is(wrapError(ASTER_STATUS_ABI_MISMATCH), ErrABIMismatch) {
		t.Error("ABI_MISMATCH does not map to ErrABIMismatch")
	}
}

// ABI 4.0: an Endpoint is a node started by aster_node_start (bare
// endpoints were removed); id, address and secret key come from the node.
func TestABI_EndpointIsNodeBacked(t *testing.T) {
	rt, err := NewRuntime(t.Context(), DefaultRuntimeConfig())
	if err != nil {
		t.Fatal(err)
	}
	defer rt.Close()
	cfg := DefaultEndpointConfig()
	cfg.RelayMode = RelayModeDisabled
	cfg.EnableDiscovery = false
	cfg.ALPNs = []string{"aster/test"}
	ep, err := NewEndpoint(t.Context(), rt, cfg)
	if err != nil {
		t.Fatalf("NewEndpoint: %v", err)
	}
	id, err := ep.NodeID()
	if err != nil || len(id) != 64 {
		t.Fatalf("NodeID = %q, %v (want 64 hex chars)", id, err)
	}
	if ep.Node() == nil || ep.Node().Handle() != ep.Handle() {
		t.Fatal("endpoint is not a view of its node")
	}
	addr, err := ep.AddrInfo()
	if err != nil || addr.EndpointID != id {
		t.Fatalf("AddrInfo = %+v, %v", addr, err)
	}
	if err := ep.Close(t.Context()); err != nil {
		t.Fatalf("Close: %v", err)
	}
}

// ABI 4.0: two endpoints talk on a custom ALPN; Accept takes the connection
// from the node's ALPN queue (aster_accept on a node handle).
func TestEndpoint_ConnectAccept(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 30*time.Second)
	defer cancel()
	rt, err := NewRuntime(ctx, DefaultRuntimeConfig())
	if err != nil {
		t.Fatal(err)
	}
	defer rt.Close()
	cfg := DefaultEndpointConfig()
	cfg.RelayMode = RelayModeDisabled
	cfg.EnableDiscovery = false
	cfg.BindAddr = "127.0.0.1:0"
	cfg.ALPNs = []string{"go/ep-test"}
	server, err := NewEndpoint(ctx, rt, cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer server.Close(ctx)
	client, err := NewEndpoint(ctx, rt, cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer client.Close(ctx)
	addr, err := server.AddrInfo()
	if err != nil {
		t.Fatal(err)
	}
	accepted := make(chan *Connection, 1)
	go func() {
		c, err := server.Accept(ctx)
		if err != nil {
			t.Errorf("Accept: %v", err)
		}
		accepted <- c
	}()
	conn, err := client.ConnectNodeAddr(ctx, addr, "go/ep-test")
	if err != nil {
		t.Fatalf("ConnectNodeAddr: %v", err)
	}
	defer conn.Close("done")
	select {
	case c := <-accepted:
		if c == nil {
			t.Fatal("no accepted connection")
		}
		defer c.Close("done")
	case <-ctx.Done():
		t.Fatal("accept timed out")
	}
}
