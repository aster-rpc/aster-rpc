//go:build cgo

package aster

import (
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"strings"
	"testing"
	"time"
)

func TestDocBatch_OrderedDuplicatesMissingEmptyAndLarge(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 30*time.Second)
	defer cancel()
	n := startTestNode(t, ctx)
	doc, e := n.Docs().Create(ctx)
	if e != nil {
		t.Fatal(e)
	}
	defer doc.Close()
	author, e := n.Docs().CreateAuthor(ctx)
	if e != nil {
		t.Fatal(e)
	}
	small := []byte{0, 255, 1, 0, 9}
	hash, e := doc.SetBytesDurable(ctx, author, "small", small)
	if e != nil {
		t.Fatal(e)
	}
	large := bytes.Repeat([]byte("large local value\x00"), 100000)
	largeHash, e := doc.SetBytesDurable(ctx, author, "large", large)
	if e != nil {
		t.Fatal(e)
	}
	scope, e := n.Blobs().NewCustodyScope()
	if e != nil {
		t.Fatal(e)
	}
	defer scope.Close()
	empty, e := scope.AddBytes(ctx, nil, BLOB_FORMAT_RAW)
	if e != nil {
		t.Fatal(e)
	}
	missing := BlobID(strings.Repeat("a", 64))
	values, e := doc.ReadEntryContentsIfComplete(ctx, []BlobID{missing, hash, empty, largeHash, hash, missing})
	if e != nil || len(values) != 6 {
		t.Fatalf("batch %d %v", len(values), e)
	}
	if values[0] != nil || values[5] != nil || values[2] == nil || len(values[2]) != 0 || !bytes.Equal(values[1], small) || !bytes.Equal(values[3], large) || !bytes.Equal(values[4], small) {
		t.Fatalf("batch slot shape/values incorrect")
	}
	values[1][0] = 42
	if !bytes.Equal(values[4], small) {
		t.Fatal("duplicate slots alias")
	}
	retry, e := doc.ReadEntryContentsIfComplete(ctx, []BlobID{hash})
	if e != nil || !bytes.Equal(retry[0], small) {
		t.Fatalf("returned data aliases store %v", e)
	}
	values, e = doc.ReadEntryContentsIfComplete(ctx, nil)
	if e != nil || values == nil || len(values) != 0 {
		t.Fatalf("empty batch %+v %v", values, e)
	}
}

func TestDocBatch_LocalOnlyAndCustodyGC(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 30*time.Second)
	defer cancel()
	provider := startTestNode(t, ctx)
	n := startTestNode(t, ctx)
	remote, e := provider.Blobs().AddBytes(ctx, []byte("available remotely but not fetched"))
	if e != nil {
		t.Fatal(e)
	}
	addr, e := provider.NodeAddr()
	if e != nil {
		t.Fatal(e)
	}
	if e = n.AddNodeAddr(addr); e != nil {
		t.Fatal(e)
	}
	doc, e := n.Docs().Create(ctx)
	if e != nil {
		t.Fatal(e)
	}
	defer doc.Close()
	values, e := doc.ReadEntryContentsIfComplete(ctx, []BlobID{remote})
	if e != nil || len(values) != 1 || values[0] != nil {
		t.Fatalf("nonlocal content fetched %v %v", values, e)
	}
	if has, e := n.Blobs().Has(string(remote)); e != nil || has {
		t.Fatalf("read imported remote content %v %v", has, e)
	}
	scope, e := n.Blobs().NewCustodyScope()
	if e != nil {
		t.Fatal(e)
	}
	defer scope.Close()
	data := bytes.Repeat([]byte("guarded local export"), 100000)
	hash, e := scope.AddBytes(ctx, data, BLOB_FORMAT_RAW)
	if e != nil {
		t.Fatal(e)
	}
	custodyGC(t, ctx, n, string(hash), true)
	values, e = doc.ReadEntryContentsIfComplete(ctx, []BlobID{hash})
	if e != nil || !bytes.Equal(values[0], data) {
		t.Fatalf("protected local export %v", e)
	}
	if e = scope.Close(); e != nil {
		t.Fatal(e)
	}
	custodyGC(t, ctx, n, string(hash), false)
	after, e := doc.ReadEntryContentsIfComplete(ctx, []BlobID{hash})
	if e != nil || after[0] != nil {
		t.Fatalf("collected content %v %v", after, e)
	}
	if !bytes.Equal(values[0], data) {
		t.Fatal("returned copy changed after GC")
	}
}

func TestDocBatch_InvalidCancellationAndRetry(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 30*time.Second)
	defer cancel()
	n := startTestNode(t, ctx)
	doc, e := n.Docs().Create(ctx)
	if e != nil {
		t.Fatal(e)
	}
	defer doc.Close()
	scope, e := n.Blobs().NewCustodyScope()
	if e != nil {
		t.Fatal(e)
	}
	defer scope.Close()
	data := bytes.Repeat([]byte("cancel local batch"), 100000)
	hash, e := scope.AddBytes(ctx, data, BLOB_FORMAT_RAW)
	if e != nil {
		t.Fatal(e)
	}
	for _, bad := range []BlobID{"", "bad", BlobID(strings.Repeat("g", 64))} {
		if _, e := doc.ReadEntryContentsIfComplete(ctx, []BlobID{hash, bad}); !errors.Is(e, ErrInvalidArgument) {
			t.Fatalf("bad hash %q: %v", bad, e)
		}
	}
	canceled, stop := context.WithCancel(ctx)
	stop()
	if _, e = doc.ReadEntryContentsIfComplete(canceled, []BlobID{hash}); !errors.Is(e, context.Canceled) {
		t.Fatalf("precanceled %v", e)
	}
	hashes := make([]BlobID, 8)
	for i := range hashes {
		hashes[i] = hash
	}
	count := 0
	for i := 0; i < 8; i++ {
		racing, stop := context.WithCancel(ctx)
		submitted := &custodyCloseContext{Context: racing, close: stop}
		values, e := doc.ReadEntryContentsIfComplete(submitted, hashes)
		if errors.Is(e, context.Canceled) {
			count++
		} else if e != nil {
			t.Fatal(e)
		} else if len(values) != len(hashes) || !bytes.Equal(values[0], data) {
			t.Fatal("completed batch malformed")
		}
		values, e = doc.ReadEntryContentsIfComplete(ctx, []BlobID{hash})
		if e != nil || !bytes.Equal(values[0], data) {
			t.Fatalf("retry %v", e)
		}
	}
	if count == 0 {
		t.Fatal("never exercised cancellation")
	}
	deadline := time.Now().Add(waitBudget())
	for {
		n.runtime.mu.Lock()
		pending := len(n.runtime.ops) + len(n.runtime.pending) + len(n.runtime.abandoned)
		n.runtime.mu.Unlock()
		if pending == 0 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("canceled batches retained %d completion entries", pending)
		}
		time.Sleep(time.Millisecond)
	}
	if e = doc.Close(); e != nil {
		t.Fatal(e)
	}
	if _, e = doc.ReadEntryContentsIfComplete(ctx, nil); !errors.Is(e, ErrNotFound) {
		t.Fatalf("closed document %v", e)
	}
}

func TestDocBatch_DecoderStrictness(t *testing.T) {
	var b bytes.Buffer
	binary.Write(&b, binary.LittleEndian, uint32(4))
	b.WriteByte(0)
	for _, v := range [][]byte{{}, {0, 255, 7}, {8}} {
		b.WriteByte(1)
		binary.Write(&b, binary.LittleEndian, uint64(len(v)))
		b.Write(v)
	}
	valid := b.Bytes()
	out, e := decodeDocContentsBatch(valid, 4)
	if e != nil || len(out) != 4 || out[0] != nil || out[1] == nil || len(out[1]) != 0 || !bytes.Equal(out[2], []byte{0, 255, 7}) {
		t.Fatalf("decode %+v %v", out, e)
	}
	valid[len(valid)-1] = 99
	if out[3][0] != 8 {
		t.Fatal("decoder output aliases payload")
	}
	for n := 0; n < len(valid); n++ {
		if _, e := decodeDocContentsBatch(valid[:n], 4); !errors.Is(e, ErrInternal) {
			t.Fatalf("accepted truncation %d: %v", n, e)
		}
	}
	cases := [][]byte{append(bytes.Clone(valid), 0), {1, 0, 0, 0, 2}, {1, 0, 0, 0, 1, 255, 255, 255, 255, 255, 255, 255, 255}, {255, 255, 255, 255}}
	for _, payload := range cases {
		expected := 1
		if len(payload) > 20 {
			expected = 4
		}
		if _, e := decodeDocContentsBatch(payload, expected); !errors.Is(e, ErrInternal) {
			t.Fatalf("accepted malformed %x: %v", payload, e)
		}
	}
	if _, e := decodeDocContentsBatch(valid, 3); e == nil {
		t.Fatal("accepted mismatched count")
	}
	// A matching but impossible count must fail before result allocation.
	impossible := make([]byte, 4)
	binary.LittleEndian.PutUint32(impossible, uint32(1<<31-1))
	if _, err := decodeDocContentsBatch(impossible, 1<<31-1); !errors.Is(err, ErrInternal) {
		t.Fatalf("accepted oversized count: %v", err)
	}
	if out, e := decodeDocContentsBatch([]byte{0, 0, 0, 0}, 0); e != nil || out == nil || len(out) != 0 {
		t.Fatalf("empty decode %+v %v", out, e)
	}
}
