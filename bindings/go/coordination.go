//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"errors"
	"fmt"
	"sync/atomic"
	"time"
	"unsafe"
)

// C ABI 4.4: the coordination client (wrapped, never ported: journaling,
// redirect routing and the delivery contract stay in Rust) and the placement
// manager over it. Design: docs/_internal/bindings/coordination-abi-design.md.
//
// The delivery contract in Go terms: every sequenced mutation resolves to a
// *ResolvedMutation that must be Acknowledge()d once the application has
// durably handled it; until then no new mutation is permitted
// (ErrCoordinationAwaitingAck). A result nobody acknowledged is redelivered
// by Unacknowledged after reopening. A committed typed rejection is a
// resolved result (OutcomeCode 100), not an error.
//
// aster_operation_cancel does not stop a started coordination operation (a
// half-sent mutation is the journal's to resolve), so cancelling a context
// abandons the wait, not the operation: a result or handle that completes
// later is collected in the background and released (a late-resolved mutation
// stays journaled and is redelivered by Unacknowledged).

// ─── errors ──────────────────────────────────────────────────────────────

// CoordinationErrorKind is aster_coordination_error_t.
type CoordinationErrorKind int32

const (
	CoordAwaitingAck   CoordinationErrorKind = C.ASTER_COORDINATION_ERROR_AWAITING_ACK
	CoordUnresolved    CoordinationErrorKind = C.ASTER_COORDINATION_ERROR_UNRESOLVED
	CoordJournalLocked CoordinationErrorKind = C.ASTER_COORDINATION_ERROR_JOURNAL_LOCKED
	CoordJournalFatal  CoordinationErrorKind = C.ASTER_COORDINATION_ERROR_JOURNAL_FATAL
	CoordRPC           CoordinationErrorKind = C.ASTER_COORDINATION_ERROR_RPC
	CoordUnavailable   CoordinationErrorKind = C.ASTER_COORDINATION_ERROR_UNAVAILABLE
	CoordClosed        CoordinationErrorKind = C.ASTER_COORDINATION_ERROR_CLOSED
	CoordRejected      CoordinationErrorKind = C.ASTER_COORDINATION_ERROR_REJECTED
	CoordInvalid       CoordinationErrorKind = C.ASTER_COORDINATION_ERROR_INVALID
	CoordOther         CoordinationErrorKind = C.ASTER_COORDINATION_ERROR_OTHER
)

// Sentinels matched by errors.Is against a *CoordinationError of that kind.
var (
	// ErrCoordinationAwaitingAck: a mutation is resolved and unacknowledged;
	// acknowledge it (or take it with Unacknowledged) first.
	ErrCoordinationAwaitingAck = errors.New("coordination: awaiting acknowledgement")
	// ErrCoordinationUnresolved: a request from an earlier run is half
	// finished; call Resume first.
	ErrCoordinationUnresolved = errors.New("coordination: unresolved request")
	// ErrCoordinationJournalLocked: another client holds the journal's lifetime lock.
	ErrCoordinationJournalLocked = errors.New("coordination: journal locked")
	ErrCoordinationJournalFatal  = errors.New("coordination: journal fatal")
	// ErrCoordinationRPC: the group answered with an RPC status (see RPCCode).
	ErrCoordinationRPC = errors.New("coordination: rpc error")
	// ErrCoordinationUnavailable: no member answered; the sequence stays open
	// and resumes with the same bytes.
	ErrCoordinationUnavailable = errors.New("coordination: unavailable")
	// ErrCoordinationClosed: the client was closed (a mutation that resolved
	// after Close stays journaled and is redelivered by Unacknowledged).
	ErrCoordinationClosed  = errors.New("coordination: closed")
	ErrCoordinationInvalid = errors.New("coordination: invalid request")
)

var coordKindErrors = map[CoordinationErrorKind]error{
	CoordAwaitingAck: ErrCoordinationAwaitingAck, CoordUnresolved: ErrCoordinationUnresolved,
	CoordJournalLocked: ErrCoordinationJournalLocked, CoordJournalFatal: ErrCoordinationJournalFatal,
	CoordRPC: ErrCoordinationRPC, CoordUnavailable: ErrCoordinationUnavailable, CoordClosed: ErrCoordinationClosed,
	CoordInvalid: ErrCoordinationInvalid,
}

// CoordinationError is a failed coordination operation: Kind is the
// coordination error kind, RPCCode the RPC status for CoordRPC, Status the
// coarse aster_status_t. errors.Is matches the kind sentinels and the
// mapped status (RPC PERMISSION_DENIED matches ErrPermissionDenied, RPC
// UNAUTHENTICATED ErrUnauthenticated, CoordClosed ErrAlreadyClosed, ...).
type CoordinationError struct {
	Op      string
	Kind    CoordinationErrorKind
	RPCCode int32
	Status  int32
	Message string
}

func (e *CoordinationError) Error() string {
	s := fmt.Sprintf("%s: coordination error kind %d", e.Op, e.Kind)
	if e.Kind == CoordRPC {
		s += fmt.Sprintf(" (rpc status %d)", e.RPCCode)
	}
	if e.Message != "" {
		s += ": " + e.Message
	}
	return s
}

// Is matches the kind sentinels and the sentinel of the mapped status.
func (e *CoordinationError) Is(target error) bool {
	if k, ok := coordKindErrors[e.Kind]; ok && k == target {
		return true
	}
	return statusToError[e.Status] == target
}

// coordEventError turns a failed completion into a *CoordinationError (or the
// generic event error when it is not a coordination failure).
func coordEventError(rt *Runtime, ev *Event, what string) error {
	if ev.Kind == ASTER_EVENT_ERROR && ev.ErrorCode >= C.ASTER_COORDINATION_ERROR_AWAITING_ACK &&
		ev.ErrorCode <= C.ASTER_COORDINATION_ERROR_OTHER {
		msg := string(rt.takePayload(ev))
		return &CoordinationError{Op: what, Kind: CoordinationErrorKind(ev.ErrorCode), RPCCode: int32(ev.Flags),
			Status: int32(ev.Status), Message: msg}
	}
	return rt.eventError(ev, what)
}

// ─── events ──────────────────────────────────────────────────────────────

const (
	eventCoordClientOpened  = C.ASTER_EVENT_COORDINATION_CLIENT_OPENED
	eventCoordResolved      = C.ASTER_EVENT_COORDINATION_RESOLVED
	eventCoordResult        = C.ASTER_EVENT_COORDINATION_RESULT
	eventCoordLeaseAcquired = C.ASTER_EVENT_COORDINATION_LEASE_ACQUIRED
)

// coordOp runs one coordination call. cleanup (if non-nil) disposes of the
// handle a late completion carries.
func coordOp(ctx context.Context, rt *Runtime, what string, call func(op *C.aster_operation_t) C.int32_t,
	cleanup func(*Event)) (*Event, error) {
	ev, err := startOwnedOp(ctx, rt, what, call, cleanup)
	if err != nil && errors.Is(err, ErrAlreadyClosed) && !errors.Is(err, ErrCoordinationClosed) {
		// The synchronous ALREADY_CLOSED of a closed client / lease: the same
		// condition as the CLOSED event kind.
		err = fmt.Errorf("%w: %w", ErrCoordinationClosed, err)
	}
	return ev, err
}

// coordPayload runs a call whose success is one event kind and returns its
// payload.
func coordPayload(ctx context.Context, rt *Runtime, what string, kind uint32,
	call func(op *C.aster_operation_t) C.int32_t) ([]byte, error) {
	ev, err := coordOp(ctx, rt, what, call, nil)
	if err != nil {
		return nil, err
	}
	if ev.Kind != kind {
		return nil, coordEventError(rt, ev, what)
	}
	return rt.takePayload(ev), nil
}

// ─── client ──────────────────────────────────────────────────────────────

// CoordinationOptions open a coordination client.
type CoordinationOptions struct {
	// GroupID is the 32-byte group id.
	GroupID []byte
	// SeedTickets are routing hints (Open-credential tickets); they grant
	// nothing: access is the group's replicated ACL against the node's identity.
	SeedTickets []string
	// JournalDir holds the durable client's journal (required unless
	// ReadOnly). The journal's lifetime lock is held until Close; a second
	// open over it fails with ErrCoordinationJournalLocked.
	JournalDir string
	// ReadOnly opens a client without a journal: reads and ids only.
	ReadOnly bool
}

// CoordinationClient is a coordination client bound to a node's identity and
// a group (aster_coordination_client_t).
type CoordinationClient struct {
	rt       *Runtime
	handle   uint64
	groupID  []byte
	localID  []byte
	readOnly bool
	closed   atomic.Bool
}

// OpenCoordinationClient opens a client on node for opts.GroupID. If ctx ends
// first, a client opened meanwhile is closed (releasing its journal lock).
func OpenCoordinationClient(ctx context.Context, node *Node, opts CoordinationOptions) (*CoordinationClient, error) {
	if len(opts.GroupID) != 32 {
		return nil, fmt.Errorf("coordination group id must be 32 bytes, got %d: %w", len(opts.GroupID), ErrInvalidArgument)
	}
	var a cArena
	defer a.free()
	var o C.aster_coordination_client_options_t
	if err := wrapStatus("aster_coordination_client_options_init", C.aster_coordination_client_options_init(&o)); err != nil {
		return nil, err
	}
	gid := a.alloc(32)
	C.memcpy(gid, unsafe.Pointer(&opts.GroupID[0]), 32)
	o.group_id = (*C.uint8_t)(gid)
	o.seed_tickets = a.list(opts.SeedTickets)
	if opts.ReadOnly {
		o.flags = C.ASTER_COORDINATION_CLIENT_READ_ONLY
	} else {
		o.journal_dir = a.str(opts.JournalDir)
	}
	rt := node.runtime
	ev, err := coordOp(ctx, rt, "aster_coordination_client_open", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_coordination_client_open(C.uint64_t(rt.handle), C.uint64_t(node.handle), &o, 0, op)
	}, func(late *Event) {
		if late.Kind == eventCoordClientOpened && late.Handle != 0 {
			C.aster_coordination_client_free(C.uint64_t(rt.handle), C.uint64_t(late.Handle))
		}
	})
	if err != nil {
		return nil, err
	}
	if ev.Kind != eventCoordClientOpened {
		return nil, coordEventError(rt, ev, "aster_coordination_client_open")
	}
	rt.releaseEventBuffer(ev)
	c := &CoordinationClient{rt: rt, handle: ev.Handle, groupID: append([]byte(nil), opts.GroupID...), readOnly: opts.ReadOnly}
	var group, local [32]C.uint8_t
	if err := wrapStatus("aster_coordination_client_ids", C.aster_coordination_client_ids(C.uint64_t(rt.handle),
		C.uint64_t(c.handle), &group[0], &local[0])); err != nil {
		_ = c.Close()
		return nil, err
	}
	c.localID = C.GoBytes(unsafe.Pointer(&local[0]), 32)
	return c, nil
}

func (c *CoordinationClient) rtH() C.uint64_t { return C.uint64_t(c.rt.handle) }

// GroupID is the 32-byte group id.
func (c *CoordinationClient) GroupID() []byte { return append([]byte(nil), c.groupID...) }

// LocalID is the 32-byte local actor id: the node's identity as the group
// authenticates it.
func (c *CoordinationClient) LocalID() []byte { return append([]byte(nil), c.localID...) }

// Handle is the native aster_coordination_client_t.
func (c *CoordinationClient) Handle() uint64 { return c.handle }

// Close closes the client and frees its handle: new calls fail with
// ErrCoordinationClosed, resolved-mutation, lease and placement-manager
// handles become inert (leases stop renewing and lapse by TTL; unacknowledged
// results stay journaled and are redelivered after reopening). Operations
// already in flight complete, and the journal lock is released when the last
// of them ends -- a mutation or lease CompareAndSet in flight can take its
// full drive budget, so Close must not be modelled as prompt. Idempotent.
func (c *CoordinationClient) Close() error {
	if c.closed.Swap(true) {
		return nil
	}
	err := wrapStatus("aster_coordination_client_free", C.aster_coordination_client_free(c.rtH(), C.uint64_t(c.handle)))
	if errors.Is(err, ErrNotFound) {
		return nil
	}
	return err
}

// ClientID is the journal's current 16-byte client id (durable client only).
func (c *CoordinationClient) ClientID(ctx context.Context) ([]byte, error) {
	return coordPayload(ctx, c.rt, "aster_coordination_client_id", ASTER_EVENT_BYTES_RESULT, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_coordination_client_id(c.rtH(), C.uint64_t(c.handle), 0, op)
	})
}

// ─── mutations ───────────────────────────────────────────────────────────

// ResolvedMutation is a mutation's resolved, journaled result. Acknowledge it
// after durably handling it; until then no new mutation is permitted.
type ResolvedMutation struct {
	Reply MutationReply

	c      *CoordinationClient
	handle uint64
	used   atomic.Bool
}

func (c *CoordinationClient) resolvedFrom(ev *Event, what string) (*ResolvedMutation, error) {
	if ev.Kind != eventCoordResolved {
		return nil, coordEventError(c.rt, ev, what)
	}
	handle := ev.Handle
	payload := c.rt.takePayload(ev)
	if handle == 0 {
		return nil, nil
	}
	reply, err := DecodeMutationReply(payload)
	if err != nil {
		C.aster_coordination_resolved_free(c.rtH(), C.uint64_t(handle))
		return nil, fmt.Errorf("%s: %w", what, err)
	}
	return &ResolvedMutation{Reply: reply, c: c, handle: handle}, nil
}

func (c *CoordinationClient) lateResolved(late *Event) {
	if late.Kind == eventCoordResolved && late.Handle != 0 {
		// Dropped unacknowledged: stays journaled, redelivered by Unacknowledged.
		C.aster_coordination_resolved_free(c.rtH(), C.uint64_t(late.Handle))
	}
}

// Mutate runs one sequenced mutation. The reply is fsynced to the journal
// before it is returned. A committed typed rejection is a result (see
// Reply.OutcomeCode), not an error. A request the C ABI would refuse (wrong
// id length, unknown op) is an error wrapping ErrInvalidArgument and ErrUCv1.
func (c *CoordinationClient) Mutate(ctx context.Context, req MutationRequest) (*ResolvedMutation, error) {
	enc, err := EncodeMutationRequest(req)
	if err != nil {
		return nil, fmt.Errorf("coordination mutate: %w: %w", ErrInvalidArgument, err)
	}
	var a cArena
	defer a.free()
	ev, err := coordOp(ctx, c.rt, "aster_coordination_mutate", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_coordination_mutate(c.rtH(), C.uint64_t(c.handle), a.bytes(enc), 0, op)
	}, c.lateResolved)
	if err != nil {
		return nil, err
	}
	r, err := c.resolvedFrom(ev, "aster_coordination_mutate")
	if err == nil && r == nil {
		err = fmt.Errorf("aster_coordination_mutate: resolved without a handle: %w", ErrInternal)
	}
	return r, err
}

// Unacknowledged returns the saved result a previous run resolved and nobody
// acknowledged, or nil when there is none. Call it on startup before any
// mutation.
func (c *CoordinationClient) Unacknowledged(ctx context.Context) (*ResolvedMutation, error) {
	ev, err := coordOp(ctx, c.rt, "aster_coordination_unacknowledged", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_coordination_unacknowledged(c.rtH(), C.uint64_t(c.handle), 0, op)
	}, c.lateResolved)
	if err != nil {
		return nil, err
	}
	return c.resolvedFrom(ev, "aster_coordination_unacknowledged")
}

// Resume drives any half-finished request from a previous run to resolution
// and returns its result, or nil when the journal is ready.
func (c *CoordinationClient) Resume(ctx context.Context) (*ResolvedMutation, error) {
	ev, err := coordOp(ctx, c.rt, "aster_coordination_resume", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_coordination_resume(c.rtH(), C.uint64_t(c.handle), 0, op)
	}, c.lateResolved)
	if err != nil {
		return nil, err
	}
	return c.resolvedFrom(ev, "aster_coordination_resume")
}

// Acknowledge records that the application durably handled this result,
// freeing the journal for the next sequence. It consumes the handle whatever
// the outcome: on failure (including ErrCoordinationClosed after the client
// was closed) the result stays journaled and is redelivered by Unacknowledged.
func (r *ResolvedMutation) Acknowledge(ctx context.Context) error {
	if r.used.Swap(true) {
		return fmt.Errorf("coordination: result already acknowledged or released: %w", ErrNotFound)
	}
	_, err := coordPayload(ctx, r.c.rt, "aster_coordination_resolved_acknowledge", ASTER_EVENT_UNIT_RESULT, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_coordination_resolved_acknowledge(r.c.rtH(), C.uint64_t(r.handle), 0, op)
	})
	return err
}

// Release drops the result WITHOUT acknowledging it: it stays journaled,
// Unacknowledged redelivers it, and no new mutation is permitted until it is
// acknowledged. Idempotent.
func (r *ResolvedMutation) Release() error {
	if r.used.Swap(true) {
		return nil
	}
	err := wrapStatus("aster_coordination_resolved_free", C.aster_coordination_resolved_free(r.c.rtH(), C.uint64_t(r.handle)))
	if errors.Is(err, ErrNotFound) {
		return nil
	}
	return err
}

// Acquire acquires (namespace, key) for ttl.
func (c *CoordinationClient) Acquire(ctx context.Context, res ResourceKey, ttl time.Duration) (*ResolvedMutation, error) {
	return c.Mutate(ctx, MutationRequest{Op: MutateAcquire, Resource: res, TTLMs: uint64(ttl.Milliseconds())})
}

// Renew renews a held lease under its fence.
func (c *CoordinationClient) Renew(ctx context.Context, f CoordinationFence) (*ResolvedMutation, error) {
	return c.Mutate(ctx, MutationRequest{Op: MutateRenew, Fence: f})
}

// CompareAndSet writes value if the register is at expectedValueRevision.
func (c *CoordinationClient) CompareAndSet(ctx context.Context, f CoordinationFence, expectedValueRevision uint64, value []byte) (*ResolvedMutation, error) {
	return c.Mutate(ctx, MutationRequest{Op: MutateCompareAndSet, Fence: f, ExpectedValueRevision: expectedValueRevision, Value: value})
}

// ReleaseFence releases a held lease under its fence.
func (c *CoordinationClient) ReleaseFence(ctx context.Context, f CoordinationFence) (*ResolvedMutation, error) {
	return c.Mutate(ctx, MutationRequest{Op: MutateRelease, Fence: f})
}

// ReleaseWithValue writes finalValue and releases together, or neither.
func (c *CoordinationClient) ReleaseWithValue(ctx context.Context, f CoordinationFence, expectedValueRevision uint64, finalValue []byte) (*ResolvedMutation, error) {
	return c.Mutate(ctx, MutationRequest{Op: MutateReleaseWithValue, Fence: f, ExpectedValueRevision: expectedValueRevision, Value: finalValue})
}

// Revoke revokes a resource's lease (expectedEpoch nil = any epoch).
func (c *CoordinationClient) Revoke(ctx context.Context, res ResourceKey, expectedEpoch *uint64) (*ResolvedMutation, error) {
	return c.Mutate(ctx, MutationRequest{Op: MutateRevoke, Resource: res, ExpectedEpoch: expectedEpoch})
}

// SetGrant sets a node's rights on a resource.
func (c *CoordinationClient) SetGrant(ctx context.Context, res ResourceKey, target []byte, rights, expectedACLRevision uint64) (*ResolvedMutation, error) {
	return c.Mutate(ctx, MutationRequest{Op: MutateSetGrant, Resource: res, TargetNodeID: target, Rights: rights, ExpectedACLRevision: expectedACLRevision})
}

// SetAdmin enables or disables a node as group administrator.
func (c *CoordinationClient) SetAdmin(ctx context.Context, target []byte, enabled bool, expectedAdminRevision uint64) (*ResolvedMutation, error) {
	return c.Mutate(ctx, MutationRequest{Op: MutateSetAdmin, TargetNodeID: target, Enabled: enabled, ExpectedAdminRevision: expectedAdminRevision})
}

// ChangeMembership submits a membership change.
func (c *CoordinationClient) ChangeMembership(ctx context.Context, operationID []byte, expected *LogID, voters, learners [][]byte, seeds []MemberSeed) (*ResolvedMutation, error) {
	return c.Mutate(ctx, MutationRequest{Op: MutateChangeMembership, OperationID: operationID, ExpectedMembershipLogID: expected,
		DesiredVoters: voters, RetainedLearners: learners, MemberSeeds: seeds})
}

// CancelMembership cancels a membership operation.
func (c *CoordinationClient) CancelMembership(ctx context.Context, operationID []byte, expectedPhaseCode uint32) (*ResolvedMutation, error) {
	return c.Mutate(ctx, MutationRequest{Op: MutateCancelMembership, OperationID: operationID, ExpectedPhaseCode: expectedPhaseCode})
}

// RetireOtherClient retires one of this actor's OTHER client ids (16 bytes).
func (c *CoordinationClient) RetireOtherClient(ctx context.Context, clientID []byte) (RetireReply, error) {
	if len(clientID) != 16 {
		return RetireReply{}, fmt.Errorf("client id must be 16 bytes, got %d: %w", len(clientID), ErrInvalidArgument)
	}
	p, err := coordPayload(ctx, c.rt, "aster_coordination_retire_other_client", eventCoordResult, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_coordination_retire_other_client(c.rtH(), C.uint64_t(c.handle), (*C.uint8_t)(unsafe.Pointer(&clientID[0])), 0, op)
	})
	if err != nil {
		return RetireReply{}, err
	}
	return DecodeRetireReply(p)
}

// ─── reads ───────────────────────────────────────────────────────────────

func (c *CoordinationClient) read(ctx context.Context, q ReadRequest) ([]byte, error) {
	enc, err := EncodeReadRequest(q)
	if err != nil {
		return nil, fmt.Errorf("coordination read: %w: %w", ErrInvalidArgument, err)
	}
	var a cArena
	defer a.free()
	return coordPayload(ctx, c.rt, "aster_coordination_read", eventCoordResult, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_coordination_read(c.rtH(), C.uint64_t(c.handle), a.bytes(enc), 0, op)
	})
}

// ReadLinearizable is a linearizable read of one resource (works on a
// read-only client).
func (c *CoordinationClient) ReadLinearizable(ctx context.Context, res ResourceKey) (ResourceSnapshot, error) {
	p, err := c.read(ctx, ReadRequest{Op: ReadLinearizable, Resource: res})
	if err != nil {
		return ResourceSnapshot{}, err
	}
	return DecodeReadReply(p)
}

// InspectACL reads a resource's access list.
func (c *CoordinationClient) InspectACL(ctx context.Context, res ResourceKey) (AclStatus, error) {
	p, err := c.read(ctx, ReadRequest{Op: ReadInspectACL, Resource: res})
	if err != nil {
		return AclStatus{}, err
	}
	return DecodeAclStatus(p)
}

// InspectAdmins reads the group's administrators.
func (c *CoordinationClient) InspectAdmins(ctx context.Context) (AdminStatus, error) {
	p, err := c.read(ctx, ReadRequest{Op: ReadInspectAdmins})
	if err != nil {
		return AdminStatus{}, err
	}
	return DecodeAdminStatus(p)
}

// MembershipStatus reads a membership operation's status (nil = the current one).
func (c *CoordinationClient) MembershipStatus(ctx context.Context, operationID []byte) (MembershipStatus, error) {
	p, err := c.read(ctx, ReadRequest{Op: ReadMembershipState, OperationID: operationID})
	if err != nil {
		return MembershipStatus{}, err
	}
	return DecodeMembershipStatus(p)
}

// GroupStatus reads the group's status.
func (c *CoordinationClient) GroupStatus(ctx context.Context) (GroupStatus, error) {
	p, err := c.read(ctx, ReadRequest{Op: ReadGroupStatus})
	if err != nil {
		return GroupStatus{}, err
	}
	return DecodeGroupStatus(p)
}

// ProbeClient asks what the group knows of another actor's client.
func (c *CoordinationClient) ProbeClient(ctx context.Context, nodeID, clientID []byte) (ClientProbe, error) {
	p, err := c.read(ctx, ReadRequest{Op: ReadProbeClient, TargetNodeID: nodeID, TargetClientID: clientID})
	if err != nil {
		return ClientProbe{}, err
	}
	return DecodeClientProbe(p)
}

// AuditStream reads up to limit (1..=10000) audit records after a log id (nil
// = from the floor).
func (c *CoordinationClient) AuditStream(ctx context.Context, after *LogID, limit uint64) ([]AuditRecord, error) {
	p, err := c.read(ctx, ReadRequest{Op: ReadAuditStream, After: after, Limit: limit})
	if err != nil {
		return nil, err
	}
	return DecodeAuditRecords(p)
}

// RecoveryExport reads the whole recovery export into memory (see
// RecoveryExportToFile for large groups).
func (c *CoordinationClient) RecoveryExport(ctx context.Context) (RecoveryExport, error) {
	p, err := c.read(ctx, ReadRequest{Op: ReadRecoveryExport})
	if err != nil {
		return RecoveryExport{}, err
	}
	return DecodeRecoveryExport(p)
}

// RecoveryExportToFile streams the recovery export into path (via
// path.partial, renamed into place only once the checksum held).
func (c *CoordinationClient) RecoveryExportToFile(ctx context.Context, path string) (RecoveryExportManifest, error) {
	var a cArena
	defer a.free()
	p, err := coordPayload(ctx, c.rt, "aster_coordination_recovery_export_to_file", eventCoordResult, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_coordination_recovery_export_to_file(c.rtH(), C.uint64_t(c.handle), a.str(path), 0, op)
	})
	if err != nil {
		return RecoveryExportManifest{}, err
	}
	return DecodeRecoveryExportManifest(p)
}

// ─── leases ──────────────────────────────────────────────────────────────

// CoordinationLease is a held coordination lease (aster_coordination_lease_t):
// the Rust renew loop and self-limit run in the background, and every
// transition is journaled and acknowledged by the lease itself.
type CoordinationLease struct {
	c       *CoordinationClient
	handle  uint64
	granted CoordinationFence
	done    atomic.Bool
}

// AcquireLease makes one committed acquire of (namespace, key). Granted: a
// *CoordinationLease. Denied: a *LeaseDeniedError (errors.Is ErrLeaseDenied)
// with the holder and back-off hint in its snapshot. opts as for
// LeaseOptions (zero = defaults). If ctx ends first, a lease granted
// meanwhile is released rather than leaked.
func (c *CoordinationClient) AcquireLease(ctx context.Context, namespace string, key []byte, opts LeaseOptions) (*CoordinationLease, error) {
	var a cArena
	defer a.free()
	ev, err := coordOp(ctx, c.rt, "aster_coordination_lease_acquire", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_coordination_lease_acquire(c.rtH(), C.uint64_t(c.handle), a.str(namespace), a.bytes(key),
			C.uint64_t(opts.TTL.Milliseconds()), C.uint64_t(opts.RenewEvery.Milliseconds()),
			C.uint64_t(opts.HolderMargin.Milliseconds()), 0, op)
	}, func(late *Event) {
		if late.Kind == eventCoordLeaseAcquired && late.Handle != 0 {
			(&CoordinationLease{c: c, handle: late.Handle}).releaseInBackground()
		}
	})
	if err != nil {
		return nil, err
	}
	if ev.Kind != eventCoordLeaseAcquired {
		return nil, coordEventError(c.rt, ev, "aster_coordination_lease_acquire")
	}
	handle := ev.Handle
	payload := c.rt.takePayload(ev)
	if handle == 0 { // denied: B2
		out, derr := DecodeTransitionOutcome(payload)
		if derr != nil {
			return nil, fmt.Errorf("aster_coordination_lease_acquire: %w", derr)
		}
		if out.Kind != TransitionDenied {
			return nil, fmt.Errorf("aster_coordination_lease_acquire: no handle for outcome %d: %w", out.Kind, ErrInternal)
		}
		return nil, &LeaseDeniedError{Reason: out.Reason, Snapshot: out.Snapshot}
	}
	fence, derr := DecodeCoordinationFence(payload)
	if derr != nil {
		C.aster_coordination_lease_free(c.rtH(), C.uint64_t(handle))
		return nil, fmt.Errorf("aster_coordination_lease_acquire: %w", derr)
	}
	return &CoordinationLease{c: c, handle: handle, granted: fence}, nil
}

func (l *CoordinationLease) rtH() C.uint64_t { return C.uint64_t(l.c.rt.handle) }

// GrantedFence is the fence issued at acquire.
func (l *CoordinationLease) GrantedFence() CoordinationFence { return l.granted }

// Fence returns the current fence for a protected action: ask immediately
// before each action and never cache it. ok is false once the holder must
// stop (not held, self-limit passed, released or being released, client
// closed). Never blocks.
func (l *CoordinationLease) Fence() (fence CoordinationFence, ok bool) {
	if l.done.Load() {
		return CoordinationFence{}, false
	}
	buf := make([]byte, 256)
	for {
		var n C.uintptr_t
		st := C.aster_coordination_lease_fence(l.rtH(), C.uint64_t(l.handle), (*C.uint8_t)(unsafe.Pointer(&buf[0])), C.uintptr_t(len(buf)), &n)
		switch st {
		case 0:
			f, err := DecodeCoordinationFence(buf[:n])
			return f, err == nil
		case C.ASTER_STATUS_BUFFER_TOO_SMALL:
			buf = make([]byte, int(n))
		default:
			return CoordinationFence{}, false
		}
	}
}

// Status is the holder-side status (LeaseStatusReleased once released, being
// released, or its client closed). Never blocks.
func (l *CoordinationLease) Status() (LeaseStatus, error) {
	if l.done.Load() {
		return LeaseStatusReleased, nil // released or freed: the handle is gone
	}
	var st C.uint32_t
	if err := wrapStatus("aster_coordination_lease_status", C.aster_coordination_lease_status(l.rtH(), C.uint64_t(l.handle), &st)); err != nil {
		return 0, err
	}
	return LeaseStatus(st), nil
}

// CompareAndSet writes the register under the lease's current fence;
// journaled and acknowledged by the lease. A Release, Close or client close
// called after this returns waits for it to finish.
func (l *CoordinationLease) CompareAndSet(ctx context.Context, expectedValueRevision uint64, newValue []byte) (MutationReply, error) {
	var a cArena
	defer a.free()
	p, err := coordPayload(ctx, l.c.rt, "aster_coordination_lease_compare_and_set", eventCoordResult, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_coordination_lease_compare_and_set(l.rtH(), C.uint64_t(l.handle), C.uint64_t(expectedValueRevision), a.bytes(newValue), 0, op)
	})
	if err != nil {
		return MutationReply{}, err
	}
	return DecodeMutationReply(p)
}

// Release is a graceful release through the lease's own transition. It
// consumes the lease and WAITS for an in-flight CompareAndSet (which may take
// its full drive budget): it is not prompt.
func (l *CoordinationLease) Release(ctx context.Context) error {
	if l.done.Swap(true) {
		return nil
	}
	_, err := coordPayload(ctx, l.c.rt, "aster_coordination_lease_release", ASTER_EVENT_UNIT_RESULT, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_coordination_lease_release(l.rtH(), C.uint64_t(l.handle), 0, op)
	})
	return err
}

// ReleaseWithValue is the atomic final handoff: write finalValue and release
// together, or neither. Consumes the lease; as slow as Release.
func (l *CoordinationLease) ReleaseWithValue(ctx context.Context, expectedValueRevision uint64, finalValue []byte) (MutationReply, error) {
	if l.done.Swap(true) {
		return MutationReply{}, fmt.Errorf("coordination lease already released: %w", ErrNotFound)
	}
	var a cArena
	defer a.free()
	p, err := coordPayload(ctx, l.c.rt, "aster_coordination_lease_release_with_value", eventCoordResult, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_coordination_lease_release_with_value(l.rtH(), C.uint64_t(l.handle), C.uint64_t(expectedValueRevision), a.bytes(finalValue), 0, op)
	})
	if err != nil {
		return MutationReply{}, err
	}
	return DecodeMutationReply(p)
}

// Close frees the lease WITHOUT releasing it: renewals stop and it lapses by
// TTL. Prefer Release. Idempotent.
func (l *CoordinationLease) Close() error {
	if l.done.Swap(true) {
		return nil
	}
	err := wrapStatus("aster_coordination_lease_free", C.aster_coordination_lease_free(l.rtH(), C.uint64_t(l.handle)))
	if errors.Is(err, ErrNotFound) {
		return nil
	}
	return err
}

// releaseInBackground disposes of a lease granted after its acquire gave up:
// release it (bounded), falling back to free.
func (l *CoordinationLease) releaseInBackground() {
	var op C.aster_operation_t
	if C.aster_coordination_lease_release(l.rtH(), C.uint64_t(l.handle), 0, &op) != 0 {
		C.aster_coordination_lease_free(l.rtH(), C.uint64_t(l.handle))
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
	defer cancel()
	if ev, err := l.c.rt.await(ctx, uint64(op)); err == nil {
		l.c.rt.releaseEventBuffer(ev)
	}
}

// ─── placement ───────────────────────────────────────────────────────────

// PlacementManager reconciles the group's membership against a policy
// (aster_coordination_placement_manager_t). Its policy source (kind 32) and
// provisioner (kind 33) are Go, answered on an Upcalls table; its topology is
// a node's own shared view or Go (kind 17). Provider failures (error,
// unimplemented, timeout, malformed answer) FAIL the pass: these kinds have
// no fail-open. It changes membership only through the client's durable
// journal, authorized by the replicated administrator predicate.
type PlacementManager struct {
	c      *CoordinationClient
	handle uint64
	closed atomic.Bool
}

// PlacementPolicySource supplies the policy of each pass (kind 32).
type PlacementPolicySource func(ctx context.Context) (PlacementPolicy, error)

// PlacementProvisioner prepares a node to join and returns its member seed
// (kind 33); the seed's node id must be the requested node. The request is
// idempotent and may repeat.
type PlacementProvisioner func(ctx context.Context, req ProvisionRequest) (MemberSeed, error)

// TopologySource supplies the topology view when no node's own shared view
// is used (kind 17).
type TopologySource func(ctx context.Context) (TopologySnapshot, error)

// ServePlacementPolicy serves the placement policy kind (32).
func (u *Upcalls) ServePlacementPolicy(src PlacementPolicySource) error {
	return u.HandleKind(UpcallPlacementPolicy, func(ctx context.Context, req *UpcallRequest) ([]byte, error) {
		if err := DecodePlacementPolicyRequest(req.Payload); err != nil {
			return nil, err
		}
		p, err := src(ctx)
		if err != nil {
			return nil, err
		}
		return EncodePlacementPolicy(p), nil
	})
}

// ServePlacementProvisioner serves the placement provision kind (33).
func (u *Upcalls) ServePlacementProvisioner(p PlacementProvisioner) error {
	return u.HandleKind(UpcallPlacementProvision, func(ctx context.Context, req *UpcallRequest) ([]byte, error) {
		r, err := DecodeProvisionRequest(req.Payload)
		if err != nil {
			return nil, err
		}
		seed, err := p(ctx, r)
		if err != nil {
			return nil, err
		}
		return EncodeMemberSeed(seed), nil
	})
}

// ServeTopologySnapshot serves the topology snapshot kind (17).
func (u *Upcalls) ServeTopologySnapshot(src TopologySource) error {
	return u.HandleKind(UpcallTopologySnapshot, func(ctx context.Context, req *UpcallRequest) ([]byte, error) {
		if err := DecodeTopologySnapshotRequest(req.Payload); err != nil {
			return nil, err
		}
		t, err := src(ctx)
		if err != nil {
			return nil, err
		}
		return EncodeTopologySnapshot(t), nil
	})
}

// NewPlacementManager creates a manager over a durable client. topology, if
// non-nil, is the node whose own shared view is the topology (it must have
// joined a swarm with Node.EnableShared); nil serves the topology from Go
// (Upcalls.ServeTopologySnapshot). up answers the policy and provisioner
// kinds; keep it open while the manager runs.
func NewPlacementManager(c *CoordinationClient, up *Upcalls, topology *Node) (*PlacementManager, error) {
	if up == nil || up.rt != c.rt {
		return nil, fmt.Errorf("placement manager: upcalls must share the client's runtime: %w", ErrInvalidArgument)
	}
	var tn C.uint64_t
	if topology != nil {
		if topology.runtime != c.rt {
			return nil, fmt.Errorf("placement manager: topology node must share the client's runtime: %w", ErrInvalidArgument)
		}
		tn = C.uint64_t(topology.handle)
	}
	var h C.aster_coordination_placement_manager_t
	if err := wrapStatus("aster_coordination_placement_manager_new", C.aster_coordination_placement_manager_new(
		c.rtH(), C.uint64_t(c.handle), C.uint64_t(up.handle), tn, &h)); err != nil {
		return nil, err
	}
	return &PlacementManager{c: c, handle: uint64(h)}, nil
}

// Reconcile runs one reconciliation pass (passes on one manager are
// serialized) and reports what it decided and did. A provider failure fails
// the pass (never fails open).
func (m *PlacementManager) Reconcile(ctx context.Context) (PlacementReport, error) {
	p, err := coordPayload(ctx, m.c.rt, "aster_coordination_placement_reconcile", eventCoordResult, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_coordination_placement_reconcile(m.c.rtH(), C.uint64_t(m.handle), 0, op)
	})
	if err != nil {
		return PlacementReport{}, err
	}
	return DecodePlacementReport(p)
}

// Close frees the manager (after any running pass). Idempotent.
func (m *PlacementManager) Close() error {
	if m.closed.Swap(true) {
		return nil
	}
	err := wrapStatus("aster_coordination_placement_manager_free", C.aster_coordination_placement_manager_free(m.c.rtH(), C.uint64_t(m.handle)))
	if errors.Is(err, ErrNotFound) {
		return nil
	}
	return err
}
