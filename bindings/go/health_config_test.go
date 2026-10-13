//go:build cgo

package aster

import (
	"context"
	"errors"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestHealthConfig_DefaultsEnvTOML(t *testing.T) {
	d := DefaultConfig()
	if d.HealthPort != 0 || d.HealthHost != "127.0.0.1" || d.HealthEnabled != nil || d.IsHealthEnabled() {
		t.Fatalf("defaults %+v", d)
	}

	t.Setenv("ASTER_HEALTH_PORT", "18080")
	t.Setenv("ASTER_HEALTH_HOST", "0.0.0.0")
	c, err := ConfigFromEnv()
	if err != nil {
		t.Fatal(err)
	}
	if c.HealthPort != 18080 || c.HealthHost != "0.0.0.0" || c.HealthEnabled != nil || !c.IsHealthEnabled() {
		t.Fatalf("from env %+v", c)
	}
	t.Setenv("ASTER_HEALTH_ENABLED", "off")
	if c, err = ConfigFromEnv(); err != nil || c.HealthEnabled == nil || *c.HealthEnabled || c.IsHealthEnabled() {
		t.Fatalf("enabled=off: %+v %v", c, err)
	}
	t.Setenv("ASTER_HEALTH_PORT", "70000")
	if _, err := ConfigFromEnv(); err == nil {
		t.Fatal("out-of-range port accepted")
	}
	t.Setenv("ASTER_HEALTH_PORT", "")
	t.Setenv("ASTER_HEALTH_HOST", "")
	t.Setenv("ASTER_HEALTH_ENABLED", "")

	path := filepath.Join(t.TempDir(), "aster.toml")
	if err := os.WriteFile(path, []byte("[health]\nport = 9091\nhost = \"10.0.0.5\"\nenabled = false\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if c, err = ConfigFromTOML(path); err != nil {
		t.Fatal(err)
	}
	if c.HealthPort != 9091 || c.HealthHost != "10.0.0.5" || c.HealthEnabled == nil || *c.HealthEnabled || c.IsHealthEnabled() {
		t.Fatalf("from toml %+v", c)
	}
}

func TestHealthConfig_ServerStartsListenerFromConfig(t *testing.T) {
	cfg := testCfg()
	on := true
	cfg.HealthEnabled = &on // port 0 + enabled: an ephemeral port
	f := newRPCFixture(t, ServerOptions{Config: cfg, Services: []*Service{probeService(&probeState{})}})
	h := f.srv.Health()
	if h == nil {
		t.Fatal("no health listener from config")
	}
	addr, err := h.Addr()
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(addr, "127.0.0.1:") {
		t.Fatalf("addr %q, want localhost", addr)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, "http://"+addr+"/healthz", nil)
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != 200 || !strings.Contains(string(body), `"status":"ok"`) {
		t.Fatalf("/healthz = %d %s", resp.StatusCode, body)
	}
	// The configuration path must attach readiness to the serving RPC server,
	// rather than merely start an independent, live health listener.
	if code, body := httpGet(t, addr, "/readyz"); code != 200 ||
		!strings.Contains(body, `"status":"ready"`) || !strings.Contains(body, `"services":1`) {
		t.Fatalf("config /readyz = %d %s", code, body)
	}
	if err := h.SetCheck("config-cache", false); err != nil {
		t.Fatal(err)
	}
	if code, body := httpGet(t, addr, "/readyz"); code != 503 || !strings.Contains(body, `"config-cache":false`) {
		t.Fatalf("config failing check = %d %s", code, body)
	}
	if err := h.SetCheck("config-cache", true); err != nil {
		t.Fatal(err)
	}
	if code, body := httpGet(t, addr, "/readyz"); code != 200 {
		t.Fatalf("config check recovery = %d %s", code, body)
	}
}

func TestHealthConfig_LiveDisabledBindPrecedenceAndReuse(t *testing.T) {
	occupied, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer occupied.Close()
	port := uint16(occupied.Addr().(*net.TCPAddr).Port)
	cfg := testCfg()
	cfg.HealthPort = port
	off := false
	cfg.HealthEnabled = &off
	disabled := newRPCFixture(t, ServerOptions{Config: cfg})
	if disabled.srv.Health() != nil {
		t.Fatal("disabled config created listener")
	}
	disabled.srv.Close()
	on := true
	cfg.HealthEnabled = &on
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	bad, err := StartServer(ctx, ServerOptions{Config: cfg})
	if err == nil {
		bad.Close()
		t.Fatal("occupied health bind succeeded")
	}
	explicit := newRPCFixture(t, ServerOptions{Config: cfg, Health: &HealthConfig{Host: "127.0.0.1", Port: 0}})
	addr, err := explicit.srv.Health().Addr()
	if err != nil {
		t.Fatal(err)
	}
	if addr == occupied.Addr().String() {
		t.Fatal("explicit health ignored")
	}
	if code, body := httpGet(t, addr, "/readyz"); code != 200 {
		t.Fatalf("ready=%d %s", code, body)
	}
	explicit.srv.Close()
	rebound, err := net.Listen("tcp", addr)
	if err != nil {
		t.Fatalf("released port: %v", err)
	}
	rebound.Close()
}

func TestHealthConfig_DirectHealthUsesNodeConfig(t *testing.T) {
	cfg := testCfg()
	on := true
	cfg.HealthEnabled = &on
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	node, err := StartNode(ctx, cfg, NodeOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer node.Close()
	// The caller can mutate its config after creation without changing this node.
	on = false
	for _, opts := range []*HealthConfig{nil, {FromNodeConfig: true}} {
		health, err := StartHealth(node, opts)
		if err != nil {
			t.Fatalf("node health config not used: %v", err)
		}
		addr, err := health.Addr()
		if err != nil {
			health.Stop()
			t.Fatal(err)
		}
		if code, body := httpGet(t, addr, "/healthz"); code != 200 {
			health.Stop()
			t.Fatalf("health %d %s", code, body)
		}
		health.Stop()
	}
}

func TestHealthConfig_DirectDisabledAllowsExplicitOverride(t *testing.T) {
	cfg := testCfg()
	off := false
	cfg.HealthEnabled = &off
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	node, err := StartNode(ctx, cfg, NodeOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer node.Close()
	for _, opts := range []*HealthConfig{nil, {FromNodeConfig: true}} {
		h, err := StartHealth(node, opts)
		if h != nil {
			h.Stop()
			t.Fatal("disabled node config started health")
		}
		if !errors.Is(err, ErrInvalidArgument) {
			t.Fatalf("disabled error: %v", err)
		}
	}
	h, err := StartHealth(node, &HealthConfig{Host: "127.0.0.1"})
	if err != nil {
		t.Fatal(err)
	}
	defer h.Stop()
	addr, err := h.Addr()
	if err != nil {
		t.Fatal(err)
	}
	if code, body := httpGet(t, addr, "/healthz"); code != 200 {
		t.Fatalf("explicit health %d %s", code, body)
	}
}
