//go:build cgo

package aster

import (
	"context"
	"io"
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
	cfg.AllowAllConsumers = true
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
}
