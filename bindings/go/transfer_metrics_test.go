//go:build cgo

package aster

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// C ABI 7.4: node-wide blob counters and per-provider bytes after a real
// fetch, and per-peer traffic in the topology view.
func TestTransferMetrics_StatsAndFetchReportV2(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	provider := startTestNode(t, ctx)
	fetcher := startTestNode(t, ctx)

	before, err := fetcher.Blobs().TransferStats()
	if err != nil {
		t.Fatal(err)
	}
	if before.FetchedPayloadBytes != 0 || before.ServedPayloadBytes != 0 {
		t.Fatalf("fresh node: %+v", before)
	}

	data := bytes.Repeat([]byte("aster-transfer-metrics-"), 4000)
	hash, err := provider.Blobs().AddBytes(ctx, data)
	if err != nil {
		t.Fatal(err)
	}
	paddr, _ := provider.NodeAddr()
	if err := fetcher.AddNodeAddr(paddr); err != nil {
		t.Fatal(err)
	}
	got, report, err := fetcher.Blobs().DownloadHashFrom(ctx, string(hash), []string{paddr.EndpointID}, BLOB_FORMAT_RAW, FetchWhole, false)
	if err != nil {
		t.Fatalf("download from: %v", err)
	}
	if !bytes.Equal(got, data) || len(report.Providers) != 1 {
		t.Fatalf("fetch: %d bytes, report %+v", len(got), report)
	}
	p := report.Providers[0]
	if p.BytesTransferred == 0 || p.BytesTransferred != report.BytesTransferred || p.InFlight != 0 || p.CompletedRequests != 1 {
		t.Fatalf("provider record %+v (report total %d)", p, report.BytesTransferred)
	}
	if p.WireBytes != nil && (*p.WireBytes == 0 || *p.WireBytes >= p.BytesTransferred) {
		t.Fatalf("zstd wire bytes %d for %d payload bytes of a repetitive blob", *p.WireBytes, p.BytesTransferred)
	}

	fetched, err := fetcher.Blobs().TransferStats()
	if err != nil {
		t.Fatal(err)
	}
	if fetched.FetchedPayloadBytes < uint64(len(data)) || fetched.FetchTransfers.PlainOK+fetched.FetchTransfers.ZstdOK == 0 {
		t.Fatalf("fetcher stats %+v", fetched)
	}
	deadline := time.Now().Add(10 * time.Second)
	for {
		served, err := provider.Blobs().TransferStats()
		if err != nil {
			t.Fatal(err)
		}
		if served.ServedPayloadBytes >= uint64(len(data)) {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("provider stats %+v", served)
		}
		time.Sleep(50 * time.Millisecond)
	}
}

func TestTransferMetrics_ParseFetchReportV1AndV2(t *testing.T) {
	header := func(version byte, n uint32, total uint64) []byte {
		h := make([]byte, 16)
		h[0] = version
		binary.LittleEndian.PutUint32(h[4:], n)
		binary.LittleEndian.PutUint64(h[8:], total)
		return h
	}
	id := bytes.Repeat([]byte{0x0a}, 32)

	v1 := append(header(1, 1, 9), id...)
	v1 = binary.LittleEndian.AppendUint32(v1, 3)
	v1 = binary.LittleEndian.AppendUint32(v1, 2)
	v1 = binary.LittleEndian.AppendUint32(v1, 1)
	v1 = append(v1, "xyz"...)
	r, rest, err := parseFetchReport(v1)
	if err != nil || string(rest) != "xyz" || r.BytesTransferred != 9 || len(r.Providers) != 1 ||
		r.Providers[0].NodeID != strings.Repeat("0a", 32) || r.Providers[0].Attempts != 3 || r.Providers[0].WireBytes != nil {
		t.Fatalf("v1: %+v %q %v", r, rest, err)
	}

	v2 := header(2, 2, 12)
	for _, wire := range []uint64{5, 0} {
		v2 = append(v2, id...)
		for _, x := range []uint32{3, 2, 1, 4, 6} { // attempts, failures, completed, in_flight, plain_fallbacks
			v2 = binary.LittleEndian.AppendUint32(v2, x)
		}
		flags := uint32(0)
		if wire != 0 {
			flags = 1
		}
		v2 = binary.LittleEndian.AppendUint32(v2, flags)
		v2 = binary.LittleEndian.AppendUint64(v2, 10)
		v2 = binary.LittleEndian.AppendUint64(v2, wire)
	}
	v2 = append(v2, "xyz"...)
	r, rest, err = parseFetchReport(v2)
	if err != nil || string(rest) != "xyz" || len(r.Providers) != 2 {
		t.Fatalf("v2: %+v %q %v", r, rest, err)
	}
	a, b := r.Providers[0], r.Providers[1]
	if a.InFlight != 4 || a.PlainFallbacks != 6 || a.BytesTransferred != 10 || a.WireBytes == nil || *a.WireBytes != 5 ||
		b.BytesTransferred != 10 || b.WireBytes != nil || b.CompletedRequests != 1 {
		t.Fatalf("v2 records: %+v %+v", a, b)
	}

	for _, bad := range [][]byte{v2[:20], header(3, 0, 0), header(2, 1<<31, 0), {1, 0}} {
		if _, _, err := parseFetchReport(bad); !errors.Is(err, ErrInternal) {
			t.Fatalf("malformed report accepted (%d bytes): %v", len(bad), err)
		}
	}
}

func TestTransferMetrics_TopologyPeerJSON(t *testing.T) {
	data := []byte(`{"node_id":"id","level":1,"quic_traffic":{"blobs":{"quic_bytes_sent":1,"quic_bytes_received":2},` +
		`"blobs-zstd":{"quic_bytes_sent":3,"quic_bytes_received":4},"docs":{"quic_bytes_sent":0,"quic_bytes_received":0},` +
		`"gossip":{"quic_bytes_sent":0,"quic_bytes_received":0},"rpc":{"quic_bytes_sent":18446744073709551615,"quic_bytes_received":5},` +
		`"other":{"quic_bytes_sent":0,"quic_bytes_received":0}},"blob_transfer":{"fetched_payload_bytes":7,"fetched_wire_bytes":8,` +
		`"served_payload_bytes":9,"plain_fallbacks":10}}`)
	var peer TopologyPeerView
	if err := json.Unmarshal(data, &peer); err != nil {
		t.Fatal(err)
	}
	q := peer.QuicTraffic
	if q.Blobs.Sent != 1 || q.BlobsZstd.Received != 4 || q.RPC.Sent != ^uint64(0) || q.Total().Received != 11 {
		t.Fatalf("quic traffic %+v", q)
	}
	b := peer.BlobTransfer
	if b.FetchedPayloadBytes != 7 || b.FetchedWireBytes != 8 || b.ServedPayloadBytes != 9 || b.PlainFallbacks != 10 {
		t.Fatalf("blob transfer %+v", b)
	}
}

// [metrics] peer labels (C ABI 7.4) round-trip through aster_config_set_metrics
// / aster_config_metrics and resolve from env and TOML through Rust.
func TestConfig_MetricsPeerLabels(t *testing.T) {
	d := DefaultConfig()
	if d.MetricsPeerLabels || d.MetricsPeerLabelCap != 64 {
		t.Fatalf("defaults: peer labels %v cap %d", d.MetricsPeerLabels, d.MetricsPeerLabelCap)
	}
	var a cArena
	defer a.free()
	n, err := (&AsterConfig{MetricsPeerLabels: true, MetricsPeerLabelCap: 3, BindAddrV6: "[::]:7842"}).toNative(&a, nil)
	if err != nil {
		t.Fatal(err)
	}
	var back AsterConfig
	back.fromNative(&n)
	releaseNative(&n)
	if !back.MetricsPeerLabels || back.MetricsPeerLabelCap != 3 || back.BindAddrV6 != "[::]:7842" {
		t.Fatalf("round trip: %+v", back)
	}

	t.Setenv("ASTER_METRICS_PEER_LABELS", "true")
	t.Setenv("ASTER_METRICS_PEER_LABEL_CAP", "5")
	env, err := ConfigFromEnv()
	if err != nil {
		t.Fatal(err)
	}
	if !env.MetricsPeerLabels || env.MetricsPeerLabelCap != 5 {
		t.Fatalf("from env: %+v", env)
	}
	t.Setenv("ASTER_METRICS_PEER_LABELS", "")
	t.Setenv("ASTER_METRICS_PEER_LABEL_CAP", "")

	path := filepath.Join(t.TempDir(), "aster.toml")
	if err := os.WriteFile(path, []byte("[metrics]\npeer_labels = true\npeer_label_cap = 7\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	file, err := ConfigFromTOML(path)
	if err != nil {
		t.Fatal(err)
	}
	if !file.MetricsPeerLabels || file.MetricsPeerLabelCap != 7 {
		t.Fatalf("from toml: %+v", file)
	}

	// A node started with peer labels serves its ops listener with them.
	cfg := testCfg()
	cfg.MetricsPeerLabels, cfg.MetricsPeerLabelCap = true, 2
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	node, err := StartNode(ctx, cfg, NodeOptions{})
	if err != nil {
		t.Fatal(err)
	}
	node.Close()
}
