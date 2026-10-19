//go:build cgo

package aster

/*
#include <stdint.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"fmt"
	"io"
	"sync/atomic"
	"time"
)

// peerPollSlice is how long one native poll blocks, so Recv notices a
// cancelled context and Close within it.
const peerPollSlice = 100 * time.Millisecond

// PeerIdentity is who a connected peer is, as verified when its first
// connection was admitted.
type PeerIdentity struct {
	// EndpointID is the peer's verified endpoint id.
	EndpointID string
	// Principal is the principal its credential names; empty unless the peer
	// has a live enrollment session.
	Principal string
	// Roles are the roles its credential grants (sorted); empty without
	// enrollment.
	Roles []string
}

// PeerSnapshot is a peer connected when PeerEvents was called.
type PeerSnapshot struct {
	Peer string
	// ALPN of its first admitted connection ("aster/1").
	ALPN string
	// Connections is how many admitted connections it has.
	Connections int
	Identity    PeerIdentity
}

// PeerEventType says what a PeerEvent reports.
type PeerEventType uint8

const (
	// PeerConnected: a peer's first admitted connection opened.
	PeerConnected PeerEventType = iota + 1
	// PeerDisconnected: a peer's last connection ended.
	PeerDisconnected
	// PeerLagged: the subscription fell behind its bounded queue and Missed
	// events were dropped. The consumer's state is stale: call PeerEvents
	// again and rebuild from the new snapshot.
	PeerLagged
)

// DisconnectReason is why a peer's last connection ended.
type DisconnectReason string

const (
	// DisconnectClosed: the peer, or this server, closed it normally.
	DisconnectClosed DisconnectReason = "closed"
	// DisconnectTimedOut: it hit its idle timeout.
	DisconnectTimedOut DisconnectReason = "timed_out"
	// DisconnectRevoked: the peer's credential expired or was revoked.
	DisconnectRevoked DisconnectReason = "revoked"
	// DisconnectDrained: the server's drain closed it (DrainCloseCode).
	DisconnectDrained DisconnectReason = "drained"
	// DisconnectError: a transport error or reset.
	DisconnectError DisconnectReason = "error"
)

// PeerEvent is one change in which peers are connected. Per peer, not per
// connection.
type PeerEvent struct {
	Type PeerEventType
	// Peer, for Connected and Disconnected.
	Peer string
	// ALPN and Identity, for Connected.
	ALPN     string
	Identity PeerIdentity
	// Reason, for Disconnected.
	Reason DisconnectReason
	// Missed, for Lagged: how many events were dropped.
	Missed uint64
}

type peerIdentityJSON struct {
	EndpointID string   `json:"endpoint_id"`
	Principal  *string  `json:"principal"`
	Roles      []string `json:"roles"`
}

func (p peerIdentityJSON) identity() PeerIdentity {
	id := PeerIdentity{EndpointID: p.EndpointID, Roles: p.Roles}
	if p.Principal != nil {
		id.Principal = *p.Principal
	}
	return id
}

// PeerSubscription is a receiver of peer events from the moment PeerEvents
// returned it. Close it when done.
type PeerSubscription struct {
	rt     C.uint64_t
	handle uint64
	closed atomic.Bool
}

// PeerEvents returns the peers connected now and a subscription to every
// connect and disconnect after them, taken together so a peer connecting
// during the call is in the snapshot or in the stream, never both and never
// neither (Rust AsterServer::peer_events).
//
// Events are per peer: Connected on a peer's first admitted aster/1
// connection (past the node's gates, so a peer enrollment or a drain
// refuses never appears), Disconnected when its last one ends. Whether a
// connected peer serves what you need is answered by reading its
// aster.services NodeInfo attribute.
//
// A consumer that falls behind the bounded queue (1024 events) gets one
// PeerLagged event; its recovery is to call PeerEvents again. Connections
// are never held up by a slow consumer.
func (s *AsterServer) PeerEvents(ctx context.Context) ([]PeerSnapshot, *PeerSubscription, error) {
	if err := ctx.Err(); err != nil {
		return nil, nil, err
	}
	if s.closing.Load() {
		return nil, nil, ErrAlreadyClosed
	}
	var receiver C.uint64_t
	var out C.aster_bytes_t
	status, msg := ffiCall(func() C.int32_t {
		return C.aster_server_peer_events(s.ds.handle, &receiver, &out)
	})
	if err := wrapStatusMsg("aster_server_peer_events", status, msg); err != nil {
		return nil, nil, err
	}
	sub := &PeerSubscription{rt: s.ds.rt, handle: uint64(receiver)}
	var wire []struct {
		Peer        string           `json:"peer"`
		ALPN        string           `json:"alpn"`
		Connections int              `json:"connections"`
		Identity    peerIdentityJSON `json:"identity"`
	}
	if err := takeJSON(out, &wire); err != nil {
		_ = sub.Close()
		return nil, nil, fmt.Errorf("peer snapshot: %w", err)
	}
	snapshot := make([]PeerSnapshot, 0, len(wire))
	for _, p := range wire {
		snapshot = append(snapshot, PeerSnapshot{
			Peer: p.Peer, ALPN: p.ALPN, Connections: p.Connections, Identity: p.Identity.identity(),
		})
	}
	return snapshot, sub, nil
}

// Close releases the subscription and wakes a pending Recv with io.EOF.
func (s *PeerSubscription) Close() error {
	if s.closed.Swap(true) {
		return nil
	}
	status, msg := ffiCall(func() C.int32_t { return C.aster_peer_events_release(C.uint64_t(s.handle)) })
	if status == C.ASTER_STATUS_NOT_FOUND {
		return nil
	}
	return wrapStatusMsg("aster_peer_events_release", status, msg)
}

// Recv returns the next event, io.EOF once the subscription is closed or the
// server has ended, or ctx's error. A cancelled Recv consumes nothing.
func (s *PeerSubscription) Recv(ctx context.Context) (*PeerEvent, error) {
	for {
		if s.closed.Load() {
			return nil, io.EOF
		}
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		slice := peerPollSlice
		if dl, ok := ctx.Deadline(); ok {
			slice = min(slice, max(time.Until(dl), time.Millisecond))
		}
		var out C.aster_bytes_t
		status, msg := ffiCall(func() C.int32_t {
			return C.aster_peer_events_poll(s.rt, C.uint64_t(s.handle), C.uint32_t(drainMillis(slice)), &out)
		})
		switch status {
		case C.ASTER_STATUS_TIMEOUT:
			continue
		case C.ASTER_STATUS_NOT_FOUND, C.ASTER_STATUS_ALREADY_CLOSED:
			return nil, io.EOF
		}
		if err := wrapStatusMsg("aster_peer_events_poll", status, msg); err != nil {
			return nil, err
		}
		var wire struct {
			Type     string           `json:"type"`
			Peer     string           `json:"peer"`
			ALPN     string           `json:"alpn"`
			Identity peerIdentityJSON `json:"identity"`
			Reason   string           `json:"reason"`
			Missed   uint64           `json:"missed"`
		}
		if err := takeJSON(out, &wire); err != nil {
			return nil, fmt.Errorf("peer event: %w", err)
		}
		return decodePeerEvent(wire.Type, wire.Peer, wire.ALPN, wire.Identity, wire.Reason, wire.Missed)
	}
}

func decodePeerEvent(kind, peer, alpn string, identity peerIdentityJSON, reason string, missed uint64) (*PeerEvent, error) {
	switch kind {
	case "connected":
		return &PeerEvent{Type: PeerConnected, Peer: peer, ALPN: alpn, Identity: identity.identity()}, nil
	case "disconnected":
		return &PeerEvent{Type: PeerDisconnected, Peer: peer, Reason: DisconnectReason(reason)}, nil
	case "lagged":
		return &PeerEvent{Type: PeerLagged, Missed: missed}, nil
	}
	return nil, fmt.Errorf("peer event: unknown type %q: %w", kind, ErrInternal)
}
