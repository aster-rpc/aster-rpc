//go:build cgo

package aster

import (
	"bytes"
	"context"
	"encoding/hex"
	"errors"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func custodyHash(t *testing.T, data []byte) string {
	t.Helper()
	h, e := Blake3Hex(data)
	if e != nil {
		t.Fatal(e)
	}
	return h
}
func custodyGC(t *testing.T, ctx context.Context, n *Node, hash string, want bool) {
	t.Helper()
	if e := n.Blobs().GCRunOnce(ctx); e != nil {
		t.Fatal(e)
	}
	got, e := n.Blobs().Has(hash)
	if e != nil || got != want {
		t.Fatalf("GC hash %s: has=%v want=%v err=%v", hash, got, want, e)
	}
}

func TestBlobCustody_MultipleRootsCopyAndNoAutomaticTags(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	n := startTestNode(t, ctx)
	s, e := n.Blobs().NewCustodyScope()
	if e != nil {
		t.Fatal(e)
	}
	defer s.Close()
	data := []byte("copied\x00bytes")
	h := custodyHash(t, data)
	if e = s.Protect(ctx, h, BLOB_FORMAT_RAW); e != nil {
		t.Fatal(e)
	} // Not local yet.
	if got, e := s.AddBytes(ctx, data, BLOB_FORMAT_RAW); e != nil || string(got) != h {
		t.Fatalf("add %s %v", got, e)
	}
	path := filepath.Join(t.TempDir(), "copy ü.bin")
	other := []byte("file copy")
	os.WriteFile(path, other, 0600)
	h2, e := s.AddPath(ctx, path, BLOB_FORMAT_RAW)
	if e != nil {
		t.Fatal(e)
	}
	os.WriteFile(path, []byte("changed"), 0600)
	os.Remove(path)
	custodyGC(t, ctx, n, h, true)
	custodyGC(t, ctx, n, string(h2), true)
	if got, e := s.Read(ctx, string(h2)); e != nil || !bytes.Equal(got, other) {
		t.Fatalf("copied read %q %v", got, e)
	}
	if tags, e := n.Tags().ListAll(ctx); e != nil || len(tags) != 0 {
		t.Fatalf("temporary roots created tags %+v %v", tags, e)
	}
	if e = s.Close(); e != nil {
		t.Fatal(e)
	}
	if e = s.Close(); e != nil {
		t.Fatal(e)
	}
	if _, e = s.Read(ctx, h); !errors.Is(e, ErrAlreadyClosed) {
		t.Fatalf("closed scope %v", e)
	}
	custodyGC(t, ctx, n, h, false)
	custodyGC(t, ctx, n, string(h2), false)
}

func TestBlobCustody_HashSeqGraphAndNamedRetention(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	n := startTestNode(t, ctx)
	childScope, e := n.Blobs().NewCustodyScope()
	if e != nil {
		t.Fatal(e)
	}
	defer childScope.Close()
	child, e := childScope.AddBytes(ctx, []byte("HashSeq child"), BLOB_FORMAT_RAW)
	if e != nil {
		t.Fatal(e)
	}
	sequence, _ := hex.DecodeString(string(child))
	s, e := n.Blobs().NewCustodyScope()
	if e != nil {
		t.Fatal(e)
	}
	defer s.Close()
	root, e := s.AddBytes(ctx, sequence, BLOB_FORMAT_HASH_SEQ)
	if e != nil {
		t.Fatal(e)
	}
	childScope.Close()
	custodyGC(t, ctx, n, string(child), true)
	if got, e := s.Read(ctx, string(root)); e != nil || !bytes.Equal(got, sequence) {
		t.Fatalf("raw HashSeq %x %v", got, e)
	}
	if e = s.Retain(ctx, string(root), BLOB_FORMAT_HASH_SEQ, "verified/graph"); e != nil {
		t.Fatal(e)
	}
	s.Close()
	custodyGC(t, ctx, n, string(child), true)
	tags, e := n.Tags().ListAll(ctx)
	if e != nil || len(tags) != 1 || tags[0].Format != TAG_FORMAT_HASH_SEQ {
		t.Fatalf("typed tag %+v %v", tags, e)
	}
	n.Tags().Delete(ctx, "verified/graph")
	custodyGC(t, ctx, n, string(root), false)
	custodyGC(t, ctx, n, string(child), false)
}

func TestBlobCustody_VerifiedImportsPreserveOldTagAndNoExtraRoots(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	n := startTestNode(t, ctx)
	old := []byte("old retained")
	h := custodyHash(t, old)
	const tag = "verified/ü"
	if got, e := n.Blobs().AddBytesVerified(ctx, old, h, BLOB_FORMAT_RAW, tag); e != nil || string(got) != h {
		t.Fatalf("verified %s %v", got, e)
	}
	bad := []byte("mismatching content")
	badHash := custodyHash(t, bad)
	if _, e := n.Blobs().AddBytesVerified(ctx, bad, h, BLOB_FORMAT_RAW, tag); !errors.Is(e, ErrVerificationFailed) {
		t.Fatalf("mismatch %v", e)
	}
	custodyGC(t, ctx, n, h, true)
	custodyGC(t, ctx, n, badHash, false)
	path := filepath.Join(t.TempDir(), "verified.bin")
	data := []byte("replacement copied path")
	os.WriteFile(path, data, 0600)
	replacement := custodyHash(t, data)
	if got, e := n.Blobs().AddPathVerified(ctx, path, replacement, BLOB_FORMAT_RAW, tag); e != nil || string(got) != replacement {
		t.Fatalf("path %s %v", got, e)
	}
	os.Remove(path)
	tags, e := n.Tags().ListAll(ctx)
	if e != nil || len(tags) != 1 || string(tags[0].Hash) != replacement {
		t.Fatalf("roots %+v %v", tags, e)
	}
	custodyGC(t, ctx, n, h, false)
	custodyGC(t, ctx, n, replacement, true)
	n.Tags().Delete(ctx, tag)
	custodyGC(t, ctx, n, replacement, false)
}

func TestBlobCustody_ScopedFetchProtectedBeforeAndAfterDownload(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	provider := startTestNode(t, ctx)
	n := startTestNode(t, ctx)
	data := bytes.Repeat([]byte("scoped fetch"), 10000)
	h, e := provider.Blobs().AddBytes(ctx, data)
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
	s, e := n.Blobs().NewCustodyScope()
	if e != nil {
		t.Fatal(e)
	}
	defer s.Close()
	if e = s.Protect(ctx, string(h), BLOB_FORMAT_RAW); e != nil {
		t.Fatal(e)
	}
	got, report, e := s.DownloadHashFrom(ctx, string(h), []string{addr.EndpointID}, BLOB_FORMAT_RAW, FetchWhole, false)
	if e != nil || !bytes.Equal(got, data) || len(report.Providers) != 1 || report.BytesTransferred == 0 {
		t.Fatalf("fetch %d %+v %v", len(got), report, e)
	}
	custodyGC(t, ctx, n, string(h), true)
	s.Close()
	custodyGC(t, ctx, n, string(h), false)
}

// Done is first inspected by awaitCustodyOp, after synchronous native acceptance.
type custodyCloseContext struct {
	context.Context
	close func()
	once  bool
}

func (c *custodyCloseContext) Done() <-chan struct{} {
	if !c.once {
		c.once = true
		c.close()
	}
	return c.Context.Done()
}

func TestBlobCustody_CloseAcceptedOperationAndCancellationArbitration(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	n := startTestNode(t, ctx)
	s, e := n.Blobs().NewCustodyScope()
	if e != nil {
		t.Fatal(e)
	}
	defer s.Close()
	data := bytes.Repeat([]byte("accepted copy"), 1<<20)
	h := custodyHash(t, data)
	closing := &custodyCloseContext{Context: ctx, close: func() {
		if e := s.Close(); e != nil {
			t.Fatal(e)
		}
	}}
	if got, e := s.AddBytes(closing, data, BLOB_FORMAT_RAW); e != nil || string(got) != h {
		t.Fatalf("accepted close %s %v", got, e)
	}
	custodyGC(t, ctx, n, h, false)
	canceled := 0
	for i := 0; i < 12; i++ {
		pending, stop := context.WithCancel(ctx)
		racing := &custodyCloseContext{Context: pending, close: stop}
		got, e := n.Blobs().AddBytesVerified(racing, data, h, BLOB_FORMAT_RAW, "race/commit")
		if e == nil {
			if string(got) != h {
				t.Fatalf("committed %s", got)
			}
		} else if errors.Is(e, context.Canceled) {
			canceled++
			if tags, e := n.Tags().ListAll(ctx); e != nil || len(tags) != 0 {
				t.Fatalf("canceled commit left root %+v %v", tags, e)
			}
		} else {
			t.Fatal(e)
		}
		n.Tags().DeletePrefix(ctx, "race/")
		custodyGC(t, ctx, n, h, false)
	}
	if canceled == 0 {
		t.Fatal("race never exercised cooperative cancellation")
	}
	// Delay cancellation until the tag proves native commit completed, while
	// the original completion remains owned by awaitCustodyOp's channel.
	late, stop := context.WithCancel(ctx)
	committed := &custodyCloseContext{Context: late, close: func() {
		for {
			tags, err := n.Tags().ListAll(ctx)
			if err != nil {
				t.Fatal(err)
			}
			if len(tags) == 1 && tags[0].Name == "late/commit" {
				break
			}
			if ctx.Err() != nil {
				t.Fatal(ctx.Err())
			}
			time.Sleep(time.Millisecond)
		}
		stop()
	}}
	if got, err := n.Blobs().AddBytesVerified(committed, data, h, BLOB_FORMAT_RAW, "late/commit"); err != nil || string(got) != h || late.Err() == nil {
		t.Fatalf("committed success lost to cancellation: %s %v", got, err)
	}
}

func TestBlobCustody_VerifiedRestartAndInvalidInputs(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	cfg := testCfg()
	cfg.StoragePath = t.TempDir()
	n, e := StartNode(ctx, cfg, NodeOptions{})
	if e != nil {
		t.Fatal(e)
	}
	defer n.Close()
	data := []byte("persisted verified root")
	h := custodyHash(t, data)
	if _, e = n.Blobs().AddBytesVerified(ctx, data, h, BLOB_FORMAT_RAW, "restart/object"); e != nil {
		t.Fatal(e)
	}
	canceled, stop := context.WithCancel(ctx)
	stop()
	if _, e = n.Blobs().AddBytesVerified(canceled, []byte("no"), h, BLOB_FORMAT_RAW, "bad/canceled"); !errors.Is(e, context.Canceled) {
		t.Fatal(e)
	}
	if _, e = n.Blobs().AddBytesVerified(ctx, nil, "bad", BLOB_FORMAT_RAW, "bad/hash"); e == nil {
		t.Fatal("invalid hash accepted")
	}
	if _, e = n.Blobs().AddPathVerified(ctx, "", h, BLOB_FORMAT_RAW, "bad/path"); e == nil {
		t.Fatal("invalid path accepted")
	}
	if _, e = n.Blobs().AddBytesVerified(ctx, nil, h, BLOB_FORMAT_RAW, ""); e == nil {
		t.Fatal("empty tag accepted")
	}
	if _, e = n.Blobs().AddBytesVerified(ctx, nil, h, BlobFormat(99), "bad/format"); e == nil {
		t.Fatal("invalid format accepted")
	}
	if e = n.Close(); e != nil {
		t.Fatal(e)
	}
	n, e = StartNode(ctx, cfg, NodeOptions{})
	if e != nil {
		t.Fatal(e)
	}
	defer n.Close()
	tags, e := n.Tags().ListAll(ctx)
	if e != nil || len(tags) != 1 || tags[0].Name != "restart/object" {
		t.Fatalf("restart tags %+v %v", tags, e)
	}
	custodyGC(t, ctx, n, h, true)
	if got, e := n.Blobs().ReadRange(ctx, h, 0, uint64(len(data))); e != nil || !bytes.Equal(got, data) {
		t.Fatalf("restarted content %q %v", got, e)
	}
}
