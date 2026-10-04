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

// EndpointConfig configures an Endpoint.
//
// ABI 4.0 removed bare endpoints (aster_endpoint_create and
// aster_endpoint_config_t): an Endpoint is now a node started by
// aster_node_start, and these fields are mapped onto AsterConfig.
type EndpointConfig struct {
	RelayMode            uint32 // RelayMode* constant
	SecretKey            []byte
	ALPNs                []string
	RelayURLs            []string
	EnableDiscovery      bool // -> AsterConfig.LocalDiscovery
	EnableHooks          bool
	HookTimeoutMs        uint64
	BindAddr             string
	ClearIPTransports    bool
	ClearRelayTransports bool
	PortMapperConfig     uint32 // 0 = default, 1 = disabled
	ProxyURL             string
	ProxyFromEnv         bool
	DataDir              string // -> AsterConfig.StoragePath ("" = in memory)
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

// toAsterConfig maps the endpoint knobs onto a node config.
func (cfg *EndpointConfig) toAsterConfig() (AsterConfig, error) {
	c := DefaultConfig()
	switch cfg.RelayMode {
	case RelayModeDefault:
		c.RelayMode = ""
	case RelayModeCustom:
		c.RelayMode = "custom"
	case RelayModeDisabled:
		c.RelayMode = "disabled"
	case RelayModeStaging:
		c.RelayMode = "staging"
	default:
		return c, fmt.Errorf("endpoint config: relay mode %d: %w", cfg.RelayMode, ErrInvalidArgument)
	}
	c.RelayURLs = cfg.RelayURLs
	c.SecretKey = cfg.SecretKey
	c.ALPNs = cfg.ALPNs
	c.LocalDiscovery = cfg.EnableDiscovery
	c.EnableHooks = cfg.EnableHooks
	c.HookTimeoutMs = cfg.HookTimeoutMs
	c.HookFailureMode = cfg.HookFailureMode
	c.BindAddr = cfg.BindAddr
	c.ClearIPTransports = cfg.ClearIPTransports
	c.ClearRelayTransports = cfg.ClearRelayTransports
	if cfg.PortMapperConfig == 1 {
		c.Portmapper = PortmapperDisabled
	}
	c.ProxyURL = cfg.ProxyURL
	c.ProxyFromEnv = cfg.ProxyFromEnv
	c.StoragePath = cfg.DataDir
	c.EnableMonitoring = true // bare endpoints always had monitoring on
	return c, nil
}

// Endpoint connects to remote nodes and accepts incoming connections on its
// ALPNs. Since ABI 4.0 it is a view of a node (see EndpointConfig); Node
// returns it for the rest of the node API.
type Endpoint struct {
	handle  uint64
	runtime *Runtime
	node    *Node
}

// Handle returns the raw handle (a node handle).
func (e *Endpoint) Handle() uint64 {
	return e.handle
}

// Node returns the node behind this endpoint.
func (e *Endpoint) Node() *Node { return e.node }

// NewEndpoint starts a node on runtime configured from cfg and returns it as
// an Endpoint. The endpoint holds a reference to runtime until Close.
func NewEndpoint(ctx context.Context, runtime *Runtime, cfg EndpointConfig) (*Endpoint, error) {
	acfg, err := cfg.toAsterConfig()
	if err != nil {
		return nil, err
	}
	node, err := StartNode(ctx, acfg, NodeOptions{Runtime: runtime})
	if err != nil {
		return nil, fmt.Errorf("endpoint create: %w", err)
	}
	return &Endpoint{handle: node.handle, runtime: node.runtime, node: node}, nil
}

// NodeID returns this endpoint's node identifier as a hex string.
func (e *Endpoint) NodeID() (string, error) { return e.node.NodeID() }

// ConnectNodeAddr connects to a remote node using the given NodeAddr and ALPN.
// This allows direct peer connections without requiring address lookup.
func (e *Endpoint) ConnectNodeAddr(ctx context.Context, addr *NodeAddr, alpn string) (*Connection, error) {
	return connectCore(ctx, e.runtime, e.handle, addr, alpn)
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
	if err := wrapStatus("aster_connect", C.aster_connect(C.uint64_t(e.runtime.handle), C.uint64_t(e.handle), &config, 0, &opID)); err != nil {
		return nil, err
	}
	ev, err := e.runtime.await(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("connect %s: %w", alpn, err)
	}
	if ev.Kind != ASTER_EVENT_CONNECTED {
		return nil, e.runtime.eventError(ev, "aster_connect")
	}
	e.runtime.releaseEventBuffer(ev)
	return &Connection{handle: ev.Handle, runtime: e.runtime}, nil
}

// Accept accepts the next incoming connection on one of the endpoint's ALPNs.
func (e *Endpoint) Accept(ctx context.Context) (*Connection, error) { return e.node.Accept(ctx) }

// Close closes the endpoint (its node) and releases the runtime reference.
func (e *Endpoint) Close(ctx context.Context) error {
	_ = ctx
	return e.node.Close()
}

// AddrInfo returns the structured address of this endpoint.
func (e *Endpoint) AddrInfo() (*NodeAddr, error) { return e.node.NodeAddr() }

// ExportSecretKey exports this endpoint's 32-byte secret key seed.
func (e *Endpoint) ExportSecretKey() ([]byte, error) { return e.node.ExportSecretKey() }

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
