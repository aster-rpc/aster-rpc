//go:build cgo

package main

import (
	"context"
	"database/sql"
	"errors"
	"fmt"

	aster "sdk.getaster.now/go"
)

// Schema is idempotent. Lease rows are never deleted: the epoch is the
// fencing token and must never be reused, so a deleted row (which would
// restart at epoch 0) is refused by a trigger.
const Schema = `
CREATE TABLE IF NOT EXISTS leases (
    resource   text    PRIMARY KEY,
    epoch      bigint  NOT NULL DEFAULT 0 CHECK (epoch >= 0),
    holder     text,
    held       boolean NOT NULL DEFAULT false,
    ttl_ms     bigint  NOT NULL DEFAULT 0 CHECK (ttl_ms >= 0),
    -- Volatile expiry anchor, on the database's own clock.
    granted_at timestamptz,
    CHECK (held = (holder IS NOT NULL))
);

CREATE OR REPLACE FUNCTION leases_no_delete() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'lease rows are never deleted (epoch % of % would be reused)', OLD.epoch, OLD.resource;
END $$;

DROP TRIGGER IF EXISTS leases_no_delete ON leases;
CREATE TRIGGER leases_no_delete BEFORE DELETE ON leases
    FOR EACH ROW EXECUTE FUNCTION leases_no_delete();

-- The protected business data. lease_epoch/lease_holder record which grant
-- produced the current value (audit); the guard is the lease row itself.
CREATE TABLE IF NOT EXISTS work (
    resource     text PRIMARY KEY,
    payload      text NOT NULL DEFAULT '',
    lease_epoch  bigint NOT NULL DEFAULT 0,
    lease_holder text NOT NULL DEFAULT '',
    updated_at   timestamptz NOT NULL DEFAULT now()
);
`

// rowState mirrors aster::lease::RowState plus the expiry facts the database
// evaluated on its own clock under the row lock.
type rowState struct {
	epoch       uint64
	holder      sql.NullString
	held        bool
	ttlMs       uint64
	expired     bool
	remainingMs uint64
}

// PGSerializer is a lease serialization point over Postgres: each Transition
// is one transaction holding the row's FOR UPDATE lock, applying exactly the
// state machine of core/src/lease.rs (transition). Expiry is evaluated on the
// database clock, never on a caller's clock.
//
// A database fault is returned as an error, which the binding reports to the
// holder engine as a backend error: never as a denial (a lost connection must
// not look like "someone else holds it", nor fence a live holder).
type PGSerializer struct{ DB *sql.DB }

var _ aster.LeaseSerializer = (*PGSerializer)(nil)

// lockedRow reads resource's row FOR UPDATE inside tx, creating the initial
// row (epoch 0, free) first so there is always something to lock.
func lockedRow(ctx context.Context, tx *sql.Tx, resource string) (rowState, error) {
	if _, err := tx.ExecContext(ctx,
		`INSERT INTO leases (resource) VALUES ($1) ON CONFLICT (resource) DO NOTHING`, resource); err != nil {
		return rowState{}, fmt.Errorf("lease row init: %w", err)
	}
	return scanRow(tx.QueryRowContext(ctx, rowQuery+` FOR UPDATE`, resource))
}

// rowQuery computes expiry exactly like MemorySerializer: expired iff held and
// (no anchor, or now - granted_at >= ttl). clock_timestamp() is the wall
// clock at evaluation, not the transaction start.
const rowQuery = `
SELECT epoch, holder, held, ttl_ms,
       held AND (granted_at IS NULL OR clock_timestamp() - granted_at >= ttl_ms * interval '1 millisecond'),
       CASE WHEN held AND granted_at IS NOT NULL
            THEN GREATEST(0, ttl_ms - floor(extract(epoch FROM clock_timestamp() - granted_at) * 1000))::bigint
            ELSE 0 END
FROM leases WHERE resource = $1`

func scanRow(r *sql.Row) (rowState, error) {
	var s rowState
	var epoch, ttl, rem int64
	if err := r.Scan(&epoch, &s.holder, &s.held, &ttl, &s.expired, &rem); err != nil {
		return rowState{}, err
	}
	s.epoch, s.ttlMs, s.remainingMs = uint64(epoch), uint64(ttl), uint64(rem)
	return s, nil
}

func snapshotOf(resource string, s rowState) aster.LeaseSnapshot {
	snap := aster.LeaseSnapshot{Resource: resource, Epoch: s.epoch, Held: s.held}
	if s.holder.Valid {
		h := s.holder.String
		snap.Holder = &h
	}
	if s.held {
		rem := s.remainingMs
		snap.RemainingMs = &rem
	}
	return snap
}

// Transition applies op to resource atomically (one transaction, one row lock).
func (p *PGSerializer) Transition(ctx context.Context, resource string, op aster.LeaseOp) (out aster.TransitionOutcome, err error) {
	tx, err := p.DB.BeginTx(ctx, nil)
	if err != nil {
		return out, fmt.Errorf("lease serializer: begin: %w", err)
	}
	defer func() {
		if err != nil {
			_ = tx.Rollback()
		}
	}()
	row, err := lockedRow(ctx, tx, resource)
	if err != nil {
		return out, fmt.Errorf("lease serializer: read %q: %w", resource, err)
	}

	deny := func(reason aster.DenyReason) (aster.TransitionOutcome, error) {
		// Nothing changed: commit releases the lock (and keeps the initial
		// row if this transaction created it, which is harmless).
		if err := tx.Commit(); err != nil {
			return aster.TransitionOutcome{}, fmt.Errorf("lease serializer: commit: %w", err)
		}
		return aster.TransitionOutcome{Kind: aster.TransitionDenied, Reason: reason, Snapshot: snapshotOf(resource, row)}, nil
	}
	isHolder := row.holder.Valid && row.holder.String == op.Actor

	// The state machine of core/src/lease.rs `transition`, predicate for
	// predicate and in the same order.
	var (
		next      = row
		resetAnch bool
		result    aster.TransitionOutcome
	)
	switch op.Kind {
	case aster.LeaseAcquire:
		if row.held && !row.expired {
			return deny(aster.DenyHeldByOther)
		}
		next.epoch = row.epoch + 1
		next.holder = sql.NullString{String: op.Actor, Valid: true}
		next.held, next.ttlMs, resetAnch = true, op.TTLMs, true
		result = aster.TransitionOutcome{Kind: aster.TransitionGranted,
			Fence: aster.Fence{Resource: resource, Epoch: next.epoch, Holder: op.Actor}}
	case aster.LeaseRenew:
		switch {
		case !row.held:
			return deny(aster.DenyNotHeld)
		case op.Epoch != row.epoch:
			return deny(aster.DenyEpochMismatch)
		case !isHolder:
			return deny(aster.DenyNotHolder)
		case row.expired:
			return deny(aster.DenyExpired)
		}
		resetAnch = true
		result = aster.TransitionOutcome{Kind: aster.TransitionRenewed}
	case aster.LeaseRelease:
		// No expiry predicate: while nobody has taken over, the holder may
		// release; the epoch bump fences its own in-flight writes.
		switch {
		case op.Epoch != row.epoch:
			return deny(aster.DenyEpochMismatch)
		case !isHolder:
			return deny(aster.DenyNotHolder)
		}
		next.epoch, next.holder, next.held = row.epoch+1, sql.NullString{}, false
		result = aster.TransitionOutcome{Kind: aster.TransitionReleased}
	case aster.LeaseRevoke:
		// Unconditional fence advance; authorization (who may revoke) is the
		// serializer's admission decision, made before the state machine.
		next.epoch, next.holder, next.held = row.epoch+1, sql.NullString{}, false
		result = aster.TransitionOutcome{Kind: aster.TransitionRevoked}
	default:
		return out, fmt.Errorf("lease serializer: unknown op kind %d", op.Kind)
	}

	// One CAS: the WHERE epoch = old guard is redundant under FOR UPDATE but
	// makes a lost lock (or a manual edit) a loud backend error, never a
	// silent epoch reuse.
	res, err := tx.ExecContext(ctx, `
UPDATE leases SET epoch = $2, holder = $3, held = $4, ttl_ms = $5,
       granted_at = CASE WHEN $6 THEN clock_timestamp() ELSE NULL END
WHERE resource = $1 AND epoch = $7 AND epoch <= $2`,
		resource, int64(next.epoch), next.holder, next.held, int64(next.ttlMs), resetAnch, int64(row.epoch))
	if err != nil {
		return out, fmt.Errorf("lease serializer: write %q: %w", resource, err)
	}
	if n, err := res.RowsAffected(); err != nil || n != 1 {
		return out, fmt.Errorf("lease serializer: CAS on %q affected %d rows (%v)", resource, n, err)
	}
	if err = tx.Commit(); err != nil {
		return out, fmt.Errorf("lease serializer: commit: %w", err)
	}
	return result, nil
}

// Snapshot is advisory: it takes no lock and may be stale on return. Nothing
// may gate a write on it.
func (p *PGSerializer) Snapshot(ctx context.Context, resource string) (aster.LeaseSnapshot, error) {
	row, err := scanRow(p.DB.QueryRowContext(ctx, rowQuery, resource))
	if errors.Is(err, sql.ErrNoRows) {
		return aster.LeaseSnapshot{Resource: resource}, nil // never acquired: epoch 0, free
	}
	if err != nil {
		return aster.LeaseSnapshot{}, fmt.Errorf("lease serializer: snapshot %q: %w", resource, err)
	}
	return snapshotOf(resource, row), nil
}

// FencedWrite is the resource-side half: it applies payload to the work row
// for fence.Resource iff the fence rule holds for writer -- carried epoch ==
// the lease row's epoch AND the row is held AND writer is its holder --
// evaluated atomically with the write.
//
// writer must be the authenticated identity of the caller (on Aster, the
// QUIC-proven peer node id), never a field the caller supplied.
//
// The FOR SHARE lock conflicts with the serializer's FOR UPDATE, so no
// transition can commit between the check and the write. The UPDATE repeats
// the full predicate (epoch AND held AND holder) so the statement is safe
// on its own even if someone deletes the CheckFence call.
func FencedWrite(ctx context.Context, db *sql.DB, fence aster.Fence, writer, payload string) (rej *aster.FenceRejection, err error) {
	tx, err := db.BeginTx(ctx, nil)
	if err != nil {
		return nil, err
	}
	defer func() {
		if err != nil || rej != nil {
			_ = tx.Rollback()
		}
	}()
	var (
		epoch  int64
		held   bool
		holder sql.NullString
	)
	err = tx.QueryRowContext(ctx,
		`SELECT epoch, held, holder FROM leases WHERE resource = $1 FOR SHARE`, fence.Resource).
		Scan(&epoch, &held, &holder)
	if errors.Is(err, sql.ErrNoRows) {
		epoch, held, err = 0, false, nil // never acquired
	}
	if err != nil {
		return nil, err
	}
	if rej := aster.CheckFence(uint64(epoch), held, holder.String, fence, writer); rej != nil {
		return rej, nil
	}
	res, err := tx.ExecContext(ctx, `
UPDATE work SET payload = $4, lease_epoch = $2, lease_holder = $3, updated_at = now()
WHERE resource = $1
  AND EXISTS (SELECT 1 FROM leases l
              WHERE l.resource = $1 AND l.epoch = $2 AND l.held AND l.holder = $3)`,
		fence.Resource, int64(fence.Epoch), writer, payload)
	if err != nil {
		return nil, err
	}
	if n, err := res.RowsAffected(); err != nil || n != 1 {
		return nil, fmt.Errorf("fenced write on %q affected %d rows (%v)", fence.Resource, n, err)
	}
	return nil, tx.Commit()
}
