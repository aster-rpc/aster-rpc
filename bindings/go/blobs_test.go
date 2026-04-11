//go:build cgo

package aster

import (
	"encoding/json"
	"testing"
)

func TestBlobID(t *testing.T) {
	// Valid 64-char hex string
	hex64 := "a" + string(make([]byte, 63))
	id := BlobID(hex64)
	if string(id) != hex64 {
		t.Errorf("expected %s, got %s", hex64, id)
	}
}

func TestBlobStatusString(t *testing.T) {
	tests := []struct {
		status   BlobStatus
		expected string
	}{
		{BLOB_STATUS_NOT_FOUND, "NOT_FOUND"},
		{BLOB_STATUS_PARTIAL, "PARTIAL"},
		{BLOB_STATUS_COMPLETE, "COMPLETE"},
		{BlobStatus(999), "UNKNOWN"},
	}

	for _, tt := range tests {
		t.Run(tt.expected, func(t *testing.T) {
			if got := tt.status.String(); got != tt.expected {
				t.Errorf("BlobStatus.String() = %v, want %v", got, tt.expected)
			}
		})
	}
}

func TestBlobFormatCodes(t *testing.T) {
	if BLOB_FORMAT_RAW != 0 {
		t.Errorf("BLOB_FORMAT_RAW = %d, want 0", BLOB_FORMAT_RAW)
	}
	if BLOB_FORMAT_HASH_SEQ != 1 {
		t.Errorf("BLOB_FORMAT_HASH_SEQ = %d, want 1", BLOB_FORMAT_HASH_SEQ)
	}
}

func TestBlobTicket(t *testing.T) {
	ticket := BlobTicket("blob1abc123")
	if string(ticket) != "blob1abc123" {
		t.Errorf("expected blob1abc123, got %s", ticket)
	}
}

func TestBlobEntry(t *testing.T) {
	hash := BlobID("b" + string(make([]byte, 63)))
	entry := BlobEntry{
		Name: "test.bin",
		Hash: hash,
		Size: 2048,
	}
	if entry.Name != "test.bin" {
		t.Errorf("expected test.bin, got %s", entry.Name)
	}
	if entry.Hash != hash {
		t.Errorf("expected hash %s, got %s", hash, entry.Hash)
	}
	if entry.Size != 2048 {
		t.Errorf("expected 2048, got %d", entry.Size)
	}
}

func TestBlobCollection(t *testing.T) {
	hash := BlobID("c" + string(make([]byte, 63)))
	entry := BlobEntry{Name: "file.txt", Hash: hash, Size: 1024}
	collection := &BlobCollection{
		Entries: []BlobEntry{entry},
	}
	if len(collection.Entries) != 1 {
		t.Errorf("expected 1 entry, got %d", len(collection.Entries))
	}
	if collection.Entries[0].Name != "file.txt" {
		t.Errorf("expected file.txt, got %s", collection.Entries[0].Name)
	}
}

func TestBlobInfo(t *testing.T) {
	hash := BlobID("d" + string(make([]byte, 63)))
	info := &BlobInfo{
		Hash:   hash,
		Size:   4096,
		Status: BLOB_STATUS_COMPLETE,
	}
	if info.Hash != hash {
		t.Errorf("expected hash %s, got %s", hash, info.Hash)
	}
	if info.Size != 4096 {
		t.Errorf("expected 4096, got %d", info.Size)
	}
	if info.Status != BLOB_STATUS_COMPLETE {
		t.Errorf("expected COMPLETE, got %v", info.Status)
	}
}

func TestParseBlobCollectionJSON(t *testing.T) {
	// JSON format: [[name, hash, size], ...]
	jsonStr := `[["file1.txt","` + "a1b2c3d4e5f6" + string(make([]byte, 52)) + `",1024],["file2.txt","` + "f6e5d4c3b2a1" + string(make([]byte, 52)) + `",2048]]`

	collection, err := parseBlobCollectionJSON(jsonStr)
	if err != nil {
		t.Fatalf("parseBlobCollectionJSON failed: %v", err)
	}

	if len(collection.Entries) != 2 {
		t.Fatalf("expected 2 entries, got %d", len(collection.Entries))
	}

	if collection.Entries[0].Name != "file1.txt" {
		t.Errorf("expected file1.txt, got %s", collection.Entries[0].Name)
	}
	if collection.Entries[0].Size != 1024 {
		t.Errorf("expected size 1024, got %d", collection.Entries[0].Size)
	}

	if collection.Entries[1].Name != "file2.txt" {
		t.Errorf("expected file2.txt, got %s", collection.Entries[1].Name)
	}
	if collection.Entries[1].Size != 2048 {
		t.Errorf("expected size 2048, got %d", collection.Entries[1].Size)
	}
}

func TestParseBlobCollectionJSON_empty(t *testing.T) {
	collection, err := parseBlobCollectionJSON(`[]`)
	if err != nil {
		t.Fatalf("parseBlobCollectionJSON failed: %v", err)
	}
	if len(collection.Entries) != 0 {
		t.Errorf("expected 0 entries, got %d", len(collection.Entries))
	}
}

func TestParseBlobCollectionJSON_invalid(t *testing.T) {
	// Invalid JSON should return empty collection, not error
	_, err := parseBlobCollectionJSON(`not json`)
	if err != nil {
		t.Fatalf("expected no error for invalid json, got %v", err)
	}
}

func TestBlobStatusConstants(t *testing.T) {
	// Ensure constants match expected values
	if IROH_EVENT_BLOB_ADDED != 30 {
		t.Errorf("IROH_EVENT_BLOB_ADDED = %d, want 30", IROH_EVENT_BLOB_ADDED)
	}
	if IROH_EVENT_BLOB_READ != 31 {
		t.Errorf("IROH_EVENT_BLOB_READ = %d, want 31", IROH_EVENT_BLOB_READ)
	}
	if IROH_EVENT_BLOB_DOWNLOADED != 32 {
		t.Errorf("IROH_EVENT_BLOB_DOWNLOADED = %d, want 32", IROH_EVENT_BLOB_DOWNLOADED)
	}
}

func TestBlobIDType(t *testing.T) {
	// BlobID is just a string type alias, verify it works with string operations
	id := BlobID("test")
	if string(id) != "test" {
		t.Errorf("expected test, got %s", id)
	}
}

// BenchmarkBlobCollectionParse benchmarks JSON parsing for collections.
func BenchmarkBlobCollectionParse(b *testing.B) {
	jsonStr := `[["file1.txt","` + "a1b2c3d4e5f6" + string(make([]byte, 52)) + `",1024],["file2.txt","` + "f6e5d4c3b2a1" + string(make([]byte, 52)) + `",2048]]`

	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		parseBlobCollectionJSON(jsonStr)
	}
}

// Ensure json package is used (to verify import)
var _ = json.Marshal
