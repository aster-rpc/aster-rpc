//go:build cgo

// Example: a Postgres-backed lease serializer and fenced writes.
//
// A paused, deposed controller must not overwrite its successor's work:
//
//  1. controller-a acquires the lease (epoch N) and reads its fence.
//  2. controller-a "pauses" (GC pause, SIGSTOP, VM migration): it stops
//     renewing and sleeps past the TTL with the fence still in hand.
//  3. controller-b acquires (epoch N+1, the expired-takeover path) and writes.
//  4. controller-a wakes and sends its write with the old fence: refused.
//
// Then: a writer replaying the *current* epoch under the wrong identity is
// refused (the epoch is not a bearer token), the naive `WHERE epoch = $current`
// check is shown accepting exactly that write, and a release advances the
// fence so the releasing holder's own late writes are refused too.
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

func main() {
	if err := run(); err != nil {
		log.Fatal(err)
	}
}

func run() error {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	dsn := os.Getenv("PG_DSN")
	if dsn == "" {
		dsn = "postgres://postgres@127.0.0.1:5432/postgres?sslmode=disable"
	}
	db, err := sql.Open("pgx", dsn)
	if err != nil {
		return err
	}
	defer db.Close()
	if _, err := db.ExecContext(ctx, Schema); err != nil {
		return fmt.Errorf("schema: %w", err)
	}
	if _, err := db.ExecContext(ctx,
		`INSERT INTO work (resource) VALUES ($1) ON CONFLICT DO NOTHING`, resource); err != nil {
		return err
	}

	// The Rust holder engine (acquire, background renewals, self-limit) runs
	// over our Postgres serializer via upcalls.
	rt, err := aster.NewRuntime(ctx, aster.DefaultRuntimeConfig())
	if err != nil {
		return err
	}
	defer rt.Close()
	up, err := aster.NewUpcalls(rt, 0)
	if err != nil {
		return err
	}
	defer up.Close()
	if err := up.ServeLeaseSerializer(&PGSerializer{DB: db}); err != nil {
		return err
	}
	if err := up.ServeFencedResource(&PGFencedResource{DB: db}); err != nil {
		return err
	}
	ser, err := aster.NewForeignLeaseSerializer(up)
	if err != nil {
		return err
	}
	defer ser.Close()
	work, err := aster.NewForeignFencedResource(up)
	if err != nil {
		return err
	}
	defer work.Close()

	write := func(who string, fence aster.Fence, payload string) error {
		res, err := work.Mutate(ctx, fence, who, []byte(payload))
		if err != nil {
			return err // outcome unknown: never treat as either result
		}
		if res.Applied() {
			fmt.Printf("  %-13s write %-28q fence(epoch=%d, holder=%s): APPLIED\n", who, payload, fence.Epoch, fence.Holder)
		} else {
			detail := res.Rejection.Reason.String()
			if res.Rejection.Current != 0 {
				detail += fmt.Sprintf(", carried=%d current=%d", res.Rejection.Carried, res.Rejection.Current)
			}
			fmt.Printf("  %-13s write %-28q fence(epoch=%d, holder=%s): REFUSED (%s)\n",
				who, payload, fence.Epoch, fence.Holder, detail)
		}
		return nil
	}

	snap, err := ser.Snapshot(ctx, resource)
	if err != nil {
		return err
	}
	fmt.Printf("start: %s epoch=%d held=%v (epochs persist across runs; never reused)\n", resource, snap.Epoch, snap.Held)

	// 1. controller-a acquires.
	leaseA, err := ser.Acquire(ctx, resource, "controller-a", aster.LeaseOptions{TTL: ttl})
	if err != nil {
		return fmt.Errorf("controller-a acquire: %w", err)
	}
	fenceA, ok := leaseA.Fence()
	if !ok {
		return errors.New("controller-a: no fence right after acquire")
	}
	fmt.Printf("\n1. controller-a acquired epoch %d\n", fenceA.Epoch)
	if err := write("controller-a", fenceA, "a: scale pool to 3"); err != nil {
		return err
	}

	// controller-b is refused while controller-a is live (no live steal).
	_, err = ser.Acquire(ctx, resource, "controller-b", aster.LeaseOptions{TTL: ttl})
	var denied *aster.LeaseDeniedError
	if !errors.As(err, &denied) {
		return fmt.Errorf("controller-b should be denied while a is live, got %v", err)
	}
	fmt.Printf("   controller-b acquire while a is live: denied (%s, retry in ~%dms)\n", denied.Reason, *denied.Snapshot.RemainingMs)

	// 2. controller-a pauses with fenceA in hand. Closing the handle stops
	// the in-process renewals, which is exactly what a frozen process does.
	_ = leaseA.Close()
	fmt.Printf("\n2. controller-a paused (no renewals), sleeping %v past its TTL of %v\n", ttl+500*time.Millisecond, ttl)
	time.Sleep(ttl + 500*time.Millisecond)

	// 3. controller-b takes over (expired-takeover is ACQUIRE; epoch bumps).
	leaseB, err := ser.Acquire(ctx, resource, "controller-b", aster.LeaseOptions{TTL: 10 * time.Second})
	if err != nil {
		return fmt.Errorf("controller-b takeover: %w", err)
	}
	fenceB, _ := leaseB.Fence()
	fmt.Printf("\n3. controller-b acquired epoch %d (a's lease expired)\n", fenceB.Epoch)
	if err := write("controller-b", fenceB, "b: scale pool to 5"); err != nil {
		return err
	}

	// 4. controller-a wakes and writes with the fence it read before pausing.
	fmt.Printf("\n4. controller-a wakes up and sends its delayed write\n")
	if err := write("controller-a", fenceA, "a: scale pool to 3 (stale)"); err != nil {
		return err
	}

	// 5. The epoch is public (any snapshot reader sees it). Presenting the
	// current epoch is not enough: the writer must be the holder.
	cur, err := ser.Snapshot(ctx, resource)
	if err != nil {
		return err
	}
	stolen := aster.Fence{Resource: resource, Epoch: cur.Epoch, Holder: "controller-a"}
	fmt.Printf("\n5. controller-a replays the current epoch %d (read from a snapshot)\n", cur.Epoch)
	if err := write("controller-a", stolen, "a: replayed current epoch"); err != nil {
		return err
	}
	// Evaluate (without applying) the predicate a naive epoch-only guard,
	// `UPDATE work ... WHERE (SELECT epoch FROM leases ...) = $epoch`, uses.
	var naiveWouldPass bool
	if err := db.QueryRowContext(ctx,
		`SELECT (SELECT epoch FROM leases WHERE resource = $1) = $2`, resource, int64(cur.Epoch)).Scan(&naiveWouldPass); err != nil {
		return err
	}
	fmt.Printf("   a naive \"WHERE epoch = %d\" guard would have accepted that write: %v\n", cur.Epoch, naiveWouldPass)

	// 6. Release advances the fence: b's own late writes are refused after.
	if err := leaseB.Release(ctx); err != nil {
		return err
	}
	fmt.Printf("\n6. controller-b released (fence advances)\n")
	if err := write("controller-b", fenceB, "b: late write after release"); err != nil {
		return err
	}

	var payload, holder string
	var epoch int64
	if err := db.QueryRowContext(ctx,
		`SELECT payload, lease_epoch, lease_holder FROM work WHERE resource = $1`, resource).Scan(&payload, &epoch, &holder); err != nil {
		return err
	}
	end, err := ser.Snapshot(ctx, resource)
	if err != nil {
		return err
	}
	fmt.Printf("\nfinal work row: %q written by %s at epoch %d\n", payload, holder, epoch)
	fmt.Printf("final lease:    epoch=%d held=%v\n", end.Epoch, end.Held)
	if payload != "b: scale pool to 5" {
		return fmt.Errorf("successor's work was overwritten: %q", payload)
	}
	fmt.Println("OK: the deposed controller did not overwrite its successor's work")
	return nil
}
