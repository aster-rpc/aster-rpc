//go:build cgo

package aster

import (
	"bytes"
	"context"
	"errors"
	"testing"
	"time"
)

func TestDocNativeReadQueryDelete(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 30*time.Second)
	defer cancel()
	node := startTestNode(t, ctx)
	docs := node.Docs()
	doc, err := docs.Create(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer doc.Close()
	id, err := doc.ID()
	if err != nil || len(id) != 64 {
		t.Fatalf("ID = %q, %v", id, err)
	}
	author, err := docs.CreateAuthor(ctx)
	if err != nil {
		t.Fatal(err)
	}
	value := []byte("hello\x00\xff")
	key := "policy/\x00\xff"
	if err := doc.SetBytes(ctx, author, key, value); err != nil {
		t.Fatal(err)
	}
	exact, err := doc.GetExact(ctx, author, key)
	if err != nil || exact == nil {
		t.Fatalf("GetExact = %+v, %v", exact, err)
	}
	if exact.Key != key || exact.Author != author || exact.ContentLen != uint64(len(value)) || !bytes.Equal(exact.Value, value) || exact.ContentHash != "" || exact.Timestamp != 0 {
		t.Fatalf("GetExact = %+v", exact)
	}
	exact.Value[0] = 'x'
	again, err := doc.GetExact(ctx, author, key)
	if err != nil || again == nil || !bytes.Equal(again.Value, value) {
		t.Fatalf("second read = %+v, %v", again, err)
	}
	if missing, err := doc.GetExact(ctx, author, "missing"); err != nil || missing != nil {
		t.Fatalf("missing = %+v, %v", missing, err)
	}

	entries, err := doc.Query(ctx, QUERY_MODE_KEY_PREFIX, "", 0)
	if err != nil || len(entries) != 1 {
		t.Fatalf("query = %+v, %v", entries, err)
	}
	entry := entries[0]
	if entry.Key != key || entry.Author != author || len(entry.ContentHash) != 64 || entry.ContentLen != uint64(len(value)) || entry.Timestamp == 0 || entry.Value != nil {
		t.Fatalf("metadata = %+v", entry)
	}
	content, err := doc.ReadEntryContent(ctx, entry.ContentHash)
	if err != nil || !bytes.Equal(content, value) {
		t.Fatalf("content = %x, %v", content, err)
	}

	// Each query mode uses the same metadata ABI, including a multi-author result.
	second, err := docs.CreateAuthor(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if err := doc.SetBytes(ctx, second, key, []byte("newest")); err != nil {
		t.Fatal(err)
	}
	entries, err = doc.Query(ctx, QUERY_MODE_KEY_EXACT, key, 0)
	if err != nil || len(entries) != 2 {
		t.Fatalf("exact query = %+v, %v", entries, err)
	}
	limited, err := doc.Query(ctx, QUERY_MODE_KEY_EXACT, key, 1)
	if err != nil || len(limited) != 1 {
		t.Fatalf("limited query = %+v, %v", limited, err)
	}
	for _, mode := range []QueryMode{QUERY_MODE_LATEST_EXACT, QUERY_MODE_LATEST_PREFIX} {
		latest, err := doc.Query(ctx, mode, key, 0)
		if err != nil || len(latest) != 1 || latest[0].Author != second {
			t.Fatalf("latest (%d) = %+v, %v", mode, latest, err)
		}
	}
	if n, err := doc.Del(ctx, author, "policy/"); err != nil || n != 1 {
		t.Fatalf("Del = %d, %v", n, err)
	}
	if missing, err := doc.GetExact(ctx, author, key); err != nil || missing != nil {
		t.Fatalf("deleted = %+v, %v", missing, err)
	}
	if n, err := doc.Del(ctx, second, ""); err != nil || n != 1 {
		t.Fatalf("Del all = %d, %v", n, err)
	}
	if entries, err := doc.Query(ctx, QUERY_MODE_KEY_PREFIX, "", 0); err != nil || len(entries) != 0 {
		t.Fatalf("after deletion = %+v, %v", entries, err)
	}

	if _, err := doc.ShareWithAddr(ctx, 99); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("invalid share mode = %v", err)
	}
	if err := doc.StartSync(ctx, "invalid-peer"); err == nil {
		t.Fatal("invalid peer accepted")
	}
	if _, err := doc.GetExact(ctx, "", ""); err == nil {
		t.Fatal("invalid author accepted")
	}
	if err := doc.SetBytes(ctx, "", "", nil); err == nil {
		t.Fatal("invalid write accepted")
	}
}

func TestDocNativeAddressedShareAndExplicitSync(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 30*time.Second)
	defer cancel()
	provider := startTestNode(t, ctx)
	consumer := startTestNode(t, ctx)
	source, err := provider.Docs().Create(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer source.Close()
	author, err := provider.Docs().CreateAuthor(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if err := source.SetBytes(ctx, author, "manifest/root", []byte("first")); err != nil {
		t.Fatal(err)
	}
	ticket, err := source.ShareWithAddr(ctx, DOC_SHARE_READ)
	if err != nil || ticket == "" {
		t.Fatalf("share with addr = %q, %v", ticket, err)
	}
	joined, err := consumer.Docs().Join(ctx, ticket)
	if err != nil {
		t.Fatal(err)
	}
	defer joined.Close()
	wantID, err := source.ID()
	if err != nil {
		t.Fatal(err)
	}
	if got, err := joined.ID(); err != nil || got != wantID {
		t.Fatalf("joined ID = %q, %v; want %q", got, err, wantID)
	}
	if err := joined.Leave(ctx); err != nil {
		t.Fatal(err)
	}
	addr, err := provider.NodeAddr()
	if err != nil {
		t.Fatal(err)
	}
	if err := consumer.AddNodeAddr(addr); err != nil {
		t.Fatal(err)
	}
	if err := joined.StartSync(ctx, addr.EndpointID); err != nil {
		t.Fatal(err)
	}
	waitDocValue(t, ctx, joined, author, "manifest/root", "first")
	if err := source.SetBytes(ctx, author, "manifest/root", []byte("second")); err != nil {
		t.Fatal(err)
	}
	waitDocValue(t, ctx, joined, author, "manifest/root", "second")
	if err := joined.Leave(ctx); err != nil {
		t.Fatal(err)
	}
	if err := joined.StartSync(ctx); err != nil {
		t.Fatalf("legacy StartSync: %v", err)
	}
	if err := joined.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := joined.ID(); !errors.Is(err, ErrNotFound) {
		t.Fatalf("ID after close = %v", err)
	}
	if _, err := consumer.Docs().Join(ctx, ""); err == nil {
		t.Fatal("empty ticket accepted")
	}
	if _, err := source.Share(ctx, DOC_SHARE_WRITE); err != nil {
		t.Fatalf("legacy Share: %v", err)
	}
}

func waitDocValue(t *testing.T, ctx context.Context, doc *Doc, author AuthorID, key, value string) {
	t.Helper()
	ticker := time.NewTicker(20 * time.Millisecond)
	defer ticker.Stop()
	for {
		got, err := doc.GetExact(ctx, author, key)
		if err == nil && got != nil && string(got.Value) == value {
			return
		}
		select {
		case <-ctx.Done():
			t.Fatalf("waiting for %q: got %+v, %v: %v", value, got, err, ctx.Err())
		case <-ticker.C:
		}
	}
}
