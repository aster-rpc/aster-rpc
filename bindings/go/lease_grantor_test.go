//go:build cgo

package aster

import (
	"context"
	"errors"
	"testing"
	"time"
)

// A server serves aster.lease.Grantor over a Go serialization point; clients
// acquire through it and are bound to their authenticated node ids (Rust
// aster::rpc::lease, guide §5).
func TestLeaseGrantor_GoSerializerBehindStartServer(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	gs := newGoSerializer()
	srv, err := StartServer(ctx, ServerOptions{Config: testCfg(), LeaseGrantor: &LeaseGrantor{Serializer: gs}})
	if err != nil {
		t.Fatal(err)
	}
	defer srv.Close()
	addr := addrOf(t, srv.Node())

	a := startClient(t, ctx, testCfg())
	b := startClient(t, ctx, testCfg())
	idA, _ := a.ID()
	idB, _ := b.ID()
	serA, err := a.LeaseGrantor(ctx, &addr)
	if err != nil {
		t.Fatal(err)
	}
	defer serA.Close()
	serB, err := b.LeaseGrantor(ctx, &addr)
	if err != nil {
		t.Fatal(err)
	}
	defer serB.Close()

	// A TTL that survives a stall on a loaded host; renewals every 500 ms so
	// the check below sees one.
	lease, err := serA.Acquire(ctx, "db/primary", idA, LeaseOptions{TTL: 10 * time.Second, RenewEvery: 500 * time.Millisecond})
	if err != nil {
		t.Fatal(err)
	}
	defer lease.Close()
	if fence, ok := lease.Fence(); !ok || fence.Holder != idA || fence.Resource != "db/primary" {
		t.Fatalf("fence %+v %v", fence, ok)
	}
	// b is refused while a holds it.
	if _, err := serB.Acquire(ctx, "db/primary", idB, LeaseOptions{TTL: 3 * time.Second}); !errors.Is(err, ErrLeaseDenied) {
		t.Fatalf("b while a holds: %v", err)
	}
	// An op for another actor never reaches the grantor.
	if _, err := serB.Acquire(ctx, "db/other", idA, LeaseOptions{}); err == nil || errors.Is(err, ErrLeaseDenied) {
		t.Fatalf("b acquiring as a: %v", err)
	}
	// Revoke needs the operator role (Gate 3) by default.
	if _, err := serA.Revoke(ctx, "db/primary", idA); err == nil {
		t.Fatal("revoke without the operator role")
	}
	// Renewals flow through the grantor; the Go serializer saw only the
	// authenticated identities.
	time.Sleep(1500 * time.Millisecond)
	if st, err := lease.Status(); err != nil || st != LeaseHeld {
		t.Fatalf("lease status %v %v", st, err)
	}
	gs.mu.Lock()
	renewed := false
	for _, op := range gs.seen {
		if op.Actor != idA && op.Actor != idB {
			t.Errorf("serializer saw actor %q", op.Actor)
		}
		renewed = renewed || op.Kind == LeaseRenew
	}
	gs.mu.Unlock()
	if !renewed {
		t.Fatal("no renewal reached the Go serializer")
	}
	if err := lease.Release(ctx); err != nil {
		t.Fatal(err)
	}
	leaseB, err := serB.Acquire(ctx, "db/primary", idB, LeaseOptions{TTL: 3 * time.Second})
	if err != nil {
		t.Fatalf("b after a released: %v", err)
	}
	defer leaseB.Close()
	// The server's own handle reads the grantor's rows.
	snap, err := srv.LeaseSerializer().Snapshot(ctx, "db/primary")
	if err != nil || !snap.Held || snap.Holder == nil || *snap.Holder != idB {
		t.Fatalf("snapshot %+v %v", snap, err)
	}
}

// With no Go serializer the grantor runs on Rust's in-memory one; Requires
// gates every method.
func TestLeaseGrantor_MemorySerializerAndRequires(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	srv, err := StartServer(ctx, ServerOptions{Config: testCfg(), LeaseGrantor: &LeaseGrantor{
		Requires: RequireRole("lease.holder"), RevokeUngated: true,
	}})
	if err != nil {
		t.Fatal(err)
	}
	defer srv.Close()
	addr := addrOf(t, srv.Node())
	a := startClient(t, ctx, testCfg())
	idA, _ := a.ID()
	ser, err := a.LeaseGrantor(ctx, &addr)
	if err != nil {
		t.Fatal(err)
	}
	defer ser.Close()
	if _, err := ser.Acquire(ctx, "r", idA, LeaseOptions{}); err == nil || errors.Is(err, ErrLeaseDenied) {
		t.Fatalf("without the role: %v", err)
	}
	if err := srv.SetPeerAttributes(idA, map[string]string{"aster.role": "lease.holder"}); err != nil {
		t.Fatal(err)
	}
	lease, err := ser.Acquire(ctx, "r", idA, LeaseOptions{})
	if err != nil {
		t.Fatalf("with the role: %v", err)
	}
	defer lease.Close()
	// revoke is gated only by Requires now.
	if _, err := ser.Revoke(ctx, "r", idA); err != nil {
		t.Fatalf("revoke with RevokeUngated: %v", err)
	}
}
