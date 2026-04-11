//go:build cgo

package aster

import (
	"testing"
)

func TestGossipPeer(t *testing.T) {
	peer := GossipPeer{
		ID:   "peer123",
		Addr: "/ipv4/127.0.0.1/udp/1234",
	}
	if peer.ID != "peer123" {
		t.Errorf("expected peer123, got %s", peer.ID)
	}
	if peer.Addr != "/ipv4/127.0.0.1/udp/1234" {
		t.Errorf("expected /ipv4/127.0.0.1/udp/1234, got %s", peer.Addr)
	}
}

func TestGossipMessage(t *testing.T) {
	msg := GossipMessage{
		Content:   []byte("hello world"),
		Topic:     "test-topic",
		PeerID:    "peer456",
		Timestamp: 1234567890,
	}
	if string(msg.Content) != "hello world" {
		t.Errorf("expected hello world, got %s", string(msg.Content))
	}
	if msg.Topic != "test-topic" {
		t.Errorf("expected test-topic, got %s", msg.Topic)
	}
	if msg.PeerID != "peer456" {
		t.Errorf("expected peer456, got %s", msg.PeerID)
	}
	if msg.Timestamp != 1234567890 {
		t.Errorf("expected 1234567890, got %d", msg.Timestamp)
	}
}

func TestGossipTopic(t *testing.T) {
	topic := &GossipTopic{
		runtime: nil,
		handle:  42,
	}
	if topic.handle != 42 {
		t.Errorf("expected handle 42, got %d", topic.handle)
	}
}

func TestGossipEventConstants(t *testing.T) {
	if IROH_EVENT_GOSSIP_SUBSCRIBED != 50 {
		t.Errorf("IROH_EVENT_GOSSIP_SUBSCRIBED = %d, want 50", IROH_EVENT_GOSSIP_SUBSCRIBED)
	}
	if IROH_EVENT_GOSSIP_BROADCAST_DONE != 51 {
		t.Errorf("IROH_EVENT_GOSSIP_BROADCAST_DONE = %d, want 51", IROH_EVENT_GOSSIP_BROADCAST_DONE)
	}
	if IROH_EVENT_GOSSIP_RECEIVED != 52 {
		t.Errorf("IROH_EVENT_GOSSIP_RECEIVED = %d, want 52", IROH_EVENT_GOSSIP_RECEIVED)
	}
	if IROH_EVENT_GOSSIP_NEIGHBOR_UP != 53 {
		t.Errorf("IROH_EVENT_GOSSIP_NEIGHBOR_UP = %d, want 53", IROH_EVENT_GOSSIP_NEIGHBOR_UP)
	}
	if IROH_EVENT_GOSSIP_NEIGHBOR_DOWN != 54 {
		t.Errorf("IROH_EVENT_GOSSIP_NEIGHBOR_DOWN = %d, want 54", IROH_EVENT_GOSSIP_NEIGHBOR_DOWN)
	}
	if IROH_EVENT_GOSSIP_LAGGED != 55 {
		t.Errorf("IROH_EVENT_GOSSIP_LAGGED = %d, want 55", IROH_EVENT_GOSSIP_LAGGED)
	}
}

func TestParseGossipPeer(t *testing.T) {
	// Build a peer data buffer: id_len(4) + id + addr_len(4) + addr
	id := "peer123"
	addr := "/ipv4/127.0.0.1/udp/1234"
	idLen := uint32(len(id))
	addrLen := uint32(len(addr))
	buf := make([]byte, 4+int(idLen)+4+int(addrLen))

	offset := 0
	// id_len
	buf[offset] = byte(idLen)
	buf[offset+1] = byte(idLen >> 8)
	buf[offset+2] = byte(idLen >> 16)
	buf[offset+3] = byte(idLen >> 24)
	offset += 4

	// id
	copy(buf[offset:], id)
	offset += int(idLen)

	// addr_len
	buf[offset] = byte(addrLen)
	buf[offset+1] = byte(addrLen >> 8)
	buf[offset+2] = byte(addrLen >> 16)
	buf[offset+3] = byte(addrLen >> 24)
	offset += 4

	// addr
	copy(buf[offset:], addr)

	peer, err := parseGossipPeer(&buf[0], len(buf))
	if err != nil {
		t.Fatalf("parseGossipPeer failed: %v", err)
	}
	if peer.ID != id {
		t.Errorf("expected ID %s, got %s", id, peer.ID)
	}
	if peer.Addr != addr {
		t.Errorf("expected Addr %s, got %s", addr, peer.Addr)
	}
}

func TestParseGossipMessage(t *testing.T) {
	// Build a message data buffer: timestamp(8) + topic_len(4) + topic + peer_id_len(4) + peer_id + content
	topic := "test-topic"
	peerID := "peer456"
	content := []byte("hello world")
	timestamp := uint64(1234567890)

	topicLen := uint32(len(topic))
	peerIDLen := uint32(len(peerID))

	buf := make([]byte, 8+4+int(topicLen)+4+int(peerIDLen)+len(content))

	offset := 0
	// timestamp
	binary.LittleEndian.PutUint64(buf[offset:], timestamp)
	offset += 8

	// topic_len
	binary.LittleEndian.PutUint32(buf[offset:], topicLen)
	offset += 4

	// topic
	copy(buf[offset:], topic)
	offset += int(topicLen)

	// peer_id_len
	binary.LittleEndian.PutUint32(buf[offset:], peerIDLen)
	offset += 4

	// peer_id
	copy(buf[offset:], peerID)
	offset += int(peerIDLen)

	// content
	copy(buf[offset:], content)

	msg, err := parseGossipMessage(&buf[0], len(buf))
	if err != nil {
		t.Fatalf("parseGossipMessage failed: %v", err)
	}
	if msg.Topic != topic {
		t.Errorf("expected Topic %s, got %s", topic, msg.Topic)
	}
	if msg.PeerID != peerID {
		t.Errorf("expected PeerID %s, got %s", peerID, msg.PeerID)
	}
	if msg.Timestamp != timestamp {
		t.Errorf("expected Timestamp %d, got %d", timestamp, msg.Timestamp)
	}
	if string(msg.Content) != string(content) {
		t.Errorf("expected Content %s, got %s", string(content), string(msg.Content))
	}
}

func TestParseGossipMessageNilData(t *testing.T) {
	_, err := parseGossipMessage(nil, 0)
	if err == nil {
		t.Errorf("expected error for nil data, got nil")
	}
}
