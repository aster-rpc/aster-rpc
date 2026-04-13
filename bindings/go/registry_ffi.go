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
	"context"
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

// ─── Async doc-backed ops (event kinds 80-84) ──────────────────────────

// bytesC builds a C.struct_iroh_bytes_t from a Go []byte. The empty case
// (len=0) returns a zero struct with a nil ptr; the FFI side checks the
// length first and ignores the pointer when len is 0.
func bytesC(b []byte) C.struct_iroh_bytes_t {
	if len(b) == 0 {
		return C.struct_iroh_bytes_t{}
	}
	return C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&b[0])),
		len: C.uintptr_t(len(b)),
	}
}

// ResolveAsync runs the full Rust resolve pipeline (pointer lookup,
// list_leases, monotonic seq filter, mandatory filters, rank) for the
// given options against the given registry doc. Returns the winning
// lease, or nil if no candidate survived.
func (d *Doc) ResolveAsync(ctx context.Context, opts ResolveOptionsJSON) (*EndpointLease, error) {
	optsJSON, err := json.Marshal(opts)
	if err != nil {
		return nil, fmt.Errorf("marshal opts: %w", err)
	}
	var opID C.iroh_operation_t
	r := C.aster_registry_resolve(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.handle),
		bytesC(optsJSON),
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_registry_resolve: %w", Error(r))
	}
	ev, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("resolve poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_REGISTRY_RESOLVED {
		return nil, fmt.Errorf("resolve: unexpected event %d", ev.Kind)
	}
	if ev.Status == IROH_STATUS_NOT_FOUND {
		return nil, nil
	}
	if ev.DataPtr == nil || ev.DataLen == 0 {
		return nil, nil
	}
	payload := C.GoBytes(ev.DataPtr, C.int(ev.DataLen))
	var lease EndpointLease
	if err := json.Unmarshal(payload, &lease); err != nil {
		return nil, fmt.Errorf("decode lease: %w", err)
	}
	return &lease, nil
}

// PublishAsync publishes a lease and/or an artifact in a single op.
// Either may be nil to skip; at least one must be supplied. When
// publishing an artifact, service and version are required. topic is
// optional gossip topic to broadcast on (nil to skip).
func (d *Doc) PublishAsync(
	ctx context.Context,
	authorID string,
	lease *EndpointLease,
	artifact *ArtifactRef,
	service string,
	version int32,
	channel string,
	topic *GossipTopic,
) error {
	if lease == nil && artifact == nil {
		return fmt.Errorf("PublishAsync requires at least one of lease or artifact")
	}
	var leaseBytes, artifactBytes []byte
	if lease != nil {
		b, err := json.Marshal(lease)
		if err != nil {
			return fmt.Errorf("marshal lease: %w", err)
		}
		leaseBytes = b
	}
	if artifact != nil {
		b, err := json.Marshal(artifact)
		if err != nil {
			return fmt.Errorf("marshal artifact: %w", err)
		}
		artifactBytes = b
	}
	authorBytes := []byte(authorID)
	serviceBytes := []byte(service)
	channelBytes := []byte(channel)
	var topicHandle uint64
	if topic != nil {
		topicHandle = topic.handle
	}

	var opID C.iroh_operation_t
	r := C.aster_registry_publish(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.handle),
		bytesC(authorBytes),
		bytesC(leaseBytes),
		bytesC(artifactBytes),
		bytesC(serviceBytes),
		C.int32_t(version),
		bytesC(channelBytes),
		C.uint64_t(topicHandle),
		0,
		&opID,
	)
	if r != 0 {
		return fmt.Errorf("aster_registry_publish: %w", Error(r))
	}
	ev, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return fmt.Errorf("publish poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_REGISTRY_PUBLISHED {
		return fmt.Errorf("publish: unexpected event %d", ev.Kind)
	}
	return nil
}

// RenewLeaseAsync renews an existing lease in place: bumps lease_seq +
// timestamps, updates health/load, rewrites the row. Pass math.NaN() for
// load to leave it unset.
func (d *Doc) RenewLeaseAsync(
	ctx context.Context,
	authorID, service, contractID, endpointID, health string,
	load float32,
	leaseDurationS int32,
	topic *GossipTopic,
) error {
	authorBytes := []byte(authorID)
	serviceBytes := []byte(service)
	contractBytes := []byte(contractID)
	endpointBytes := []byte(endpointID)
	healthBytes := []byte(health)
	var topicHandle uint64
	if topic != nil {
		topicHandle = topic.handle
	}

	var opID C.iroh_operation_t
	r := C.aster_registry_renew_lease(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.handle),
		bytesC(authorBytes),
		bytesC(serviceBytes),
		bytesC(contractBytes),
		bytesC(endpointBytes),
		bytesC(healthBytes),
		C.float(load),
		C.int32_t(leaseDurationS),
		C.uint64_t(topicHandle),
		0,
		&opID,
	)
	if r != 0 {
		return fmt.Errorf("aster_registry_renew_lease: %w", Error(r))
	}
	ev, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return fmt.Errorf("renew poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_REGISTRY_RENEWED {
		return fmt.Errorf("renew_lease: unexpected event %d", ev.Kind)
	}
	return nil
}

// AclAddWriterAsync adds an author to the per-doc registry ACL writer
// set, persisting the updated list under _aster/acl/writers.
func (d *Doc) AclAddWriterAsync(ctx context.Context, authorID, writerID string) error {
	return d.aclMutateWriter(ctx, authorID, writerID, true)
}

// AclRemoveWriterAsync removes an author from the per-doc registry ACL
// writer set and persists the updated list.
func (d *Doc) AclRemoveWriterAsync(ctx context.Context, authorID, writerID string) error {
	return d.aclMutateWriter(ctx, authorID, writerID, false)
}

func (d *Doc) aclMutateWriter(ctx context.Context, authorID, writerID string, add bool) error {
	authorBytes := []byte(authorID)
	writerBytes := []byte(writerID)
	var opID C.iroh_operation_t
	var r C.int32_t
	if add {
		r = C.aster_registry_acl_add_writer(
			C.uint64_t(d.runtime.handle),
			C.uint64_t(d.handle),
			bytesC(authorBytes),
			bytesC(writerBytes),
			0,
			&opID,
		)
	} else {
		r = C.aster_registry_acl_remove_writer(
			C.uint64_t(d.runtime.handle),
			C.uint64_t(d.handle),
			bytesC(authorBytes),
			bytesC(writerBytes),
			0,
			&opID,
		)
	}
	if r != 0 {
		op := "aster_registry_acl_add_writer"
		if !add {
			op = "aster_registry_acl_remove_writer"
		}
		return fmt.Errorf("%s: %w", op, Error(r))
	}
	ev, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return fmt.Errorf("acl mutate poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_REGISTRY_ACL_UPDATED {
		return fmt.Errorf("acl mutate: unexpected event %d", ev.Kind)
	}
	return nil
}

// AclListWritersAsync lists the current writer set for the per-doc
// registry ACL. Returns an empty slice when the ACL is in open mode.
func (d *Doc) AclListWritersAsync(ctx context.Context) ([]string, error) {
	var opID C.iroh_operation_t
	r := C.aster_registry_acl_list_writers(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.handle),
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("aster_registry_acl_list_writers: %w", Error(r))
	}
	ev, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("acl list poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_REGISTRY_ACL_LISTED {
		return nil, fmt.Errorf("acl list: unexpected event %d", ev.Kind)
	}
	if ev.DataPtr == nil || ev.DataLen == 0 {
		return []string{}, nil
	}
	payload := C.GoBytes(ev.DataPtr, C.int(ev.DataLen))
	var writers []string
	if err := json.Unmarshal(payload, &writers); err != nil {
		return nil, fmt.Errorf("decode writers: %w", err)
	}
	return writers, nil
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
