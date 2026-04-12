//go:build cgo

package aster

import (
	"encoding/base64"
	"encoding/hex"
	"fmt"
	"os"
	"strconv"
	"strings"

	"github.com/BurntSushi/toml"
)

// AsterConfig is the unified configuration for AsterServer.
//
// Three-layer resolution (later wins):
//  1. Built-in defaults (ephemeral key, in-memory store, all gates open).
//  2. TOML config file (aster.toml) -- requires a TOML library (future).
//  3. ASTER_* environment variables.
type AsterConfig struct {
	// Trust
	RootPubkey               []byte
	RootPubkeyFile           string
	EnrollmentCredentialFile string
	EnrollmentCredentialIID  string
	AllowAllConsumers        bool
	AllowAllProducers        bool

	// Connect
	EndpointAddr string

	// Storage
	StoragePath string

	// Network
	SecretKey            []byte
	RelayMode            string
	BindAddr             string
	EnableMonitoring     bool
	EnableHooks          bool
	HookTimeoutMs        int
	ClearIpTransports    bool
	ClearRelayTransports bool
	PortmapperConfig     string
	ProxyUrl             string
	ProxyFromEnv         bool
	LocalDiscovery       bool

	// Logging
	LogFormat string // "json" or "text"
	LogLevel  string // "debug", "info", "warning", "error"
	LogMask   bool

	// Identity
	IdentityFile string
}

// DefaultAsterConfig returns an AsterConfig with built-in defaults.
func DefaultAsterConfig() AsterConfig {
	return AsterConfig{
		AllowAllProducers: true,
		HookTimeoutMs:     5000,
		LogFormat:          "text",
		LogLevel:           "info",
		LogMask:            true,
	}
}

// LoadFromEnv returns a config with defaults overridden by ASTER_* env vars.
func LoadFromEnv() AsterConfig {
	c := DefaultAsterConfig()
	c.ApplyEnv()
	return c
}

// LoadFromFile returns a config loaded from an aster.toml file, with
// ASTER_* env var overrides (env wins).
func LoadFromFile(path string) (AsterConfig, error) {
	c := DefaultAsterConfig()
	if err := c.ApplyToml(path); err != nil {
		return c, err
	}
	c.ApplyEnv()
	return c, nil
}

// tomlConfig mirrors the aster.toml structure for BurntSushi/toml decoding.
type tomlConfig struct {
	Trust   tomlTrust   `toml:"trust"`
	Connect tomlConnect `toml:"connect"`
	Storage tomlStorage `toml:"storage"`
	Network tomlNetwork `toml:"network"`
	Logging tomlLogging `toml:"logging"`
}
type tomlTrust struct {
	RootPubkey              string `toml:"root_pubkey"`
	RootPubkeyFile          string `toml:"root_pubkey_file"`
	EnrollmentCredential    string `toml:"enrollment_credential"`
	EnrollmentCredentialIID string `toml:"enrollment_credential_iid"`
	AllowAllConsumers       *bool  `toml:"allow_all_consumers"`
	AllowAllProducers       *bool  `toml:"allow_all_producers"`
}
type tomlConnect struct {
	EndpointAddr string `toml:"endpoint_addr"`
}
type tomlStorage struct {
	Path string `toml:"path"`
}
type tomlNetwork struct {
	SecretKey            string `toml:"secret_key"`
	RelayMode            string `toml:"relay_mode"`
	BindAddr             string `toml:"bind_addr"`
	EnableMonitoring     *bool  `toml:"enable_monitoring"`
	EnableHooks          *bool  `toml:"enable_hooks"`
	HookTimeoutMs        *int   `toml:"hook_timeout_ms"`
	ClearIpTransports    *bool  `toml:"clear_ip_transports"`
	ClearRelayTransports *bool  `toml:"clear_relay_transports"`
	PortmapperConfig     string `toml:"portmapper_config"`
	ProxyUrl             string `toml:"proxy_url"`
	ProxyFromEnv         *bool  `toml:"proxy_from_env"`
	LocalDiscovery       *bool  `toml:"local_discovery"`
}
type tomlLogging struct {
	Format string `toml:"format"`
	Level  string `toml:"level"`
	Mask   *bool  `toml:"mask"`
}

// ApplyToml loads values from an aster.toml file. Call before ApplyEnv
// so that env vars take precedence.
func (c *AsterConfig) ApplyToml(path string) error {
	var tc tomlConfig
	if _, err := toml.DecodeFile(path, &tc); err != nil {
		return fmt.Errorf("parsing %s: %w", path, err)
	}

	// Trust
	if tc.Trust.RootPubkey != "" {
		if b, err := hex.DecodeString(tc.Trust.RootPubkey); err == nil {
			c.RootPubkey = b
		}
	}
	if tc.Trust.RootPubkeyFile != "" {
		c.RootPubkeyFile = tc.Trust.RootPubkeyFile
	}
	if tc.Trust.EnrollmentCredential != "" {
		c.EnrollmentCredentialFile = tc.Trust.EnrollmentCredential
	}
	if tc.Trust.EnrollmentCredentialIID != "" {
		c.EnrollmentCredentialIID = tc.Trust.EnrollmentCredentialIID
	}
	if tc.Trust.AllowAllConsumers != nil {
		c.AllowAllConsumers = *tc.Trust.AllowAllConsumers
	}
	if tc.Trust.AllowAllProducers != nil {
		c.AllowAllProducers = *tc.Trust.AllowAllProducers
	}

	// Connect
	if tc.Connect.EndpointAddr != "" {
		c.EndpointAddr = tc.Connect.EndpointAddr
	}

	// Storage
	if tc.Storage.Path != "" {
		c.StoragePath = tc.Storage.Path
	}

	// Network
	if tc.Network.SecretKey != "" {
		if b, err := base64.StdEncoding.DecodeString(tc.Network.SecretKey); err == nil {
			c.SecretKey = b
		}
	}
	if tc.Network.RelayMode != "" {
		c.RelayMode = tc.Network.RelayMode
	}
	if tc.Network.BindAddr != "" {
		c.BindAddr = tc.Network.BindAddr
	}
	if tc.Network.PortmapperConfig != "" {
		c.PortmapperConfig = tc.Network.PortmapperConfig
	}
	if tc.Network.ProxyUrl != "" {
		c.ProxyUrl = tc.Network.ProxyUrl
	}
	if tc.Network.EnableMonitoring != nil {
		c.EnableMonitoring = *tc.Network.EnableMonitoring
	}
	if tc.Network.EnableHooks != nil {
		c.EnableHooks = *tc.Network.EnableHooks
	}
	if tc.Network.HookTimeoutMs != nil {
		c.HookTimeoutMs = *tc.Network.HookTimeoutMs
	}
	if tc.Network.ClearIpTransports != nil {
		c.ClearIpTransports = *tc.Network.ClearIpTransports
	}
	if tc.Network.ClearRelayTransports != nil {
		c.ClearRelayTransports = *tc.Network.ClearRelayTransports
	}
	if tc.Network.ProxyFromEnv != nil {
		c.ProxyFromEnv = *tc.Network.ProxyFromEnv
	}
	if tc.Network.LocalDiscovery != nil {
		c.LocalDiscovery = *tc.Network.LocalDiscovery
	}

	// Logging
	if tc.Logging.Format != "" {
		c.LogFormat = strings.ToLower(tc.Logging.Format)
	}
	if tc.Logging.Level != "" {
		c.LogLevel = strings.ToLower(tc.Logging.Level)
	}
	if tc.Logging.Mask != nil {
		c.LogMask = *tc.Logging.Mask
	}

	return nil
}

// ApplyEnv overrides fields from ASTER_* environment variables.
func (c *AsterConfig) ApplyEnv() {
	envStr("ASTER_ROOT_PUBKEY_FILE", &c.RootPubkeyFile)
	envStr("ASTER_ENROLLMENT_CREDENTIAL", &c.EnrollmentCredentialFile)
	envStr("ASTER_ENROLLMENT_CREDENTIAL_IID", &c.EnrollmentCredentialIID)
	envStr("ASTER_ENDPOINT_ADDR", &c.EndpointAddr)
	envStr("ASTER_STORAGE_PATH", &c.StoragePath)
	envStr("ASTER_RELAY_MODE", &c.RelayMode)
	envStr("ASTER_BIND_ADDR", &c.BindAddr)
	envStr("ASTER_PORTMAPPER_CONFIG", &c.PortmapperConfig)
	envStr("ASTER_PROXY_URL", &c.ProxyUrl)
	envStr("ASTER_IDENTITY_FILE", &c.IdentityFile)
	envStr("ASTER_LOG_FORMAT", &c.LogFormat)
	envStr("ASTER_LOG_LEVEL", &c.LogLevel)

	envBool("ASTER_ALLOW_ALL_CONSUMERS", &c.AllowAllConsumers)
	envBool("ASTER_ALLOW_ALL_PRODUCERS", &c.AllowAllProducers)
	envBool("ASTER_ENABLE_MONITORING", &c.EnableMonitoring)
	envBool("ASTER_ENABLE_HOOKS", &c.EnableHooks)
	envBool("ASTER_CLEAR_IP_TRANSPORTS", &c.ClearIpTransports)
	envBool("ASTER_CLEAR_RELAY_TRANSPORTS", &c.ClearRelayTransports)
	envBool("ASTER_PROXY_FROM_ENV", &c.ProxyFromEnv)
	envBool("ASTER_LOCAL_DISCOVERY", &c.LocalDiscovery)
	envBool("ASTER_LOG_MASK", &c.LogMask)

	envInt("ASTER_HOOK_TIMEOUT_MS", &c.HookTimeoutMs)

	envHex("ASTER_ROOT_PUBKEY", &c.RootPubkey)
	envBase64("ASTER_SECRET_KEY", &c.SecretKey)
}

// ToEndpointConfig converts network fields to an EndpointConfig for the FFI.
func (c *AsterConfig) ToEndpointConfig() EndpointConfig {
	ec := EndpointConfig{
		ALPNs: []string{AsterALPN},
	}
	if c.SecretKey != nil {
		ec.SecretKey = c.SecretKey
	}
	if c.BindAddr != "" {
		ec.BindAddr = c.BindAddr
	}
	ec.EnableDiscovery = c.LocalDiscovery
	ec.EnableHooks = c.EnableHooks
	ec.ClearRelayTransports = c.ClearRelayTransports
	return ec
}

// ResolveRootPubkey returns the root public key, trying RootPubkey then
// RootPubkeyFile. Returns nil if neither is set or the file is unreadable.
func (c *AsterConfig) ResolveRootPubkey() []byte {
	if c.RootPubkey != nil {
		return c.RootPubkey
	}
	if c.RootPubkeyFile != "" {
		if data, err := os.ReadFile(c.RootPubkeyFile); err == nil {
			content := strings.TrimSpace(string(data))
			if len(content) == 64 {
				if b, err := hex.DecodeString(content); err == nil {
					c.RootPubkey = b
					return b
				}
			}
		}
	}
	return nil
}

// ── env helpers ──────────────────────────────────────────────────────────

func envStr(key string, dst *string) {
	if v, ok := os.LookupEnv(key); ok {
		*dst = strings.TrimSpace(v)
	}
}

func envBool(key string, dst *bool) {
	v, ok := os.LookupEnv(key)
	if !ok {
		return
	}
	switch strings.ToLower(strings.TrimSpace(v)) {
	case "true", "1", "yes", "on":
		*dst = true
	case "false", "0", "no", "off":
		*dst = false
	}
}

func envInt(key string, dst *int) {
	if v, ok := os.LookupEnv(key); ok {
		if n, err := strconv.Atoi(strings.TrimSpace(v)); err == nil {
			*dst = n
		}
	}
}

func envHex(key string, dst *[]byte) {
	if v, ok := os.LookupEnv(key); ok {
		v = strings.TrimSpace(v)
		if b, err := hex.DecodeString(v); err == nil {
			*dst = b
		}
	}
}

func envBase64(key string, dst *[]byte) {
	if v, ok := os.LookupEnv(key); ok {
		v = strings.TrimSpace(v)
		if b, err := base64.StdEncoding.DecodeString(v); err == nil {
			*dst = b
		}
	}
}
