//go:build cgo

package aster

import (
	"context"
	"errors"
	"io"
	"testing"
)

// A client connecting is a Connected event (or is already in the snapshot),
// and its closing is a Disconnected with reason closed.
func TestPeerEvents_ConnectAndDisconnect(t *testing.T) {
	svc := interopService()
	f := newRPCFixture(t, ServerOptions{Services: []*Service{svc}})
	ctx, cancel := context.WithTimeout(context.Background(), waitBudget())
	defer cancel()
	clientID, err := f.cl.ID()
	if err != nil {
		t.Fatal(err)
	}

	snapshot, sub, err := f.srv.PeerEvents(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer sub.Close()
	// The fixture's client may or may not be admitted by now: it is in the
	// snapshot, or its Connected follows, never both.
	seen := 0
	for _, p := range snapshot {
		if p.Peer == clientID {
			seen++
			if p.ALPN != "aster/1" || p.Connections < 1 || p.Identity.EndpointID != clientID || p.Identity.Principal != "" {
				t.Fatalf("snapshot entry %+v", p)
			}
		}
	}
	if seen == 0 {
		ev, err := sub.Recv(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if ev.Type != PeerConnected || ev.Peer != clientID || ev.ALPN != "aster/1" || ev.Identity.EndpointID != clientID {
			t.Fatalf("event %+v", ev)
		}
	}

	if err := f.cl.Close(); err != nil {
		t.Fatal(err)
	}
	ev, err := sub.Recv(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if ev.Type != PeerDisconnected || ev.Peer != clientID || ev.Reason != DisconnectClosed {
		t.Fatalf("event %+v", ev)
	}

	// Close wakes a pending Recv with io.EOF; a closed subscription stays EOF.
	done := make(chan error, 1)
	go func() { _, err := sub.Recv(ctx); done <- err }()
	if err := sub.Close(); err != nil {
		t.Fatal(err)
	}
	if err := <-done; !errors.Is(err, io.EOF) {
		t.Fatalf("pending Recv after Close: %v", err)
	}
	if _, err := sub.Recv(ctx); !errors.Is(err, io.EOF) {
		t.Fatalf("closed Recv: %v", err)
	}
	if err := sub.Close(); err != nil {
		t.Fatal(err)
	}
}
