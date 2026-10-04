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
	"encoding/hex"
	"encoding/json"
	"fmt"
)

// ContractID represents a contract identifier as a hex string.
type ContractID string

// CanonicalJSON provides JSON normalization utilities.
type CanonicalJSON struct{}

// Normalize parses JSON, sorts all keys recursively, and re-serializes in compact form.
func (CanonicalJSON) Normalize(jsonStr string) (string, error) {
	var a cArena
	defer a.free()
	in := a.str(jsonStr)
	out, err := callWithBuffer(len(jsonStr)+64, func(buf *C.uint8_t, n *C.uintptr_t) C.int32_t {
		return C.aster_canonical_json(in.ptr, in.len, buf, n)
	})
	if err != nil {
		return "", fmt.Errorf("aster_canonical_json: %w", err)
	}
	return string(out), nil
}

// ComputeSigningBytes computes canonical signing bytes from credential JSON.
func ComputeSigningBytes(credentialJSON string) ([]byte, error) {
	var a cArena
	defer a.free()
	in := a.str(credentialJSON)
	out, err := callWithBuffer(1024, func(buf *C.uint8_t, n *C.uintptr_t) C.int32_t {
		return C.aster_signing_bytes(in.ptr, in.len, buf, n)
	})
	if err != nil {
		return nil, fmt.Errorf("aster_signing_bytes: %w", err)
	}
	return out, nil
}

// ComputeContractID computes a contract id (64 hex) from a ServiceContract
// JSON string (aster_contract_id; canonicalisation and BLAKE3 run in Rust).
func ComputeContractID(contractJSON string) (ContractID, error) {
	var a cArena
	defer a.free()
	in := a.str(contractJSON)
	out, err := callWithBuffer(128, func(buf *C.uint8_t, n *C.uintptr_t) C.int32_t {
		return C.aster_contract_id(in.ptr, in.len, buf, n)
	})
	if err != nil {
		return "", fmt.Errorf("aster_contract_id: %w", err)
	}
	return ContractID(out), nil
}

// ComputeCanonicalBytes returns the canonical bytes of a "ServiceContract",
// "TypeDef" or "MethodDef" given as JSON (aster_canonical_bytes).
func ComputeCanonicalBytes(typeName, jsonStr string) ([]byte, error) {
	var a cArena
	defer a.free()
	tn := a.str(typeName)
	in := a.str(jsonStr)
	out, err := callWithBuffer(4096, func(buf *C.uint8_t, n *C.uintptr_t) C.int32_t {
		return C.aster_canonical_bytes(tn.ptr, tn.len, in.ptr, in.len, buf, n)
	})
	if err != nil {
		return nil, fmt.Errorf("aster_canonical_bytes(%s): %w", typeName, err)
	}
	return out, nil
}

// Blake3Hex is the BLAKE3 hash of data as 64 lowercase hex (aster_blake3_hex).
func Blake3Hex(data []byte) (string, error) {
	var a cArena
	defer a.free()
	in := a.bytes(data)
	out, err := callWithBuffer(64, func(buf *C.uint8_t, n *C.uintptr_t) C.int32_t {
		return C.aster_blake3_hex(in.ptr, in.len, buf, n)
	})
	if err != nil {
		return "", fmt.Errorf("aster_blake3_hex: %w", err)
	}
	return string(out), nil
}

// TicketContent represents the decoded content of an AsterTicket.
type TicketContent struct {
	EndpointID     string
	RelayAddr      string
	DirectAddrs    []string
	CredentialType string
	// CredentialData is the raw credential payload.
	CredentialData string
}

// EncodeTicket encodes ticket components to an "aster1<base58>" string
// (aster_ticket_encode).
func EncodeTicket(content TicketContent) (string, error) {
	if content.EndpointID == "" {
		return "", fmt.Errorf("encode ticket: empty endpoint id: %w", ErrInvalidArgument)
	}
	var a cArena
	defer a.free()
	id := a.str(content.EndpointID)
	relay := a.str(content.RelayAddr)
	var direct C.struct_aster_bytes_t
	if len(content.DirectAddrs) > 0 {
		js, err := json.Marshal(content.DirectAddrs)
		if err != nil {
			return "", err
		}
		direct = a.bytes(js)
	}
	ctype := a.str(content.CredentialType)
	cdata := a.str(content.CredentialData)

	out, err := callWithBuffer(1024, func(buf *C.uint8_t, n *C.uintptr_t) C.int32_t {
		return C.aster_ticket_encode(id.ptr, id.len, relay.ptr, relay.len, direct.ptr, direct.len,
			ctype.ptr, ctype.len, cdata.ptr, cdata.len, buf, n)
	})
	if err != nil {
		return "", fmt.Errorf("aster_ticket_encode: %w", err)
	}
	return string(out), nil
}

// DecodeTicket decodes an "aster1<base58>" ticket (aster_ticket_decode).
func DecodeTicket(ticket string) (*TicketContent, error) {
	if ticket == "" {
		return nil, fmt.Errorf("decode ticket: empty ticket: %w", ErrInvalidArgument)
	}
	var a cArena
	defer a.free()
	in := a.str(ticket)
	out, err := callWithBuffer(4096, func(buf *C.uint8_t, n *C.uintptr_t) C.int32_t {
		return C.aster_ticket_decode(in.ptr, in.len, buf, n)
	})
	if err != nil {
		return nil, fmt.Errorf("aster_ticket_decode: %w", err)
	}
	return parseTicketJSON(string(out))
}

// parseTicketJSON parses the JSON written by aster_ticket_decode:
// {"endpoint_id": "...", "relay_addr": "ip:port" | null, "direct_addrs": [...],
// "credential_type": "..." | null, "credential_data_hex": "..." | null}.
// A raw "credential_data" string is accepted as well.
func parseTicketJSON(jsonStr string) (*TicketContent, error) {
	var raw struct {
		EndpointID        string   `json:"endpoint_id"`
		RelayAddr         *string  `json:"relay_addr"`
		DirectAddrs       []string `json:"direct_addrs"`
		CredentialType    *string  `json:"credential_type"`
		CredentialDataHex *string  `json:"credential_data_hex"`
		CredentialData    *string  `json:"credential_data"`
	}
	if err := json.Unmarshal([]byte(jsonStr), &raw); err != nil {
		return nil, fmt.Errorf("parse ticket json: %w", err)
	}
	content := &TicketContent{EndpointID: raw.EndpointID, DirectAddrs: raw.DirectAddrs}
	if raw.RelayAddr != nil {
		content.RelayAddr = *raw.RelayAddr
	}
	if raw.CredentialType != nil {
		content.CredentialType = *raw.CredentialType
	}
	switch {
	case raw.CredentialDataHex != nil:
		b, err := hex.DecodeString(*raw.CredentialDataHex)
		if err != nil {
			return nil, fmt.Errorf("parse ticket json: credential_data_hex: %w", err)
		}
		content.CredentialData = string(b)
	case raw.CredentialData != nil:
		content.CredentialData = *raw.CredentialData
	}
	return content, nil
}

// callWithBuffer calls a C function that writes into (buf, *len) and returns
// BUFFER_TOO_SMALL with the required size in *len, growing the buffer once.
func callWithBuffer(initial int, call func(buf *C.uint8_t, n *C.uintptr_t) C.int32_t) ([]byte, error) {
	size := initial
	for attempt := 0; attempt < 2; attempt++ {
		buf := C.malloc(C.size_t(size))
		n := C.uintptr_t(size)
		r := call((*C.uint8_t)(buf), &n)
		if r == C.ASTER_STATUS_OK {
			out := C.GoBytes(buf, C.int(n))
			C.free(buf)
			return out, nil
		}
		C.free(buf)
		if r == C.ASTER_STATUS_BUFFER_TOO_SMALL && int(n) > size {
			size = int(n)
			continue
		}
		return nil, statusError(r)
	}
	return nil, statusError(C.ASTER_STATUS_BUFFER_TOO_SMALL)
}
