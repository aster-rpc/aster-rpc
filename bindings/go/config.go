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
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"strconv"
	"strings"

	"github.com/BurntSushi/toml"
)

// Portmapper selects NAT port mapping (aster_portmapper_t).
type Portmapper uint32

const (
	PortmapperDefault  Portmapper = C.ASTER_PORTMAPPER_DEFAULT // unset: the Rust default
	PortmapperEnabled  Portmapper = C.ASTER_PORTMAPPER_ENABLED
	PortmapperDisabled Portmapper = C.ASTER_PORTMAPPER_DISABLED
)

// TransportConfig tunes QUIC (aster_config_t.transport_*). Nil keeps the Rust
// default.
type TransportConfig struct {
	MaxConcurrentBidiStreams  *uint64
	MaxConcurrentUniStreams   *uint64
	StreamReceiveWindow       *uint64
	ReceiveWindow             *uint64
	SendWindow                *uint64
	MaxIdleTimeoutMs          *uint64
	KeepAliveIntervalMs       *uint64
	InitialMtu                *uint32
	DatagramReceiveBufferSize *uint64
	DatagramSendBufferSize    *uint64
	SendFairness              *bool
	EnableSegmentationOffload *bool
}

// AsterConfig configures a node (and an AsterServer / AsterClient). The node
// knobs map one-to-one onto the C ABI aster_config_t and are resolved by the
// Rust aster::AsterConfig, so ConfigFromEnv / ConfigFromTOML read ASTER_*
// variables and aster.toml exactly as the Rust SDK and the other bindings do.
//
//	cfg, err := aster.ConfigFromEnv()
//	cfg.RelayMode, cfg.BindAddr = "disabled", "127.0.0.1:0"
//	node, err := aster.StartNode(ctx, cfg, aster.NodeOptions{})
//
// The binding-level settings at the end (trust, connect, logging) are read
// from the same sources by this package.
type AsterConfig struct {
	// RelayMode: "" / "default", "disabled", "staging", "custom" (with
	// RelayURLs), or a relay URL (= custom with that URL).
	RelayMode string
	RelayURLs []string
	// SecretKey is a 32-byte Ed25519 secret key; nil = SecretKeyFile or ephemeral.
	SecretKey     []byte
	SecretKeyFile string
	// StoragePath is the persistent store directory; "" = in memory.
	StoragePath  string
	GCIntervalMs uint64
	// BindAddr, e.g. "127.0.0.1:0"; "" = all interfaces.
	BindAddr string
	// ALPNs the node accepts, in addition to those added by a server/client.
	ALPNs        []string
	IdentityFile string
	ProxyURL     string
	ProxyFromEnv bool
	// HookTimeoutMs is the admission-hook timeout; 0 = 5000 ms.
	HookTimeoutMs   uint64
	HookFailureMode uint32 // HookFailOpen (default) or HookFailClosed
	// EnableHooks raises admission events (Node.Admissions) when no Gate is attached.
	EnableHooks          bool
	EnableMonitoring     bool
	LocalDiscovery       bool
	ClearIPTransports    bool
	ClearRelayTransports bool
	Portmapper           Portmapper
	Transport            TransportConfig
	// BlobZstdLevel is the zstd level for blob transport (Rust
	// AsterConfig::blob_zstd_level): nil = the default (3), 0 = off (the node
	// serves and fetches blobs on the plain ALPN only), 1..19 = that level
	// (larger values are clamped to 19). Env ASTER_BLOB_ZSTD_LEVEL; TOML
	// [storage] blob_zstd_level.
	BlobZstdLevel *int
	// ReuseLastPort re-binds a persistent node's last UDP ports on restart
	// (Rust AsterConfig::reuse_last_port): nil = the default (on). Env
	// ASTER_REUSE_LAST_PORT; TOML [network] reuse_last_port.
	ReuseLastPort *bool

	// ── Trust and Enrollment ([trust], [enrollment]; design §15.3) ──
	//
	// Read by ConfigFromEnv / ConfigFromTOML through the Rust
	// aster::AsterConfig, so names, defaults and precedence (defaults < TOML
	// < environment < code) are the Rust SDK's exactly. A node is open
	// exactly when enrollment is not configured (IsEnrollmentEnabled).

	// RootPubkey (32 bytes) or RootPubkeyFile (64 hex, or JSON
	// {"public_key": "<hex>"}) is the trusted root. Env
	// ASTER_ROOT_PUBKEY(_FILE); TOML [trust] root_pubkey(_file).
	RootPubkey     []byte
	RootPubkeyFile string
	// NextRootCommitment (64 hex), RootSeq and RootRotations are the root
	// rotation pin; without the commitment every rotation statement is
	// refused. Env ASTER_NEXT_ROOT_COMMITMENT, ASTER_ROOT_SEQ,
	// ASTER_ROOT_ROTATIONS (comma-separated); TOML [trust]
	// next_root_commitment, root_seq, root_rotations.
	NextRootCommitment string
	RootSeq            uint64
	RootRotations      []string
	// EnrollmentCredentialFile is this node's credential chain file (aster
	// enroll node --out); EnrollmentCredentialChain is the chain inline and
	// wins over the file. Either enables enrollment. Env
	// ASTER_ENROLLMENT_CREDENTIAL / ASTER_ENROLLMENT_CREDENTIAL_CHAIN; TOML
	// [trust] enrollment_credential / enrollment_credential_chain.
	EnrollmentCredentialFile  string
	EnrollmentCredentialChain string
	// Enrollment is the [enrollment] section (ASTER_ENROLLMENT_*).
	Enrollment EnrollmentConfig

	// ── Binding-level settings ──

	EndpointAddr string
	LogFormat    string // "text" or "json"
	LogLevel     string
	LogMask      bool

	// Ops listener (/healthz, /readyz, /metrics; Rust aster::ops). Env
	// ASTER_HEALTH_PORT / _HOST / _ENABLED; TOML [health] port / host / enabled.
	// HealthPort 0 = disabled unless HealthEnabled is true (then ephemeral).
	HealthPort uint16
	// HealthHost: "" = 127.0.0.1 (localhost only; pod probes need "0.0.0.0").
	HealthHost string
	// HealthEnabled forces the listener on or off; nil = on when HealthPort != 0.
	HealthEnabled *bool
}

// IsHealthEnabled reports whether StartServer starts the ops listener from
// this config: HealthEnabled when set, else HealthPort != 0.
func (c *AsterConfig) IsHealthEnabled() bool {
	if c.HealthEnabled != nil {
		return *c.HealthEnabled
	}
	return c.HealthPort != 0
}

func bindingDefaults(c *AsterConfig) {
	c.LogFormat = "text"
	c.LogLevel = "info"
	c.LogMask = true
	c.HealthHost = "127.0.0.1"
}

// DefaultConfig returns the Rust defaults (aster_config_init).
func DefaultConfig() AsterConfig {
	var n C.aster_config_t
	C.aster_config_init(&n)
	var c AsterConfig
	bindingDefaults(&c)
	c.fromNative(&n)
	return c
}

// ConfigFromEnv is Rust AsterConfig::from_env plus the binding-level ASTER_*
// variables. A malformed variable is an error (ErrInvalidArgument).
func ConfigFromEnv() (AsterConfig, error) {
	var n C.aster_config_t
	if err := ffiStatus(func() C.int32_t { return C.aster_config_from_env(&n) }); err != nil {
		return AsterConfig{}, fmt.Errorf("aster_config_from_env: %w", err)
	}
	defer C.aster_config_release(&n)
	var c AsterConfig
	bindingDefaults(&c)
	c.fromNative(&n)
	if err := c.applyBindingEnv(); err != nil {
		return AsterConfig{}, err
	}
	var trust C.aster_bytes_t
	if err := wrapCall("aster_config_enrollment_from_env", func() C.int32_t { return C.aster_config_enrollment_from_env(&trust) }); err != nil {
		return AsterConfig{}, err
	}
	if err := c.applyTrustJSON(trust); err != nil {
		return AsterConfig{}, err
	}
	return c, nil
}

// ConfigFromTOML is Rust AsterConfig::from_file (the file, then ASTER_*
// overrides) plus the file's [trust] / [connect] / [logging] sections and
// their env overrides.
func ConfigFromTOML(path string) (AsterConfig, error) {
	var a cArena
	defer a.free()
	var n C.aster_config_t
	if err := ffiStatus(func() C.int32_t { return C.aster_config_from_toml(a.str(path), &n) }); err != nil {
		return AsterConfig{}, fmt.Errorf("aster_config_from_toml(%s): %w", path, err)
	}
	defer C.aster_config_release(&n)
	var c AsterConfig
	bindingDefaults(&c)
	c.fromNative(&n)
	if err := c.applyBindingTOML(path); err != nil {
		return AsterConfig{}, err
	}
	if err := c.applyBindingEnv(); err != nil {
		return AsterConfig{}, err
	}
	var trust C.aster_bytes_t
	if err := wrapCall("aster_config_enrollment_from_toml", func() C.int32_t { return C.aster_config_enrollment_from_toml(a.str(path), &trust) }); err != nil {
		return AsterConfig{}, fmt.Errorf("%s: %w", path, err)
	}
	if err := c.applyTrustJSON(trust); err != nil {
		return AsterConfig{}, err
	}
	return c, nil
}

// ResolveRootPubkey returns RootPubkey, else the key in RootPubkeyFile, or nil
// when neither yields a 32-byte key (RootKey reports why).
func (c *AsterConfig) ResolveRootPubkey() []byte {
	key, err := c.RootKey()
	if err != nil {
		return nil
	}
	return key
}

// RootKey returns the configured root public key: RootPubkey, else the key in
// RootPubkeyFile (64 hex, or JSON {"public_key": "<hex>"}, as the Rust config
// reads it); nil when neither is set. When both are set they must agree.
func (c *AsterConfig) RootKey() ([]byte, error) {
	if c.RootPubkey != nil && len(c.RootPubkey) != 32 {
		return nil, fmt.Errorf("RootPubkey: %d bytes, want 32: %w", len(c.RootPubkey), ErrInvalidArgument)
	}
	if c.RootPubkeyFile == "" {
		return c.RootPubkey, nil
	}
	data, err := os.ReadFile(c.RootPubkeyFile)
	if err != nil {
		return nil, fmt.Errorf("RootPubkeyFile: %w", err)
	}
	text := strings.TrimSpace(string(data))
	if strings.HasPrefix(text, "{") {
		var wrapped struct {
			PublicKey string `json:"public_key"`
		}
		if err := json.Unmarshal([]byte(text), &wrapped); err != nil {
			return nil, fmt.Errorf("RootPubkeyFile %s: %w", c.RootPubkeyFile, err)
		}
		text = wrapped.PublicKey
	}
	key, err := hex.DecodeString(text)
	if err != nil || len(key) != 32 {
		return nil, fmt.Errorf("RootPubkeyFile %s: want a 64-hex key: %w", c.RootPubkeyFile, ErrInvalidArgument)
	}
	if c.RootPubkey != nil && !bytes.Equal(c.RootPubkey, key) {
		return nil, fmt.Errorf("RootPubkey and RootPubkeyFile %s hold different keys: %w", c.RootPubkeyFile, ErrInvalidArgument)
	}
	return key, nil
}

// String redacts the secret key.
func (c AsterConfig) String() string {
	key := "nil"
	if c.SecretKey != nil {
		key = "<redacted>"
	}
	return fmt.Sprintf("AsterConfig{RelayMode:%q BindAddr:%q StoragePath:%q ALPNs:%v SecretKey:%s EnableHooks:%v Enrollment:%v}",
		c.RelayMode, c.BindAddr, c.StoragePath, c.ALPNs, key, c.EnableHooks, c.IsEnrollmentEnabled())
}

// ─── aster_config_t marshalling ───────────────────────────────────────────

func (c *AsterConfig) toNative(a *cArena, extraALPNs []string) (C.aster_config_t, error) {
	var n C.aster_config_t
	C.aster_config_init(&n)

	urls := append([]string(nil), c.RelayURLs...)
	mode := strings.ToLower(strings.TrimSpace(c.RelayMode))
	switch mode {
	case "", "default":
		if len(urls) == 0 {
			n.relay_mode = C.uint32_t(RelayModeDefault)
		} else {
			n.relay_mode = C.uint32_t(RelayModeCustom)
		}
	case "custom":
		n.relay_mode = C.uint32_t(RelayModeCustom)
	case "disabled":
		n.relay_mode = C.uint32_t(RelayModeDisabled)
	case "staging":
		n.relay_mode = C.uint32_t(RelayModeStaging)
	default: // a bare URL means custom (same rule as Rust)
		n.relay_mode = C.uint32_t(RelayModeCustom)
		if len(urls) == 0 {
			urls = append(urls, strings.TrimSpace(c.RelayMode))
		}
	}
	n.relay_urls = a.list(urls)
	if len(c.SecretKey) > 0 {
		if len(c.SecretKey) != 32 {
			return n, fmt.Errorf("secret key must be 32 bytes, got %d: %w", len(c.SecretKey), ErrInvalidArgument)
		}
		n.secret_key = a.bytes(c.SecretKey)
	}
	n.secret_key_file = a.str(c.SecretKeyFile)
	n.storage_path = a.str(c.StoragePath)
	n.gc_interval_ms = C.uint64_t(c.GCIntervalMs)
	n.bind_addr = a.str(c.BindAddr)
	n.alpns = a.list(mergeALPNs(c.ALPNs, extraALPNs))
	n.identity_file = a.str(c.IdentityFile)
	n.proxy_url = a.str(c.ProxyURL)
	n.hook_timeout_ms = C.uint64_t(c.HookTimeoutMs)
	n.hook_failure_mode = C.uint32_t(c.HookFailureMode)
	n.enable_hooks = boolToUint32(c.EnableHooks)
	n.enable_monitoring = boolToUint32(c.EnableMonitoring)
	n.local_discovery = boolToUint32(c.LocalDiscovery)
	n.clear_ip_transports = boolToUint32(c.ClearIPTransports)
	n.clear_relay_transports = boolToUint32(c.ClearRelayTransports)
	n.portmapper = C.uint32_t(c.Portmapper)
	n.proxy_from_env = boolToUint32(c.ProxyFromEnv)
	n.blob_zstd_level = C.ASTER_BLOB_ZSTD_LEVEL_DEFAULT
	if lvl := c.BlobZstdLevel; lvl != nil {
		switch {
		case *lvl < 0:
			return n, fmt.Errorf("BlobZstdLevel must be 0 (off) or a level, got %d: %w", *lvl, ErrInvalidArgument)
		case *lvl == 0:
			n.blob_zstd_level = C.ASTER_BLOB_ZSTD_LEVEL_OFF
		default:
			n.blob_zstd_level = C.int32_t(min(*lvl, C.ASTER_BLOB_ZSTD_LEVEL_MAX))
		}
	}
	n.reuse_last_port = C.ASTER_REUSE_LAST_PORT_DEFAULT
	if c.ReuseLastPort != nil {
		n.reuse_last_port = C.ASTER_REUSE_LAST_PORT_DISABLED
		if *c.ReuseLastPort {
			n.reuse_last_port = C.ASTER_REUSE_LAST_PORT_ENABLED
		}
	}

	var set C.uint32_t
	t := &c.Transport
	u64 := func(v *uint64, bit C.uint32_t, field *C.uint64_t) {
		if v != nil {
			*field = C.uint64_t(*v)
			set |= bit
		}
	}
	u64(t.MaxConcurrentBidiStreams, C.ASTER_TRANSPORT_MAX_CONCURRENT_BIDI_STREAMS, &n.transport_max_concurrent_bidi_streams)
	u64(t.MaxConcurrentUniStreams, C.ASTER_TRANSPORT_MAX_CONCURRENT_UNI_STREAMS, &n.transport_max_concurrent_uni_streams)
	u64(t.StreamReceiveWindow, C.ASTER_TRANSPORT_STREAM_RECEIVE_WINDOW, &n.transport_stream_receive_window)
	u64(t.ReceiveWindow, C.ASTER_TRANSPORT_RECEIVE_WINDOW, &n.transport_receive_window)
	u64(t.SendWindow, C.ASTER_TRANSPORT_SEND_WINDOW, &n.transport_send_window)
	u64(t.MaxIdleTimeoutMs, C.ASTER_TRANSPORT_MAX_IDLE_TIMEOUT_MS, &n.transport_max_idle_timeout_ms)
	u64(t.KeepAliveIntervalMs, C.ASTER_TRANSPORT_KEEP_ALIVE_INTERVAL_MS, &n.transport_keep_alive_interval_ms)
	u64(t.DatagramReceiveBufferSize, C.ASTER_TRANSPORT_DATAGRAM_RECEIVE_BUFFER_SIZE, &n.transport_datagram_receive_buffer_size)
	u64(t.DatagramSendBufferSize, C.ASTER_TRANSPORT_DATAGRAM_SEND_BUFFER_SIZE, &n.transport_datagram_send_buffer_size)
	if t.InitialMtu != nil {
		n.transport_initial_mtu = C.uint32_t(*t.InitialMtu)
		set |= C.ASTER_TRANSPORT_INITIAL_MTU
	}
	if t.SendFairness != nil {
		n.transport_send_fairness = boolToUint32(*t.SendFairness)
		set |= C.ASTER_TRANSPORT_SEND_FAIRNESS
	}
	if t.EnableSegmentationOffload != nil {
		n.transport_enable_segmentation_offload = boolToUint32(*t.EnableSegmentationOffload)
		set |= C.ASTER_TRANSPORT_ENABLE_SEGMENTATION_OFFLOAD
	}
	n.transport_set = set
	return n, nil
}

func mergeALPNs(lists ...[]string) []string {
	seen := map[string]bool{}
	var out []string
	for _, l := range lists {
		for _, a := range l {
			if a != "" && !seen[a] {
				seen[a] = true
				out = append(out, a)
			}
		}
	}
	return out
}

func (c *AsterConfig) fromNative(n *C.aster_config_t) {
	switch uint32(n.relay_mode) {
	case RelayModeCustom:
		c.RelayMode = "custom"
	case RelayModeDisabled:
		c.RelayMode = "disabled"
	case RelayModeStaging:
		c.RelayMode = "staging"
	default:
		c.RelayMode = "default"
	}
	c.RelayURLs = readList(n.relay_urls)
	c.SecretKey = viewBytes(n.secret_key)
	c.SecretKeyFile = string(viewBytes(n.secret_key_file))
	c.StoragePath = string(viewBytes(n.storage_path))
	c.GCIntervalMs = uint64(n.gc_interval_ms)
	c.BindAddr = string(viewBytes(n.bind_addr))
	c.ALPNs = readList(n.alpns)
	c.IdentityFile = string(viewBytes(n.identity_file))
	c.ProxyURL = string(viewBytes(n.proxy_url))
	c.HookTimeoutMs = uint64(n.hook_timeout_ms)
	c.HookFailureMode = uint32(n.hook_failure_mode)
	c.EnableHooks = n.enable_hooks != 0
	c.EnableMonitoring = n.enable_monitoring != 0
	c.LocalDiscovery = n.local_discovery != 0
	c.ClearIPTransports = n.clear_ip_transports != 0
	c.ClearRelayTransports = n.clear_relay_transports != 0
	c.Portmapper = Portmapper(n.portmapper)
	c.ProxyFromEnv = n.proxy_from_env != 0
	c.BlobZstdLevel = nil
	switch lvl := int(n.blob_zstd_level); {
	case lvl == C.ASTER_BLOB_ZSTD_LEVEL_OFF:
		off := 0
		c.BlobZstdLevel = &off
	case lvl > 0:
		c.BlobZstdLevel = &lvl
	}
	c.ReuseLastPort = nil
	switch n.reuse_last_port {
	case C.ASTER_REUSE_LAST_PORT_ENABLED, C.ASTER_REUSE_LAST_PORT_DISABLED:
		reuse := n.reuse_last_port == C.ASTER_REUSE_LAST_PORT_ENABLED
		c.ReuseLastPort = &reuse
	}

	set := n.transport_set
	u64 := func(bit C.uint32_t, v C.uint64_t) *uint64 {
		if set&bit == 0 {
			return nil
		}
		x := uint64(v)
		return &x
	}
	b := func(bit C.uint32_t, v C.uint32_t) *bool {
		if set&bit == 0 {
			return nil
		}
		x := v != 0
		return &x
	}
	c.Transport = TransportConfig{
		MaxConcurrentBidiStreams:  u64(C.ASTER_TRANSPORT_MAX_CONCURRENT_BIDI_STREAMS, n.transport_max_concurrent_bidi_streams),
		MaxConcurrentUniStreams:   u64(C.ASTER_TRANSPORT_MAX_CONCURRENT_UNI_STREAMS, n.transport_max_concurrent_uni_streams),
		StreamReceiveWindow:       u64(C.ASTER_TRANSPORT_STREAM_RECEIVE_WINDOW, n.transport_stream_receive_window),
		ReceiveWindow:             u64(C.ASTER_TRANSPORT_RECEIVE_WINDOW, n.transport_receive_window),
		SendWindow:                u64(C.ASTER_TRANSPORT_SEND_WINDOW, n.transport_send_window),
		MaxIdleTimeoutMs:          u64(C.ASTER_TRANSPORT_MAX_IDLE_TIMEOUT_MS, n.transport_max_idle_timeout_ms),
		KeepAliveIntervalMs:       u64(C.ASTER_TRANSPORT_KEEP_ALIVE_INTERVAL_MS, n.transport_keep_alive_interval_ms),
		DatagramReceiveBufferSize: u64(C.ASTER_TRANSPORT_DATAGRAM_RECEIVE_BUFFER_SIZE, n.transport_datagram_receive_buffer_size),
		DatagramSendBufferSize:    u64(C.ASTER_TRANSPORT_DATAGRAM_SEND_BUFFER_SIZE, n.transport_datagram_send_buffer_size),
		SendFairness:              b(C.ASTER_TRANSPORT_SEND_FAIRNESS, n.transport_send_fairness),
		EnableSegmentationOffload: b(C.ASTER_TRANSPORT_ENABLE_SEGMENTATION_OFFLOAD, n.transport_enable_segmentation_offload),
	}
	if set&C.ASTER_TRANSPORT_INITIAL_MTU != 0 {
		mtu := uint32(n.transport_initial_mtu)
		c.Transport.InitialMtu = &mtu
	}
}

// ─── binding-level sources ─────────────────────────────────────────────────

type tomlBinding struct {
	Connect struct {
		EndpointAddr string `toml:"endpoint_addr"`
	} `toml:"connect"`
	Logging struct {
		Format string `toml:"format"`
		Level  string `toml:"level"`
		Mask   *bool  `toml:"mask"`
	} `toml:"logging"`
	Health struct {
		Port    *int64 `toml:"port"`
		Host    string `toml:"host"`
		Enabled *bool  `toml:"enabled"`
	} `toml:"health"`
}

func (c *AsterConfig) applyBindingTOML(path string) error {
	var tc tomlBinding
	if _, err := toml.DecodeFile(path, &tc); err != nil {
		return fmt.Errorf("parse %s: %w", path, err)
	}
	setStr := func(dst *string, v string) {
		if v != "" {
			*dst = v
		}
	}
	setStr(&c.EndpointAddr, tc.Connect.EndpointAddr)
	if tc.Logging.Format != "" {
		c.LogFormat = strings.ToLower(tc.Logging.Format)
	}
	if tc.Logging.Level != "" {
		c.LogLevel = strings.ToLower(tc.Logging.Level)
	}
	if tc.Logging.Mask != nil {
		c.LogMask = *tc.Logging.Mask
	}
	if tc.Health.Port != nil {
		if *tc.Health.Port < 0 || *tc.Health.Port > 65535 {
			return fmt.Errorf("%s: health.port out of range: %d: %w", path, *tc.Health.Port, ErrInvalidArgument)
		}
		c.HealthPort = uint16(*tc.Health.Port)
	}
	setStr(&c.HealthHost, tc.Health.Host)
	if tc.Health.Enabled != nil {
		v := *tc.Health.Enabled
		c.HealthEnabled = &v
	}
	return nil
}

func (c *AsterConfig) applyBindingEnv() error {
	env := func(k string) string { return strings.TrimSpace(os.Getenv(k)) }
	envBool := func(k string, dst *bool) {
		switch strings.ToLower(env(k)) {
		case "true", "1", "yes", "on":
			*dst = true
		case "false", "0", "no", "off":
			*dst = false
		}
	}
	setStr := func(dst *string, k string) {
		if v := env(k); v != "" {
			*dst = v
		}
	}
	setStr(&c.EndpointAddr, "ASTER_ENDPOINT_ADDR")
	if v := env("ASTER_LOG_FORMAT"); v != "" {
		c.LogFormat = strings.ToLower(v)
	}
	if v := env("ASTER_LOG_LEVEL"); v != "" {
		c.LogLevel = strings.ToLower(v)
	}
	envBool("ASTER_LOG_MASK", &c.LogMask)
	if v := env("ASTER_HEALTH_PORT"); v != "" {
		p, err := strconv.ParseUint(v, 10, 16)
		if err != nil {
			return fmt.Errorf("ASTER_HEALTH_PORT: expected a port number, got %q: %w", v, ErrInvalidArgument)
		}
		c.HealthPort = uint16(p)
	}
	setStr(&c.HealthHost, "ASTER_HEALTH_HOST")
	switch strings.ToLower(env("ASTER_HEALTH_ENABLED")) {
	case "true", "1", "yes", "on":
		t := true
		c.HealthEnabled = &t
	case "false", "0", "no", "off":
		f := false
		c.HealthEnabled = &f
	}
	return nil
}
