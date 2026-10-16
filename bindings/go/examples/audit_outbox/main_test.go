//go:build cgo

package main

import (
	"context"
	"errors"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

// TestSenderReceiverEndToEnd runs the demo: the outbox delivers to the Go
// receiver through a lost acknowledgement (identical retry re-acknowledged),
// the acknowledgement log and watch report every event, an identical replay is
// acknowledged again and conflicting bytes under a stored ID get 409.
func TestSenderReceiverEndToEnd(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()
	var out strings.Builder
	if err := Run(ctx, &out); err != nil {
		t.Fatalf("%v\n%s", err, out.String())
	}
	for _, want := range []string{
		"acknowledgement log (3 entries)",
		"lost acknowledgement: receiver saw",
		"identical retry: 200",
		`same ID, different bytes: 409 {"error":"integrity_conflict","version":1}`,
	} {
		if !strings.Contains(out.String(), want) {
			t.Errorf("output lacks %q", want)
		}
	}
	t.Log("\n" + out.String())
}

// TestSenderAgainstStandaloneReceiver runs the sender subcommand's path.
func TestSenderAgainstStandaloneReceiver(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	dir := t.TempDir()
	receiver := &Receiver{Dir: dir + "/receiver"}
	endpoint := serve(t, receiver)
	var out strings.Builder
	if err := send(ctx, &out, endpoint, dir+"/outbox", 2, 0); err != nil {
		t.Fatalf("%v\n%s", err, out.String())
	}
	if !strings.Contains(out.String(), "acknowledgement log (2 entries)") {
		t.Fatalf("sender output:\n%s", out.String())
	}
	// A second run resumes after the last cursor it printed.
	out.Reset()
	if err := send(ctx, &out, endpoint, dir+"/outbox", 1, 2); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out.String(), "acknowledgement log (1 entries)") || !strings.Contains(out.String(), "cursor=3 seq=3") {
		t.Fatalf("resumed output:\n%s", out.String())
	}
}

// TestReceiverRefusesStoredConflict exercises the store check directly: an ID
// already stored with other bytes is a conflict even when the new bytes are
// self-consistent, and nothing of a conflicting batch is written.
func TestReceiverRefusesStoredConflict(t *testing.T) {
	receiver := &Receiver{Dir: t.TempDir()}
	id := "aster-audit-v1:" + strings.Repeat("a", 32) + ":1:" + strings.Repeat("b", 64)
	if err := receiver.commit([]verified{{id: id, bytes: []byte("one")}}); err != nil {
		t.Fatal(err)
	}
	if err := receiver.commit([]verified{{id: id, bytes: []byte("one")}}); err != nil {
		t.Fatalf("identical retry: %v", err)
	}
	other := "aster-audit-v1:" + strings.Repeat("a", 32) + ":2:" + strings.Repeat("c", 64)
	err := receiver.commit([]verified{{id: other, bytes: []byte("two")}, {id: id, bytes: []byte("changed")}})
	if !errors.Is(err, errIntegrityConflict) {
		t.Fatalf("conflict: %v", err)
	}
	if _, err := receiver.Envelope(other); err == nil {
		t.Fatal("a conflicting batch stored part of itself")
	}
	if BatchID([]string{"x"}) == BatchID([]string{"y"}) || len(BatchID(nil)) != 64 {
		t.Fatal("batch digest")
	}
}

func serve(t *testing.T, receiver *Receiver) string {
	t.Helper()
	server := httptest.NewServer(routes(receiver))
	t.Cleanup(server.Close)
	return server.URL + "/audit"
}
