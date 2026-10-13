//go:build cgo

package main

import (
	"context"
	"database/sql"
	"fmt"
	"os"
	"strings"
	"testing"
	"time"

	aster "sdk.getaster.now/go"
)

// These tests need a Postgres: set PG_DSN (skipped otherwise). Each test uses
// a fresh resource name, so they are safe against a shared database.

func testDB(t *testing.T) *sql.DB {
	t.Helper()
	dsn := os.Getenv("PG_DSN")
	if dsn == "" {
		t.Skip("PG_DSN not set")
	}
	db, err := sql.Open("pgx", dsn)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = db.Close() })
	if _, err := db.Exec(Schema); err != nil {
		t.Fatal(err)
	}
	return db
}

func freshResource(t *testing.T) string {
	return fmt.Sprintf("test/%s/%d", t.Name(), time.Now().UnixNano())
}

func op(kind aster.LeaseOpKind, actor string, epoch uint64) aster.LeaseOp {
	return aster.LeaseOp{Kind: kind, Actor: actor, Epoch: epoch, TTLMs: 300}
}

// mustTransition fails the test on a backend error and checks the kind (and,
// for a denial, the reason).
func mustTransition(t *testing.T, s *PGSerializer, res string, o aster.LeaseOp, kind aster.TransitionKind, reason aster.DenyReason) aster.TransitionOutcome {
	t.Helper()
	out, err := s.Transition(context.Background(), res, o)
	if err != nil {
		t.Fatalf("%+v: %v", o, err)
	}
	if out.Kind != kind || (kind == aster.TransitionDenied && out.Reason != reason) {
		t.Fatalf("%+v: got kind %d reason %s, want kind %d reason %s", o, out.Kind, out.Reason, kind, reason)
	}
	return out
}

// The predicates of core/src/lease.rs `transition`, in order.
func TestPGSerializer_StateMachine(t *testing.T) {
	db := testDB(t)
	s := &PGSerializer{DB: db}
	r := freshResource(t)
	const (
		granted  = aster.TransitionGranted
		renewed  = aster.TransitionRenewed
		released = aster.TransitionReleased
		revoked  = aster.TransitionRevoked
		denied   = aster.TransitionDenied
	)

	// Free row: renew is NotHeld; release is NotHolder (no expiry or held
	// predicate on release, the holder comparison fails).
	mustTransition(t, s, r, op(aster.LeaseRenew, "a", 0), denied, aster.DenyNotHeld)
	mustTransition(t, s, r, op(aster.LeaseRelease, "a", 0), denied, aster.DenyNotHolder)

	g := mustTransition(t, s, r, op(aster.LeaseAcquire, "a", 0), granted, 0)
	if g.Fence != (aster.Fence{Resource: r, Epoch: 1, Holder: "a"}) {
		t.Fatalf("fence %+v", g.Fence)
	}
	d := mustTransition(t, s, r, op(aster.LeaseAcquire, "b", 0), denied, aster.DenyHeldByOther)
	if d.Snapshot.Holder == nil || *d.Snapshot.Holder != "a" || d.Snapshot.RemainingMs == nil {
		t.Fatalf("denial snapshot %+v", d.Snapshot)
	}
	mustTransition(t, s, r, op(aster.LeaseRenew, "a", 7), denied, aster.DenyEpochMismatch)
	mustTransition(t, s, r, op(aster.LeaseRenew, "b", 1), denied, aster.DenyNotHolder)
	mustTransition(t, s, r, op(aster.LeaseRenew, "a", 1), renewed, 0)

	time.Sleep(400 * time.Millisecond) // past the 300 ms ttl
	mustTransition(t, s, r, op(aster.LeaseRenew, "a", 1), denied, aster.DenyExpired)
	// Expired takeover is ACQUIRE; the epoch bumps.
	mustTransition(t, s, r, op(aster.LeaseAcquire, "b", 0), granted, 0)
	mustTransition(t, s, r, op(aster.LeaseRelease, "a", 1), denied, aster.DenyEpochMismatch)
	mustTransition(t, s, r, op(aster.LeaseRelease, "a", 2), denied, aster.DenyNotHolder)
	mustTransition(t, s, r, op(aster.LeaseRelease, "b", 2), released, 0)
	mustTransition(t, s, r, op(aster.LeaseRevoke, "admin", 0), revoked, 0)

	snap, err := s.Snapshot(context.Background(), r)
	if err != nil || snap.Epoch != 4 || snap.Held || snap.Holder != nil {
		t.Fatalf("snapshot %+v, %v", snap, err)
	}
	// Epochs are never reused: deleting the row is refused.
	if _, err := db.Exec(`DELETE FROM leases WHERE resource = $1`, r); err == nil {
		t.Fatal("deleting a lease row succeeded")
	}
}

func TestFencedWrite_EpochAndHolder(t *testing.T) {
	db := testDB(t)
	s := &PGSerializer{DB: db}
	r := freshResource(t)
	ctx := context.Background()
	if _, err := db.Exec(`INSERT INTO work (resource) VALUES ($1)`, r); err != nil {
		t.Fatal(err)
	}
	mustTransition(t, s, r, op(aster.LeaseAcquire, "a", 0), aster.TransitionGranted, 0)
	f := aster.Fence{Resource: r, Epoch: 1, Holder: "a"}

	if rej, err := FencedWrite(ctx, db, f, "a", "ok"); err != nil || rej != nil {
		t.Fatalf("holder write: %v %v", rej, err)
	}
	// Right epoch, wrong writer: the epoch is not a bearer token.
	if rej, err := FencedWrite(ctx, db, f, "mallory", "x"); err != nil || rej == nil || rej.Reason != aster.RejectNotHolder {
		t.Fatalf("impostor: %v %v", rej, err)
	}
	if rej, _ := FencedWrite(ctx, db, aster.Fence{Resource: r, Epoch: 0, Holder: "a"}, "a", "x"); rej == nil || rej.Reason != aster.RejectStaleEpoch {
		t.Fatalf("stale: %v", rej)
	}
	if rej, _ := FencedWrite(ctx, db, aster.Fence{Resource: r, Epoch: 9, Holder: "a"}, "a", "x"); rej == nil || rej.Reason != aster.RejectFutureEpoch {
		t.Fatalf("future: %v", rej)
	}
	mustTransition(t, s, r, op(aster.LeaseRelease, "a", 1), aster.TransitionReleased, 0)
	if rej, _ := FencedWrite(ctx, db, f, "a", "x"); rej == nil || rej.Reason != aster.RejectStaleEpoch {
		t.Fatalf("after release: %v", rej)
	}
	var payload string
	if err := db.QueryRow(`SELECT payload FROM work WHERE resource = $1`, r).Scan(&payload); err != nil || payload != "ok" {
		t.Fatalf("payload %q, %v", payload, err)
	}
}

// A database fault is a backend error, never a denial.
func TestPGSerializer_DBFaultIsAnError(t *testing.T) {
	broken, err := sql.Open("pgx", "postgres://nobody@127.0.0.1:1/none?sslmode=disable&connect_timeout=1")
	if err != nil {
		t.Fatal(err)
	}
	defer broken.Close()
	s := &PGSerializer{DB: broken}
	if out, err := s.Transition(context.Background(), "x", op(aster.LeaseAcquire, "a", 0)); err == nil {
		t.Fatalf("unreachable database answered %+v", out)
	}
	if _, err := FencedWrite(context.Background(), broken, aster.Fence{Resource: "x", Epoch: 1, Holder: "a"}, "a", "p"); err == nil {
		t.Fatal("unreachable database: fenced write did not error")
	}
}

// TestGrantorScenario runs the demo: the Postgres serializer behind a
// LeaseGrantor on StartServer, controllers as StartClients, fenced writes
// through Work.Write.
func TestGrantorScenario(t *testing.T) {
	db := testDB(t)
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	var out strings.Builder
	if err := Run(ctx, db, &out); err != nil {
		t.Fatalf("%v\n%s", err, out.String())
	}
	t.Log("\n" + out.String())
}
