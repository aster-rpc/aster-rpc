//go:build cgo

package aster

/*
#include <stdint.h>
#include <string.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"encoding/binary"
	"encoding/hex"
	"fmt"
)

// Transfer metrics (C ABI 7.4): node-wide blob counters, per-provider fetch
// figures (fetch report v2) and per-peer traffic in the topology view.

// BlobTransferCounts are finished blob transfers by encoding and result. A
// fetch transfer is one provider attempt; a serve transfer is one request a
// peer made of this node.
type BlobTransferCounts struct {
	PlainOK     uint64
	PlainFailed uint64
	ZstdOK      uint64
	ZstdFailed  uint64
}

// BlobTransferStats are a node's blob transfer counters since it started
// (Rust aster::BlobTransferStats). Monotonic. Payload is blob content as
// decoded, whatever the encoding: compression coverage is
// *ZstdPayloadBytes / *PayloadBytes, its saving *PlainBytes - *WireBytes.
type BlobTransferStats struct {
	// FetchedPayloadBytes this node fetched, plain and zstd.
	FetchedPayloadBytes uint64
	// FetchedZstdPayloadBytes is the part that came over zstd.
	FetchedZstdPayloadBytes uint64
	// FetchedWireBytes are compressed bytes received while fetching over zstd.
	FetchedWireBytes uint64
	// FetchedPlainBytes are the protocol bytes those decompressed to.
	FetchedPlainBytes uint64
	// ServedPayloadBytes this node served, plain and zstd.
	ServedPayloadBytes uint64
	// ServedZstdPayloadBytes is the part that went out over zstd.
	ServedZstdPayloadBytes uint64
	// ServedWireBytes are compressed bytes sent while serving over zstd.
	ServedWireBytes uint64
	// ServedPlainBytes are the protocol bytes those were compressed from.
	ServedPlainBytes uint64
	// PlainFallbacks are fetch attempts that went plain because the provider
	// does not serve zstd.
	PlainFallbacks uint64
	// PlainServes are requests served over plain transport.
	PlainServes uint64
	// FetchTransfers / ServeTransfers count transfers by encoding and result.
	FetchTransfers BlobTransferCounts
	ServeTransfers BlobTransferCounts
	// FetchDecodeErrors / ServeDecodeErrors count zstd streams that failed on
	// malformed or truncated zstd data.
	FetchDecodeErrors uint64
	ServeDecodeErrors uint64
}

// TransferStats returns the node's blob transfer counters
// (aster_blobs_transfer_stats, C ABI 7.4).
func (b *Blobs) TransferStats() (BlobTransferStats, error) {
	rt := b.runtime.retain()
	defer rt.release()
	var s C.aster_blob_transfer_stats_t
	s.struct_size = C.uint32_t(C.sizeof_aster_blob_transfer_stats_t)
	if err := wrapCall("aster_blobs_transfer_stats", func() C.int32_t {
		return C.aster_blobs_transfer_stats(C.uint64_t(rt.handle), C.uint64_t(b.node.handle), &s)
	}); err != nil {
		return BlobTransferStats{}, err
	}
	return BlobTransferStats{
		FetchedPayloadBytes:     uint64(s.fetched_payload_bytes),
		FetchedZstdPayloadBytes: uint64(s.fetched_zstd_payload_bytes),
		FetchedWireBytes:        uint64(s.fetched_wire_bytes),
		FetchedPlainBytes:       uint64(s.fetched_plain_bytes),
		ServedPayloadBytes:      uint64(s.served_payload_bytes),
		ServedZstdPayloadBytes:  uint64(s.served_zstd_payload_bytes),
		ServedWireBytes:         uint64(s.served_wire_bytes),
		ServedPlainBytes:        uint64(s.served_plain_bytes),
		PlainFallbacks:          uint64(s.plain_fallbacks),
		PlainServes:             uint64(s.plain_serves),
		FetchTransfers: BlobTransferCounts{
			PlainOK: uint64(s.fetch_plain_ok), PlainFailed: uint64(s.fetch_plain_failed),
			ZstdOK: uint64(s.fetch_zstd_ok), ZstdFailed: uint64(s.fetch_zstd_failed),
		},
		ServeTransfers: BlobTransferCounts{
			PlainOK: uint64(s.serve_plain_ok), PlainFailed: uint64(s.serve_plain_failed),
			ZstdOK: uint64(s.serve_zstd_ok), ZstdFailed: uint64(s.serve_zstd_failed),
		},
		FetchDecodeErrors: uint64(s.fetch_decode_errors),
		ServeDecodeErrors: uint64(s.serve_decode_errors),
	}, nil
}

// fetchReportFlags are the download flags every reporting fetch sends: the
// caller's ASTER_FETCH_TO_STORE choice, always with fetch report v2.
func fetchReportFlags(toStore bool) C.uint32_t {
	flags := C.uint32_t(C.ASTER_FETCH_REPORT_V2)
	if toStore {
		flags |= C.ASTER_FETCH_TO_STORE
	}
	return flags
}

// parseFetchReport reads a fetch report (v1 or v2) and returns the bytes
// after it.
func parseFetchReport(p []byte) (FetchReport, []byte, error) {
	const header = int(C.ASTER_FETCH_REPORT_HEADER_SIZE)
	if len(p) < header {
		return FetchReport{}, nil, fmt.Errorf("fetch report: bad header: %w", ErrInternal)
	}
	var record int
	switch p[0] {
	case C.ASTER_FETCH_REPORT_VERSION:
		record = int(C.ASTER_FETCH_PROVIDER_RECORD_SIZE)
	case C.ASTER_FETCH_REPORT_VERSION_2:
		record = int(C.ASTER_FETCH_PROVIDER_RECORD_V2_SIZE)
	default:
		return FetchReport{}, nil, fmt.Errorf("fetch report: version %d: %w", p[0], ErrInternal)
	}
	n := int(binary.LittleEndian.Uint32(p[4:8]))
	r := FetchReport{BytesTransferred: binary.LittleEndian.Uint64(p[8:16])}
	if n < 0 || n > (len(p)-header)/record {
		return FetchReport{}, nil, fmt.Errorf("fetch report: truncated: %w", ErrInternal)
	}
	for i := 0; i < n; i++ {
		rec := p[header+i*record : header+(i+1)*record]
		pr := ProviderReport{
			NodeID:            hex.EncodeToString(rec[:32]),
			Attempts:          binary.LittleEndian.Uint32(rec[32:36]),
			Failures:          binary.LittleEndian.Uint32(rec[36:40]),
			CompletedRequests: binary.LittleEndian.Uint32(rec[40:44]),
		}
		if p[0] == C.ASTER_FETCH_REPORT_VERSION_2 {
			pr.InFlight = binary.LittleEndian.Uint32(rec[44:48])
			pr.PlainFallbacks = binary.LittleEndian.Uint32(rec[48:52])
			pr.BytesTransferred = binary.LittleEndian.Uint64(rec[56:64])
			if binary.LittleEndian.Uint32(rec[52:56])&C.ASTER_FETCH_PROVIDER_HAS_WIRE_BYTES != 0 {
				wire := binary.LittleEndian.Uint64(rec[64:72])
				pr.WireBytes = &wire
			}
		}
		r.Providers = append(r.Providers, pr)
	}
	return r, p[header+n*record:], nil
}

// TopologyQuicBytes are QUIC-level bytes on a peer's connections (UDP
// datagram payload: headers, ACKs and retransmits included).
type TopologyQuicBytes struct {
	Sent     uint64 `json:"quic_bytes_sent"`
	Received uint64 `json:"quic_bytes_received"`
}

// TopologyQuicTraffic is a peer's QUIC bytes by protocol family, cumulative
// since the node started and across reconnects (C ABI 7.4).
type TopologyQuicTraffic struct {
	Blobs     TopologyQuicBytes `json:"blobs"`
	BlobsZstd TopologyQuicBytes `json:"blobs-zstd"`
	Docs      TopologyQuicBytes `json:"docs"`
	Gossip    TopologyQuicBytes `json:"gossip"`
	RPC       TopologyQuicBytes `json:"rpc"`
	Other     TopologyQuicBytes `json:"other"`
}

// Total sums every protocol family.
func (t TopologyQuicTraffic) Total() TopologyQuicBytes {
	var sum TopologyQuicBytes
	for _, b := range []TopologyQuicBytes{t.Blobs, t.BlobsZstd, t.Docs, t.Gossip, t.RPC, t.Other} {
		sum.Sent += b.Sent
		sum.Received += b.Received
	}
	return sum
}

// TopologyPeerBlobStats are the exact blob payload (and zstd wire) bytes
// exchanged with one peer, cumulative like TopologyQuicTraffic (C ABI 7.4).
type TopologyPeerBlobStats struct {
	FetchedPayloadBytes     uint64 `json:"fetched_payload_bytes"`
	FetchedZstdPayloadBytes uint64 `json:"fetched_zstd_payload_bytes"`
	FetchedWireBytes        uint64 `json:"fetched_wire_bytes"`
	FetchedPlainBytes       uint64 `json:"fetched_plain_bytes"`
	ServedPayloadBytes      uint64 `json:"served_payload_bytes"`
	ServedZstdPayloadBytes  uint64 `json:"served_zstd_payload_bytes"`
	ServedWireBytes         uint64 `json:"served_wire_bytes"`
	ServedPlainBytes        uint64 `json:"served_plain_bytes"`
	PlainFallbacks          uint64 `json:"plain_fallbacks"`
}

// setMetricsNative copies the [metrics] settings into n's library storage
// (aster_config_set_metrics, C ABI 7.4). The caller releases n.
func (c *AsterConfig) setMetricsNative(n *C.aster_config_t) error {
	if !c.MetricsPeerLabels && c.MetricsPeerLabelCap == 0 {
		return nil
	}
	var m C.aster_metrics_config_t
	m.struct_size = C.uint32_t(C.sizeof_aster_metrics_config_t)
	m.peer_labels = boolToUint32(c.MetricsPeerLabels)
	m.peer_label_cap = C.uint32_t(c.MetricsPeerLabelCap)
	if err := ffiStatus(func() C.int32_t { return C.aster_config_set_metrics(n, &m) }); err != nil {
		return fmt.Errorf("aster_config_set_metrics: %w", err)
	}
	return nil
}

// metricsFromNative reads the [metrics] settings of n (aster_config_metrics).
func (c *AsterConfig) metricsFromNative(n *C.aster_config_t) {
	var m C.aster_metrics_config_t
	m.struct_size = C.uint32_t(C.sizeof_aster_metrics_config_t)
	if C.aster_config_metrics(n, &m) != C.ASTER_STATUS_OK {
		return
	}
	c.MetricsPeerLabels = m.peer_labels != 0
	c.MetricsPeerLabelCap = uint32(m.peer_label_cap)
}
