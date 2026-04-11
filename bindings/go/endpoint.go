//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include "iroh_ffi.h"
*/
import "C"

import (
	"context"
	"encoding/hex"
	"fmt"
	"unsafe"
)

// EndpointConfig configures an endpoint.
type EndpointConfig struct {
	RelayMode            uint32
	SecretKey            []byte
	ALPNs                []string
	RelayURLs            []string
	EnableDiscovery      bool
	EnableHooks          bool
	BindAddr             string
	PortMapperConfig     uint32
	ProxyURL             string
	DataDir              string
	ClearRelayTransports bool
}

// DefaultEndpointConfig returns a sensible default endpoint configuration.
func DefaultEndpointConfig() EndpointConfig {
	return EndpointConfig{
		RelayMode:       0,
		EnableDiscovery: true,
		EnableHooks:     false,
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
	// Allocate config struct with C malloc to avoid cgo pointer issues.
	// iroh_endpoint_config_t layout (from iroh_ffi.h):
	//   uint32_t struct_size;              // offset 0
	//   uint32_t relay_mode;              // offset 4
	//   struct iroh_bytes_t secret_key;   // offset 8 (ptr at 8, len at 16)
	//   struct iroh_bytes_list_t alpns;   // offset 24 (items at 24, len at 32)
	//   struct iroh_bytes_list_t relay_urls; // offset 40 (items at 40, len at 48)
	//   uint32_t enable_discovery;         // offset 56
	//   uint32_t enable_hooks;            // offset 60
	//   uint64_t hook_timeout_ms;         // offset 64
	//   struct iroh_bytes_t bind_addr;   // offset 72 (ptr at 72, len at 80)
	//   uint32_t clear_ip_transports;     // offset 88
	//   uint32_t clear_relay_transports;  // offset 92
	//   uint32_t portmapper_config;       // offset 96
	//   struct iroh_bytes_t proxy_url;    // offset 100 (ptr at 100, len at 108)
	//   uint32_t proxy_from_env;         // offset 116
	//   struct iroh_bytes_t data_dir_utf8; // offset 120 (ptr at 120, len at 128)
	// Total: 136 bytes
	configC := C.malloc(136)
	defer C.free(configC)

	// struct_size at offset 0
	*(*C.uint32_t)(unsafe.Pointer(uintptr(configC) + 0)) = 56
	// relay_mode at offset 4
	*(*C.uint32_t)(unsafe.Pointer(uintptr(configC) + 4)) = C.uint32_t(cfg.RelayMode)
	// enable_discovery at offset 56
	*(*C.uint32_t)(unsafe.Pointer(uintptr(configC) + 56)) = boolToUint32(cfg.EnableDiscovery)
	// enable_hooks at offset 60
	*(*C.uint32_t)(unsafe.Pointer(uintptr(configC) + 60)) = boolToUint32(cfg.EnableHooks)
	// hook_timeout_ms at offset 64 (8 bytes, set to 0 = default)
	*(*C.uint64_t)(unsafe.Pointer(uintptr(configC) + 64)) = 0
	// clear_ip_transports at offset 88 (set to 0 = false)
	*(*C.uint32_t)(unsafe.Pointer(uintptr(configC) + 88)) = 0
	// clear_relay_transports at offset 92
	*(*C.uint32_t)(unsafe.Pointer(uintptr(configC) + 92)) = boolToUint32(cfg.ClearRelayTransports)
	// portmapper_config at offset 96 (set to 0 = default)
	*(*C.uint32_t)(unsafe.Pointer(uintptr(configC) + 96)) = C.uint32_t(cfg.PortMapperConfig)
	// proxy_from_env at offset 116 (set to 0 = false)
	*(*C.uint32_t)(unsafe.Pointer(uintptr(configC) + 116)) = 0

	// secret_key: iroh_bytes_t at offset 8 (ptr at 8, len at 16)
	if len(cfg.SecretKey) > 0 {
		cBuf := C.malloc(C.size_t(len(cfg.SecretKey)))
		C.memcpy(cBuf, unsafe.Pointer(&cfg.SecretKey[0]), C.size_t(len(cfg.SecretKey)))
		*(*unsafe.Pointer)(unsafe.Pointer(uintptr(configC) + 8)) = cBuf
		*(*C.uintptr_t)(unsafe.Pointer(uintptr(configC) + 16)) = C.uintptr_t(len(cfg.SecretKey))
	}

	// alpns: iroh_bytes_list_t at offset 24 (items_ptr at 24, len at 32)
	if len(cfg.ALPNs) > 0 {
		// Allocate array of iroh_bytes_t structs (16 bytes each)
		alpnArrayC := C.malloc(C.size_t(len(cfg.ALPNs) * 16))
		for i, alpn := range cfg.ALPNs {
			b := []byte(alpn)
			cBuf := C.malloc(C.size_t(len(b)))
			C.memcpy(cBuf, unsafe.Pointer(&b[0]), C.size_t(len(b)))
			itemPtr := (*C.struct_iroh_bytes_t)(unsafe.Pointer(uintptr(alpnArrayC) + uintptr(i)*16))
			itemPtr.ptr = (*C.uint8_t)(cBuf)
			itemPtr.len = C.uintptr_t(len(b))
		}
		*(*unsafe.Pointer)(unsafe.Pointer(uintptr(configC) + 24)) = alpnArrayC
		*(*C.uintptr_t)(unsafe.Pointer(uintptr(configC) + 32)) = C.uintptr_t(len(cfg.ALPNs))
	}

	// relay_urls: iroh_bytes_list_t at offset 40 (items_ptr at 40, len at 48)
	if len(cfg.RelayURLs) > 0 {
		urlArrayC := C.malloc(C.size_t(len(cfg.RelayURLs) * 16))
		for i, url := range cfg.RelayURLs {
			b := []byte(url)
			cBuf := C.malloc(C.size_t(len(b)))
			C.memcpy(cBuf, unsafe.Pointer(&b[0]), C.size_t(len(b)))
			itemPtr := (*C.struct_iroh_bytes_t)(unsafe.Pointer(uintptr(urlArrayC) + uintptr(i)*16))
			itemPtr.ptr = (*C.uint8_t)(cBuf)
			itemPtr.len = C.uintptr_t(len(b))
		}
		*(*unsafe.Pointer)(unsafe.Pointer(uintptr(configC) + 40)) = urlArrayC
		*(*C.uintptr_t)(unsafe.Pointer(uintptr(configC) + 48)) = C.uintptr_t(len(cfg.RelayURLs))
	}

	// proxy_url: iroh_bytes_t at offset 100 (ptr at 100, len at 108)
	if len(cfg.ProxyURL) > 0 {
		proxyB := []byte(cfg.ProxyURL)
		cBuf := C.malloc(C.size_t(len(proxyB)))
		C.memcpy(cBuf, unsafe.Pointer(&proxyB[0]), C.size_t(len(proxyB)))
		*(*unsafe.Pointer)(unsafe.Pointer(uintptr(configC) + 100)) = cBuf
		*(*C.uintptr_t)(unsafe.Pointer(uintptr(configC) + 108)) = C.uintptr_t(len(proxyB))
	}

	// data_dir_utf8: iroh_bytes_t at offset 120 (ptr at 120, len at 128)
	if len(cfg.DataDir) > 0 {
		dirB := []byte(cfg.DataDir)
		cBuf := C.malloc(C.size_t(len(dirB)))
		C.memcpy(cBuf, unsafe.Pointer(&dirB[0]), C.size_t(len(dirB)))
		*(*unsafe.Pointer)(unsafe.Pointer(uintptr(configC) + 120)) = cBuf
		*(*C.uintptr_t)(unsafe.Pointer(uintptr(configC) + 128)) = C.uintptr_t(len(dirB))
	}

	// bind_addr: iroh_bytes_t at offset 72 (ptr at 72, len at 80)
	if len(cfg.BindAddr) > 0 {
		addrB := []byte(cfg.BindAddr)
		cBuf := C.malloc(C.size_t(len(addrB)))
		C.memcpy(cBuf, unsafe.Pointer(&addrB[0]), C.size_t(len(addrB)))
		*(*unsafe.Pointer)(unsafe.Pointer(uintptr(configC) + 72)) = cBuf
		*(*C.uintptr_t)(unsafe.Pointer(uintptr(configC) + 80)) = C.uintptr_t(len(addrB))
	}

	var opID C.iroh_operation_t
	r := C.iroh_endpoint_create(
		C.uint64_t(runtime.handle),
		(*C.struct_iroh_endpoint_config_t)(configC),
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_endpoint_create: %w", Error(r))
	}

	ev, err := runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("endpoint create poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_ENDPOINT_CREATED {
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

	r := C.iroh_endpoint_id(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		(*C.uint8_t)(unsafe.Pointer(&buf[0])),
		64,
		&outLen,
	)
	if r != 0 {
		return "", fmt.Errorf("iroh_endpoint_id: %w", Error(r))
	}
	if outLen == 0 {
		return "", nil
	}
	return hex.EncodeToString(buf[:int(outLen)]), nil
}

// ConnectNodeAddr connects to a remote node using the given NodeAddr and ALPN.
// This allows direct peer connections without requiring address lookup.
func (e *Endpoint) ConnectNodeAddr(ctx context.Context, addr *NodeAddr, alpn string) (*Connection, error) {
	// Encode endpoint_id (hex string as bytes)
	idBytes := []byte(addr.EndpointID)
	idBuf := C.malloc(C.size_t(len(idBytes)))
	C.memcpy(idBuf, unsafe.Pointer(&idBytes[0]), C.size_t(len(idBytes)))
	defer C.free(idBuf)

	// Encode relay_url
	var relayBuf *C.uint8_t
	var relayLen C.uintptr_t
	if addr.RelayURL != "" {
		urlBytes := []byte(addr.RelayURL)
		relayBuf = (*C.uint8_t)(C.malloc(C.size_t(len(urlBytes))))
		C.memcpy(unsafe.Pointer(relayBuf), unsafe.Pointer(&urlBytes[0]), C.size_t(len(urlBytes)))
		relayLen = C.uintptr_t(len(urlBytes))
	}

	// Encode direct_addresses
	daPtrs := make([]*C.uint8_t, len(addr.DirectAddresses))
	daLens := make([]C.uintptr_t, len(addr.DirectAddresses))
	for i, da := range addr.DirectAddresses {
		b := []byte(da)
		daPtrs[i] = (*C.uint8_t)(C.malloc(C.size_t(len(b))))
		C.memcpy(unsafe.Pointer(daPtrs[i]), unsafe.Pointer(&b[0]), C.size_t(len(b)))
		daLens[i] = C.uintptr_t(len(b))
	}

	// Build iroh_bytes_t array for direct addresses
	daItems := make([]C.struct_iroh_bytes_t, len(addr.DirectAddresses))
	for i := 0; i < len(addr.DirectAddresses); i++ {
		daItems[i].ptr = daPtrs[i]
		daItems[i].len = daLens[i]
	}

	// Encode ALPN
	alpnBytes := []byte(alpn)
	alpnBuf := C.malloc(C.size_t(len(alpnBytes)))
	C.memcpy(alpnBuf, unsafe.Pointer(&alpnBytes[0]), C.size_t(len(alpnBytes)))
	defer C.free(alpnBuf)

	// Build config struct with C-allocated memory for the struct itself
	// iroh_connect_config_t: struct_size(4) + flags(4) + node_id(16) + alpn(16) + addr(8) = 48 bytes
	configC := C.malloc(48)
	defer C.free(configC)

	// Write struct_size = 48
	*(*C.uint32_t)(configC) = 48
	// Write flags = 0
	*(*C.uint32_t)(unsafe.Pointer(uintptr(configC) + 4)) = 0

	// node_id at offset 8: struct iroh_bytes_t { ptr(8), len(8) }
	nodeIDPtr := (*C.struct_iroh_bytes_t)(unsafe.Pointer(uintptr(configC) + 8))
	nodeIDPtr.ptr = (*C.uint8_t)(idBuf)
	nodeIDPtr.len = C.uintptr_t(len(idBytes))

	// alpn at offset 24: struct iroh_bytes_t { ptr(8), len(8) }
	alpnPtr := (*C.struct_iroh_bytes_t)(unsafe.Pointer(uintptr(configC) + 24))
	alpnPtr.ptr = (*C.uint8_t)(alpnBuf)
	alpnPtr.len = C.uintptr_t(len(alpnBytes))

	// addr at offset 40: *iroh_node_addr_t
	addrPtr := (*unsafe.Pointer)(unsafe.Pointer(uintptr(configC) + 40))

	// Build iroh_node_addr_t in a separate C allocation (48 bytes)
	nodeAddrC := C.malloc(48)
	defer C.free(nodeAddrC)

	// endpoint_id at offset 0: struct iroh_bytes_t { ptr(8), len(8) }
	nodeIDField := (*C.struct_iroh_bytes_t)(nodeAddrC)
	nodeIDField.ptr = (*C.uint8_t)(idBuf)
	nodeIDField.len = C.uintptr_t(len(idBytes))

	// relay_url at offset 16: struct iroh_bytes_t { ptr(8), len(8) }
	relayField := (*C.struct_iroh_bytes_t)(unsafe.Pointer(uintptr(nodeAddrC) + 16))
	relayField.ptr = relayBuf
	relayField.len = relayLen

	// direct_addresses at offset 32: struct iroh_bytes_list_t { items_ptr(8), len(8) }
	daListField := (*C.struct_iroh_bytes_list_t)(unsafe.Pointer(uintptr(nodeAddrC) + 32))
	if len(daItems) > 0 {
		daItemsC := C.malloc(C.size_t(len(daItems) * 16))
		for i := 0; i < len(daItems); i++ {
			itemPtr := (*C.struct_iroh_bytes_t)(unsafe.Pointer(uintptr(daItemsC) + uintptr(i)*16))
			itemPtr.ptr = daItems[i].ptr
			itemPtr.len = daItems[i].len
		}
		daListField.items = (*C.struct_iroh_bytes_t)(daItemsC)
		daListField.len = C.uintptr_t(len(daItems))
	}

	*addrPtr = nodeAddrC

	config := (*C.struct_iroh_connect_config_t)(configC)

	var opID C.iroh_operation_t
	r := C.iroh_connect(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		config,
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_connect: %w", Error(r))
	}

	ev, err := e.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("connect poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_CONNECTED {
		return nil, fmt.Errorf("connect: unexpected event %d", ev.Kind)
	}

	return &Connection{handle: ev.Handle, runtime: e.runtime}, nil
}

// Connect connects to a remote node identified by nodeIDHex using the given ALPN.
func (e *Endpoint) Connect(ctx context.Context, nodeIDHex, alpn string) (*Connection, error) {
	// nodeIDHex is expected to be a hex string. We pass it directly to Rust
	// where it will be parsed.
	nodeIDBytes := []byte(nodeIDHex)

	// Allocate C memory for node ID and ALPN.
	nodeIDBuf := C.malloc(C.size_t(len(nodeIDBytes)))
	C.memcpy(nodeIDBuf, unsafe.Pointer(&nodeIDBytes[0]), C.size_t(len(nodeIDBytes)))
	alpnB := []byte(alpn)
	alpnBuf := C.malloc(C.size_t(len(alpnB)))
	C.memcpy(alpnBuf, unsafe.Pointer(&alpnB[0]), C.size_t(len(alpnB)))

	config := C.struct_iroh_connect_config_t{
		struct_size: 48,
		flags:        0,
		node_id: C.struct_iroh_bytes_t{
			ptr: (*C.uint8_t)(nodeIDBuf),
			len: C.uintptr_t(len(nodeIDBytes)),
		},
		alpn: C.struct_iroh_bytes_t{
			ptr: (*C.uint8_t)(alpnBuf),
			len: C.uintptr_t(len(alpnB)),
		},
		addr: nil,
	}

	var opID C.iroh_operation_t
	r := C.iroh_connect(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		&config,
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_connect: %w", Error(r))
	}

	ev, err := e.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("connect poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_CONNECTED {
		return nil, fmt.Errorf("connect: unexpected event %d", ev.Kind)
	}

	return &Connection{handle: ev.Handle, runtime: e.runtime}, nil
}

// Accept accepts an incoming connection.
func (e *Endpoint) Accept(ctx context.Context) (*Connection, error) {
	var opID C.iroh_operation_t
	r := C.iroh_accept(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_accept: %w", Error(r))
	}

	ev, err := e.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("accept poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_CONNECTION_ACCEPTED {
		return nil, fmt.Errorf("accept: unexpected event %d", ev.Kind)
	}

	return &Connection{handle: ev.Handle, runtime: e.runtime}, nil
}

// Close closes the endpoint.
func (e *Endpoint) Close(ctx context.Context) error {
	var opID C.iroh_operation_t
	r := C.iroh_endpoint_close(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		0,
		&opID,
	)
	if r != 0 {
		return fmt.Errorf("iroh_endpoint_close: %w", Error(r))
	}

	ev, err := e.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return fmt.Errorf("endpoint close poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_CLOSED {
		return fmt.Errorf("endpoint close: unexpected event %d", ev.Kind)
	}
	return nil
}

// AddrInfo returns the structured address of this endpoint.
func (e *Endpoint) AddrInfo() (*NodeAddr, error) {
	var addrBuf [4096]byte
	var addr C.iroh_node_addr_t

	r := C.iroh_endpoint_addr_info(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		(*C.uint8_t)(unsafe.Pointer(&addrBuf[0])),
		4096,
		&addr,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_endpoint_addr_info: %w", Error(r))
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
		// The iroh_bytes_t array is at the end of the buffer.
		// Each iroh_bytes_t is 16 bytes (ptr + len).
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

	r := C.iroh_endpoint_export_secret_key(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		(*C.uint8_t)(unsafe.Pointer(&buf[0])),
		32,
		&outLen,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_endpoint_export_secret_key: %w", Error(r))
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
	SendIPv4            uint64
	SendIPv6            uint64
	SendRelay           uint64
	RecvDataIPv4        uint64
	RecvDataIPv6        uint64
	RecvDataRelay       uint64
	RecvDatagrams       uint64
	NumConnsDirect      uint64
	NumConnsOpened      uint64
	NumConnsClosed      uint64
	PathsDirect         uint64
	PathsRelay          uint64
	HolepunchAttempts   uint64
	RelayHomeChange     uint64
}

// RemoteInfo queries information about a remote endpoint by node ID.
func (e *Endpoint) RemoteInfo(ctx context.Context, nodeID string) (*RemoteEndpointInfo, error) {
	nodeIDBytes := []byte(nodeID)
	nodeIDBuf := C.malloc(C.size_t(len(nodeIDBytes)))
	C.memcpy(nodeIDBuf, unsafe.Pointer(&nodeIDBytes[0]), C.size_t(len(nodeIDBytes)))
	defer C.free(nodeIDBuf)

	nodeIDC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(nodeIDBuf),
		len: C.uintptr_t(len(nodeIDBytes)),
	}

	// iroh_remote_info_t: struct_size(4) + node_id(16) + is_connected(4) + connection_type(4) + relay_url(16) + last_handshake_ns(8) + bytes_sent(8) + bytes_received(8) = 68 bytes
	var info C.iroh_remote_info_t

	r := C.iroh_endpoint_remote_info(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		nodeIDC,
		&info,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_endpoint_remote_info: %w", Error(r))
	}

	return parseRemoteEndpointInfo(&info), nil
}

// RemoteInfoList returns a list of all known remote endpoints.
func (e *Endpoint) RemoteInfoList(ctx context.Context, max int) ([]RemoteEndpointInfo, error) {
	// iroh_remote_info_t: 68 bytes
	infos := make([]C.iroh_remote_info_t, max)
	var outCount C.uintptr_t

	r := C.iroh_endpoint_remote_info_list(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		&infos[0],
		C.uintptr_t(max),
		&outCount,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_endpoint_remote_info_list: %w", Error(r))
	}

	result := make([]RemoteEndpointInfo, int(outCount))
	for i := 0; i < int(outCount); i++ {
		result[i] = *parseRemoteEndpointInfo(&infos[i])
	}

	return result, nil
}

// TransportMetrics returns current transport-layer metrics for this endpoint.
func (e *Endpoint) TransportMetrics() (*TransportMetrics, error) {
	var metrics C.iroh_transport_metrics_t

	r := C.iroh_endpoint_transport_metrics(
		C.uint64_t(e.runtime.handle),
		C.uint64_t(e.handle),
		&metrics,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_endpoint_transport_metrics: %w", Error(r))
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

// parseRemoteEndpointInfo parses a C iroh_remote_info_t struct.
func parseRemoteEndpointInfo(info *C.iroh_remote_info_t) *RemoteEndpointInfo {
	result := &RemoteEndpointInfo{
		ConnectionType:  uint32(info.connection_type),
		LastHandshakeNS: uint64(info.last_handshake_ns),
		BytesSent:       uint64(info.bytes_sent),
		BytesReceived:   uint64(info.bytes_received),
	}

	if info.is_connected != 0 {
		result.IsConnected = true
	}

	// node_id: iroh_bytes_t at offset 4 (ptr at 4, len at 12)
	nodeIDPtr := (*unsafe.Pointer)(unsafe.Pointer(uintptr(unsafe.Pointer(info)) + 4))
	nodeIDLen := (*C.uintptr_t)(unsafe.Pointer(uintptr(unsafe.Pointer(info)) + 12))
	if nodeIDPtr != nil && *nodeIDLen > 0 {
		result.NodeID = C.GoStringN((*C.char)(*nodeIDPtr), C.int(*nodeIDLen))
	}

	// relay_url: iroh_bytes_t at offset 16 (ptr at 16, len at 24)
	relayPtr := (*unsafe.Pointer)(unsafe.Pointer(uintptr(unsafe.Pointer(info)) + 16))
	relayLen := (*C.uintptr_t)(unsafe.Pointer(uintptr(unsafe.Pointer(info)) + 24))
	if relayPtr != nil && *relayLen > 0 {
		result.RelayURL = C.GoStringN((*C.char)(*relayPtr), C.int(*relayLen))
	}

	return result
}
