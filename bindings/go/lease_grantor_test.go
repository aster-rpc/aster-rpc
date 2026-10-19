//go:build cgo

package aster

import (
	"context"
	"errors"
	"testing"
	"time"
)

// Wiring only: a server serves aster.lease.Grantor over a Go serialization
// point with Requires and RevokeUngated set, and a client reaches it through
// AsterClient.LeaseGrantor. The grantor's behaviour (holder binding, the
// default operator gate on revoke, renewals over RPC) is Rust's:
// aster/tests/it/rpc_lease.rs and ffi/tests/enrollment.rs
// lease_grantor_serves_clients_bound_to_their_identity.
func TestLeaseGrantor_GoSerializerBehindStartServer(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	gs := newGoSerializer()
	srv, err := StartServer(ctx, ServerOptions{Config: testCfg(), LeaseGrantor: &LeaseGrantor{
		Serializer: gs, Requires: RequireRole("lease.holder"), RevokeUngated: true,
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

	// Requires reaches the grantor: refused at Gate 3, not by the serializer.
	if _, err := ser.Acquire(ctx, "db/primary", idA, LeaseOptions{}); err == nil || errors.Is(err, ErrLeaseDenied) {
		t.Fatalf("without the role: %v", err)
	}
	if err := srv.SetPeerAttributes(idA, map[string]string{"aster.role": "lease.holder"}); err != nil {
		t.Fatal(err)
	}
	lease, err := ser.Acquire(ctx, "db/primary", idA, LeaseOptions{TTL: 10 * time.Second})
	if err != nil {
		t.Fatalf("with the role: %v", err)
	}
	defer lease.Close()
	if fence, ok := lease.Fence(); !ok || fence.Holder != idA || fence.Resource != "db/primary" {
		t.Fatalf("fence %+v %v", fence, ok)
	}
	// The Go serializer decided it, for the authenticated identity.
	if gs.count(LeaseAcquire) != 1 {
		t.Fatalf("the Go serializer saw %d acquires", gs.count(LeaseAcquire))
	}
	// The server's own handle reads the grantor's rows.
	snap, err := srv.LeaseSerializer().Snapshot(ctx, "db/primary")
	if err != nil || !snap.Held || snap.Holder == nil || *snap.Holder != idA {
		t.Fatalf("snapshot %+v %v", snap, err)
	}
	// RevokeUngated: revoke is gated only by Requires.
	if out, err := ser.Revoke(ctx, "db/primary", idA); err != nil || out.Kind != TransitionRevoked {
		t.Fatalf("revoke with RevokeUngated: %+v %v", out, err)
	}
}
