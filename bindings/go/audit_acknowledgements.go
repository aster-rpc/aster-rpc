//go:build cgo

package aster

/*
#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"encoding/json"
	"fmt"
	"time"
)

// ASTER_EVENT_AUDIT_OUTBOX_ACKNOWLEDGEMENTS completes an acknowledgement-log read (ABI 7.1).
const ASTER_EVENT_AUDIT_OUTBOX_ACKNOWLEDGEMENTS = C.ASTER_EVENT_AUDIT_OUTBOX_ACKNOWLEDGEMENTS

// AuditAcknowledgementPageMax is the largest page one acknowledgement read returns.
const AuditAcknowledgementPageMax = 1000

// auditAckWaitMax mirrors ASTER_AUDIT_OUTBOX_MAX_ACK_WAIT_MS.
const auditAckWaitMax = time.Duration(C.ASTER_AUDIT_OUTBOX_MAX_ACK_WAIT_MS) * time.Millisecond

// AuditAcknowledgement is one event the receiver acknowledged as durably stored.
// The outbox writes it in the same local transaction that deletes the event, so
// an event is either still pending or present here (until pruned by the
// configured count/age bounds). Cursor increases with each acknowledgement and
// is never reused; pass the last one seen to continue reading.
type AuditAcknowledgement struct {
	Cursor   uint64 `json:"cursor"`
	EventID  string `json:"event_id"`
	Sequence uint64 `json:"sequence"`
	// BatchID is the batch digest the receiver echoed in its durable acknowledgement.
	BatchID string `json:"batch_id"`
	// AcknowledgedAtMS is local Unix milliseconds when the acknowledgement committed.
	AcknowledgedAtMS uint64 `json:"acknowledged_at_ms"`
}

// AcknowledgedAt returns AcknowledgedAtMS as a time.Time.
func (a AuditAcknowledgement) AcknowledgedAt() time.Time {
	return time.UnixMilli(int64(a.AcknowledgedAtMS))
}

// Acknowledgements returns up to limit (1..AuditAcknowledgementPageMax) logged
// acknowledgements with a cursor greater than after, oldest first. Pass 0 to
// start at the oldest retained entry.
func (a *AuditOutbox) Acknowledgements(ctx context.Context, after uint64, limit int) ([]AuditAcknowledgement, error) {
	return a.WaitAcknowledgements(ctx, after, limit, 0)
}

// WaitAcknowledgements is Acknowledgements, but when nothing follows after it
// waits up to wait (at most one hour) for the next acknowledgement. It returns
// an empty page when the wait ends first. Cancelling ctx abandons the wait.
func (a *AuditOutbox) WaitAcknowledgements(ctx context.Context, after uint64, limit int, wait time.Duration) ([]AuditAcknowledgement, error) {
	if limit < 1 || limit > AuditAcknowledgementPageMax {
		return nil, auditInvalid(AuditInvalidEvent, "acknowledgement limit must be between 1 and 1000")
	}
	if wait < 0 || wait > auditAckWaitMax {
		return nil, auditInvalid(AuditInvalidEvent, "acknowledgement wait must be between 0 and one hour")
	}
	payload, err := json.Marshal(map[string]any{"version": 1, "after": after, "limit": limit, "wait_ms": wait.Milliseconds()})
	if err != nil {
		return nil, auditInvalid(AuditInvalidEvent, "invalid acknowledgement request")
	}
	var arena cArena
	defer arena.free()
	input := arena.bytes(payload)
	data, err := a.call(ctx, "audit acknowledgements", C.ASTER_EVENT_AUDIT_OUTBOX_ACKNOWLEDGEMENTS, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_audit_outbox_acknowledgements(C.aster_runtime_t(a.rt.handle), C.aster_audit_outbox_t(a.handle), input, 0, op)
	})
	if err != nil {
		return nil, err
	}
	var result struct {
		Version          int                    `json:"version"`
		Acknowledgements []AuditAcknowledgement `json:"acknowledgements"`
	}
	if err := json.Unmarshal(data, &result); err != nil {
		return nil, err
	}
	if result.Version != 1 {
		return nil, fmt.Errorf("unsupported audit acknowledgements version")
	}
	return result.Acknowledgements, nil
}

// WatchAcknowledgements streams acknowledgements with a cursor greater than
// after until ctx ends or the outbox fails. The acknowledgement channel closes
// when the stream stops; the error channel then carries the cause (nil when
// ctx ended). Delivery is lossless while entries are retained: record the last
// Cursor to resume after a restart.
func (a *AuditOutbox) WatchAcknowledgements(ctx context.Context, after uint64) (<-chan AuditAcknowledgement, <-chan error) {
	out := make(chan AuditAcknowledgement)
	errs := make(chan error, 1)
	go func() {
		defer close(out)
		for {
			page, err := a.WaitAcknowledgements(ctx, after, AuditAcknowledgementPageMax, 30*time.Second)
			if ctx.Err() != nil {
				errs <- nil
				return
			}
			if err != nil {
				errs <- err
				return
			}
			for _, ack := range page {
				select {
				case out <- ack:
					after = ack.Cursor
				case <-ctx.Done():
					errs <- nil
					return
				}
			}
		}
	}()
	return out, errs
}
