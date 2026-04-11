//go:build cgo

package aster

import (
	"testing"
)

func TestAuthorID(t *testing.T) {
	// Valid 64-char hex string
	hex64 := "f" + string(make([]byte, 63))
	id := AuthorID(hex64)
	if string(id) != hex64 {
		t.Errorf("expected %s, got %s", hex64, id)
	}
}

func TestDocID(t *testing.T) {
	// Valid 64-char hex string
	hex64 := "d" + string(make([]byte, 63))
	id := DocID(hex64)
	if string(id) != hex64 {
		t.Errorf("expected %s, got %s", hex64, id)
	}
}

func TestQueryMode(t *testing.T) {
	tests := []struct {
		mode    QueryMode
		value   uint32
		name    string
	}{
		{QUERY_MODE_AUTHOR, 0, "AUTHOR"},
		{QUERY_MODE_ALL, 1, "ALL"},
		{QUERY_MODE_PREFIX, 2, "PREFIX"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if uint32(tt.mode) != tt.value {
				t.Errorf("expected %d, got %d", tt.value, tt.mode)
			}
		})
	}
}

func TestDocEventType(t *testing.T) {
	tests := []struct {
		eventType DocEventType
		value     uint32
		name      string
	}{
		{DOC_EVENT_INSERT_LOCAL, 0, "INSERT_LOCAL"},
		{DOC_EVENT_INSERT_REMOTE, 1, "INSERT_REMOTE"},
		{DOC_EVENT_CONTENT_READY, 2, "CONTENT_READY"},
		{DOC_EVENT_PENDING_CONTENT, 3, "PENDING_CONTENT"},
		{DOC_EVENT_NEIGHBOR_UP, 4, "NEIGHBOR_UP"},
		{DOC_EVENT_NEIGHBOR_DOWN, 5, "NEIGHBOR_DOWN"},
		{DOC_EVENT_SYNC_FINISHED, 6, "SYNC_FINISHED"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if uint32(tt.eventType) != tt.value {
				t.Errorf("expected %d, got %d", tt.value, tt.eventType)
			}
		})
	}
}

func TestDocEntry(t *testing.T) {
	author := AuthorID("a" + string(make([]byte, 63)))
	hash := BlobID("b" + string(make([]byte, 63)))
	entry := DocEntry{
		Key:         "testkey",
		Author:      author,
		ContentHash: hash,
		Value:       []byte("test value"),
	}
	if entry.Key != "testkey" {
		t.Errorf("expected testkey, got %s", entry.Key)
	}
	if entry.Author != author {
		t.Errorf("expected author %s, got %s", author, entry.Author)
	}
	if entry.ContentHash != hash {
		t.Errorf("expected hash %s, got %s", hash, entry.ContentHash)
	}
	if string(entry.Value) != "test value" {
		t.Errorf("expected test value, got %s", string(entry.Value))
	}
}

func TestDocConstants(t *testing.T) {
	// Doc event constants
	if IROH_EVENT_DOC_CREATED != 40 {
		t.Errorf("IROH_EVENT_DOC_CREATED = %d, want 40", IROH_EVENT_DOC_CREATED)
	}
	if IROH_EVENT_DOC_JOINED != 41 {
		t.Errorf("IROH_EVENT_DOC_JOINED = %d, want 41", IROH_EVENT_DOC_JOINED)
	}
	if IROH_EVENT_DOC_SET != 42 {
		t.Errorf("IROH_EVENT_DOC_SET = %d, want 42", IROH_EVENT_DOC_SET)
	}
	if IROH_EVENT_DOC_GET != 43 {
		t.Errorf("IROH_EVENT_DOC_GET = %d, want 43", IROH_EVENT_DOC_GET)
	}
	if IROH_EVENT_DOC_SHARED != 44 {
		t.Errorf("IROH_EVENT_DOC_SHARED = %d, want 44", IROH_EVENT_DOC_SHARED)
	}
	if IROH_EVENT_AUTHOR_CREATED != 45 {
		t.Errorf("IROH_EVENT_AUTHOR_CREATED = %d, want 45", IROH_EVENT_AUTHOR_CREATED)
	}
	if IROH_EVENT_DOC_QUERY != 46 {
		t.Errorf("IROH_EVENT_DOC_QUERY = %d, want 46", IROH_EVENT_DOC_QUERY)
	}
	if IROH_EVENT_DOC_SUBSCRIBED != 47 {
		t.Errorf("IROH_EVENT_DOC_SUBSCRIBED = %d, want 47", IROH_EVENT_DOC_SUBSCRIBED)
	}
	if IROH_EVENT_DOC_EVENT != 48 {
		t.Errorf("IROH_EVENT_DOC_EVENT = %d, want 48", IROH_EVENT_DOC_EVENT)
	}
	if IROH_EVENT_DOC_JOINED_AND_SUBSCRIBED != 49 {
		t.Errorf("IROH_EVENT_DOC_JOINED_AND_SUBSCRIBED = %d, want 49", IROH_EVENT_DOC_JOINED_AND_SUBSCRIBED)
	}
}

func TestGossipConstants(t *testing.T) {
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

func TestHookConstants(t *testing.T) {
	if IROH_EVENT_HOOK_BEFORE_CONNECT != 70 {
		t.Errorf("IROH_EVENT_HOOK_BEFORE_CONNECT = %d, want 70", IROH_EVENT_HOOK_BEFORE_CONNECT)
	}
	if IROH_EVENT_HOOK_AFTER_CONNECT != 71 {
		t.Errorf("IROH_EVENT_HOOK_AFTER_CONNECT = %d, want 71", IROH_EVENT_HOOK_AFTER_CONNECT)
	}
	if IROH_EVENT_HOOK_INVOCATION_RELEASED != 72 {
		t.Errorf("IROH_EVENT_HOOK_INVOCATION_RELEASED = %d, want 72", IROH_EVENT_HOOK_INVOCATION_RELEASED)
	}
}
