//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"encoding/json"
)

// C ABI 5.1: aster.lease.Grantor served by an AsterServer and used by
// AsterClients, mirroring Rust aster::rpc::lease (guide
// docs/aster-leases-getstarted.md §5).

// LeaseGrantor serves aster.lease.Grantor from a server
// (ServerOptions.LeaseGrantor; Rust aster::rpc::lease::LeaseGrantor): a
// designated serialization point that other nodes acquire, renew and
// release leases through (AsterClient.LeaseGrantor). The grantor binds every
// operation to the caller's authenticated node id, never a request field,
// so a node can only ever hold, renew or release leases as itself. A dead
// grantor stops grants and renewals (holders run to their self-limit and
// stop); there is no automatic failover.
type LeaseGrantor struct {
	// Serializer is the serialization point, implemented in Go (for example
	// over Postgres) and served on an upcall table the server owns. Nil =
	// Rust's in-memory reference serializer.
	Serializer LeaseSerializer
	// Requires gates every method (Gate 3); nil = none, so admission (Gate
	// 0, e.g. Enrollment) is the boundary. Without it any admitted caller
	// can burn epochs (availability, never correctness).
	Requires *Requirement
	// RevokeRequires gates revoke; nil = RequireRole("operator").
	RevokeRequires *Requirement
	// RevokeUngated leaves revoke gated only by Requires.
	RevokeUngated bool
}

// grantorBackend is the serializer a server's grantor runs on.
type grantorBackend struct {
	upcalls    *Upcalls // nil for the in-memory serializer
	serializer *LeaseSerializerHandle
}

func (g *LeaseGrantor) backend(rt *Runtime) (*grantorBackend, error) {
	if g.Serializer == nil {
		s, err := NewMemoryLeaseSerializer(rt, nil)
		if err != nil {
			return nil, err
		}
		return &grantorBackend{serializer: s}, nil
	}
	up, err := NewUpcalls(rt, 0)
	if err != nil {
		return nil, err
	}
	if err := up.ServeLeaseSerializer(g.Serializer); err != nil {
		_ = up.Close()
		return nil, err
	}
	s, err := NewForeignLeaseSerializer(up)
	if err != nil {
		_ = up.Close()
		return nil, err
	}
	return &grantorBackend{upcalls: up, serializer: s}, nil
}

func (b *grantorBackend) close() {
	_ = b.serializer.Close()
	if b.upcalls != nil {
		_ = b.upcalls.Close()
	}
}

// requirements encodes Requires and the revoke gate for
// aster_server_add_lease_grantor (empty = default, "null" = none).
func (g *LeaseGrantor) requirements() (requires, revoke []byte, err error) {
	if g.Requires != nil {
		if requires, err = json.Marshal(g.Requires); err != nil {
			return nil, nil, err
		}
	}
	switch {
	case g.RevokeUngated:
		revoke = []byte("null")
	case g.RevokeRequires != nil:
		if revoke, err = json.Marshal(g.RevokeRequires); err != nil {
			return nil, nil, err
		}
	}
	return requires, revoke, nil
}

// LeaseGrantor returns a lease serializer backed by the aster.lease.Grantor
// the server at addr serves (Rust GrantorSerializer): Acquire, Snapshot and
// Revoke are calls to it, and acquired leases renew through it in the
// background. Acquire with holder = this client's node id (ID): the grantor
// binds this client's calls to it, and any other holder fails locally. The
// client enrolls with the server first when its config enables enrollment.
// Close the serializer when done.
func (c *AsterClient) LeaseGrantor(ctx context.Context, addr *NodeAddr) (*LeaseSerializerHandle, error) {
	rc, err := c.Connect(ctx, addr)
	if err != nil {
		return nil, err
	}
	var h C.aster_lease_serializer_t
	if err := wrapStatus("aster_lease_serializer_grantor_new", C.aster_lease_serializer_grantor_new(
		C.uint64_t(c.node.runtime.handle), C.uint64_t(c.node.handle), C.aster_connection_t(rc.conn.handle), &h)); err != nil {
		return nil, err
	}
	return &LeaseSerializerHandle{rt: c.node.runtime, handle: uint64(h)}, nil
}
