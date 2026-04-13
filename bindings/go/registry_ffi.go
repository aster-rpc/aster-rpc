//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>

#include "iroh_ffi.h"
*/
import "C"

import (
	"encoding/json"
	"fmt"
	"unsafe"
)

// NowEpochMsFromRust returns the shared wall-clock reading used across all bindings
// for lease freshness checks.
func NowEpochMsFromRust() int64 {
	return int64(C.aster_registry_now_epoch_ms())
}

// IsFreshRust asks the Rust registry layer whether a lease is still fresh.
// Returns true if fresh, false if expired. Returns an error on invalid input.
func IsFreshRust(lease *EndpointLease, leaseDurationS int32) (bool, error) {
	if lease == nil {
		return false, fmt.Errorf("lease is nil")
	}
	js, err := lease.ToJSON()
	if err != nil {
		return false, fmt.Errorf("marshal lease: %w", err)
	}
	bs := []byte(js)
	var ptr *C.uint8_t
	if len(bs) > 0 {
		ptr = (*C.uint8_t)(unsafe.Pointer(&bs[0]))
	}
	result := C.aster_registry_is_fresh(ptr, C.uintptr_t(len(bs)), C.int32_t(leaseDurationS))
	if result < 0 {
		return false, fmt.Errorf("aster_registry_is_fresh failed: %d", int(result))
	}
	return result == 1, nil
}

// IsRoutableRust reports whether the health status (read from a lease) is READY or DEGRADED.
// Delegates to Rust so all bindings apply the same rule.
func IsRoutableRust(status string) bool {
	bs := []byte(status)
	var ptr *C.uint8_t
	if len(bs) > 0 {
		ptr = (*C.uint8_t)(unsafe.Pointer(&bs[0]))
	}
	return C.aster_registry_is_routable(ptr, C.uintptr_t(len(bs))) == 1
}

// ResolveOptionsJSON is the Go mirror of Rust ResolveOptions passed through the FFI as JSON.
type ResolveOptionsJSON struct {
	Service                  string   `json:"service"`
	Version                  *int32   `json:"version,omitempty"`
	Channel                  *string  `json:"channel,omitempty"`
	ContractID               *string  `json:"contract_id,omitempty"`
	Strategy                 string   `json:"strategy"`
	CallerAlpn               string   `json:"caller_alpn"`
	CallerSerializationModes []string `json:"caller_serialization_modes"`
	CallerPolicyRealm        *string  `json:"caller_policy_realm,omitempty"`
	LeaseDurationS           int32    `json:"lease_duration_s"`
}

// DefaultResolveOptions returns sane defaults matching the Rust side.
func DefaultResolveOptions(service string) ResolveOptionsJSON {
	return ResolveOptionsJSON{
		Service:                  service,
		Strategy:                 "round_robin",
		CallerAlpn:               "aster/1",
		CallerSerializationModes: []string{"fory-xlang"},
		LeaseDurationS:           45,
	}
}

// FilterAndRankRust applies the §11.9 mandatory filters and ranking strategy to a list
// of leases via the Rust FFI. Returns the ranked survivors in best-first order; the top
// element is the resolved winner. An empty slice means no candidate passed.
func FilterAndRankRust(
	leases []*EndpointLease, opts ResolveOptionsJSON,
) ([]*EndpointLease, error) {
	leasesJSON, err := json.Marshal(leases)
	if err != nil {
		return nil, fmt.Errorf("marshal leases: %w", err)
	}
	optsJSON, err := json.Marshal(opts)
	if err != nil {
		return nil, fmt.Errorf("marshal opts: %w", err)
	}

	var leasesPtr *C.uint8_t
	if len(leasesJSON) > 0 {
		leasesPtr = (*C.uint8_t)(unsafe.Pointer(&leasesJSON[0]))
	}
	var optsPtr *C.uint8_t
	if len(optsJSON) > 0 {
		optsPtr = (*C.uint8_t)(unsafe.Pointer(&optsJSON[0]))
	}

	// Start with a reasonable buffer and grow on BUFFER_TOO_SMALL.
	bufCap := C.uintptr_t(16 * 1024)
	for attempt := 0; attempt < 2; attempt++ {
		buf := make([]byte, bufCap)
		outLen := bufCap
		status := C.aster_registry_filter_and_rank(
			leasesPtr, C.uintptr_t(len(leasesJSON)),
			optsPtr, C.uintptr_t(len(optsJSON)),
			(*C.uint8_t)(unsafe.Pointer(&buf[0])),
			&outLen,
		)
		if status == 0 {
			written := int(outLen)
			var out []*EndpointLease
			if err := json.Unmarshal(buf[:written], &out); err != nil {
				return nil, fmt.Errorf("unmarshal ranked leases: %w", err)
			}
			return out, nil
		}
		// BUFFER_TOO_SMALL (outLen was set to required size) → retry once.
		if outLen > bufCap {
			bufCap = outLen
			continue
		}
		return nil, fmt.Errorf("aster_registry_filter_and_rank failed: %d", int(status))
	}
	return nil, fmt.Errorf("aster_registry_filter_and_rank: buffer grow failed")
}

// RegistryKeyRust returns a registry doc key produced by the shared Rust key-schema helpers.
// Prefer using the pure-Go helpers (ContractKey, LeaseKey, ...) for hot paths; this exists
// so tests can pin Go key generation against the authoritative Rust source of truth.
func RegistryKeyRust(kind int32, a1, a2, a3 string) ([]byte, error) {
	b1, b2, b3 := []byte(a1), []byte(a2), []byte(a3)
	var p1, p2, p3 *C.uint8_t
	if len(b1) > 0 {
		p1 = (*C.uint8_t)(unsafe.Pointer(&b1[0]))
	}
	if len(b2) > 0 {
		p2 = (*C.uint8_t)(unsafe.Pointer(&b2[0]))
	}
	if len(b3) > 0 {
		p3 = (*C.uint8_t)(unsafe.Pointer(&b3[0]))
	}
	buf := make([]byte, 512)
	outLen := C.uintptr_t(len(buf))
	status := C.aster_registry_key(
		C.int32_t(kind),
		p1, C.uintptr_t(len(b1)),
		p2, C.uintptr_t(len(b2)),
		p3, C.uintptr_t(len(b3)),
		(*C.uint8_t)(unsafe.Pointer(&buf[0])),
		&outLen,
	)
	if status != 0 {
		return nil, fmt.Errorf("aster_registry_key failed: %d", int(status))
	}
	return buf[:int(outLen)], nil
}
