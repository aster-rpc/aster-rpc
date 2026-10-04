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
		// ABI 4.1 / 4.2 (ffi/src/layout.rs)
		"aster_upcall_event_t":            40,
		"aster_topology_admitted_stats_t": 32,
		"aster_http_foreign_options_t":    48,
		"aster_route_spec_t":              40,
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

// ABI 4.1 / 4.2 field offsets and enum values (pinned in ffi/src/layout.rs
// and the header); the plain-Go mirrors in upcall_types.go and the UCv1
// vectors must agree with them.
func TestABI_UpcallAndExposeLayouts(t *testing.T) {
	if unsafe.Sizeof(uintptr(0)) != 8 {
		t.Skip("pinned offsets are for LP64")
	}
	pin := func(what string, got, want map[string]uintptr) {
		t.Helper()
		if len(got) != len(want) {
			t.Errorf("%s: %d offsets checked, want %d", what, len(got), len(want))
		}
		for f, off := range want {
			if got[f] != off {
				t.Errorf("offsetof(%s, %s) = %d, want %d", what, f, got[f], off)
			}
		}
	}
	pin("aster_upcall_event_t", upcallEventOffsets(), map[string]uintptr{
		"invocation_id": 0, "kind": 8, "cancel_reason": 10, "payload_len": 12,
		"deadline_unix_ms": 16, "payload_ptr": 24, "buffer": 32})
	pin("aster_http_foreign_options_t", httpForeignOptionsOffsets(), map[string]uintptr{
		"struct_size": 0, "flags": 4, "inline_request_max": 8, "max_request_body": 16,
		"max_response_body": 24, "max_chunk": 32, "response_buffer": 40, "reserved": 44})
	pin("aster_route_spec_t", routeSpecOffsets(), map[string]uintptr{
		"host": 0, "service_id": 16, "port": 32, "protocol": 36})

	want := map[string]int64{
		"UPCALL_KIND_CANCEL": 0, "UPCALL_KIND_LEASE_TRANSITION": 1, "UPCALL_KIND_LEASE_SNAPSHOT": 2,
		"UPCALL_KIND_LEASE_AUTHORIZE": 3, "UPCALL_KIND_FENCED_MUTATE": 4, "UPCALL_KIND_TOPOLOGY_ADMITTED": 16,
		"UPCALL_KIND_EXPOSE_AUTHORIZE": 48, "UPCALL_KIND_EXPOSE_HTTP": 49, "UPCALL_KIND_EXPOSE_ROUTE_POLICY": 50,
		"UPCALL_KIND_ADMISSION_CONNECT": 64, "UPCALL_KIND_ADMISSION_HANDSHAKE": 65,
		"UPCALL_STATUS_OK": 0, "UPCALL_STATUS_ERROR": 1, "UPCALL_STATUS_UNIMPLEMENTED": 2,
		"UPCALL_CANCEL_NONE": 0, "UPCALL_CANCEL_TIMEOUT": 1, "UPCALL_CANCEL_ABANDONED": 2, "UPCALL_CANCEL_SHUTDOWN": 3,
		"LEASE_STATUS_HELD": 0, "LEASE_STATUS_RELEASED": 1, "LEASE_STATUS_FENCED": 2, "LEASE_STATUS_LOST": 3,
		"EVENT_LEASE_ACQUIRED": 107, "EVENT_LEASE_STATUS": 108,
		"HTTP_BODY_OK": 0, "HTTP_BODY_UNKNOWN_STREAM": 1, "HTTP_BODY_CANCELLED": 2, "HTTP_BODY_TOO_LARGE": 3,
		"HTTP_BODY_CHUNK_TOO_LARGE": 4, "HTTP_BODY_FINISHED": 5, "HTTP_BODY_TIMEOUT": 6, "HTTP_BODY_READ": 7,
		"HTTP_BODY_INVALID_TRAILERS": 8, "HTTP_BODY_BLOCKING_ON_RUNTIME": 9,
		"HTTP_BODY_INVALID_ARGUMENT": -1, "HTTP_BODY_UNKNOWN_HANDLE": -2,
		"HOOK_FAILURE_FAIL_OPEN": 0, "HOOK_FAILURE_FAIL_CLOSED": 1, "UPCALL_DEFAULT_CAPACITY": 1024,
	}
	got := abiEnums()
	for name, w := range want {
		if got[name] != w {
			t.Errorf("%s = %d, want %d", name, got[name], w)
		}
	}
	if len(got) != len(want) {
		t.Errorf("%d enums read from the header, %d pinned", len(got), len(want))
	}
	// The plain-Go mirrors.
	if UpcallExposeHTTP != 49 || UpcallExposeAuthorize != 48 || UpcallExposeRoutePolicy != 50 ||
		UpcallLeaseTransition != 1 || UpcallTopologyAdmitted != 16 || UpcallStatusUnimplemented != 2 ||
		CancelShutdown != 3 {
		t.Error("upcall_types.go drifted from the header")
	}
	// ABI 4.1 + 4.2 are both in the library.
	if _, minor, _ := Version(); minor < 2 {
		t.Errorf("library ABI minor %d, binding needs >= 2", minor)
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
