//go:build cgo

package aster

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"
	"time"
)

// zstdBlobsALPN is the compressed blobs ALPN a node serves unless
// BlobZstdLevel is 0 (aster::alpns::BLOBS_ZSTD).
const zstdBlobsALPN = "/iroh-bytes/4+zstd/1"

func ptrInt(v int) *int { return &v }

func TestConfig_BlobZstdLevelAndReuseLastPort(t *testing.T) {
	d := DefaultConfig()
	if d.BlobZstdLevel == nil || *d.BlobZstdLevel != 3 || d.ReuseLastPort == nil || !*d.ReuseLastPort {
		t.Fatalf("defaults: level %v reuse %v", d.BlobZstdLevel, d.ReuseLastPort)
	}

	// nil = the C default, 0 = off, n = level (clamped), and back.
	var a cArena
	defer a.free()
	for _, tc := range []struct {
		level     *int
		reuse     *bool
		wantLevel int32
		wantReuse uint32
	}{
		{nil, nil, 0, 0},                   // ASTER_BLOB_ZSTD_LEVEL_DEFAULT, ASTER_REUSE_LAST_PORT_DEFAULT
		{ptrInt(0), ptrBool(false), -1, 2}, // ASTER_BLOB_ZSTD_LEVEL_OFF, ASTER_REUSE_LAST_PORT_DISABLED
		{ptrInt(7), ptrBool(true), 7, 1},   // ASTER_REUSE_LAST_PORT_ENABLED
		{ptrInt(40), nil, 19, 0},
	} {
		c := AsterConfig{BlobZstdLevel: tc.level, ReuseLastPort: tc.reuse}
		n, err := c.toNative(&a, nil)
		if err != nil {
			t.Fatal(err)
		}
		if int32(n.blob_zstd_level) != tc.wantLevel || uint32(n.reuse_last_port) != tc.wantReuse {
			t.Fatalf("%v/%v -> level %d reuse %d", tc.level, tc.reuse, n.blob_zstd_level, n.reuse_last_port)
		}
		var back AsterConfig
		back.fromNative(&n)
		if (tc.level == nil) != (back.BlobZstdLevel == nil) || (tc.reuse == nil) != (back.ReuseLastPort == nil) {
			t.Fatalf("round trip %v/%v -> %v/%v", tc.level, tc.reuse, back.BlobZstdLevel, back.ReuseLastPort)
		}
	}
	if _, err := (&AsterConfig{BlobZstdLevel: ptrInt(-1)}).toNative(&a, nil); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("negative level: %v", err)
	}

	// Env and TOML resolve through Rust AsterConfig.
	t.Setenv("ASTER_BLOB_ZSTD_LEVEL", "off")
	t.Setenv("ASTER_REUSE_LAST_PORT", "false")
	c, err := ConfigFromEnv()
	if err != nil {
		t.Fatal(err)
	}
	if c.BlobZstdLevel == nil || *c.BlobZstdLevel != 0 || c.ReuseLastPort == nil || *c.ReuseLastPort {
		t.Fatalf("from env: level %v reuse %v", c.BlobZstdLevel, c.ReuseLastPort)
	}
	t.Setenv("ASTER_BLOB_ZSTD_LEVEL", "")
	t.Setenv("ASTER_REUSE_LAST_PORT", "")
	path := filepath.Join(t.TempDir(), "aster.toml")
	if err := os.WriteFile(path, []byte("[storage]\nblob_zstd_level = 9\n\n[network]\nreuse_last_port = false\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	c, err = ConfigFromTOML(path)
	if err != nil {
		t.Fatal(err)
	}
	if c.BlobZstdLevel == nil || *c.BlobZstdLevel != 9 || c.ReuseLastPort == nil || *c.ReuseLastPort {
		t.Fatalf("from toml: level %v reuse %v", c.BlobZstdLevel, c.ReuseLastPort)
	}
}

// BlobZstdLevel 0 reaches core: the node no longer serves the zstd blobs
// ALPN, so a dial on it fails the handshake.
func TestNode_BlobZstdOffDropsTheZstdALPN(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	client, err := StartNode(ctx, testCfg(), NodeOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer client.Close()
	for _, tc := range []struct {
		level  *int
		served bool
	}{{nil, true}, {ptrInt(0), false}} {
		cfg := testCfg()
		cfg.BlobZstdLevel = tc.level
		server, err := StartNode(ctx, cfg, NodeOptions{})
		if err != nil {
			t.Fatal(err)
		}
		addr, err := server.NodeAddr()
		if err != nil {
			t.Fatal(err)
		}
		dctx, dcancel := context.WithTimeout(ctx, 15*time.Second)
		conn, err := client.Connect(dctx, addr, zstdBlobsALPN)
		dcancel()
		if (err == nil) != tc.served {
			t.Fatalf("level %v: served=%v, dial err %v", tc.level, tc.served, err)
		}
		if conn != nil {
			_ = conn.Close("")
		}
		_ = server.Close()
	}
}

// ReuseLastPort false reaches core through the config-driven server path: a
// persistent node records no bind-ports.json.
func TestServer_ReuseLastPortOffRecordsNoBindPorts(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	for _, tc := range []struct {
		env      string
		recorded bool
	}{{"", true}, {"false", false}} {
		t.Setenv("ASTER_REUSE_LAST_PORT", tc.env)
		t.Setenv("ASTER_RELAY_MODE", "disabled")
		t.Setenv("ASTER_BIND_ADDR", "127.0.0.1:0")
		dir := t.TempDir()
		t.Setenv("ASTER_STORAGE_PATH", dir)
		cfg, err := ConfigFromEnv()
		if err != nil {
			t.Fatal(err)
		}
		srv, err := StartServer(ctx, ServerOptions{Config: cfg})
		if err != nil {
			t.Fatal(err)
		}
		_ = srv.Close()
		_, err = os.Stat(filepath.Join(dir, "bind-ports.json"))
		if (err == nil) != tc.recorded {
			t.Fatalf("ASTER_REUSE_LAST_PORT=%q: bind-ports.json recorded=%v, want %v", tc.env, err == nil, tc.recorded)
		}
	}
}
