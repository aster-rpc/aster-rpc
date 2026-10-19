//go:build cgo

package aster

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"slices"
	"strings"
	"testing"
	"time"
)

func topologyNode(t *testing.T, ctx context.Context, monitored bool) *Node {
	t.Helper()
	cfg := testCfg()
	cfg.EnableMonitoring = monitored
	n, err := StartNode(ctx, cfg, NodeOptions{ExtraALPNs: []string{"pc16/probe"}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = n.Close() })
	return n
}
func topologyEventually(t *testing.T, ctx context.Context, check func() bool) {
	t.Helper()
	for {
		if check() {
			return
		}
		select {
		case <-ctx.Done():
			t.Fatal("topology did not converge: ", ctx.Err())
		// short-wait-ok: poll tick; ctx bounds the overall wait
		case <-time.After(50 * time.Millisecond):
		}
	}
}
func TestTopology_NoMonitoringAndPrejoin(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 15*time.Second)
	defer cancel()
	n := topologyNode(t, ctx, false)
	view := n.Topology()
	if enabled, err := view.HasMonitoring(); err != nil || enabled {
		t.Fatalf("monitoring %v %v", enabled, err)
	}
	if peers, err := view.Peers(); err != nil || len(peers) != 0 {
		t.Fatalf("peers %v %v", peers, err)
	}
	if peer, err := view.Peer(strings.Repeat("0", 64)); err != nil || peer != nil {
		t.Fatalf("unknown peer %v %v", peer, err)
	}
	if _, err := view.Peer("bad"); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("invalid peer: %v", err)
	}
	if observation, err := view.Snapshot(ctx); observation != nil || err == nil {
		t.Fatalf("snapshot before join: %v %v", observation, err)
	}
	if err := view.EnableShared(ctx, make([]byte, 32), nil, nil, TopologySwarmOptions{}); err == nil {
		t.Fatal("non-monitored join accepted")
	}
}
func TestTopology_MeasuredPeersSharedSnapshotAndOwnership(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 40*time.Second)
	defer cancel()
	nodes := []*Node{topologyNode(t, ctx, true), topologyNode(t, ctx, true), topologyNode(t, ctx, true)}
	ids := make([]string, 3)
	for i, n := range nodes {
		ids[i], _ = n.NodeID()
		for _, other := range nodes {
			if other != n {
				addr, err := other.NodeAddr()
				if err != nil {
					t.Fatal(err)
				}
				if err = n.AddNodeAddr(addr); err != nil {
					t.Fatal(err)
				}
			}
		}
	}
	accepted := make(chan *Connection, 1)
	acceptErr := make(chan error, 1)
	go func() {
		c, e := nodes[1].Accept(ctx)
		if e != nil {
			acceptErr <- e
		} else {
			accepted <- c
		}
	}()
	addr, err := nodes[1].NodeAddr()
	if err != nil {
		t.Fatal(err)
	}
	connection, err := nodes[0].Connect(ctx, addr, "pc16/probe")
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close("topology test")
	select {
	case c := <-accepted:
		defer c.Close("topology test")
	case err := <-acceptErr:
		t.Fatal(err)
	case <-ctx.Done():
		t.Fatal(ctx.Err())
	}
	topologyEventually(t, ctx, func() bool {
		peer, e := nodes[0].Topology().Peer(ids[1])
		return e == nil && peer != nil && peer.Samples > 0
	})
	peer, err := nodes[0].Topology().Peer(ids[1])
	if err != nil || peer.Level != TopologySameHost || !peer.Direct || peer.LastMeasuredUnixMillis == 0 || peer.RemoteAddr == nil {
		t.Fatalf("measured peer %+v %v", peer, err)
	}
	// C ABI 7.4: a measured peer has exchanged QUIC bytes on some protocol.
	if total := peer.QuicTraffic.Total(); total.Sent == 0 || total.Received == 0 {
		t.Fatalf("measured peer without QUIC traffic: %+v", peer.QuicTraffic)
	}
	peers, err := nodes[0].Topology().Peers()
	if err != nil || !slices.ContainsFunc(peers, func(p TopologyPeerView) bool { return p.NodeID == ids[1] }) {
		t.Fatalf("peers %v %v", peers, err)
	}
	up, err := NewUpcalls(nodes[0].Runtime(), 8)
	if err != nil {
		t.Fatal(err)
	}
	defer up.Close()
	filter, err := NewAdmittedFilter(up, AdmittedFilterOptions{TTL: time.Hour})
	if err != nil {
		t.Fatal(err)
	}
	defer filter.Close()
	for _, id := range ids {
		if err := filter.Set(id, true); err != nil {
			t.Fatal(err)
		}
	}
	secret := bytes.Repeat([]byte{36}, 32)
	zero, refresh, ttl := uint32(0), uint32(100), uint32(60000)
	options := TopologySwarmOptions{PositionRefreshMillis: &refresh, MinHoldMillis: &zero, LivenessTTLMillis: &ttl}
	for i, n := range nodes {
		var f *AdmittedFilterHandle
		if i == 0 {
			f = filter
		}
		if err := n.Topology().EnableShared(ctx, secret, f, ids, options); err != nil {
			t.Fatal(err)
		}
	}
	topologyEventually(t, ctx, func() bool {
		s, e := nodes[0].Topology().Snapshot(ctx)
		if e != nil {
			return false
		}
		defer s.Close()
		clusters, e := s.Clusters()
		return e == nil && len(clusters) == 1 && len(clusters[0].Members) == 3
	})
	if clusters, e := nodes[0].Topology().Clusters(ctx); e != nil || len(clusters) != 1 {
		t.Fatalf("clusters %v %v", clusters, e)
	}
	if own, e := nodes[0].Topology().MyCluster(ctx); e != nil || own == nil || len(own.Members) != 3 {
		t.Fatalf("my cluster %v %v", own, e)
	}
	if verdict, e := nodes[0].Topology().Separated(ctx, ids[0], ids[1]); e != nil || verdict != TopologyConnected {
		t.Fatalf("separated %v %v", verdict, e)
	}
	captured, err := nodes[0].Topology().Snapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer captured.Close()
	cluster, err := captured.ClusterOf(ids[1])
	if err != nil || cluster == nil || len(cluster.Members) != 3 {
		t.Fatalf("cluster %+v %v", cluster, err)
	}
	if verdict, err := captured.Separated(ids[0], ids[1]); err != nil || verdict != TopologyConnected {
		t.Fatalf("connected %v %v", verdict, err)
	}
	if verdict, err := captured.Separated(ids[0], strings.Repeat("0", 64)); err != nil || verdict != TopologyUnknown {
		t.Fatalf("unknown %v %v", verdict, err)
	}
	if _, err := captured.Separated("bad", ids[0]); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("bad ID accepted: %v", err)
	}
	if err := filter.Set(ids[2], false); err != nil {
		t.Fatal(err)
	}
	topologyEventually(t, ctx, func() bool {
		s, e := nodes[0].Topology().Snapshot(ctx)
		if e != nil {
			return false
		}
		defer s.Close()
		c, e := s.ClusterOf(ids[2])
		return e == nil && c == nil
	})
	old, err := captured.ClusterOf(ids[2])
	if err != nil || old == nil || len(old.Members) != 3 {
		t.Fatalf("captured view changed %+v %v", old, err)
	}
	old.Members[0] = "mutated host copy"
	again, err := captured.Clusters()
	if err != nil || slices.Contains(again[0].Members, "mutated host copy") {
		t.Fatal("host data aliases snapshot")
	}
	for i := 0; i < 8; i++ {
		racing, stop := context.WithCancel(ctx)
		go stop()
		s, e := nodes[0].Topology().Snapshot(racing)
		if e == nil {
			if e = s.Close(); e != nil {
				t.Fatal(e)
			}
		} else if !errors.Is(e, context.Canceled) {
			t.Fatal(e)
		}
	}
	// Captured shared evidence outlives node shutdown while retaining its runtime.
	if err := nodes[0].Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := captured.Clusters(); err != nil {
		t.Fatalf("snapshot lost after node close: %v", err)
	}
	if err := captured.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := captured.Clusters(); !errors.Is(err, ErrAlreadyClosed) {
		t.Fatalf("closed snapshot: %v", err)
	}
}
func TestTopology_PeerJSONPreservesFullUint64(t *testing.T) {
	data := []byte(`{"node_id":"id","level":4,"rtt_us":18446744073709551615,"jitter_us":4294967297,"throughput_bps":18446744073709551615,"samples":4294967298,"remote_addr":null}`)
	var peer TopologyPeerView
	if err := json.Unmarshal(data, &peer); err != nil {
		t.Fatal(err)
	}
	if peer.RTTMicros == nil || *peer.RTTMicros != ^uint64(0) || peer.JitterMicros == nil || *peer.JitterMicros != 4294967297 || peer.ThroughputBPS != ^uint64(0) || peer.Samples != 4294967298 || peer.RemoteAddr != nil {
		t.Fatalf("peer widths %+v", peer)
	}
	for _, bad := range []string{`{"rtt_us":18446744073709551616}`, `{"rtt_us":-1}`, `{"rtt_us":"1"}`, `{} {}`} {
		if err := json.Unmarshal([]byte(bad), &peer); err == nil {
			t.Fatalf("malformed metric accepted: %s", bad)
		}
	}
}
func TestTopology_RPCOptInMonitoringAndNativeRequirement(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 20*time.Second)
	defer cancel()
	if s, err := StartServer(ctx, ServerOptions{Config: testCfg(), Topology: true}); err == nil {
		_ = s.Close()
		t.Fatal("topology RPC without monitoring accepted")
	}
	cfg := testCfg()
	cfg.EnableMonitoring = true
	for _, enabled := range []bool{false, true} {
		s, err := StartServer(ctx, ServerOptions{Config: cfg, Topology: enabled, TopologyRequirement: RequireRole("observer")})
		if err != nil {
			t.Fatal(err)
		}
		cert, err := GenerateSelfSignedCertificate(nil, nil)
		if err != nil {
			t.Fatal(err)
		}
		listener, err := s.StartPublicHTTP(ctx, HTTPListenerConfig{BindAddr: "127.0.0.1:0", CertificatePEM: cert.CertificatePEM, PrivateKeyPEM: cert.PrivateKeyPEM})
		if err != nil {
			_ = s.Close()
			t.Fatal(err)
		}
		request, err := http.NewRequestWithContext(ctx, http.MethodPost, "https://"+listener.TCPAddr()+"/aster/aster.net.Topology/peers", bytes.NewReader(EncodeFrame(nil, FlagEndStream)))
		if err != nil {
			t.Fatal(err)
		}
		response, err := httpListenerClient(t).Do(request)
		if err != nil {
			t.Fatal(err)
		}
		body, err := io.ReadAll(response.Body)
		_ = response.Body.Close()
		if err != nil {
			t.Fatal(err)
		}
		var status RpcStatus
		found := false
		for len(body) > 0 {
			payload, flags, rest, e := DecodeFrame(body)
			if e != nil {
				t.Fatal(e)
			}
			body = rest
			if flags&FlagTrailer != 0 {
				status, e = DecodeRpcStatus(payload)
				if e != nil {
					t.Fatal(e)
				}
				found = true
			}
		}
		want := StatusNotFound
		if enabled {
			want = StatusPermissionDenied
		}
		if !found || status.Code != want {
			t.Fatalf("enabled=%v status %+v HTTP %v", enabled, status, response.Status)
		}
		if err := listener.Stop(0); err != nil {
			t.Fatal(err)
		}
		if err := s.Close(); err != nil {
			t.Fatal(err)
		}
	}
}
