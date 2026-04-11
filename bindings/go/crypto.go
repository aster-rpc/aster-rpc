//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#include "iroh_ffi.h"
*/
import "C"

import (
	"fmt"
	"strings"
	"unsafe"
)

// ContractID represents a contract identifier as a hex string.
type ContractID string

// CanonicalJSON provides JSON normalization utilities.
type CanonicalJSON struct{}

// Normalize parses JSON, sorts all keys recursively, and re-serializes in compact form.
func (CanonicalJSON) Normalize(jsonStr string) (string, error) {
	jsonBytes := []byte(jsonStr)
	inBuf := C.malloc(C.size_t(len(jsonBytes)))
	C.memcpy(inBuf, unsafe.Pointer(&jsonBytes[0]), C.size_t(len(jsonBytes)))
	defer C.free(inBuf)

	// Output buffer - JSON shouldn't grow significantly on normalization
	outBuf := C.malloc(4096)
	outLen := C.uintptr_t(4096)
	defer C.free(outBuf)

	r := C.aster_canonical_json(
		(*C.uint8_t)(inBuf),
		C.uintptr_t(len(jsonBytes)),
		(*C.uint8_t)(outBuf),
		&outLen,
	)
	if r != 0 {
		return "", fmt.Errorf("aster_canonical_json: %w", Error(r))
	}

	return C.GoStringN((*C.char)(outBuf), C.int(outLen)), nil
}

// SigningBytes computes canonical signing bytes from credential JSON.
func ComputeSigningBytes(credentialJSON string) ([]byte, error) {
	credBytes := []byte(credentialJSON)
	inBuf := C.malloc(C.size_t(len(credBytes)))
	C.memcpy(inBuf, unsafe.Pointer(&credBytes[0]), C.size_t(len(credBytes)))
	defer C.free(inBuf)

	// Output buffer - signing bytes are typically small
	outBuf := C.malloc(256)
	outLen := C.uintptr_t(256)
	defer C.free(outBuf)

	r := C.aster_signing_bytes(
		(*C.uint8_t)(inBuf),
		C.uintptr_t(len(credBytes)),
		(*C.uint8_t)(outBuf),
		&outLen,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_signing_bytes: %w", Error(r))
	}

	return C.GoBytes(outBuf, C.int(outLen)), nil
}

// ContractIDFromJSON computes a contract ID from a ServiceContract JSON string.
func ComputeContractID(contractJSON string) (ContractID, error) {
	contractBytes := []byte(contractJSON)
	inBuf := C.malloc(C.size_t(len(contractBytes)))
	C.memcpy(inBuf, unsafe.Pointer(&contractBytes[0]), C.size_t(len(contractBytes)))
	defer C.free(inBuf)

	// Contract ID is 64 hex chars = 32 bytes
	outBuf := C.malloc(64)
	outLen := C.uintptr_t(64)
	defer C.free(outBuf)

	r := C.aster_contract_id(
		(*C.uint8_t)(inBuf),
		C.uintptr_t(len(contractBytes)),
		(*C.uint8_t)(outBuf),
		&outLen,
	)
	if r != 0 {
		return "", fmt.Errorf("aster_contract_id: %w", Error(r))
	}

	return ContractID(C.GoStringN((*C.char)(outBuf), C.int(outLen))), nil
}

// TicketContent represents the decoded content of an AsterTicket.
type TicketContent struct {
	EndpointID      string
	RelayAddr      string
	DirectAddrs    []string
	CredentialType string
	CredentialData string
}

// EncodeTicket encodes ticket components to an Aster base58 string.
func EncodeTicket(content TicketContent) (string, error) {
	// endpoint_id
	idBytes := []byte(content.EndpointID)
	idBuf := C.malloc(C.size_t(len(idBytes)))
	C.memcpy(idBuf, unsafe.Pointer(&idBytes[0]), C.size_t(len(idBytes)))
	defer C.free(idBuf)

	// relay_addr (can be empty)
	var relayBuf *C.uint8_t
	var relayLen C.uintptr_t
	if content.RelayAddr != "" {
		relayBytes := []byte(content.RelayAddr)
		relayBuf = (*C.uint8_t)(C.malloc(C.size_t(len(relayBytes))))
		C.memcpy(unsafe.Pointer(relayBuf), unsafe.Pointer(&relayBytes[0]), C.size_t(len(relayBytes)))
		relayLen = C.uintptr_t(len(relayBytes))
	}

	// direct_addrs as JSON
	directAddrsJSON := "["
	for i, addr := range content.DirectAddrs {
		if i > 0 {
			directAddrsJSON += ","
		}
		directAddrsJSON += "\"" + addr + "\""
	}
	directAddrsJSON += "]"
	daBytes := []byte(directAddrsJSON)
	daBuf := C.malloc(C.size_t(len(daBytes)))
	C.memcpy(daBuf, unsafe.Pointer(&daBytes[0]), C.size_t(len(daBytes)))
	defer C.free(daBuf)

	// credential_type
	ctBytes := []byte(content.CredentialType)
	ctBuf := C.malloc(C.size_t(len(ctBytes)))
	C.memcpy(ctBuf, unsafe.Pointer(&ctBytes[0]), C.size_t(len(ctBytes)))
	defer C.free(ctBuf)

	// credential_data
	cdBytes := []byte(content.CredentialData)
	cdBuf := C.malloc(C.size_t(len(cdBytes)))
	C.memcpy(cdBuf, unsafe.Pointer(&cdBytes[0]), C.size_t(len(cdBytes)))
	defer C.free(cdBuf)

	// Output buffer - base58 encoded ticket is typically longer than inputs
	outBuf := C.malloc(256)
	outLen := C.uintptr_t(256)
	defer C.free(outBuf)

	r := C.aster_ticket_encode(
		(*C.uint8_t)(idBuf),
		C.uintptr_t(len(idBytes)),
		relayBuf,
		relayLen,
		(*C.uint8_t)(daBuf),
		C.uintptr_t(len(daBytes)),
		(*C.uint8_t)(ctBuf),
		C.uintptr_t(len(ctBytes)),
		(*C.uint8_t)(cdBuf),
		C.uintptr_t(len(cdBytes)),
		(*C.uint8_t)(outBuf),
		&outLen,
	)
	if r != 0 {
		return "", fmt.Errorf("aster_ticket_encode: %w", Error(r))
	}

	return C.GoStringN((*C.char)(outBuf), C.int(outLen)), nil
}

// DecodeTicket decodes an Aster base58 ticket string to its components.
func DecodeTicket(ticket string) (*TicketContent, error) {
	ticketBytes := []byte(ticket)
	inBuf := C.malloc(C.size_t(len(ticketBytes)))
	C.memcpy(inBuf, unsafe.Pointer(&ticketBytes[0]), C.size_t(len(ticketBytes)))
	defer C.free(inBuf)

	// Output JSON buffer
	outBuf := C.malloc(4096)
	outLen := C.uintptr_t(4096)
	defer C.free(outBuf)

	r := C.aster_ticket_decode(
		(*C.uint8_t)(inBuf),
		C.uintptr_t(len(ticketBytes)),
		(*C.uint8_t)(outBuf),
		&outLen,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_ticket_decode: %w", Error(r))
	}

	jsonStr := C.GoStringN((*C.char)(outBuf), C.int(outLen))
	return parseTicketJSON(jsonStr)
}

// parseTicketJSON parses the JSON output of ticket_decode.
// Format: {"endpoint_id": "hex...", "relay_addr": "ip:port" | null, "direct_addrs": [...], "credential_type": "...", "credential_data": "..."}
func parseTicketJSON(jsonStr string) (*TicketContent, error) {
	// Simple JSON parsing without external dependency
	// Format is known, so we can parse it manually

	content := &TicketContent{}

	// Extract endpoint_id
	if idx := strings.Index(jsonStr, `"endpoint_id"`); idx >= 0 {
		start := strings.Index(jsonStr[idx:], `"`) + idx + len(`"endpoint_id"`) + 2
		end := start
		for end < len(jsonStr) && jsonStr[end] != '"' {
			end++
		}
		content.EndpointID = jsonStr[start:end]
	}

	// Extract relay_addr
	if idx := strings.Index(jsonStr, `"relay_addr"`); idx >= 0 {
		// Find the value after the key
		start := strings.Index(jsonStr[idx:], `"`) + idx + len(`"relay_addr"`) + 2
		// Check if null
		if strings.HasPrefix(jsonStr[start:], "null") {
			content.RelayAddr = ""
		} else {
			// Find the closing quote
			end := start + 1
			for end < len(jsonStr) && jsonStr[end] != '"' {
				end++
			}
			content.RelayAddr = jsonStr[start:end]
		}
	}

	// Extract direct_addrs (JSON array)
	if idx := strings.Index(jsonStr, `"direct_addrs"`); idx >= 0 {
		start := strings.Index(jsonStr[idx:], `[`) + idx
		end := strings.Index(jsonStr[start:], `]`) + start + 1
		arrayStr := jsonStr[start:end]
		// Parse array elements
		for {
			elemStart := strings.Index(arrayStr, `"`)
			if elemStart < 0 {
				break
			}
			elemStart++
			elemEnd := strings.Index(arrayStr[elemStart:], `"`) + elemStart
			if elemEnd < elemStart {
				break
			}
			content.DirectAddrs = append(content.DirectAddrs, arrayStr[elemStart:elemEnd])
			arrayStr = arrayStr[elemEnd+1:]
		}
	}

	// Extract credential_type
	if idx := strings.Index(jsonStr, `"credential_type"`); idx >= 0 {
		start := strings.Index(jsonStr[idx:], `"`) + idx + len(`"credential_type"`) + 2
		end := start
		for end < len(jsonStr) && jsonStr[end] != '"' {
			end++
		}
		content.CredentialType = jsonStr[start:end]
	}

	// Extract credential_data
	if idx := strings.Index(jsonStr, `"credential_data"`); idx >= 0 {
		start := strings.Index(jsonStr[idx:], `"`) + idx + len(`"credential_data"`) + 2
		end := start
		for end < len(jsonStr) && jsonStr[end] != '"' {
			end++
		}
		content.CredentialData = jsonStr[start:end]
	}

	return content, nil
}
