//go:build cgo

package aster

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"testing"
	"time"
)

// C ABI 7.3 network settings: the Go fields round-trip through
// aster_config_set_network / aster_config_network, env and TOML resolve
// through the Rust AsterConfig, and a started node applies them.
func TestConfig_NetworkSettings(t *testing.T) {
	d := DefaultConfig()
	if d.Discovery != DiscoveryDefault || d.BindAddrV6 != "" || len(d.AdvertiseAddrs) != 0 || d.RelayCAOnly {
		t.Fatalf("defaults: %+v", d)
	}

	var a cArena
	defer a.free()
	c := AsterConfig{
		BindAddrV6:     "[::]:7842",
		AdvertiseAddrs: []string{"203.0.113.10:31842"},
		RelayCACerts:   []string{"/etc/aster/relay-ca.pem"},
		RelayCAOnly:    true,
		Discovery:      DiscoveryNone,
	}
	n, err := c.toNative(&a, nil)
	if err != nil {
		t.Fatal(err)
	}
	var back AsterConfig
	back.fromNative(&n)
	releaseNative(&n)
	if back.BindAddrV6 != c.BindAddrV6 || !reflect.DeepEqual(back.AdvertiseAddrs, c.AdvertiseAddrs) ||
		!reflect.DeepEqual(back.RelayCACerts, c.RelayCACerts) || !back.RelayCAOnly || back.Discovery != DiscoveryNone {
		t.Fatalf("round trip: %+v", back)
	}
	// Servers alone select custom, as in Rust.
	n, err = (&AsterConfig{DiscoveryPkarrRelays: []string{"https://dns.example.internal/pkarr"}}).toNative(&a, nil)
	if err != nil {
		t.Fatal(err)
	}
	back = AsterConfig{}
	back.fromNative(&n)
	releaseNative(&n)
	if back.Discovery != DiscoveryCustom || len(back.DiscoveryPkarrRelays) != 1 {
		t.Fatalf("custom: %+v", back)
	}
	for _, bad := range []AsterConfig{
		{Discovery: "mdns"},
		{Discovery: DiscoveryNone, DiscoveryDNSOrigin: "dns.example.internal"},
	} {
		if n, err := bad.toNative(&a, nil); !errors.Is(err, ErrInvalidArgument) {
			releaseNative(&n)
			t.Fatalf("%+v: %v", bad, err)
		}
	}

	t.Setenv("ASTER_BIND_ADDR_V6", "[::]:7842")
	t.Setenv("ASTER_ADVERTISE_ADDRS", "203.0.113.10:31842, 198.51.100.7:31842")
	t.Setenv("ASTER_RELAY_CA_CERTS", "/etc/aster/relay-ca.pem")
	t.Setenv("ASTER_RELAY_CA_ONLY", "1")
	t.Setenv("ASTER_DISCOVERY", "none")
	env, err := ConfigFromEnv()
	if err != nil {
		t.Fatal(err)
	}
	if env.BindAddrV6 != "[::]:7842" || len(env.AdvertiseAddrs) != 2 || !slices.Equal(env.RelayCACerts, []string{"/etc/aster/relay-ca.pem"}) ||
		!env.RelayCAOnly || env.Discovery != DiscoveryNone {
		t.Fatalf("from env: %+v", env)
	}
	for _, k := range []string{"ASTER_BIND_ADDR_V6", "ASTER_ADVERTISE_ADDRS", "ASTER_RELAY_CA_CERTS", "ASTER_RELAY_CA_ONLY", "ASTER_DISCOVERY"} {
		t.Setenv(k, "")
	}
	path := filepath.Join(t.TempDir(), "aster.toml")
	body := "[network]\nbind_addr_v6 = \"[::]:7843\"\nadvertise_addrs = [\"203.0.113.10:31843\"]\n" +
		"discovery_dns_origin = \"dns.example.internal\"\ndiscovery_pkarr_relays = [\"https://dns.example.internal/pkarr\"]\n"
	if err := os.WriteFile(path, []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
	file, err := ConfigFromTOML(path)
	if err != nil {
		t.Fatal(err)
	}
	if file.BindAddrV6 != "[::]:7843" || file.Discovery != DiscoveryCustom || file.DiscoveryDNSOrigin != "dns.example.internal" ||
		len(file.DiscoveryPkarrRelays) != 1 || len(file.AdvertiseAddrs) != 1 {
		t.Fatalf("from toml: %+v", file)
	}
}

func TestStartNode_AdvertisesAddressWithDiscoveryNone(t *testing.T) {
	const advertised = "203.0.113.10:31842"
	cfg := DefaultConfig()
	cfg.RelayMode, cfg.BindAddr = "disabled", "127.0.0.1:0"
	cfg.AdvertiseAddrs = []string{advertised}
	cfg.Discovery = DiscoveryNone
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	node, err := StartNode(ctx, cfg, NodeOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer node.Close()
	deadline := time.Now().Add(10 * time.Second)
	for {
		addr, err := node.NodeAddr()
		if err != nil {
			t.Fatal(err)
		}
		if slices.Contains(addr.DirectAddresses, advertised) {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("advertised address missing from %v", addr.DirectAddresses)
		}
		time.Sleep(50 * time.Millisecond)
	}

	bad := cfg
	bad.RelayCACerts = []string{"/nonexistent/aster-relay-ca.pem"}
	if n, err := StartNode(ctx, bad, NodeOptions{}); err == nil {
		n.Close()
		t.Fatal("a missing relay CA file must fail the start")
	}
}
