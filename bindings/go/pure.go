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

// Pure functions of ABI 3.4 (typed ids, HPKE, attestation). All are
// synchronous and need no runtime. Keys and ids are 32-byte arrays.

// Key is a 32-byte key or id.
type Key [32]byte

// IDKind selects the id type for ParseID / FormatID (aster_id_kind_t).
type IDKind uint32

const (
	IDNodeID          IDKind = C.ASTER_ID_NODE_ID
	IDPublicKey       IDKind = C.ASTER_ID_PUBLIC_KEY
	IDSecretKey       IDKind = C.ASTER_ID_SECRET_KEY
	IDHash            IDKind = C.ASTER_ID_HASH
	IDNamespaceID     IDKind = C.ASTER_ID_NAMESPACE_ID
	IDNamespaceSecret IDKind = C.ASTER_ID_NAMESPACE_SECRET
	IDAuthorID        IDKind = C.ASTER_ID_AUTHOR_ID
)

func keyPtr(k *Key) *C.uint8_t { return (*C.uint8_t)(unsafe.Pointer(&k[0])) }

// cKey copies a key into C memory owned by a.
func cKey(a *cArena, k Key) *C.uint8_t {
	p := a.alloc(32)
	copy(unsafe.Slice((*byte)(p), 32), k[:])
	return (*C.uint8_t)(p)
}

func cOutKey(a *cArena) (*C.uint8_t, func() Key) {
	p := a.alloc(32)
	return (*C.uint8_t)(p), func() Key {
		var k Key
		copy(k[:], unsafe.Slice((*byte)(p), 32))
		return k
	}
}

// ParseID parses 64 hex chars (either case) as an id of the given kind.
// Ed25519 kinds (node id, public key, author id) must be a curve point.
func ParseID(kind IDKind, text string) (Key, error) {
	var a cArena
	defer a.free()
	out, get := cOutKey(&a)
	if err := statusError(C.aster_id_parse(C.uint32_t(kind), a.str(text), out)); err != nil {
		return Key{}, fmt.Errorf("aster_id_parse: %w", err)
	}
	return get(), nil
}

// FormatID formats an id as lowercase hex (does not validate).
func FormatID(kind IDKind, id Key) (string, error) {
	var a cArena
	defer a.free()
	out := (*C.uint8_t)(a.alloc(65))
	if err := statusError(C.aster_id_format(C.uint32_t(kind), cKey(&a, id), out)); err != nil {
		return "", fmt.Errorf("aster_id_format: %w", err)
	}
	return C.GoStringN((*C.char)(unsafe.Pointer(out)), 64), nil
}

// ValidateID reports whether id is valid for kind.
func ValidateID(kind IDKind, id Key) error {
	var a cArena
	defer a.free()
	return wrapStatus("aster_id_validate", C.aster_id_validate(C.uint32_t(kind), cKey(&a, id)))
}

// GenerateSecretKey returns a fresh 32-byte Ed25519 secret key.
func GenerateSecretKey() (Key, error) {
	var a cArena
	defer a.free()
	out, get := cOutKey(&a)
	var n C.uintptr_t
	if err := statusError(C.aster_secret_key_generate(out, 32, &n)); err != nil {
		return Key{}, fmt.Errorf("aster_secret_key_generate: %w", err)
	}
	return get(), nil
}

// SecretKeyPublicKey returns the Ed25519 public key (= node id) of secret.
func SecretKeyPublicKey(secret Key) (Key, error) {
	var a cArena
	defer a.free()
	out, get := cOutKey(&a)
	if err := statusError(C.aster_secret_key_public_key(cKey(&a, secret), out)); err != nil {
		return Key{}, fmt.Errorf("aster_secret_key_public_key: %w", err)
	}
	return get(), nil
}

// NamespaceSecretID returns the namespace id of a namespace secret.
func NamespaceSecretID(secret Key) (Key, error) {
	var a cArena
	defer a.free()
	out, get := cOutKey(&a)
	if err := statusError(C.aster_namespace_secret_id(cKey(&a, secret), out)); err != nil {
		return Key{}, fmt.Errorf("aster_namespace_secret_id: %w", err)
	}
	return get(), nil
}

// ─── HPKE ───────────────────────────────────────────────────────────────────

// HPKEGenerateKeypair returns a fresh X25519 (private, public) key pair.
func HPKEGenerateKeypair() (private, public Key, err error) {
	var a cArena
	defer a.free()
	priv, getPriv := cOutKey(&a)
	pub, getPub := cOutKey(&a)
	if err := statusError(C.aster_hpke_generate_keypair(priv, pub)); err != nil {
		return Key{}, Key{}, fmt.Errorf("aster_hpke_generate_keypair: %w", err)
	}
	return getPriv(), getPub(), nil
}

func keyToKey(name string, in Key, f func(*C.uint8_t, *C.uint8_t) C.int32_t) (Key, error) {
	var a cArena
	defer a.free()
	out, get := cOutKey(&a)
	if err := statusError(f(cKey(&a, in), out)); err != nil {
		return Key{}, fmt.Errorf("%s: %w", name, err)
	}
	return get(), nil
}

// HPKEPublicKeyFromPrivate derives the X25519 public key.
func HPKEPublicKeyFromPrivate(private Key) (Key, error) {
	return keyToKey("aster_hpke_public_key_from_private", private, func(i, o *C.uint8_t) C.int32_t {
		return C.aster_hpke_public_key_from_private(i, o)
	})
}

// X25519SecretFromIdentity derives the X25519 secret of an Ed25519 identity
// secret (libsodium-compatible).
func X25519SecretFromIdentity(identitySecret Key) (Key, error) {
	return keyToKey("aster_hpke_x25519_secret_from_identity", identitySecret, func(i, o *C.uint8_t) C.int32_t {
		return C.aster_hpke_x25519_secret_from_identity(i, o)
	})
}

// X25519PublicFromIdentity derives the X25519 public key of an Ed25519
// identity public key.
func X25519PublicFromIdentity(identityPublic Key) (Key, error) {
	return keyToKey("aster_hpke_x25519_public_from_identity", identityPublic, func(i, o *C.uint8_t) C.int32_t {
		return C.aster_hpke_x25519_public_from_identity(i, o)
	})
}

// X25519PublicFromNodeID derives the X25519 public key of a node id (64 hex).
func X25519PublicFromNodeID(nodeID string) (Key, error) {
	var a cArena
	defer a.free()
	out, get := cOutKey(&a)
	if err := statusError(C.aster_hpke_x25519_public_from_node_id(a.str(nodeID), out)); err != nil {
		return Key{}, fmt.Errorf("aster_hpke_x25519_public_from_node_id: %w", err)
	}
	return get(), nil
}

// HPKESeal seals plaintext to an X25519 public key with aad; the result is a
// Fory aster.hpke.Envelope.
func HPKESeal(recipientPublic Key, aad, plaintext []byte) ([]byte, error) {
	var a cArena
	defer a.free()
	var out C.struct_aster_bytes_t
	if err := statusError(C.aster_hpke_seal(cKey(&a, recipientPublic), a.bytes(aad), a.bytes(plaintext), &out)); err != nil {
		return nil, fmt.Errorf("aster_hpke_seal: %w", err)
	}
	return takeRustBytes(out), nil
}

// HPKEOpen opens an envelope; a wrong key, AAD or corrupt envelope is
// ErrVerificationFailed.
func HPKEOpen(recipientPrivate Key, aad, envelope []byte) ([]byte, error) {
	var a cArena
	defer a.free()
	var out C.struct_aster_bytes_t
	if err := statusError(C.aster_hpke_open(cKey(&a, recipientPrivate), a.bytes(aad), a.bytes(envelope), &out)); err != nil {
		return nil, fmt.Errorf("aster_hpke_open: %w", err)
	}
	return nonNil(takeRustBytes(out)), nil
}

func nonNil(b []byte) []byte {
	if b == nil {
		return []byte{}
	}
	return b
}

// ─── Attestation ────────────────────────────────────────────────────────────

// AttestationRole is a participant's role in a verified chain.
type AttestationRole uint32

const (
	RoleNone         AttestationRole = C.ASTER_ATTESTATION_ROLE_NONE
	RoleNode         AttestationRole = C.ASTER_ATTESTATION_ROLE_NODE
	RoleAnchor       AttestationRole = C.ASTER_ATTESTATION_ROLE_ANCHOR
	RoleIntermediate AttestationRole = C.ASTER_ATTESTATION_ROLE_INTERMEDIATE
)

// AttestOptions are the optional validity bounds of AttestRootNode (unix
// seconds; zero = the Rust default).
type AttestOptions struct {
	Epoch     int64
	NotBefore int64
	NotAfter  int64
}

// AttestRootNode issues a root -> node ownership attestation chain (wire bytes).
func AttestRootNode(rootSecret, nodeSecret Key, opts *AttestOptions) ([]byte, error) {
	var a cArena
	defer a.free()
	var copts *C.aster_attest_options_t
	if opts != nil {
		copts = (*C.aster_attest_options_t)(a.alloc(unsafe.Sizeof(C.aster_attest_options_t{})))
		copts.struct_size = C.uint32_t(unsafe.Sizeof(C.aster_attest_options_t{}))
		copts.epoch = C.int64_t(opts.Epoch)
		copts.not_before = C.int64_t(opts.NotBefore)
		copts.not_after = C.int64_t(opts.NotAfter)
	}
	var out C.struct_aster_bytes_t
	if err := statusError(C.aster_attest_root_node(cKey(&a, rootSecret), cKey(&a, nodeSecret), copts, &out)); err != nil {
		return nil, fmt.Errorf("aster_attest_root_node: %w", err)
	}
	return takeRustBytes(out), nil
}

// AttestationChainToText renders chain bytes as "aster.attestation.chain.v1:<base64url>".
func AttestationChainToText(chain []byte) (string, error) {
	var a cArena
	defer a.free()
	var out C.struct_aster_bytes_t
	if err := statusError(C.aster_attestation_chain_to_text(a.bytes(chain), &out)); err != nil {
		return "", fmt.Errorf("aster_attestation_chain_to_text: %w", err)
	}
	return string(takeRustBytes(out)), nil
}

// AttestationChainFromText parses the text form back to chain bytes.
func AttestationChainFromText(text string) ([]byte, error) {
	var a cArena
	defer a.free()
	var out C.struct_aster_bytes_t
	if err := statusError(C.aster_attestation_chain_from_text(a.str(text), &out)); err != nil {
		return nil, fmt.Errorf("aster_attestation_chain_from_text: %w", err)
	}
	return takeRustBytes(out), nil
}

// VerifiedChain is a chain that verified against a trusted anchor.
type VerifiedChain struct {
	Depth         uint32
	Node          Key
	Anchor        Key
	Intermediates []Key
}

// RoleOf returns id's role in the chain (the node wins over the anchor if equal).
func (v *VerifiedChain) RoleOf(id Key) AttestationRole {
	switch {
	case id == v.Node:
		return RoleNode
	case id == v.Anchor:
		return RoleAnchor
	}
	for _, k := range v.Intermediates {
		if k == id {
			return RoleIntermediate
		}
	}
	return RoleNone
}

// VerifyAttestationChain verifies chain against the trusted anchors, for
// expectedNode. Any failed check is ErrVerificationFailed.
func VerifyAttestationChain(chain []byte, anchors []Key, expectedNode Key) (*VerifiedChain, error) {
	var a cArena
	defer a.free()
	var anchorsPtr *C.uint8_t
	if len(anchors) > 0 {
		p := a.alloc(uintptr(32 * len(anchors)))
		dst := unsafe.Slice((*byte)(p), 32*len(anchors))
		for i, k := range anchors {
			copy(dst[32*i:], k[:])
		}
		anchorsPtr = (*C.uint8_t)(p)
	}
	var out C.aster_verified_chain_t
	out.struct_size = C.uint32_t(unsafe.Sizeof(out))
	if err := statusError(C.aster_attestation_verify_chain(a.bytes(chain), anchorsPtr, C.uintptr_t(len(anchors)),
		cKey(&a, expectedNode), &out)); err != nil {
		return nil, fmt.Errorf("aster_attestation_verify_chain: %w", err)
	}
	defer C.aster_verified_chain_release(&out)
	v := &VerifiedChain{Depth: uint32(out.depth)}
	copy(v.Node[:], C.GoBytes(unsafe.Pointer(&out.node[0]), 32))
	copy(v.Anchor[:], C.GoBytes(unsafe.Pointer(&out.anchor[0]), 32))
	if out.intermediates != nil && out.intermediates_count > 0 {
		raw := C.GoBytes(unsafe.Pointer(out.intermediates), C.int(32*out.intermediates_count))
		for i := 0; i < int(out.intermediates_count); i++ {
			var k Key
			copy(k[:], raw[32*i:])
			v.Intermediates = append(v.Intermediates, k)
		}
	}
	return v, nil
}
