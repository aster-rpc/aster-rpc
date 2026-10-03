//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"fmt"
	"unsafe"
)

// Relay modes (aster_relay_mode_t).
const (
	RelayModeDefault  uint32 = C.ASTER_RELAY_MODE_DEFAULT
	RelayModeCustom   uint32 = C.ASTER_RELAY_MODE_CUSTOM
	RelayModeDisabled uint32 = C.ASTER_RELAY_MODE_DISABLED
	RelayModeStaging  uint32 = C.ASTER_RELAY_MODE_STAGING
)

// Hook failure modes (aster_hook_failure_mode_t).
const (
	HookFailOpen   uint32 = C.ASTER_HOOK_FAILURE_FAIL_OPEN
	HookFailClosed uint32 = C.ASTER_HOOK_FAILURE_FAIL_CLOSED
)

// EndpointConfig configures an endpoint. Fields map 1:1 onto
// aster_endpoint_config_t.
type EndpointConfig struct {
	RelayMode            uint32
	SecretKey            []byte
	ALPNs                []string
	RelayURLs            []string
	EnableDiscovery      bool
	EnableHooks          bool
	HookTimeoutMs        uint64
	BindAddr             string
	ClearIPTransports    bool
	ClearRelayTransports bool
	PortMapperConfig     uint32
	ProxyURL             string
	ProxyFromEnv         bool
	DataDir              string
	HookFailureMode      uint32
}

// DefaultEndpointConfig returns a sensible default endpoint configuration.
func DefaultEndpointConfig() EndpointConfig {
	return EndpointConfig{
		RelayMode:       RelayModeDefault,
		EnableDiscovery: true,
		EnableHooks:     false,
	}
}

// toC fills a header-typed aster_endpoint_config_t. Pointed-to payloads live
// in the arena (C memory) so the struct satisfies cgo pointer rules.
func (cfg *EndpointConfig) toC(a *cArena) C.aster_endpoint_config_t {
	return C.aster_endpoint_config_t{
		struct_size:            C.uint32_t(unsafe.Sizeof(C.aster_endpoint_config_t{})),
		relay_mode:             C.uint32_t(cfg.RelayMode),
		secret_key:             a.bytes(cfg.SecretKey),
		alpns:                  a.list(cfg.ALPNs),
		relay_urls:             a.list(cfg.RelayURLs),
		enable_discovery:       boolToUint32(cfg.EnableDiscovery),
		enable_hooks:           boolToUint32(cfg.EnableHooks),
		hook_timeout_ms:        C.uint64_t(cfg.HookTimeoutMs),
		bind_addr:              a.str(cfg.BindAddr),
		clear_ip_transports:    boolToUint32(cfg.ClearIPTransports),
		clear_relay_transports: boolToUint32(cfg.ClearRelayTransports),
		portmapper_config:      C.uint32_t(cfg.PortMapperConfig),
		proxy_url:              a.str(cfg.ProxyURL),
		proxy_from_env:         boolToUint32(cfg.ProxyFromEnv),
		data_dir_utf8:          a.str(cfg.DataDir),
		hook_failure_mode:      C.uint32_t(cfg.HookFailureMode),
	}
}

// Endpoint is a network endpoint that can connect to remote nodes
// and accept incoming connections.
type Endpoint struct {
	handle  uint64
	runtime *Runtime
}

// Handle returns the raw endpoint handle.
func (e *Endpoint) Handle() uint64 {
	return e.handle
}

// NewEndpoint creates a new endpoint using the given runtime and config.
func NewEndpoint(ctx context.Context, runtime *Runtime, cfg EndpointConfig) (*Endpoint, error) {
	var a cArena
	defer a.free()
	configC := cfg.toC(&a)

	var opID C.aster_operation_t
	r := C.aster_endpoint_create(
		C.uint64_t(runtime.handle),
		&configC,
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_endpoint_create: %w", Error(r))
	}

	ev, err := runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("endpoint create poll: %w", err)
	}
	if ev.Kind != ASTER_EVENT_ENDPOINT_CREATED {
		return nil, fmt.Errorf("endpoint create: unexpected event %d", ev.Kind)
	}

	return &Endpoint{
		handle:  ev.Handle,
		runtime: runtime,
	}, nil
}

// NodeID returns this endpoint's node identifier as a hex string.
func (e *Endpoint) NodeID() (string, error) {
	var buf [64]byte
	var outLen C.uintptr_t

	r := C.aster_endpoint_id(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		(*C.uint8_t)(unsafe.Pointer(&buf[0])),
		64,
		&outLen,
	)
	if r != 0 {
		return "", fmt.Errorf("aster_endpoint_id: %w", Error(r))
	}
	if outLen == 0 {
		return "", nil
	}
	return string(buf[:int(outLen)]), nil // already hex text
}

// ConnectNodeAddr connects to a remote node using the given NodeAddr and ALPN.
// This allows direct peer connections without requiring address lookup.
func (e *Endpoint) ConnectNodeAddr(ctx context.Context, addr *NodeAddr, alpn string) (*Connection, error) {
	var a cArena
	defer a.free()

	nodeAddrC := (*C.aster_node_addr_t)(a.alloc(unsafe.Sizeof(C.aster_node_addr_t{})))
	nodeAddrC.endpoint_id = a.str(addr.EndpointID)
	nodeAddrC.relay_url = a.str(addr.RelayURL)
	nodeAddrC.direct_addresses = a.list(addr.DirectAddresses)

	config := (*C.aster_connect_config_t)(a.alloc(unsafe.Sizeof(C.aster_connect_config_t{})))
	config.struct_size = C.uint32_t(unsafe.Sizeof(C.aster_connect_config_t{}))
	config.flags = 0
	config.node_id = a.str(addr.EndpointID)
	config.alpn = a.str(alpn)
	config.addr = nodeAddrC

	var opID C.aster_operation_t
	r := C.aster_connect(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		config,
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_connect: %w", Error(r))
	}

	ev, err := e.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("connect poll: %w", err)
	}
	if ev.Kind != ASTER_EVENT_CONNECTED {
		return nil, fmt.Errorf("connect: unexpected event %d", ev.Kind)
	}

	return &Connection{handle: ev.Handle, runtime: e.runtime}, nil
}

// Connect connects to a remote node identified by nodeIDHex using the given ALPN.
func (e *Endpoint) Connect(ctx context.Context, nodeIDHex, alpn string) (*Connection, error) {
	// nodeIDHex is passed through as UTF-8; Rust parses it.
	var a cArena
	defer a.free()

	config := C.aster_connect_config_t{
		struct_size: C.uint32_t(unsafe.Sizeof(C.aster_connect_config_t{})),
		flags:       0,
		node_id:     a.str(nodeIDHex),
		alpn:        a.str(alpn),
		addr:        nil,
	}

	var opID C.aster_operation_t
	r := C.aster_connect(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		&config,
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_connect: %w", Error(r))
	}

	ev, err := e.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("connect poll: %w", err)
	}
	if ev.Kind != ASTER_EVENT_CONNECTED {
		return nil, fmt.Errorf("connect: unexpected event %d", ev.Kind)
	}

	return &Connection{handle: ev.Handle, runtime: e.runtime}, nil
}

// Accept accepts an incoming connection.
func (e *Endpoint) Accept(ctx context.Context) (*Connection, error) {
	var opID C.aster_operation_t
	r := C.aster_accept(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_accept: %w", Error(r))
	}

	ev, err := e.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("accept poll: %w", err)
	}
	if ev.Kind != ASTER_EVENT_CONNECTION_ACCEPTED {
		return nil, fmt.Errorf("accept: unexpected event %d", ev.Kind)
	}

	return &Connection{handle: ev.Handle, runtime: e.runtime}, nil
}

// Close closes the endpoint.
func (e *Endpoint) Close(ctx context.Context) error {
	var opID C.aster_operation_t
	r := C.aster_endpoint_close(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		0,
		&opID,
	)
	if r != 0 {
		return fmt.Errorf("aster_endpoint_close: %w", Error(r))
	}

	ev, err := e.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return fmt.Errorf("endpoint close poll: %w", err)
	}
	if ev.Kind != ASTER_EVENT_CLOSED {
		return fmt.Errorf("endpoint close: unexpected event %d", ev.Kind)
	}
	return nil
}

// AddrInfo returns the structured address of this endpoint.
func (e *Endpoint) AddrInfo() (*NodeAddr, error) {
	var addrBuf [4096]byte
	var addr C.aster_node_addr_t

	r := C.aster_endpoint_addr_info(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		(*C.uint8_t)(unsafe.Pointer(&addrBuf[0])),
		4096,
		&addr,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_endpoint_addr_info: %w", Error(r))
	}

	nodeAddr := &NodeAddr{
		DirectAddresses: []string{},
	}

	// Read endpoint_id.len from out_addr struct to know the actual length
	idLen := uintptr(addr.endpoint_id.len)
	if idLen > 0 {
		// endpoint_id bytes are at the start of addrBuf
		nodeAddr.EndpointID = string(addrBuf[:idLen])
	}

	// Read relay_url from out_addr struct
	relayLen := uintptr(addr.relay_url.len)
	if relayLen > 0 {
		// We need to find where relay_url starts in the buffer.
		// It comes after: endpoint_id (idLen bytes) + 1 null byte
		relayStart := idLen + 1
		if int(relayStart+relayLen) <= len(addrBuf) {
			nodeAddr.RelayURL = string(addrBuf[relayStart : relayStart+relayLen])
		}
	}

	// Read direct_addresses list from out_addr struct
	daListLen := uintptr(addr.direct_addresses.len)
	if daListLen > 0 && addr.direct_addresses.items != nil {
		// The aster_bytes_t array is at the end of the buffer.
		// Each aster_bytes_t is 16 bytes (ptr + len).
		// We need to figure out where this array starts.
		// The array is placed after all the direct_address strings.
		// We can't easily compute this, so we read from the struct directly.
		daItems := unsafe.Slice(addr.direct_addresses.items, daListLen)
		for i := 0; i < int(daListLen); i++ {
			item := daItems[i]
			if item.ptr != nil && item.len > 0 {
				nodeAddr.DirectAddresses = append(nodeAddr.DirectAddresses,
					C.GoStringN((*C.char)(unsafe.Pointer(item.ptr)), C.int(item.len)))
			}
		}
	}

	return nodeAddr, nil
}

// ExportSecretKey exports this endpoint's 32-byte secret key seed.
func (e *Endpoint) ExportSecretKey() ([]byte, error) {
	var buf [32]byte
	var outLen C.uintptr_t

	r := C.aster_endpoint_export_secret_key(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		(*C.uint8_t)(unsafe.Pointer(&buf[0])),
		32,
		&outLen,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_endpoint_export_secret_key: %w", Error(r))
	}
	if outLen == 0 {
		return nil, nil
	}
	return buf[:outLen], nil
}

// boolToUint32 converts a bool to uint32 (0 or 1).
func boolToUint32(b bool) C.uint32_t {
	if b {
		return 1
	}
	return 0
}

// RemoteEndpointInfo represents information about a remote endpoint.
type RemoteEndpointInfo struct {
	NodeID          string
	IsConnected     bool
	ConnectionType  uint32
	RelayURL        string
	LastHandshakeNS uint64
	BytesSent       uint64
	BytesReceived   uint64
}

// TransportMetrics represents transport-layer metrics for an endpoint.
type TransportMetrics struct {
	SendIPv4          uint64
	SendIPv6          uint64
	SendRelay         uint64
	RecvDataIPv4      uint64
	RecvDataIPv6      uint64
	RecvDataRelay     uint64
	RecvDatagrams     uint64
	NumConnsDirect    uint64
	NumConnsOpened    uint64
	NumConnsClosed    uint64
	PathsDirect       uint64
	PathsRelay        uint64
	HolepunchAttempts uint64
	RelayHomeChange   uint64
}

// RemoteInfo queries information about a remote endpoint by node ID.
func (e *Endpoint) RemoteInfo(ctx context.Context, nodeID string) (*RemoteEndpointInfo, error) {
	nodeIDBytes := []byte(nodeID)
	nodeIDBuf := C.malloc(C.size_t(len(nodeIDBytes)))
	C.memcpy(nodeIDBuf, unsafe.Pointer(&nodeIDBytes[0]), C.size_t(len(nodeIDBytes)))
	defer C.free(nodeIDBuf)

	nodeIDC := C.struct_aster_bytes_t{
		ptr: (*C.uint8_t)(nodeIDBuf),
		len: C.uintptr_t(len(nodeIDBytes)),
	}

	var info C.aster_remote_info_t

	r := C.aster_endpoint_remote_info(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		nodeIDC,
		&info,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_endpoint_remote_info: %w", Error(r))
	}

	return parseRemoteEndpointInfo(&info), nil
}

// RemoteInfoList returns a list of all known remote endpoints.
func (e *Endpoint) RemoteInfoList(ctx context.Context, max int) ([]RemoteEndpointInfo, error) {
	if max <= 0 {
		return nil, nil
	}
	infos := make([]C.aster_remote_info_t, max)
	var outCount C.uintptr_t

	r := C.aster_endpoint_remote_info_list(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		&infos[0],
		C.uintptr_t(max),
		&outCount,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_endpoint_remote_info_list: %w", Error(r))
	}

	result := make([]RemoteEndpointInfo, int(outCount))
	for i := 0; i < int(outCount); i++ {
		result[i] = *parseRemoteEndpointInfo(&infos[i])
	}

	return result, nil
}

// TransportMetrics returns current transport-layer metrics for this endpoint.
func (e *Endpoint) TransportMetrics() (*TransportMetrics, error) {
	var metrics C.aster_transport_metrics_t

	r := C.aster_endpoint_transport_metrics(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		&metrics,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_endpoint_transport_metrics: %w", Error(r))
	}

	return &TransportMetrics{
		SendIPv4:          uint64(metrics.send_ipv4),
		SendIPv6:          uint64(metrics.send_ipv6),
		SendRelay:         uint64(metrics.send_relay),
		RecvDataIPv4:      uint64(metrics.recv_data_ipv4),
		RecvDataIPv6:      uint64(metrics.recv_data_ipv6),
		RecvDataRelay:     uint64(metrics.recv_data_relay),
		RecvDatagrams:     uint64(metrics.recv_datagrams),
		NumConnsDirect:    uint64(metrics.num_conns_direct),
		NumConnsOpened:    uint64(metrics.num_conns_opened),
		NumConnsClosed:    uint64(metrics.num_conns_closed),
		PathsDirect:       uint64(metrics.paths_direct),
		PathsRelay:        uint64(metrics.paths_relay),
		HolepunchAttempts: uint64(metrics.holepunch_attempts),
		RelayHomeChange:   uint64(metrics.relay_home_change),
	}, nil
}

// parseRemoteEndpointInfo parses a C aster_remote_info_t struct.
func parseRemoteEndpointInfo(info *C.aster_remote_info_t) *RemoteEndpointInfo {
	result := &RemoteEndpointInfo{
		ConnectionType:  uint32(info.connection_type),
		LastHandshakeNS: uint64(info.last_handshake_ns),
		BytesSent:       uint64(info.bytes_sent),
		BytesReceived:   uint64(info.bytes_received),
	}

	if info.is_connected != 0 {
		result.IsConnected = true
	}

	// node_id / relay_url are Rust-owned; copy then aster_string_release.
	result.NodeID = string(takeRustBytes(info.node_id))
	result.RelayURL = string(takeRustBytes(info.relay_url))

	return result
}
