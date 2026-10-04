//go:build cgo

package aster

import (
	"bytes"
	"context"
	"errors"
	"io"
	"math/rand"
	"strings"
	"testing"
	"time"
)

func startTestNode(t *testing.T, ctx context.Context, alpns ...string) *Node {
	t.Helper()
	n, err := StartNode(ctx, testCfg(), NodeOptions{ExtraALPNs: alpns})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = n.Close() })
	return n
}

func TestDataPlane_BlobsAndTags(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	provider := startTestNode(t, ctx)
	fetcher := startTestNode(t, ctx)

	data := bytes.Repeat([]byte("aster-blob-"), 2000)
	hash, err := provider.Blobs().AddBytes(ctx, data)
	if err != nil {
		t.Fatal(err)
	}
	part, err := provider.Blobs().ReadRange(ctx, string(hash), 6, 5)
	if err != nil || string(part) != "blob-" {
		t.Fatalf("read range %q %v", part, err)
	}

	paddr, _ := provider.NodeAddr()
	if err := fetcher.AddNodeAddr(paddr); err != nil {
		t.Fatal(err)
	}
	got, report, err := fetcher.Blobs().DownloadHashFrom(ctx, string(hash), []string{paddr.EndpointID}, BLOB_FORMAT_RAW, FetchWhole, false)
	if err != nil {
		t.Fatalf("download from: %v", err)
	}
	if !bytes.Equal(got, data) || len(report.Providers) != 1 || report.Providers[0].NodeID != paddr.EndpointID ||
		report.Providers[0].Attempts == 0 || report.BytesTransferred == 0 {
		t.Fatalf("fetch: %d bytes, report %+v", len(got), report)
	}
	if b, err := fetcher.Blobs().DownloadHash(ctx, string(hash), paddr.EndpointID, BLOB_FORMAT_RAW, true); err != nil || len(b) != 0 {
		t.Fatalf("download to store: %d %v", len(b), err)
	}
	if _, _, err := fetcher.Blobs().DownloadHashFrom(ctx, string(hash), nil, BLOB_FORMAT_RAW, FetchWhole, false); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("no providers: %v", err)
	}

	tags := provider.Tags()
	for _, name := range []string{"keep/a", "drop/a", "drop/b"} {
		if err := tags.Set(ctx, name, hash, TAG_FORMAT_RAW); err != nil {
			t.Fatal(err)
		}
	}
	n, err := tags.DeletePrefix(ctx, "drop/")
	if err != nil || n != 2 {
		t.Fatalf("delete prefix: %d %v", n, err)
	}
	if err := provider.Blobs().GCRunOnce(ctx); err != nil {
		t.Fatalf("gc: %v", err)
	}
}

func TestDataPlane_Docs(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	node := startTestNode(t, ctx)
	docs := node.Docs()
	doc, err := docs.Create(ctx)
	if err != nil {
		t.Fatal(err)
	}
	author, err := docs.CreateAuthor(ctx)
	if err != nil {
		t.Fatal(err)
	}
	rep, err := doc.AuthorityReport(ctx, node)
	if err != nil {
		t.Fatal(err)
	}
	if rep.Access != AccessWrite || len(rep.NamespaceID) != 64 {
		t.Fatalf("authority %+v", rep)
	}
	if fp, err := AuthorityFingerprint(rep.NamespaceID, AccessWrite); err != nil || fp != rep.Fingerprint {
		t.Fatalf("fingerprint %x vs %x: %v", fp, rep.Fingerprint, err)
	}
	if rep2, err := docs.AuthorityReport(ctx, rep.NamespaceID); err != nil || rep2 != rep {
		t.Fatalf("docs authority %+v %v", rep2, err)
	}

	hash, err := doc.SetBytesDurable(ctx, author, "k", []byte("value"))
	if err != nil || len(hash) != 64 {
		t.Fatalf("set durable %q %v", hash, err)
	}
	if ok, err := doc.EntryExists(ctx, author, "k"); err != nil || !ok {
		t.Fatalf("exists %v %v", ok, err)
	}
	if ok, err := doc.EntryExists(ctx, author, "missing"); err != nil || ok {
		t.Fatalf("missing exists %v %v", ok, err)
	}
	st, size, err := doc.EntryContentStatus(ctx, hash)
	if err != nil || st != BLOB_STATUS_COMPLETE || size != 5 {
		t.Fatalf("content status %v %d %v", st, size, err)
	}

	again, err := docs.Open(ctx, rep.NamespaceID)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	_ = again.Close()
	if _, err := docs.Open(ctx, strings.Repeat("0", 64)); !errors.Is(err, ErrNotFound) {
		t.Fatalf("open unknown: %v", err)
	}
	// Still open here, so retiring the write secret is BUSY (a success).
	out, err := docs.RetireLocalWriteAuthority(ctx, rep.NamespaceID)
	if err != nil {
		t.Fatalf("retire: %v", err)
	}
	if out.Retired && out.Access != AccessRead {
		t.Fatalf("retire outcome %+v", out)
	}
	if err := doc.ShutdownForRoleSwap(ctx, 5000); err != nil {
		t.Fatalf("shutdown for role swap: %v", err)
	}

	ns, _ := GenerateSecretKey()
	imported, err := docs.ImportWriteNamespace(ctx, ns)
	if err != nil {
		t.Fatalf("import write: %v", err)
	}
	irep, err := imported.AuthorityReport(ctx, node)
	nsID, _ := NamespaceSecretID(ns)
	if want, _ := FormatID(IDNamespaceID, nsID); err != nil || irep.NamespaceID != want || irep.Access != AccessWrite {
		t.Fatalf("imported authority %+v %v", irep, err)
	}
}

func TestDataPlane_PathsAndPriority(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	server := startTestNode(t, ctx, "test/1")
	client := startTestNode(t, ctx, "test/1")
	addr, _ := server.NodeAddr()
	accepted := make(chan *Connection, 1)
	go func() {
		c, err := server.Accept(ctx)
		if err != nil {
			t.Errorf("accept: %v", err)
		}
		accepted <- c
	}()
	conn, err := client.Connect(ctx, addr, "test/1")
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close("")
	sc := <-accepted
	if sc == nil {
		t.FailNow()
	}
	defer sc.Close("")
	paths, err := conn.Paths()
	if err != nil || len(paths) == 0 {
		t.Fatalf("paths %+v %v", paths, err)
	}
	if p, ok, err := conn.SelectedPath(); err != nil || !ok || !p.IsSelected || p.Kind != PathDirect || p.PeerHost == "" {
		t.Fatalf("selected %+v %v %v", p, ok, err)
	}
	send, _, err := conn.OpenBi(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if err := send.SetPriority(ctx, 7); err != nil {
		t.Fatal(err)
	}
	if p, err := send.Priority(ctx); err != nil || p != 7 {
		t.Fatalf("priority %d %v", p, err)
	}

	// io.Writer / io.Reader adapters.
	got := make(chan string, 1)
	go func() {
		_, recv, err := sc.AcceptBi(ctx)
		if err != nil {
			t.Errorf("accept bi: %v", err)
			got <- ""
			return
		}
		b, err := io.ReadAll(recv.Reader(ctx))
		if err != nil {
			t.Errorf("read all: %v", err)
		}
		got <- string(b)
	}()
	w := send.Writer(ctx)
	if _, err := io.WriteString(w, strings.Repeat("hello ", 1000)); err != nil {
		t.Fatal(err)
	}
	if err := w.Close(); err != nil {
		t.Fatal(err)
	}
	if s := <-got; s != strings.Repeat("hello ", 1000) {
		t.Fatalf("reader got %d bytes", len(s))
	}
}

// A server stream larger than the QUIC windows and the per-call response
// channel, read by a client that pauses, goes through the QUEUE_FULL ->
// reserve capacity -> RESPONSE_WRITABLE -> retry path. Incompressible data so
// zstd cannot shrink it; whether the channel fills depends on scheduling, so
// retry a few times.
func TestRPC_QueueFullBackpressure(t *testing.T) {
	const items, size = 2000, 8 << 10
	chunk := make([]byte, size)
	rand.New(rand.NewSource(42)).Read(chunk)
	svc := NewService("Flood", 1, ServerStream("flood", func(_ context.Context, _ *testEchoRequest, send func(*testEchoReply) error) error {
		for i := int32(0); i < items; i++ {
			if err := send(&testEchoReply{Count: i, Data: chunk}); err != nil {
				return err
			}
		}
		return nil
	}))
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
	ctx, cancel := context.WithTimeout(context.Background(), 120*time.Second)
	defer cancel()
	for attempt := 0; attempt < 4 && f.srv.QueueFullHits() == 0; attempt++ {
		var n int32
		for r, err := range CallServerStream[testEchoRequest, testEchoReply](ctx, f.conn, "Flood", "flood", &testEchoRequest{Data: []byte{}}) {
			if err != nil {
				t.Fatalf("after %d items: %v", n, err)
			}
			if r.Count != n || !bytes.Equal(r.Data, chunk) {
				t.Fatalf("item %d: count %d, %d bytes", n, r.Count, len(r.Data))
			}
			if n == 0 {
				time.Sleep(1500 * time.Millisecond) // let the server fill the queue
			}
			n++
		}
		if n != items {
			t.Fatalf("%d items, want %d", n, items)
		}
	}
	if f.srv.QueueFullHits() == 0 {
		t.Fatal("expected at least one QUEUE_FULL")
	}
}
