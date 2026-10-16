//go:build cgo

package aster

/*
#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"encoding/json"
	"fmt"
	"sync"
)

// TopologyLadderLevel classifies passively measured proximity, from closest to farthest.
type TopologyLadderLevel uint32

const (
	TopologySameHost TopologyLadderLevel = iota
	TopologySameLAN
	TopologySameSite
	TopologySameRegion
	TopologyFar
)

// TopologySeparation is local evidence, not proof of policy or authorization.
// Unknown is a real answer: absent measurements are not evidence of distance.
type TopologySeparation uint32

const (
	TopologyUnknown TopologySeparation = iota
	TopologyConnected
	TopologySeparated
)

// TopologyPeerView copies one passive local observation. Optional measurements
// remain nil until sampled. RemoteAddr is local-only; the RPC omits it.
type TopologyPeerView struct {
	NodeID                     string              `json:"node_id"`
	Level                      TopologyLadderLevel `json:"level"`
	LevelReason                string              `json:"level_reason"`
	RTTMicros                  *uint64             `json:"rtt_us"`
	JitterMicros               *uint64             `json:"jitter_us"`
	LossPPM                    uint32              `json:"loss_ppm"`
	ThroughputBPS              uint64              `json:"throughput_bps"`
	CwndCeilingBPS             uint64              `json:"cwnd_ceiling_bps"`
	Direct                     bool                `json:"direct"`
	RelayURL                   *string             `json:"relay_url"`
	RemoteAddr                 *string             `json:"remote_addr"`
	CongestionEvents           uint64              `json:"congestion_events"`
	Samples                    uint64              `json:"samples"`
	ConfidencePPM              uint32              `json:"confidence_ppm"`
	LastMeasuredUnixMillis     uint64              `json:"last_measured_unix_ms"`
	ClusterHeldSinceUnixMillis uint64              `json:"cluster_held_since_unix_ms"`
	IsConnected                bool                `json:"is_connected"`
	// QuicTraffic is the QUIC bytes exchanged with the peer by protocol
	// family; BlobTransfer the exact blob payload bytes (C ABI 7.4).
	QuicTraffic  TopologyQuicTraffic   `json:"quic_traffic"`
	BlobTransfer TopologyPeerBlobStats `json:"blob_transfer"`
}

// TopologySwarmOptions overrides native timing/coverage defaults. Nil fields
// retain defaults. Explicit zero is preserved except position refresh (60s).
type TopologySwarmOptions struct {
	PositionRefreshMillis *uint32 `json:"position_refresh_ms,omitempty"`
	MinHoldMillis         *uint32 `json:"min_hold_ms,omitempty"`
	LivenessTTLMillis     *uint32 `json:"liveness_ttl_ms,omitempty"`
	EdgeTTLMillis         *uint32 `json:"edge_ttl_ms,omitempty"`
	MaxSkewMillis         *uint32 `json:"max_skew_ms,omitempty"`
	SeparationCoverage    *uint32 `json:"separation_coverage,omitempty"`
	WitnessR              *uint32 `json:"witness_r,omitempty"`
}

// TopologyView borrows its node's local topology; it opens no RPC listener.
// Monitoring is required to populate observations and enable a shared swarm.
type TopologyView struct{ node *Node }

func (n *Node) Topology() *TopologyView { return &TopologyView{node: n} }

func topologyJSON(call func(*C.aster_bytes_t) C.int32_t, output any) error {
	var data C.aster_bytes_t
	defer func() {
		if data.ptr != nil {
			C.aster_string_release(data.ptr, data.len)
		}
	}()
	if err := wrapCall("topology", func() C.int32_t { return call(&data) }); err != nil {
		return err
	}
	return json.Unmarshal(viewBytes(data), output)
}
func (t *TopologyView) HasMonitoring() (bool, error) {
	rt := t.node.runtime.retain()
	defer rt.release()
	var enabled C.uint32_t
	err := wrapCall("aster_topology_has_monitoring", func() C.int32_t {
		return C.aster_topology_has_monitoring(C.uint64_t(rt.handle), C.uint64_t(t.node.handle), &enabled)
	})
	return enabled != 0, err
}
func (t *TopologyView) Peers() ([]TopologyPeerView, error) {
	rt := t.node.runtime.retain()
	defer rt.release()
	var peers []TopologyPeerView
	err := topologyJSON(func(out *C.aster_bytes_t) C.int32_t {
		return C.aster_topology_peers(C.uint64_t(rt.handle), C.uint64_t(t.node.handle), out)
	}, &peers)
	return peers, err
}
func (t *TopologyView) Peer(nodeID string) (*TopologyPeerView, error) {
	rt := t.node.runtime.retain()
	defer rt.release()
	var a cArena
	defer a.free()
	var peer *TopologyPeerView
	err := topologyJSON(func(out *C.aster_bytes_t) C.int32_t {
		return C.aster_topology_peer(C.uint64_t(rt.handle), C.uint64_t(t.node.handle), a.str(nodeID), out)
	}, &peer)
	return peer, err
}

// EnableShared imports the secret namespace and starts sharing observations.
// A nil filter admits every secret holder. Zero options retain native defaults.
// Canceling the wait does not roll back joining: the node owns an installed
// publisher until node shutdown or replacement by a later join.
func (t *TopologyView) EnableShared(ctx context.Context, secret []byte, filter *AdmittedFilterHandle, bootstrap []string, options TopologySwarmOptions) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if len(secret) != 32 {
		return fmt.Errorf("topology secret must be 32 bytes: %w", ErrInvalidArgument)
	}
	if filter != nil && filter.rt != t.node.runtime {
		return fmt.Errorf("topology filter belongs to another runtime: %w", ErrInvalidArgument)
	}
	input, err := json.Marshal(options)
	if err != nil {
		return err
	}
	rt := t.node.runtime.retain()
	defer rt.release()
	var a cArena
	defer a.free()
	var handle C.uint64_t
	if filter != nil {
		handle = C.uint64_t(filter.handle)
	}
	var key [32]C.uint8_t
	for i, b := range secret {
		key[i] = C.uint8_t(b)
	}
	ev, err := startOp(ctx, rt, "aster_topology_enable_shared_with_options", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_topology_enable_shared_with_options(C.uint64_t(rt.handle), C.uint64_t(t.node.handle), &key[0], handle, a.list(bootstrap), a.bytes(input), 0, op)
	})
	if err != nil {
		return err
	}
	_, err = expect(rt, ev, uint32(ASTER_EVENT_UNIT_RESULT), "enable topology swarm")
	return err
}

// TopologyObservation owns one captured immutable native shared view. Every
// query answers from this same observation. Close it when finished. It is local
// evidence, never serialized or accepted as proof of authorization.
type TopologyObservation struct {
	runtime *Runtime
	handle  uint64
	mu      sync.Mutex
	closed  bool
}

// Snapshot requires a joined swarm. A committed snapshot wins racing context
// cancellation; otherwise cancellation consumes the definitive native terminal.
func (t *TopologyView) Snapshot(ctx context.Context) (*TopologyObservation, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	rt := t.node.runtime.retain()
	owned := false
	defer func() {
		if !owned {
			_ = rt.release()
		}
	}()
	var op C.aster_operation_t
	if err := wrapCall("aster_topology_snapshot", func() C.int32_t {
		return C.aster_topology_snapshot(C.uint64_t(rt.handle), C.uint64_t(t.node.handle), 0, &op)
	}); err != nil {
		return nil, err
	}
	ch := make(chan *Event, 1)
	rt.Register(uint64(op), ch)
	var ev *Event
	select {
	case ev = <-ch:
	case <-ctx.Done():
		ev = rt.cancelAndCollect(uint64(op), ch)
	}
	if ev == nil {
		return nil, ErrAlreadyClosed
	}
	if ev.Kind == ASTER_EVENT_OPERATION_CANCELLED {
		rt.releaseEventBuffer(ev)
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		return nil, ErrCancelled
	}
	if _, err := expect(rt, ev, uint32(ASTER_EVENT_TOPOLOGY_SNAPSHOT), "topology snapshot"); err != nil {
		return nil, err
	}
	owned = true
	return &TopologyObservation{runtime: rt, handle: ev.Handle}, nil
}
func (s *TopologyObservation) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return nil
	}
	status, statusMsg := ffiCall(func() C.int32_t {
		return C.aster_topology_snapshot_free(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle))
	})
	if status != C.ASTER_STATUS_NOT_FOUND && status != C.ASTER_STATUS_ALREADY_CLOSED {
		if err := wrapStatusMsg("aster_topology_snapshot_free", status, statusMsg); err != nil {
			return err
		}
	}
	s.closed = true
	return s.runtime.release()
}
func (s *TopologyObservation) Clusters() ([]ClusterView, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return nil, ErrAlreadyClosed
	}
	var clusters []ClusterView
	err := topologyJSON(func(out *C.aster_bytes_t) C.int32_t {
		return C.aster_topology_snapshot_clusters(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), out)
	}, &clusters)
	return clusters, err
}
func (s *TopologyObservation) ClusterOf(nodeID string) (*ClusterView, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return nil, ErrAlreadyClosed
	}
	var a cArena
	defer a.free()
	var cluster *ClusterView
	err := topologyJSON(func(out *C.aster_bytes_t) C.int32_t {
		return C.aster_topology_snapshot_cluster_of(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), a.str(nodeID), out)
	}, &cluster)
	return cluster, err
}
func (s *TopologyObservation) Separated(aID, bID string) (TopologySeparation, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return TopologyUnknown, ErrAlreadyClosed
	}
	var a cArena
	defer a.free()
	var verdict C.uint32_t
	err := wrapCall("aster_topology_snapshot_separated", func() C.int32_t {
		return C.aster_topology_snapshot_separated(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), a.str(aID), a.str(bID), &verdict)
	})
	if err == nil && verdict > 2 {
		return TopologyUnknown, fmt.Errorf("invalid topology separation %d: %w", verdict, ErrInternal)
	}
	return TopologySeparation(verdict), err
}

// Clusters reads one transient observation. Separate view calls can observe
// different eventual states; use Snapshot for a placement decision.
func (t *TopologyView) Clusters(ctx context.Context) ([]ClusterView, error) {
	s, err := t.Snapshot(ctx)
	if err != nil {
		return nil, err
	}
	defer s.Close()
	return s.Clusters()
}

// MyCluster reads this node's cluster from one transient observation.
func (t *TopologyView) MyCluster(ctx context.Context) (*ClusterView, error) {
	id, err := t.node.NodeID()
	if err != nil {
		return nil, err
	}
	s, err := t.Snapshot(ctx)
	if err != nil {
		return nil, err
	}
	defer s.Close()
	return s.ClusterOf(id)
}

// Separated reads native evidence from one transient observation. Separate
// calls can see different eventual states; Snapshot keeps all queries consistent.
func (t *TopologyView) Separated(ctx context.Context, a, b string) (TopologySeparation, error) {
	s, err := t.Snapshot(ctx)
	if err != nil {
		return TopologyUnknown, err
	}
	defer s.Close()
	return s.Separated(a, b)
}
