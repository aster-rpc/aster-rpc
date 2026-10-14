//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"fmt"
	"unsafe"
)

// Sealed grants (aster/sealed-grant/v1, ABI 3.4): HPKE-sealed payloads bound
// to a GrantContext, grant records, and reconciliation. All synchronous.

// GrantContext binds a grant envelope: every field is part of the AAD.
// Granter and Recipient are node ids (64 hex), bound verbatim.
type GrantContext struct {
	App       string
	Granter   string
	Recipient string
	Resource  []byte
	Path      string
	Role      string
}

func (g *GrantContext) toC(a *cArena) *C.aster_grant_context_t {
	c := (*C.aster_grant_context_t)(a.alloc(unsafe.Sizeof(C.aster_grant_context_t{})))
	c.struct_size = C.uint32_t(unsafe.Sizeof(C.aster_grant_context_t{}))
	c.app = a.str(g.App)
	c.granter = a.str(g.Granter)
	c.recipient = a.str(g.Recipient)
	c.resource = a.bytes(g.Resource)
	c.path = a.str(g.Path)
	c.role = a.str(g.Role)
	return c
}

// LocalAccess is a namespace capability's access (aster_local_access_t).
type LocalAccess uint32

const (
	AccessWrite LocalAccess = C.ASTER_LOCAL_ACCESS_WRITE
	AccessRead  LocalAccess = C.ASTER_LOCAL_ACCESS_READ
)

// NamespaceCapability is (AccessWrite, namespace secret) or (AccessRead, namespace id).
type NamespaceCapability struct {
	Access LocalAccess
	Key    Key
}

func bytesOut(name string, f func(out *C.struct_aster_bytes_t) C.int32_t) ([]byte, error) {
	var out C.struct_aster_bytes_t
	if err := ffiStatus(func() C.int32_t { return f(&out) }); err != nil {
		return nil, fmt.Errorf("%s: %w", name, err)
	}
	return nonNil(takeRustBytes(out)), nil
}

// GrantContextAAD returns the AAD: domain "aster/sealed-grant/v1" plus the
// six context fields, each u32-BE length-prefixed.
func GrantContextAAD(ctx GrantContext) ([]byte, error) {
	var a cArena
	defer a.free()
	return bytesOut("aster_grant_context_aad", func(out *C.struct_aster_bytes_t) C.int32_t {
		return C.aster_grant_context_aad(ctx.toC(&a), out)
	})
}

// GrantKey is the conventional doc key of a grant: "<prefix>/grants/<recipient>".
func GrantKey(prefix, recipient string) (string, error) {
	var a cArena
	defer a.free()
	b, err := bytesOut("aster_grant_key", func(out *C.struct_aster_bytes_t) C.int32_t {
		return C.aster_grant_key(a.str(prefix), a.str(recipient), out)
	})
	return string(b), err
}

// GrantCheckRole applies the role/capability rule: "read" needs
// !canWrite, "write" needs canWrite, other roles pass. A mismatch is
// ErrVerificationFailed.
func GrantCheckRole(role string, canWrite bool) error {
	var a cArena
	defer a.free()
	return wrapCall("aster_grant_check_role", func() C.int32_t { return C.aster_grant_check_role(a.str(role), boolToUint32(canWrite)) })
}

// GrantSeal seals plaintext to ctx.Recipient's identity-derived X25519 key.
func GrantSeal(ctx GrantContext, plaintext []byte) ([]byte, error) {
	var a cArena
	defer a.free()
	return bytesOut("aster_grant_seal", func(out *C.struct_aster_bytes_t) C.int32_t {
		return C.aster_grant_seal(ctx.toC(&a), a.bytes(plaintext), out)
	})
}

// GrantSealToKey seals to an explicit X25519 public key.
func GrantSealToKey(recipientPublic Key, ctx GrantContext, plaintext []byte) ([]byte, error) {
	var a cArena
	defer a.free()
	return bytesOut("aster_grant_seal_to_key", func(out *C.struct_aster_bytes_t) C.int32_t {
		return C.aster_grant_seal_to_key(cKey(&a, recipientPublic), ctx.toC(&a), a.bytes(plaintext), out)
	})
}

// GrantOpen opens an envelope with the recipient's Ed25519 identity secret;
// any mismatch is ErrVerificationFailed.
func GrantOpen(identitySecret Key, ctx GrantContext, envelope []byte) ([]byte, error) {
	var a cArena
	defer a.free()
	return bytesOut("aster_grant_open", func(out *C.struct_aster_bytes_t) C.int32_t {
		return C.aster_grant_open(cKey(&a, identitySecret), ctx.toC(&a), a.bytes(envelope), out)
	})
}

// GrantOpenWithKey opens with an explicit X25519 private key.
func GrantOpenWithKey(recipientPrivate Key, ctx GrantContext, envelope []byte) ([]byte, error) {
	var a cArena
	defer a.free()
	return bytesOut("aster_grant_open_with_key", func(out *C.struct_aster_bytes_t) C.int32_t {
		return C.aster_grant_open_with_key(cKey(&a, recipientPrivate), ctx.toC(&a), a.bytes(envelope), out)
	})
}

// EncodeNamespaceCapability Fory-encodes a namespace capability.
func EncodeNamespaceCapability(c NamespaceCapability) ([]byte, error) {
	var a cArena
	defer a.free()
	return bytesOut("aster_namespace_capability_encode", func(out *C.struct_aster_bytes_t) C.int32_t {
		return C.aster_namespace_capability_encode(C.uint32_t(c.Access), cKey(&a, c.Key), out)
	})
}

// DecodeNamespaceCapability decodes Fory namespace-capability bytes.
func DecodeNamespaceCapability(b []byte) (NamespaceCapability, error) {
	var a cArena
	defer a.free()
	var access C.uint32_t
	out, get := cOutKey(&a)
	if err := ffiStatus(func() C.int32_t { return C.aster_namespace_capability_decode(a.bytes(b), &access, out) }); err != nil {
		return NamespaceCapability{}, fmt.Errorf("aster_namespace_capability_decode: %w", err)
	}
	return NamespaceCapability{Access: LocalAccess(access), Key: get()}, nil
}

// NamespaceGrantSeal seals a namespace capability after checking the role rule.
func NamespaceGrantSeal(ctx GrantContext, c NamespaceCapability) ([]byte, error) {
	var a cArena
	defer a.free()
	return bytesOut("aster_namespace_grant_seal", func(out *C.struct_aster_bytes_t) C.int32_t {
		return C.aster_namespace_grant_seal(ctx.toC(&a), C.uint32_t(c.Access), cKey(&a, c.Key), out)
	})
}

// NamespaceGrantOpen opens a namespace-capability grant and re-checks the
// role rule.
func NamespaceGrantOpen(identitySecret Key, ctx GrantContext, envelope []byte) (NamespaceCapability, error) {
	var a cArena
	defer a.free()
	var access C.uint32_t
	out, get := cOutKey(&a)
	if err := ffiStatus(func() C.int32_t {
		return C.aster_namespace_grant_open(cKey(&a, identitySecret), ctx.toC(&a), a.bytes(envelope), &access, out)
	}); err != nil {
		return NamespaceCapability{}, fmt.Errorf("aster_namespace_grant_open: %w", err)
	}
	return NamespaceCapability{Access: LocalAccess(access), Key: get()}, nil
}

// GrantState is a grant record's state.
type GrantState uint32

const (
	GrantActive  GrantState = C.ASTER_GRANT_STATE_ACTIVE
	GrantRevoked GrantState = C.ASTER_GRANT_STATE_REVOKED
)

// GrantRecord is the doc value stored at a grant key. ACTIVE needs an
// envelope (no reason); REVOKED takes role + reason and no envelope.
type GrantRecord struct {
	State GrantState
	// ContextVersion: read it before choosing the context to open with.
	ContextVersion int32
	Role           string
	Envelope       []byte
	Reason         string
}

// EncodeGrantRecord Fory-encodes a grant record.
func EncodeGrantRecord(r GrantRecord) ([]byte, error) {
	var a cArena
	defer a.free()
	c := (*C.aster_grant_record_t)(a.alloc(unsafe.Sizeof(C.aster_grant_record_t{})))
	c.struct_size = C.uint32_t(unsafe.Sizeof(C.aster_grant_record_t{}))
	c.state = C.uint32_t(r.State)
	c.context_version = C.int32_t(r.ContextVersion)
	c.role = a.str(r.Role)
	c.envelope = a.bytes(r.Envelope)
	c.reason = a.str(r.Reason)
	return bytesOut("aster_grant_record_encode", func(out *C.struct_aster_bytes_t) C.int32_t {
		return C.aster_grant_record_encode(c, out)
	})
}

// DecodeGrantRecord decodes Fory grant-record bytes.
func DecodeGrantRecord(b []byte) (GrantRecord, error) {
	var a cArena
	defer a.free()
	var c C.aster_grant_record_t
	if err := ffiStatus(func() C.int32_t { return C.aster_grant_record_decode(a.bytes(b), &c) }); err != nil {
		return GrantRecord{}, fmt.Errorf("aster_grant_record_decode: %w", err)
	}
	defer C.aster_grant_record_release(&c)
	return GrantRecord{
		State: GrantState(c.state), ContextVersion: int32(c.context_version),
		Role: string(viewBytes(c.role)), Envelope: viewBytes(c.envelope), Reason: string(viewBytes(c.reason)),
	}, nil
}

// GrantOutcomeKind is the reconcile decision.
type GrantOutcomeKind uint32

const (
	OutcomeActive     GrantOutcomeKind = C.ASTER_GRANT_OUTCOME_ACTIVE
	OutcomeRevoked    GrantOutcomeKind = C.ASTER_GRANT_OUTCOME_REVOKED
	OutcomeUnopenable GrantOutcomeKind = C.ASTER_GRANT_OUTCOME_UNOPENABLE
	OutcomeAbsent     GrantOutcomeKind = C.ASTER_GRANT_OUTCOME_ABSENT
)

// GrantOutcome is what to do with a grant record.
type GrantOutcome struct {
	Kind GrantOutcomeKind
	// Access (namespace grants, ACTIVE): the capability's access.
	Access          LocalAccess
	DropsCapability bool
	// Payload: the opened plaintext (raw grants) or the 32-byte key
	// (namespace grants), for ACTIVE.
	Payload []byte
	Reason  string
	Role    string
}

// ReconcileGrant decides what to do with the record at ctx.Path, opening an
// active record with the recipient's identity secret. A nil record means the
// key has none (ABSENT); a non-nil empty record is present but unopenable.
func ReconcileGrant(identitySecret Key, ctx GrantContext, record []byte) (GrantOutcome, error) {
	return reconcile("aster_grant_reconcile", identitySecret, ctx, record, false)
}

// ReconcileNamespaceGrant is ReconcileGrant for namespace-capability grants;
// a role/capability mismatch is UNOPENABLE.
func ReconcileNamespaceGrant(identitySecret Key, ctx GrantContext, record []byte) (GrantOutcome, error) {
	return reconcile("aster_namespace_grant_reconcile", identitySecret, ctx, record, true)
}

func reconcile(name string, identitySecret Key, ctx GrantContext, record []byte, namespace bool) (GrantOutcome, error) {
	var a cArena
	defer a.free()
	var rec *C.struct_aster_bytes_t
	if record != nil {
		rec = (*C.struct_aster_bytes_t)(a.alloc(unsafe.Sizeof(C.struct_aster_bytes_t{})))
		*rec = a.bytes(record)
	}
	var out C.aster_grant_outcome_t
	out.struct_size = C.uint32_t(unsafe.Sizeof(out))
	var r C.int32_t
	var rMsg string
	if namespace {
		r, rMsg = ffiCall(func() C.int32_t {
			return C.aster_namespace_grant_reconcile(cKey(&a, identitySecret), ctx.toC(&a), rec, &out)
		})
	} else {
		r, rMsg = ffiCall(func() C.int32_t { return C.aster_grant_reconcile(cKey(&a, identitySecret), ctx.toC(&a), rec, &out) })
	}
	if err := statusErrorMsg(r, rMsg); err != nil {
		return GrantOutcome{}, fmt.Errorf("%s: %w", name, err)
	}
	defer C.aster_grant_outcome_release(&out)
	return GrantOutcome{
		Kind: GrantOutcomeKind(out.kind), Access: LocalAccess(out.access), DropsCapability: out.drops_capability != 0,
		Payload: viewBytes(out.payload), Reason: string(viewBytes(out.reason)), Role: string(viewBytes(out.role)),
	}, nil
}
