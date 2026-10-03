//go:build cgo

package aster

import (
	"errors"
	"testing"
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
		"aster_endpoint_config_t":    152,
		"aster_connect_config_t":     48,
		"aster_node_addr_t":          48,
		"aster_bytes_t":              16,
		"aster_bytes_list_t":         16,
		"aster_remote_info_t":        72,
		"aster_connection_info_t":    56,
		"aster_transport_snapshot_t": 56,
		"aster_transport_metrics_t":  136,
		"aster_reactor_call_t":       112,
	}
	got := cStructSizes()
	for name, w := range want {
		if got[name] != w {
			t.Errorf("sizeof(%s) = %d, want %d", name, got[name], w)
		}
	}
	if off := reactorCallEventKindOffset(); off != 0 {
		t.Errorf("offsetof(aster_reactor_call_t, event_kind) = %d, want 0", off)
	}
}

func TestABI_VersionMatchesHeader(t *testing.T) {
	major, minor, patch := Version()
	if got := major<<16 | minor<<8 | patch; got != headerABIVersion {
		t.Errorf("library ABI %#x != header ASTER_ABI_VERSION %#x", got, headerABIVersion)
	}
	if major != 3 || ABIVersionMajor != 3 {
		t.Errorf("expected ABI major 3, lib=%d binding=%d", major, ABIVersionMajor)
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

// Endpoint creation used to send a hand-built config with struct_size=56,
// which the library rejects; this exercises the header-typed config.
func TestABI_EndpointCreateUsesHeaderLayout(t *testing.T) {
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
}
