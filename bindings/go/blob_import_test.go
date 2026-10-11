//go:build cgo

package aster

import (
	"bytes"
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestBlobImport_AddPathOwnsCopy(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	node := startTestNode(t, ctx)
	path := filepath.Join(t.TempDir(), "source ü.bin")
	data := []byte("file import\x00preserves binary data")
	if err := os.WriteFile(path, data, 0o600); err != nil {
		t.Fatal(err)
	}
	hash, err := node.Blobs().AddPath(ctx, path)
	want, hashErr := Blake3Hex(data)
	if err != nil || hashErr != nil || string(hash) != want {
		t.Fatalf("import hash %q, want %q: %v %v", hash, want, err, hashErr)
	}
	if err := os.WriteFile(path, []byte("modified source"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(path); err != nil {
		t.Fatal(err)
	}
	if err := node.Blobs().GCRunOnce(ctx); err != nil {
		t.Fatal(err)
	}
	got, err := node.Blobs().ReadRange(ctx, string(hash), 0, uint64(len(data)))
	if err != nil || !bytes.Equal(got, data) {
		t.Fatalf("copied and auto-retained blob %q: %v", got, err)
	}
}

func TestBlobImport_NamedTagControlsRetention(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	node := startTestNode(t, ctx)
	path := filepath.Join(t.TempDir(), "retained.bin")
	const name = "portal/tree-ü/verified-blob"
	data := []byte("retained through the named tag only")
	if err := os.WriteFile(path, data, 0o600); err != nil {
		t.Fatal(err)
	}
	hash, err := node.Blobs().AddPathWithNamedTag(ctx, path, name)
	if err != nil {
		t.Fatal(err)
	}
	want, err := Blake3Hex(data)
	if err != nil || string(hash) != want {
		t.Fatalf("import hash %q, want %q: %v", hash, want, err)
	}
	entries, err := node.Tags().ListAll(ctx)
	if err != nil || len(entries) != 1 || entries[0].Name != name || entries[0].Hash != hash || entries[0].Format != TAG_FORMAT_RAW {
		t.Fatalf("exact named retention: %+v: %v", entries, err)
	}
	if err := os.Remove(path); err != nil {
		t.Fatal(err)
	}
	if err := node.Blobs().GCRunOnce(ctx); err != nil {
		t.Fatal(err)
	}
	got, err := node.Blobs().ReadRange(ctx, string(hash), 0, uint64(len(data)))
	if err != nil || !bytes.Equal(got, data) {
		t.Fatalf("named tag did not retain copied blob %q: %v", got, err)
	}
	// Reusing the name replaces its root rather than keeping the old blob
	// alive under an additional automatically generated tag.
	if err := os.WriteFile(path, []byte("replacement"), 0o600); err != nil {
		t.Fatal(err)
	}
	replacement, err := node.Blobs().AddPathWithNamedTag(ctx, path, name)
	if err != nil || replacement == hash {
		t.Fatalf("replace named root: %q: %v", replacement, err)
	}
	entries, err = node.Tags().ListAll(ctx)
	if err != nil || len(entries) != 1 || entries[0].Name != name || entries[0].Hash != replacement {
		t.Fatalf("replacement roots: %+v: %v", entries, err)
	}
	if err := node.Blobs().GCRunOnce(ctx); err != nil {
		t.Fatal(err)
	}
	if has, err := node.Blobs().Has(string(hash)); err != nil || has {
		t.Fatalf("old named root still retained: %v: %v", has, err)
	}
	if _, err := node.Tags().Delete(ctx, name); err != nil {
		t.Fatal(err)
	}
	if err := node.Blobs().GCRunOnce(ctx); err != nil {
		t.Fatal(err)
	}
	if has, err := node.Blobs().Has(string(replacement)); err != nil || has {
		t.Fatalf("deleted named root still retained: %v: %v", has, err)
	}
	if entries, err := node.Tags().ListAll(ctx); err != nil || len(entries) != 0 {
		t.Fatalf("deleted named root left tags: %+v: %v", entries, err)
	}
}

func TestBlobImport_InvalidArguments(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	node := startTestNode(t, ctx)
	path := filepath.Join(t.TempDir(), "empty.bin")
	if err := os.WriteFile(path, nil, 0o600); err != nil {
		t.Fatal(err)
	}
	for _, tc := range []struct {
		name string
		call func() (BlobID, error)
	}{
		{"empty path", func() (BlobID, error) { return node.Blobs().AddPath(ctx, "") }},
		{"missing path", func() (BlobID, error) { return node.Blobs().AddPath(ctx, path+".missing") }},
		{"invalid UTF-8 path", func() (BlobID, error) { return node.Blobs().AddPath(ctx, "\xff") }},
		{"missing named path", func() (BlobID, error) {
			return node.Blobs().AddPathWithNamedTag(ctx, path+".missing", "invalid/missing")
		}},
		{"invalid UTF-8 tag", func() (BlobID, error) { return node.Blobs().AddPathWithNamedTag(ctx, path, "\xff") }},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if hash, err := tc.call(); err == nil || hash != "" {
				t.Fatalf("invalid import returned %q: %v", hash, err)
			}
		})
	}
	if entries, err := node.Tags().ListAll(ctx); err != nil || len(entries) != 0 {
		t.Fatalf("failed imports left roots: %+v: %v", entries, err)
	}
	// The failures must not poison subsequent completions; importing an empty
	// file is valid even though an empty filesystem path is not.
	hash, err := node.Blobs().AddPathWithNamedTag(ctx, path, "valid/empty")
	if err != nil {
		t.Fatal(err)
	}
	status, size, err := node.Blobs().Status(string(hash))
	if err != nil || status != BLOB_STATUS_COMPLETE || size != 0 {
		t.Fatalf("empty file: %v %d: %v", status, size, err)
	}
}

func TestBlobImport_CanceledBeforeStart(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	node := startTestNode(t, ctx)
	path := filepath.Join(t.TempDir(), "canceled.bin")
	if err := os.WriteFile(path, []byte("must not be imported"), 0o600); err != nil {
		t.Fatal(err)
	}
	canceled, stop := context.WithCancel(ctx)
	stop()
	if hash, err := node.Blobs().AddPath(canceled, path); !errors.Is(err, context.Canceled) || hash != "" {
		t.Fatalf("canceled import: %q: %v", hash, err)
	}
	if hash, err := node.Blobs().AddPathWithNamedTag(canceled, path, "canceled/named"); !errors.Is(err, context.Canceled) || hash != "" {
		t.Fatalf("canceled named import: %q: %v", hash, err)
	}
	if entries, err := node.Tags().ListAll(ctx); err != nil || len(entries) != 0 {
		t.Fatalf("canceled imports created roots: %+v: %v", entries, err)
	}
	hash, err := Blake3Hex([]byte("must not be imported"))
	if err != nil {
		t.Fatal(err)
	}
	if has, err := node.Blobs().Has(hash); err != nil || has {
		t.Fatalf("canceled imports stored data: %v: %v", has, err)
	}
}
