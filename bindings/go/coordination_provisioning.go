//go:build cgo

package aster

/*
#include "aster_ffi.h"
*/
import "C"

import (
	"encoding/hex"
	"encoding/json"
	"fmt"
)

// CoordinationVoterSeed contains a hex32 node ID and its native canonical ticket.
type CoordinationVoterSeed struct {
	NodeID string `json:"node_id"`
	Ticket string `json:"ticket"`
}

// CoordinationGenesisDocument is the version-1 provisioning input. IDs, hashes
// and nonces are hex32 strings. Caller ordering is preserved; no list is sorted.
// Encoding alone does not establish validity or authorization.
type CoordinationGenesisDocument struct {
	GroupID               string                  `json:"group_id"`
	AuthorityDomain       string                  `json:"authority_domain"`
	GenesisNonce          string                  `json:"genesis_nonce"`
	DeploymentProfileCode int32                   `json:"deployment_profile_code"`
	InitialVoters         []CoordinationVoterSeed `json:"initial_voters"`
	InitialAdmins         []string                `json:"initial_admins"`
	RecoveryExportHash    *string                 `json:"recovery_export_hash"`
}

// CoordinationJoinPermitDocument contains hex32 identifiers/nonces and an
// explicit domain. Native code derives the candidate ticket and version.
type CoordinationJoinPermitDocument struct {
	GroupID          string   `json:"group_id"`
	AuthorityDomain  string   `json:"authority_domain"`
	GenesisHash      string   `json:"genesis_hash"`
	CandidateNodeID  string   `json:"candidate_node_id"`
	BootstrapSenders []string `json:"bootstrap_senders"`
	JoinNonce        string   `json:"join_nonce"`
}

// CoordinationVerifiedGenesis is copied metadata returned by native pinned-key
// verification. It is not a native authority seal and creates no group.
type CoordinationVerifiedGenesis struct {
	DocumentBytes         []byte                  `json:"-"`
	GenesisHash           string                  `json:"genesis_hash"`
	GroupID               string                  `json:"group_id"`
	AuthorityPublicKey    string                  `json:"authority_public_key"`
	AuthorityDomain       string                  `json:"authority_domain"`
	DeploymentProfileCode int32                   `json:"deployment_profile_code"`
	InitialVoters         []CoordinationVoterSeed `json:"initial_voters"`
	InitialAdmins         []string                `json:"initial_admins"`
	RecoveryExportHash    *string                 `json:"recovery_export_hash"`
}

// CoordinationVerifiedJoinPermit contains copied verified metadata. Verification
// pins the configured key/domain/candidate, but extracts group/genesis rather
// than comparing independent expectations. It does not grant membership.
type CoordinationVerifiedJoinPermit struct {
	DocumentBytes    []byte   `json:"-"`
	GroupID          string   `json:"group_id"`
	GenesisHash      string   `json:"genesis_hash"`
	CandidateNodeID  string   `json:"candidate_node_id"`
	BootstrapSenders []string `json:"bootstrap_senders"`
}

// CoordinationProvisioningError exposes a stable native Code and diagnostic
// Detail. Detail is not a stable machine-readable protocol. Unwrap yields status.
type CoordinationProvisioningError struct {
	Code   string `json:"code"`
	Detail string `json:"detail"`
	status error
}

func (e *CoordinationProvisioningError) Error() string {
	return fmt.Sprintf("coordination provisioning %s: %s", e.Code, e.Detail)
}
func (e *CoordinationProvisioningError) Unwrap() error { return e.status }

func coordinationProvisioning(call func(*C.aster_provisioning_result_t) C.int32_t) ([]byte, error) {
	var out C.aster_provisioning_result_t
	defer func() {
		if out.data.ptr != nil {
			C.aster_string_release(out.data.ptr, out.data.len)
		}
		if out.error.ptr != nil {
			C.aster_string_release(out.error.ptr, out.error.len)
		}
	}()
	status, statusMsg := ffiCall(func() C.int32_t { return call(&out) })
	if err := wrapStatusMsg("coordination provisioning", status, statusMsg); err != nil {
		var result CoordinationProvisioningError
		if decodeErr := json.Unmarshal(viewBytes(out.error), &result); decodeErr != nil {
			return nil, fmt.Errorf("%w (invalid native provisioning error: %v)", err, decodeErr)
		}
		result.status = err
		return nil, &result
	}
	return viewBytes(out.data), nil
}

func CoordinationCanonicalGenesis(document CoordinationGenesisDocument) ([]byte, error) {
	// JSON arrays remain arrays even when the caller supplies a nil slice.
	if document.InitialVoters == nil {
		document.InitialVoters = []CoordinationVoterSeed{}
	}
	if document.InitialAdmins == nil {
		document.InitialAdmins = []string{}
	}
	input, err := json.Marshal(document)
	if err != nil {
		return nil, err
	}
	var a cArena
	defer a.free()
	return coordinationProvisioning(func(out *C.aster_provisioning_result_t) C.int32_t {
		return C.aster_coordination_canonical_genesis(a.bytes(input), out)
	})
}
func CoordinationCanonicalJoinPermit(document CoordinationJoinPermitDocument) ([]byte, error) {
	if document.BootstrapSenders == nil {
		document.BootstrapSenders = []string{}
	}
	input, err := json.Marshal(document)
	if err != nil {
		return nil, err
	}
	var a cArena
	defer a.free()
	return coordinationProvisioning(func(out *C.aster_provisioning_result_t) C.int32_t {
		return C.aster_coordination_canonical_join_permit(a.bytes(input), out)
	})
}

// CoordinationGenesisSigningBytes returns the native preimage for an external
// Ed25519 signer. Opaque input bytes are not verified.
func CoordinationGenesisSigningBytes(document []byte) ([]byte, error) {
	var a cArena
	defer a.free()
	return coordinationProvisioning(func(out *C.aster_provisioning_result_t) C.int32_t {
		return C.aster_coordination_genesis_signing_bytes(a.bytes(document), out)
	})
}

// CoordinationJoinSigningBytes returns the native join preimage for an external signer.
func CoordinationJoinSigningBytes(document []byte) ([]byte, error) {
	var a cArena
	defer a.free()
	return coordinationProvisioning(func(out *C.aster_provisioning_result_t) C.int32_t {
		return C.aster_coordination_join_signing_bytes(a.bytes(document), out)
	})
}

// CoordinationGenesisHash hashes opaque document bytes without asserting authority.
func CoordinationGenesisHash(document []byte) ([]byte, error) {
	var a cArena
	defer a.free()
	return coordinationProvisioning(func(out *C.aster_provisioning_result_t) C.int32_t {
		return C.aster_coordination_genesis_hash(a.bytes(document), out)
	})
}

// CoordinationSignedDocumentFromParts assembles an envelope without verification.
// Keys and signatures are raw 32-byte and 64-byte values, respectively.
func CoordinationSignedDocumentFromParts(document, authorityKey, signature []byte) ([]byte, error) {
	var a cArena
	defer a.free()
	return coordinationProvisioning(func(out *C.aster_provisioning_result_t) C.int32_t {
		return C.aster_coordination_signed_document_from_parts(a.bytes(document), a.bytes(authorityKey), a.bytes(signature), out)
	})
}
func CoordinationCanonicalCandidateTicket(nodeID []byte) (string, error) {
	var a cArena
	defer a.free()
	data, err := coordinationProvisioning(func(out *C.aster_provisioning_result_t) C.int32_t {
		return C.aster_coordination_canonical_candidate_ticket(a.bytes(nodeID), out)
	})
	return string(data), err
}

// CoordinationVerifyGenesis pins an independently configured key and domain;
// never use the key embedded beside a signature as the trust anchor.
func CoordinationVerifyGenesis(authorityKey []byte, domain string, envelope []byte) (*CoordinationVerifiedGenesis, error) {
	var a cArena
	defer a.free()
	data, err := coordinationProvisioning(func(out *C.aster_provisioning_result_t) C.int32_t {
		return C.aster_coordination_verify_genesis(a.bytes(authorityKey), a.str(domain), a.bytes(envelope), out)
	})
	if err != nil {
		return nil, err
	}
	var result CoordinationVerifiedGenesis
	var wire struct {
		DocumentBytes string `json:"document_bytes"`
		*CoordinationVerifiedGenesis
	}
	wire.CoordinationVerifiedGenesis = &result
	if err := json.Unmarshal(data, &wire); err != nil {
		return nil, err
	}
	result.DocumentBytes, err = hex.DecodeString(wire.DocumentBytes)
	if err != nil {
		return nil, err
	}
	return &result, nil
}
func CoordinationVerifyJoinPermitForStart(authorityKey []byte, domain string, candidateID, envelope []byte) (*CoordinationVerifiedJoinPermit, error) {
	var a cArena
	defer a.free()
	data, err := coordinationProvisioning(func(out *C.aster_provisioning_result_t) C.int32_t {
		return C.aster_coordination_verify_join_permit_for_start(a.bytes(authorityKey), a.str(domain), a.bytes(candidateID), a.bytes(envelope), out)
	})
	if err != nil {
		return nil, err
	}
	var result CoordinationVerifiedJoinPermit
	var wire struct {
		DocumentBytes string `json:"document_bytes"`
		*CoordinationVerifiedJoinPermit
	}
	wire.CoordinationVerifiedJoinPermit = &result
	if err := json.Unmarshal(data, &wire); err != nil {
		return nil, err
	}
	result.DocumentBytes, err = hex.DecodeString(wire.DocumentBytes)
	if err != nil {
		return nil, err
	}
	return &result, nil
}
