//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include "iroh_ffi.h"
*/
import "C"

import (
	"context"
	"encoding/binary"
	"fmt"
	"strings"
	"unsafe"
)

// GossipTopic is a handle to a subscribed gossip topic.
type GossipTopic struct {
	runtime *Runtime
	handle  uint64
}

// GossipPeer represents a gossip peer endpoint.
type GossipPeer struct {
	ID   string
	Addr string
}

// GossipMessage represents a message received from gossip.
type GossipMessage struct {
	Content  []byte
	Topic    string
	PeerID   string
	Timestamp uint64
}

// Gossip provides gossip protocol operations.
// Get an instance via Node.Gossip().
type Gossip struct {
	node     *Node
	runtime *Runtime
}

// Node returns the parent node.
func (g *Gossip) Node() *Node {
	return g.node
}

// Subscribe subscribes to a gossip topic.
func (g *Gossip) Subscribe(ctx context.Context, topic string, peers []GossipPeer) (*GossipTopic, error) {
	var opID C.iroh_operation_t

	topicBytes := []byte(topic)
	topicC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&topicBytes[0])),
		len: C.uintptr_t(len(topicBytes)),
	}

	// Build peers list
	var peersPtr *C.struct_iroh_bytes_t
	var peersLen C.size_t
	if len(peers) > 0 {
		peersSlice := make([]C.struct_iroh_bytes_t, len(peers))
		for i, peer := range peers {
			peerBytes := []byte(peer.ID)
			peerSeg := C.struct_iroh_bytes_t{
				ptr: (*C.uint8_t)(unsafe.Pointer(&peerBytes[0])),
				len: C.uintptr_t(len(peerBytes)),
			}
			peersSlice[i] = peerSeg
		}
		peersPtr = &peersSlice[0]
		peersLen = C.size_t(len(peers))
	}

	peersListC := C.struct_iroh_bytes_list_t{
		items: peersPtr,
		len:   peersLen,
	}

	r := C.iroh_gossip_subscribe(
		C.uint64_t(g.runtime.handle),
		C.uint64_t(g.node.handle),
		topicC,
		peersListC,
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_gossip_subscribe: %w", Error(r))
	}

	ev, err := g.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("subscribe poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_GOSSIP_SUBSCRIBED {
		return nil, fmt.Errorf("subscribe: unexpected event %d", ev.Kind)
	}

	return &GossipTopic{runtime: g.runtime, handle: ev.Handle}, nil
}

// Broadcast broadcasts a message to a topic.
func (t *GossipTopic) Broadcast(ctx context.Context, data []byte) error {
	var opID C.iroh_operation_t

	dataC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&data[0])),
		len: C.uintptr_t(len(data)),
	}

	r := C.iroh_gossip_broadcast(
		C.uint64_t(t.runtime.handle),
		C.uint64_t(t.handle),
		dataC,
		0,
		&opID,
	)
	if r != 0 {
		return fmt.Errorf("iroh_gossip_broadcast: %w", Error(r))
	}

	ev, err := t.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return fmt.Errorf("broadcast poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_GOSSIP_BROADCAST_DONE {
		return fmt.Errorf("broadcast: unexpected event %d", ev.Kind)
	}

	return nil
}

// Recv receives a gossip message from the topic.
func (t *GossipTopic) Recv(ctx context.Context) (*GossipMessage, error) {
	var opID C.iroh_operation_t

	r := C.iroh_gossip_recv(
		C.uint64_t(t.runtime.handle),
		C.uint64_t(t.handle),
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_gossip_recv: %w", Error(r))
	}

	ev, err := t.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("recv poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_GOSSIP_RECEIVED {
		return nil, fmt.Errorf("recv: unexpected event %d", ev.Kind)
	}

	return parseGossipMessage(ev.DataPtr, int(ev.DataLen))
}

// Close closes the topic subscription.
func (t *GossipTopic) Close() error {
	r := C.iroh_gossip_topic_free(C.uint64_t(t.runtime.handle), C.uint64_t(t.handle))
	if r != 0 {
		return fmt.Errorf("iroh_gossip_topic_free: %w", Error(r))
	}
	return nil
}

// parseGossipMessage parses a gossip message from C data.
// Format: timestamp (8) + topic_len (4) + topic + peer_id_len (4) + peer_id + content
func parseGossipMessage(dataPtr unsafe.Pointer, dataLen int) (*GossipMessage, error) {
	if dataPtr == nil || dataLen == 0 {
		return nil, fmt.Errorf("nil gossip message data")
	}

	data := C.GoBytes(dataPtr, C.int(dataLen))
	offset := 0

	// Read timestamp (8 bytes)
	if len(data) < offset+8 {
		return nil, fmt.Errorf("data too short for timestamp")
	}
	timestamp := binary.LittleEndian.Uint64(data[offset : offset+8])
	offset += 8

	// Read topic length
	if len(data) < offset+4 {
		return nil, fmt.Errorf("data too short for topic length")
	}
	topicLen := binary.LittleEndian.Uint32(data[offset : offset+4])
	offset += 4

	// Read topic
	if len(data) < offset+int(topicLen) {
		return nil, fmt.Errorf("data too short for topic")
	}
	topic := string(data[offset : offset+int(topicLen)])
	offset += int(topicLen)

	// Read peer ID length
	if len(data) < offset+4 {
		return nil, fmt.Errorf("data too short for peer ID length")
	}
	peerIDLen := binary.LittleEndian.Uint32(data[offset : offset+4])
	offset += 4

	// Read peer ID
	if len(data) < offset+int(peerIDLen) {
		return nil, fmt.Errorf("data too short for peer ID")
	}
	peerID := string(data[offset : offset+int(peerIDLen)])
	offset += int(peerIDLen)

	// Read content (remaining bytes)
	content := data[offset:]

	return &GossipMessage{
		Content:   content,
		Topic:    topic,
		PeerID:   peerID,
		Timestamp: timestamp,
	}, nil
}

// parseGossipPeer parses a gossip peer from C data.
// Format: id_len (4) + id + addr_len (4) + addr
func parseGossipPeer(dataPtr unsafe.Pointer, dataLen int) (GossipPeer, error) {
	if dataPtr == nil || dataLen == 0 {
		return GossipPeer{}, fmt.Errorf("nil gossip peer data")
	}

	data := C.GoBytes(dataPtr, C.int(dataLen))
	offset := 0

	// Read ID length
	if len(data) < offset+4 {
		return GossipPeer{}, fmt.Errorf("data too short for peer ID length")
	}
	idLen := binary.LittleEndian.Uint32(data[offset : offset+4])
	offset += 4

	// Read ID
	if len(data) < offset+int(idLen) {
		return GossipPeer{}, fmt.Errorf("data too short for peer ID")
	}
	id := string(data[offset : offset+int(idLen)])
	offset += int(idLen)

	// Read addr length
	if len(data) < offset+4 {
		return GossipPeer{}, fmt.Errorf("data too short for addr length")
	}
	addrLen := binary.LittleEndian.Uint32(data[offset : offset+4])
	offset += 4

	// Read addr
	if len(data) < offset+int(addrLen) {
		return GossipPeer{}, fmt.Errorf("data too short for addr")
	}
	addr := string(data[offset : offset+int(addrLen)])

	return GossipPeer{
		ID:   strings.TrimSpace(id),
		Addr: strings.TrimSpace(addr),
	}, nil
}
