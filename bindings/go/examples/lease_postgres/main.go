//go:build cgo

// Example: a Postgres lease serializer behind aster.lease.Grantor, and fenced
// writes.
//
// The grantor node is an AsterServer (StartServer) that serves
// aster.lease.Grantor over PGSerializer (ServerOptions.LeaseGrantor) and the
// protected store, Work.Write, which applies a write only while the fence
// is current: carried epoch == the lease row's epoch AND the row is held AND
// the authenticated caller is its holder. The controllers are AsterClients
// (StartClient) with their own node keys; they acquire through the grantor,
// which binds every lease to the caller's authenticated node id.
//
// A paused, deposed controller must not overwrite its successor's work:
//
//  1. controller-a acquires the lease (epoch N) and reads its fence.
//  2. controller-a "pauses" (GC pause, SIGSTOP, VM migration): it stops
//     renewing and sleeps past the TTL with the fence still in hand.
//  3. controller-b acquires (epoch N+1, the expired-takeover path) and writes.
//  4. controller-a wakes and sends its write with the old fence: refused.
//
// Then: controller-a replaying the *current* epoch is refused (the epoch is
// not a bearer token; the store checks the authenticated holder), the naive
// `WHERE epoch = $current` check is shown accepting exactly that write, and
// a release advances the fence so the releasing holder's own late writes are
// refused too.
//
// Run (see README.md):
//
//	PG_DSN=postgres://postgres@127.0.0.1:5432/postgres?sslmode=disable go run .
package main

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"io"
	"log"
	"os"
	"time"

	_ "github.com/jackc/pgx/v5/stdlib"

	aster "sdk.getaster.now/go"
)

const (
	resource = "kasm/session-controller"
	ttl      = 2 * time.Second
)

// ─── the protected store: Work.Write ──────────────────────────────────────

type WriteRequest struct {
	Resource string `aster:"name=resource"`
	// Epoch is the fence epoch the writer holds (Lease.Fence at send time).
	// The holder is never a field: it is the caller's authenticated node id.
	Epoch   int64  `aster:"name=epoch"`
	Payload string `aster:"name=payload"`
}

func (WriteRequest) AsterWireType() string { return "examples.lease/WriteRequest" }

type WriteReply struct {
	Applied bool   `aster:"name=applied"`
	Reason  string `aster:"name=reason"` // stale_epoch, not_holder, not_held, future_epoch
	Carried int64  `aster:"name=carried"`
	Current int64  `aster:"name=current"`
}

func (WriteReply) AsterWireType() string { return "examples.lease/WriteReply" }

// WorkService is the store's write path on the grantor node: FencedWrite
// with the caller's authenticated node id as both the fence holder and the
// writer. A database fault is an error (outcome unknown), never "refused".
func WorkService(db *sql.DB) *aster.Service {
	return aster.NewService("Work", 1,
		aster.Unary("Write", func(ctx context.Context, r *WriteRequest) (*WriteReply, error) {
			info, ok := aster.CallInfoFromContext(ctx)
			if !ok || !info.IrohAuthenticated {
				return nil, aster.NewRpcError(aster.StatusPermissionDenied, "caller identity not authenticated")
			}
			if r.Epoch < 0 {
				return nil, aster.NewRpcError(aster.StatusInvalidArgument, "epoch must be >= 0")
			}
			fence := aster.Fence{Resource: r.Resource, Epoch: uint64(r.Epoch), Holder: info.PeerID}
			rej, err := FencedWrite(ctx, db, fence, info.PeerID, r.Payload)
			if err != nil {
				return nil, aster.NewRpcError(aster.StatusUnavailable, "store: %v", err)
			}
			if rej != nil {
				return &WriteReply{Reason: rej.Reason.String(), Carried: int64(rej.Carried), Current: int64(rej.Current)}, nil
			}
			return &WriteReply{Applied: true}, nil
		}),
	)
}

// ─── demo ─────────────────────────────────────────────────────────────────

func main() {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	dsn := os.Getenv("PG_DSN")
	if dsn == "" {
		dsn = "postgres://postgres@127.0.0.1:5432/postgres?sslmode=disable"
	}
	db, err := sql.Open("pgx", dsn)
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	if err := Run(ctx, db, os.Stdout); err != nil {
		log.Fatal(err)
	}
}

// config is the node configuration: ASTER_* variables, defaulting to a
// relay-free loopback node for the demo.
func config() (aster.AsterConfig, error) {
	cfg, err := aster.ConfigFromEnv()
	if err != nil {
		return cfg, err
	}
	if os.Getenv("ASTER_BIND_ADDR") == "" {
		cfg.BindAddr = "127.0.0.1:0"
	}
	if os.Getenv("ASTER_RELAY_MODE") == "" {
		cfg.RelayMode = "disabled"
	}
	return cfg, nil
}

// Run runs the scenario against db and returns an error if any step does not
// behave as described.
func Run(ctx context.Context, db *sql.DB, out io.Writer) error {
	say := func(format string, args ...any) { fmt.Fprintf(out, format+"\n", args...) }
	if _, err := db.ExecContext(ctx, Schema); err != nil {
		return fmt.Errorf("schema: %w", err)
	}
	if _, err := db.ExecContext(ctx,
		`INSERT INTO work (resource) VALUES ($1) ON CONFLICT DO NOTHING`, resource); err != nil {
		return err
	}
	cfg, err := config()
	if err != nil {
		return err
	}

	// The grantor node: the lease serialization point and the store.
	grantor, err := aster.StartServer(ctx, aster.ServerOptions{
		Config:       cfg,
		Services:     []*aster.Service{WorkService(db)},
		LeaseGrantor: &aster.LeaseGrantor{Serializer: &PGSerializer{DB: db}},
	})
	if err != nil {
		return err
	}
	defer grantor.Close()
	ticket, err := grantor.Ticket(ctx)
	if err != nil {
		return err
	}
	grantorAddr, err := aster.NodeAddrFromTicket(ticket)
	if err != nil {
		return err
	}

	// A controller: its own node (its own key), the grantor as its lease
	// serializer, and the store's write path.
	type controller struct {
		name   string
		id     string
		client *aster.AsterClient
		leases *aster.LeaseSerializerHandle
		work   *aster.RpcConnection
	}
	start := func(name string) (*controller, error) {
		cl, err := aster.StartClient(ctx, cfg)
		if err != nil {
			return nil, err
		}
		id, _ := cl.ID()
		leases, err := cl.LeaseGrantor(ctx, grantorAddr)
		if err != nil {
			_ = cl.Close()
			return nil, err
		}
		work, err := cl.Connect(ctx, grantorAddr)
		if err != nil {
			_ = cl.Close()
			return nil, err
		}
		return &controller{name: name, id: id, client: cl, leases: leases, work: work}, nil
	}
	a, err := start("controller-a")
	if err != nil {
		return err
	}
	defer a.client.Close()
	defer a.leases.Close()
	b, err := start("controller-b")
	if err != nil {
		return err
	}
	defer b.client.Close()
	defer b.leases.Close()
	say("controller-a is node %s, controller-b is node %s", a.id[:12], b.id[:12])

	write := func(c *controller, fence aster.Fence, payload string) error {
		r, err := aster.Call[WriteRequest, WriteReply](ctx, c.work, "Work", "Write",
			&WriteRequest{Resource: fence.Resource, Epoch: int64(fence.Epoch), Payload: payload})
		if err != nil {
			return err // outcome unknown: never treat as either result
		}
		if r.Applied {
			say("  %-13s write %-28q epoch=%d: APPLIED", c.name, payload, fence.Epoch)
			return nil
		}
		detail := r.Reason
		if r.Current != 0 {
			detail += fmt.Sprintf(", carried=%d current=%d", r.Carried, r.Current)
		}
		say("  %-13s write %-28q epoch=%d: REFUSED (%s)", c.name, payload, fence.Epoch, detail)
		return nil
	}

	snap, err := grantor.LeaseSerializer().Snapshot(ctx, resource)
	if err != nil {
		return err
	}
	say("start: %s epoch=%d held=%v (epochs persist across runs; never reused)", resource, snap.Epoch, snap.Held)

	// 1. controller-a acquires through the grantor; the grantor binds the
	// lease to a's authenticated node id.
	leaseA, err := a.leases.Acquire(ctx, resource, a.id, aster.LeaseOptions{TTL: ttl})
	if err != nil {
		return fmt.Errorf("controller-a acquire: %w", err)
	}
	fenceA, ok := leaseA.Fence()
	if !ok {
		return errors.New("controller-a: no fence right after acquire")
	}
	say("\n1. controller-a acquired epoch %d", fenceA.Epoch)
	if err := write(a, fenceA, "a: scale pool to 3"); err != nil {
		return err
	}

	// controller-b is refused while controller-a is live (no live steal).
	_, err = b.leases.Acquire(ctx, resource, b.id, aster.LeaseOptions{TTL: ttl})
	var denied *aster.LeaseDeniedError
	if !errors.As(err, &denied) {
		return fmt.Errorf("controller-b should be denied while a is live, got %v", err)
	}
	say("   controller-b acquire while a is live: denied (%s, retry in ~%dms)", denied.Reason, *denied.Snapshot.RemainingMs)

	// 2. controller-a pauses with fenceA in hand. Closing the handle stops
	// the renewals, which is exactly what a frozen process does.
	_ = leaseA.Close()
	say("\n2. controller-a paused (no renewals), sleeping %v past its TTL of %v", ttl+500*time.Millisecond, ttl)
	time.Sleep(ttl + 500*time.Millisecond)

	// 3. controller-b takes over (expired-takeover is ACQUIRE; epoch bumps).
	leaseB, err := b.leases.Acquire(ctx, resource, b.id, aster.LeaseOptions{TTL: 10 * time.Second})
	if err != nil {
		return fmt.Errorf("controller-b takeover: %w", err)
	}
	defer leaseB.Close()
	fenceB, _ := leaseB.Fence()
	say("\n3. controller-b acquired epoch %d (a's lease expired)", fenceB.Epoch)
	if err := write(b, fenceB, "b: scale pool to 5"); err != nil {
		return err
	}

	// 4. controller-a wakes and writes with the fence it read before pausing.
	say("\n4. controller-a wakes up and sends its delayed write")
	if err := write(a, fenceA, "a: scale pool to 3 (stale)"); err != nil {
		return err
	}

	// 5. The epoch is public (any snapshot reader sees it). Presenting the
	// current epoch is not enough: the store checks that the authenticated
	// caller holds it.
	cur, err := a.leases.Snapshot(ctx, resource)
	if err != nil {
		return err
	}
	say("\n5. controller-a replays the current epoch %d (read from a snapshot)", cur.Epoch)
	if err := write(a, aster.Fence{Resource: resource, Epoch: cur.Epoch}, "a: replayed current epoch"); err != nil {
		return err
	}
	// The predicate a naive epoch-only guard, `UPDATE work ... WHERE (SELECT
	// epoch FROM leases ...) = $epoch`, uses (evaluated, not applied).
	var naiveWouldPass bool
	if err := db.QueryRowContext(ctx,
		`SELECT (SELECT epoch FROM leases WHERE resource = $1) = $2`, resource, int64(cur.Epoch)).Scan(&naiveWouldPass); err != nil {
		return err
	}
	say("   a naive \"WHERE epoch = %d\" guard would have accepted that write: %v", cur.Epoch, naiveWouldPass)

	// 6. Release advances the fence: b's own late writes are refused after.
	if err := leaseB.Release(ctx); err != nil {
		return err
	}
	say("\n6. controller-b released (fence advances)")
	if err := write(b, fenceB, "b: late write after release"); err != nil {
		return err
	}

	var payload, holder string
	var epoch int64
	if err := db.QueryRowContext(ctx,
		`SELECT payload, lease_epoch, lease_holder FROM work WHERE resource = $1`, resource).Scan(&payload, &epoch, &holder); err != nil {
		return err
	}
	end, err := grantor.LeaseSerializer().Snapshot(ctx, resource)
	if err != nil {
		return err
	}
	who := "controller-b"
	if holder != b.id {
		who = holder
	}
	say("\nfinal work row: %q written by %s at epoch %d", payload, who, epoch)
	say("final lease:    epoch=%d held=%v", end.Epoch, end.Held)
	if payload != "b: scale pool to 5" || holder != b.id {
		return fmt.Errorf("successor's work was overwritten: %q by %s", payload, holder)
	}
	say("OK: the deposed controller did not overwrite its successor's work")
	return nil
}
