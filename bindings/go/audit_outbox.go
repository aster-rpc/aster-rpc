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
	"sync"
	"sync/atomic"
	"time"
	"unicode/utf8"
)

// AuditDestination identifies the consumer-operated durable-ACK receiver.
// Authorization is separate from destination identity and can be rotated.
type AuditDestination struct {
	Endpoint         string  `json:"endpoint"`
	ReceiverIdentity *string `json:"receiver_identity,omitempty"`
	Authorization    *string `json:"authorization,omitempty"`
}

// String intentionally excludes credentials and endpoint query parameters.
func (d AuditDestination) String() string { return "AuditDestination{redacted}" }

// AuditOutboxConfig controls one local durable outbox. Zero limits select defaults:
// 256 MiB retained data and 64 KiB per immutable envelope.
type AuditOutboxConfig struct {
	Directory        string           `json:"directory"`
	Destination      AuditDestination `json:"destination"`
	MaxRetainedBytes uint64           `json:"max_retained_bytes,omitempty"`
	MaxEventBytes    uint64           `json:"max_event_bytes,omitempty"`
	// AcknowledgementLogMaxEntries bounds the acknowledgement log (see
	// Acknowledgements). Nil selects 100,000; 0 disables the log; at most 1,000,000.
	AcknowledgementLogMaxEntries *uint64 `json:"acknowledgement_log_max_entries,omitempty"`
	// AcknowledgementLogMaxAgeMS prunes entries acknowledged longer ago.
	// Nil selects 7 days; 0 removes the age bound.
	AcknowledgementLogMaxAgeMS *uint64 `json:"acknowledgement_log_max_age_ms,omitempty"`
}

// AuditEvent is an explicit durable intent, not a diagnostic log record.
// Payload must encode as a JSON object. TimestampMS is a Unix millisecond timestamp.
type AuditEvent struct {
	TimestampMS uint64          `json:"timestamp_ms"`
	Kind        string          `json:"kind"`
	Payload     json.RawMessage `json:"payload"`
}

// AuditErrorKind is the stable native audit failure code.
type AuditErrorKind int32

const (
	AuditInvalidConfig AuditErrorKind = 1 + iota
	AuditInvalidEvent
	AuditEventTooLarge
	AuditStorageFull
	AuditStorage
	AuditCommitUncertain
	AuditQueueOverloaded
	AuditMaintenance
	AuditClosed
	AuditWorkerFailed
	AuditTimeout
	AuditDeliveryStalled
	AuditDisposition
	AuditNotFound
	AuditDestinationMismatch
	AuditIntegrityConflict
	AuditStaleDelivery
)

// AuditOutboxError exposes machine-readable failure classification.
type AuditOutboxError struct {
	Kind    AuditErrorKind
	Message string
}

func (e *AuditOutboxError) Error() string {
	return fmt.Sprintf("audit outbox (%d): %s", e.Kind, e.Message)
}

type AuditDeliveryError struct {
	Kind                  string  `json:"kind"`
	HTTPStatus            *uint16 `json:"http_status"`
	Sequence              uint64  `json:"sequence"`
	ImmediateFlushFailure bool    `json:"immediate_flush_failure"`
	Message               string  `json:"message"`
}

type AuditOutboxStatus struct {
	OutboxID              string              `json:"outbox_id"`
	CommittedSequence     uint64              `json:"committed_sequence"`
	PendingRecords        uint64              `json:"pending_records"`
	PendingBytes          uint64              `json:"pending_bytes"`
	DeadLetterRecords     uint64              `json:"dead_letter_records"`
	DeadLetterBytes       uint64              `json:"dead_letter_bytes"`
	DispositionReceipts   uint64              `json:"disposition_receipts"`
	DispositionBytes      uint64              `json:"disposition_bytes"`
	RetainedBytes         uint64              `json:"retained_bytes"`
	MaxRetainedBytes      uint64              `json:"max_retained_bytes"`
	QueuedRecords         uint64              `json:"queued_records"`
	QueuedBytes           uint64              `json:"queued_bytes"`
	OldestAgeMS           *uint64             `json:"oldest_age_ms"`
	Accepting             bool                `json:"accepting"`
	Ready                 bool                `json:"ready"`
	DeliveryState         string              `json:"delivery_state"`
	LastDeliveryError     *AuditDeliveryError `json:"last_delivery_error"`
	StorageWorkerAlive    bool                `json:"storage_worker_alive"`
	DeliveryWorkerAlive   bool                `json:"delivery_worker_alive"`
	Maintenance           bool                `json:"maintenance"`
	MaintenanceDurationMS *uint64             `json:"maintenance_duration_ms"`
	Closed                bool                `json:"closed"`
	StorageRecovered      bool                `json:"storage_recovered"`
	DurableCommits        uint64              `json:"durable_commits"`
	LastGroupSize         uint64              `json:"last_group_size"`
}

type AuditDispositionReceipt struct {
	EventID                string              `json:"event_id"`
	Action                 string              `json:"action"`
	TimestampMS            uint64              `json:"timestamp_ms"`
	Reason                 string              `json:"reason"`
	OperatorReference      string              `json:"operator_reference"`
	DestinationFingerprint string              `json:"destination_fingerprint"`
	Rejection              *AuditDeliveryError `json:"rejection"`
}

type AuditQuarantineExport struct {
	EventID        string                    `json:"event_id"`
	EnvelopeBase64 string                    `json:"envelope_base64"`
	Pending        bool                      `json:"pending"`
	Receipts       []AuditDispositionReceipt `json:"receipts"`
}

// AuditOutbox shares its Rust workers and durable storage across cloned handles.
// Submit success means local durability; only Flush confirms receiver acceptance.
// Cancelling a wait does not retract an event and may leave an ambiguous outcome.
type AuditOutbox struct {
	rt         *Runtime
	handle     uint64
	closed     atomic.Bool
	eventLimit int
	closeOnce  sync.Once
	closeDone  chan struct{}
	closeErr   error
}

const auditMaxEventBytes = 1024 * 1024
const auditDefaultEventBytes = 64 * 1024
const auditConfigJSONLimit = 32 * 1024
const auditOperatorJSONLimit = 8 * 1024

func auditInvalid(kind AuditErrorKind, message string) error {
	return &AuditOutboxError{Kind: kind, Message: message}
}

func auditString(value string, min, max int, kind AuditErrorKind) error {
	if len(value) < min || len(value) > max || !utf8.ValidString(value) {
		return auditInvalid(kind, "audit argument exceeds its text bounds or is invalid UTF-8")
	}
	return nil
}

func auditDestination(destination AuditDestination) error {
	if err := auditString(destination.Endpoint, 1, 4096, AuditInvalidConfig); err != nil {
		return err
	}
	if destination.ReceiverIdentity != nil {
		if err := auditString(*destination.ReceiverIdentity, 1, 128, AuditInvalidConfig); err != nil {
			return err
		}
	}
	if destination.Authorization != nil {
		if err := auditString(*destination.Authorization, 0, 4096, AuditInvalidConfig); err != nil {
			return err
		}
	}
	return nil
}

func auditEventError(rt *Runtime, ev *Event, what string) error {
	if ev.Kind == ASTER_EVENT_ERROR && ev.ErrorCode >= 1 && ev.ErrorCode <= 17 {
		return &AuditOutboxError{Kind: AuditErrorKind(ev.ErrorCode), Message: string(rt.takePayload(ev))}
	}
	return rt.eventError(ev, what)
}

func auditJSON(key string, value any, limit int, kind AuditErrorKind) ([]byte, error) {
	data, err := json.Marshal(map[string]any{"version": 1, key: value})
	if err != nil {
		return nil, auditInvalid(AuditInvalidEvent, "invalid audit JSON argument")
	}
	if len(data) > limit {
		return nil, auditInvalid(kind, "encoded audit request exceeds its input limit")
	}
	return data, nil
}

func OpenAuditOutbox(ctx context.Context, rt *Runtime, config AuditOutboxConfig) (*AuditOutbox, error) {
	if rt == nil {
		return nil, fmt.Errorf("audit runtime is nil: %w", ErrInvalidArgument)
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if err := auditString(config.Directory, 1, auditConfigJSONLimit, AuditInvalidConfig); err != nil {
		return nil, err
	}
	if err := auditDestination(config.Destination); err != nil {
		return nil, err
	}
	limit := config.MaxEventBytes
	if limit == 0 {
		limit = auditDefaultEventBytes
	}
	if limit < 256 || limit > auditMaxEventBytes || (config.MaxRetainedBytes != 0 && config.MaxRetainedBytes < 256) ||
		(config.AcknowledgementLogMaxEntries != nil && *config.AcknowledgementLogMaxEntries > 1_000_000) {
		return nil, auditInvalid(AuditInvalidConfig, "invalid audit storage/event limits")
	}
	payload, err := auditJSON("config", config, auditConfigJSONLimit, AuditInvalidConfig)
	if err != nil {
		return nil, err
	}
	var arena cArena
	defer arena.free()
	input := arena.bytes(payload)
	ev, err := startOwnedOp(ctx, rt, "aster_audit_outbox_open", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_audit_outbox_open(C.aster_runtime_t(rt.handle), input, 0, op)
	}, func(late *Event) {
		if late.Kind == C.ASTER_EVENT_AUDIT_OUTBOX_OPENED && late.Handle != 0 {
			C.aster_audit_outbox_free(C.aster_runtime_t(rt.handle), C.aster_audit_outbox_t(late.Handle))
		}
	})
	if err != nil {
		return nil, err
	}
	if ev.Kind != C.ASTER_EVENT_AUDIT_OUTBOX_OPENED {
		return nil, auditEventError(rt, ev, "audit open")
	}
	rt.releaseEventBuffer(ev)
	return &AuditOutbox{rt: rt, handle: ev.Handle, eventLimit: int(limit)}, nil
}

func (a *AuditOutbox) call(ctx context.Context, what string, expected uint32, call func(*C.aster_operation_t) C.int32_t) ([]byte, error) {
	if a.closed.Load() {
		return nil, &AuditOutboxError{Kind: AuditClosed, Message: "handle is closed"}
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	ev, err := startOwnedOp(ctx, a.rt, what, call, nil)
	if err != nil {
		return nil, err
	}
	if ev.Kind != expected {
		return nil, auditEventError(a.rt, ev, what)
	}
	return a.rt.takePayload(ev), nil
}

func (a *AuditOutbox) Submit(ctx context.Context, event AuditEvent) (string, error) {
	if a.closed.Load() {
		return "", auditInvalid(AuditClosed, "handle is closed")
	}
	if err := ctx.Err(); err != nil {
		return "", err
	}
	if err := auditString(event.Kind, 1, 128, AuditInvalidEvent); err != nil {
		return "", err
	}
	// RawMessage belongs to the caller. Refuse it before Marshal makes another
	// JSON/escaping allocation, and before the C arena copies the request.
	limit := a.eventLimit
	if limit == 0 {
		limit = auditDefaultEventBytes
	}
	if len(event.Payload) > limit {
		return "", auditInvalid(AuditEventTooLarge, "audit payload exceeds event limit")
	}
	payload, err := auditJSON("event", event, limit+1024, AuditEventTooLarge)
	if err != nil {
		return "", err
	}
	var arena cArena
	defer arena.free()
	input := arena.bytes(payload)
	result, err := a.call(ctx, "audit submit", C.ASTER_EVENT_AUDIT_OUTBOX_SUBMITTED, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_audit_outbox_submit(C.aster_runtime_t(a.rt.handle), C.aster_audit_outbox_t(a.handle), input, 0, op)
	})
	return string(result), err
}

func (a *AuditOutbox) Status(ctx context.Context) (AuditOutboxStatus, error) {
	var result struct {
		Version int               `json:"version"`
		Status  AuditOutboxStatus `json:"status"`
	}
	data, err := a.call(ctx, "audit status", C.ASTER_EVENT_AUDIT_OUTBOX_STATUS, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_audit_outbox_status(C.aster_runtime_t(a.rt.handle), C.aster_audit_outbox_t(a.handle), 0, op)
	})
	if err != nil {
		return result.Status, err
	}
	err = json.Unmarshal(data, &result)
	if err == nil && result.Version != 1 {
		err = fmt.Errorf("unsupported audit status version")
	}
	return result.Status, err
}

func (a *AuditOutbox) Flush(ctx context.Context, timeout time.Duration) error {
	if timeout < 0 {
		return &AuditOutboxError{Kind: AuditInvalidConfig, Message: "negative timeout"}
	}
	_, err := a.call(ctx, "audit flush", ASTER_EVENT_UNIT_RESULT, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_audit_outbox_flush(C.aster_runtime_t(a.rt.handle), C.aster_audit_outbox_t(a.handle), C.uint64_t(timeout.Milliseconds()), 0, op)
	})
	return err
}

func (a *AuditOutbox) Reconfigure(ctx context.Context, destination AuditDestination) error {
	if err := auditDestination(destination); err != nil {
		return err
	}
	return a.jsonUnit(ctx, "destination", destination, "reconfigure")
}

func (a *AuditOutbox) Quarantine(ctx context.Context, eventID, reason, operatorReference string) error {
	if err := auditString(eventID, 1, 133, AuditIntegrityConflict); err != nil {
		return err
	}
	if err := auditString(reason, 1, 512, AuditInvalidEvent); err != nil {
		return err
	}
	if err := auditString(operatorReference, 1, 256, AuditInvalidEvent); err != nil {
		return err
	}
	return a.jsonUnit(ctx, "quarantine", map[string]string{"event_id": eventID, "reason": reason, "operator_reference": operatorReference}, "quarantine")
}

func (a *AuditOutbox) Replay(ctx context.Context, eventID string) error {
	if err := auditString(eventID, 1, 133, AuditIntegrityConflict); err != nil {
		return err
	}
	return a.jsonUnit(ctx, "event_id", eventID, "replay")
}

func (a *AuditOutbox) jsonUnit(ctx context.Context, key string, value any, operation string) error {
	limit, kind := auditOperatorJSONLimit, AuditInvalidEvent
	if operation == "reconfigure" {
		limit, kind = auditConfigJSONLimit, AuditInvalidConfig
	}
	payload, err := auditJSON(key, value, limit, kind)
	if err != nil {
		return err
	}
	var arena cArena
	defer arena.free()
	input := arena.bytes(payload)
	_, err = a.call(ctx, "audit "+operation, ASTER_EVENT_UNIT_RESULT, func(op *C.aster_operation_t) C.int32_t {
		rt, h := C.aster_runtime_t(a.rt.handle), C.aster_audit_outbox_t(a.handle)
		switch operation {
		case "reconfigure":
			return C.aster_audit_outbox_reconfigure(rt, h, input, 0, op)
		case "quarantine":
			return C.aster_audit_outbox_quarantine(rt, h, input, 0, op)
		default:
			return C.aster_audit_outbox_replay(rt, h, input, 0, op)
		}
	})
	return err
}

func (a *AuditOutbox) Export(ctx context.Context, eventID string) (AuditQuarantineExport, error) {
	var result struct {
		Version int                   `json:"version"`
		Export  AuditQuarantineExport `json:"export"`
	}
	if err := auditString(eventID, 1, 133, AuditIntegrityConflict); err != nil {
		return result.Export, err
	}
	payload, err := auditJSON("event_id", eventID, auditOperatorJSONLimit, AuditIntegrityConflict)
	if err != nil {
		return result.Export, err
	}
	var arena cArena
	defer arena.free()
	input := arena.bytes(payload)
	data, err := a.call(ctx, "audit export", C.ASTER_EVENT_AUDIT_OUTBOX_EXPORTED, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_audit_outbox_export(C.aster_runtime_t(a.rt.handle), C.aster_audit_outbox_t(a.handle), input, 0, op)
	})
	if err != nil {
		return result.Export, err
	}
	err = json.Unmarshal(data, &result)
	if err == nil && result.Version != 1 {
		err = fmt.Errorf("unsupported audit export version")
	}
	return result.Export, err
}

// Clone shares workers and budgets. Closing either handle closes the shared outbox.
func (a *AuditOutbox) Clone() (*AuditOutbox, error) {
	if a.closed.Load() {
		return nil, &AuditOutboxError{Kind: AuditClosed, Message: "handle is closed"}
	}
	var handle C.aster_audit_outbox_t
	if err := wrapCall("audit clone", func() C.int32_t {
		return C.aster_audit_outbox_clone(C.aster_runtime_t(a.rt.handle), C.aster_audit_outbox_t(a.handle), &handle)
	}); err != nil {
		return nil, err
	}
	return &AuditOutbox{rt: a.rt, handle: uint64(handle), eventLimit: a.eventLimit}, nil
}

// Close stops delivery without requiring the receiver and preserves pending data.
// If ctx is cancelled, shutdown continues; reopening can wait for its lock release.
func (a *AuditOutbox) Close(ctx context.Context) error {
	a.closeOnce.Do(func() {
		a.closed.Store(true)
		a.closeDone = make(chan struct{})
		go func() {
			a.closeErr = a.closeCore()
			close(a.closeDone)
		}()
	})
	select {
	case <-a.closeDone:
		return a.closeErr
	case <-ctx.Done():
		return ctx.Err()
	}
}

func (a *AuditOutbox) closeCore() error {
	defer C.aster_audit_outbox_free(C.aster_runtime_t(a.rt.handle), C.aster_audit_outbox_t(a.handle))
	ev, err := startOwnedOp(context.Background(), a.rt, "audit close", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_audit_outbox_close(C.aster_runtime_t(a.rt.handle), C.aster_audit_outbox_t(a.handle), 0, op)
	}, nil)
	if err != nil {
		return err
	}
	if ev.Kind != ASTER_EVENT_UNIT_RESULT {
		return auditEventError(a.rt, ev, "audit close")
	}
	a.rt.releaseEventBuffer(ev)
	return nil
}
