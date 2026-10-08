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
	"context"
	"encoding/binary"
	"encoding/hex"
	"fmt"
	"unsafe"
)

// ABI 3.3 data plane: blob fetch strategies and reports, ranged reads, tag
// prefix delete, GC; docs authority / retire / open-import / durable writes /
// entry status; connection paths and stream priority.

// startOp runs an async FFI call and waits for its completion event.
func startOp(ctx context.Context, rt *Runtime, what string, call func(op *C.aster_operation_t) C.int32_t) (*Event, error) {
	var op C.aster_operation_t
	if err := wrapStatus(what, call(&op)); err != nil {
		return nil, err
	}
	ev, err := rt.await(ctx, uint64(op))
	if err != nil {
		return nil, fmt.Errorf("%s: %w", what, err)
	}
	return ev, nil
}

// expect checks the completion kind, returning the payload (buffer released).
func expect(rt *Runtime, ev *Event, kind uint32, what string) ([]byte, error) {
	if ev.Kind != kind {
		return nil, rt.eventError(ev, what)
	}
	return rt.takePayload(ev), nil
}

// FetchStrategy selects how a multi-provider fetch splits work.
type FetchStrategy uint32

const (
	FetchWhole FetchStrategy = C.ASTER_FETCH_STRATEGY_WHOLE
	// FetchSplitChildren needs BLOB_FORMAT_HASH_SEQ; provider order applies per child.
	FetchSplitChildren FetchStrategy = C.ASTER_FETCH_STRATEGY_SPLIT_CHILDREN
)

// ProviderReport is one provider's record in a FetchReport.
type ProviderReport struct {
	NodeID            string
	Attempts          uint32
	Failures          uint32
	CompletedRequests uint32
}

// FetchReport describes a multi-provider fetch (fetch report v1).
type FetchReport struct {
	BytesTransferred uint64
	Providers        []ProviderReport
}

func parseFetchReport(p []byte) (FetchReport, []byte, error) {
	const header, record = int(C.ASTER_FETCH_REPORT_HEADER_SIZE), int(C.ASTER_FETCH_PROVIDER_RECORD_SIZE)
	if len(p) < header || p[0] != C.ASTER_FETCH_REPORT_VERSION {
		return FetchReport{}, nil, fmt.Errorf("fetch report: bad header: %w", ErrInternal)
	}
	n := int(binary.LittleEndian.Uint32(p[4:8]))
	r := FetchReport{BytesTransferred: binary.LittleEndian.Uint64(p[8:16])}
	if len(p) < header+n*record {
		return FetchReport{}, nil, fmt.Errorf("fetch report: truncated: %w", ErrInternal)
	}
	for i := 0; i < n; i++ {
		rec := p[header+i*record:]
		r.Providers = append(r.Providers, ProviderReport{
			NodeID:            hex.EncodeToString(rec[:32]),
			Attempts:          binary.LittleEndian.Uint32(rec[32:36]),
			Failures:          binary.LittleEndian.Uint32(rec[36:40]),
			CompletedRequests: binary.LittleEndian.Uint32(rec[40:44]),
		})
	}
	return r, p[header+n*record:], nil
}

// DownloadHashFrom fetches a blob from an ordered provider list (tried in
// order, failing over), returning the blob bytes and the fetch report. With
// toStore the blob is kept in the store and no bytes are returned.
func (b *Blobs) DownloadHashFrom(ctx context.Context, hash string, providers []string, format BlobFormat,
	strategy FetchStrategy, toStore bool) ([]byte, FetchReport, error) {
	var a cArena
	defer a.free()
	var flags C.uint32_t
	if toStore {
		flags = C.ASTER_FETCH_TO_STORE
	}
	ev, err := startOp(ctx, b.runtime, "aster_blobs_download_hash_from", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_blobs_download_hash_from(C.uint64_t(b.runtime.handle), C.uint64_t(b.node.handle),
			a.str(hash), a.list(providers), C.uint32_t(format), C.uint32_t(strategy), flags, 0, op)
	})
	if err != nil {
		return nil, FetchReport{}, err
	}
	p, err := expect(b.runtime, ev, ASTER_EVENT_BLOB_FETCHED, "aster_blobs_download_hash_from")
	if err != nil {
		return nil, FetchReport{}, err
	}
	report, data, err := parseFetchReport(p)
	if err != nil {
		return nil, FetchReport{}, err
	}
	return nonNil(data), report, nil
}

// DownloadHash fetches a blob by hash from one provider (64-hex node id).
// With toStore no bytes are returned.
func (b *Blobs) DownloadHash(ctx context.Context, hash, provider string, format BlobFormat, toStore bool) ([]byte, error) {
	var a cArena
	defer a.free()
	var flags C.uint32_t
	if toStore {
		flags = C.ASTER_FETCH_TO_STORE
	}
	ev, err := startOp(ctx, b.runtime, "aster_blobs_download_hash", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_blobs_download_hash(C.uint64_t(b.runtime.handle), C.uint64_t(b.node.handle),
			a.str(hash), a.str(provider), C.uint32_t(format), flags, 0, op)
	})
	if err != nil {
		return nil, err
	}
	p, err := expect(b.runtime, ev, ASTER_EVENT_BLOB_DOWNLOADED, "aster_blobs_download_hash")
	return nonNil(p), err
}

// ReadRange reads length bytes of a local blob from offset.
func (b *Blobs) ReadRange(ctx context.Context, hash string, offset, length uint64) ([]byte, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, b.runtime, "aster_blobs_read_range", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_blobs_read_range(C.uint64_t(b.runtime.handle), C.uint64_t(b.node.handle),
			a.str(hash), C.uint64_t(offset), C.uint64_t(length), 0, op)
	})
	if err != nil {
		return nil, err
	}
	p, err := expect(b.runtime, ev, ASTER_EVENT_BLOB_READ, "aster_blobs_read_range")
	return nonNil(p), err
}

// GCRunOnce runs one node-wide blob GC pass.
func (b *Blobs) GCRunOnce(ctx context.Context) error {
	ev, err := startOp(ctx, b.runtime, "aster_blobs_gc_run_once", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_blobs_gc_run_once(C.uint64_t(b.runtime.handle), C.uint64_t(b.node.handle), 0, op)
	})
	if err != nil {
		return err
	}
	_, err = expect(b.runtime, ev, ASTER_EVENT_UNIT_RESULT, "aster_blobs_gc_run_once")
	return err
}

// DeletePrefix deletes every tag starting with prefix ("" = all tags) and
// returns how many were deleted.
func (t *Tags) DeletePrefix(ctx context.Context, prefix string) (uint64, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, t.runtime, "aster_tags_delete_prefix", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_tags_delete_prefix(C.uint64_t(t.runtime.handle), C.uint64_t(t.node.handle), a.str(prefix), 0, op)
	})
	if err != nil {
		return 0, err
	}
	p, err := expect(t.runtime, ev, ASTER_EVENT_TAG_DELETED, "aster_tags_delete_prefix")
	if err != nil {
		return 0, err
	}
	if len(p) >= 8 {
		return binary.LittleEndian.Uint64(p), nil
	}
	return uint64(ev.Flags), nil
}

// ─── Docs ───────────────────────────────────────────────────────────────────

// AuthorityReport is the local authority a node holds over a namespace.
type AuthorityReport struct {
	Access      LocalAccess
	NamespaceID string
	// Fingerprint identifies (namespace, access) without revealing the secret.
	Fingerprint Key
}

func parseAuthority(rt *Runtime, ev *Event, what string) (AuthorityReport, error) {
	p, err := expect(rt, ev, ASTER_EVENT_DOC_AUTHORITY_REPORT, what)
	if err != nil {
		return AuthorityReport{}, err
	}
	if len(p) < 64 {
		return AuthorityReport{}, fmt.Errorf("%s: short payload: %w", what, ErrInternal)
	}
	r := AuthorityReport{Access: LocalAccess(ev.Flags), NamespaceID: hex.EncodeToString(p[:32])}
	copy(r.Fingerprint[:], p[32:64])
	return r, nil
}

// AuthorityReport reports the node's authority over namespaceID; an absent
// namespace is an error.
func (d *Docs) AuthorityReport(ctx context.Context, namespaceID string) (AuthorityReport, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_docs_authority_report", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_docs_authority_report(C.uint64_t(d.runtime.handle), C.uint64_t(d.node.handle), a.str(namespaceID), 0, op)
	})
	if err != nil {
		return AuthorityReport{}, err
	}
	return parseAuthority(d.runtime, ev, "aster_docs_authority_report")
}

// AuthorityFingerprint is aster::AuthorityFingerprint::of(namespace, access).
func AuthorityFingerprint(namespaceID string, access LocalAccess) (Key, error) {
	var a cArena
	defer a.free()
	out, get := cOutKey(&a)
	if err := wrapStatus("aster_doc_authority_fingerprint",
		C.aster_doc_authority_fingerprint(a.str(namespaceID), C.uint32_t(access), out)); err != nil {
		return Key{}, err
	}
	return get(), nil
}

// RetireOutcome is the result of RetireLocalWriteAuthority.
type RetireOutcome struct {
	// Retired is false when the namespace is still open elsewhere (BUSY).
	Retired        bool
	Access         LocalAccess
	OpenReferences uint64
}

// RetireLocalWriteAuthority drops the local write secret of a namespace
// (keeping read access). BUSY is reported, not an error.
func (d *Docs) RetireLocalWriteAuthority(ctx context.Context, namespaceID string) (RetireOutcome, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_docs_retire_local_write_authority", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_docs_retire_local_write_authority(C.uint64_t(d.runtime.handle), C.uint64_t(d.node.handle), a.str(namespaceID), 0, op)
	})
	if err != nil {
		return RetireOutcome{}, err
	}
	p, err := expect(d.runtime, ev, ASTER_EVENT_DOC_RETIRE_OUTCOME, "aster_docs_retire_local_write_authority")
	if err != nil {
		return RetireOutcome{}, err
	}
	out := RetireOutcome{Retired: ev.Flags == C.ASTER_RETIRE_RETIRED}
	if len(p) >= 16 {
		out.Access = LocalAccess(binary.LittleEndian.Uint32(p[0:4]))
		out.OpenReferences = binary.LittleEndian.Uint64(p[8:16])
	}
	return out, nil
}

func (d *Docs) opened(ev *Event, what string) (*Doc, error) {
	if ev.Kind != ASTER_EVENT_DOC_OPENED {
		return nil, d.runtime.eventError(ev, what)
	}
	d.runtime.releaseEventBuffer(ev)
	if ev.Status == C.ASTER_STATUS_NOT_FOUND || ev.Handle == 0 {
		return nil, fmt.Errorf("%s: namespace not found: %w", what, ErrNotFound)
	}
	return &Doc{runtime: d.runtime, handle: ev.Handle}, nil
}

// Open opens an existing namespace; an unknown one is ErrNotFound.
func (d *Docs) Open(ctx context.Context, namespaceID string) (*Doc, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_docs_open", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_docs_open(C.uint64_t(d.runtime.handle), C.uint64_t(d.node.handle), a.str(namespaceID), 0, op)
	})
	if err != nil {
		return nil, err
	}
	return d.opened(ev, "aster_docs_open")
}

// ImportWriteNamespace opens or imports a namespace from its 32-byte secret.
func (d *Docs) ImportWriteNamespace(ctx context.Context, secret Key) (*Doc, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_docs_import_write_namespace", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_docs_import_write_namespace(C.uint64_t(d.runtime.handle), C.uint64_t(d.node.handle), a.bytes(secret[:]), 0, op)
	})
	if err != nil {
		return nil, err
	}
	return d.opened(ev, "aster_docs_import_write_namespace")
}

// ImportReadNamespace opens or imports a namespace read-only from its id.
func (d *Docs) ImportReadNamespace(ctx context.Context, namespaceID string) (*Doc, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_docs_import_read_namespace", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_docs_import_read_namespace(C.uint64_t(d.runtime.handle), C.uint64_t(d.node.handle), a.str(namespaceID), 0, op)
	})
	if err != nil {
		return nil, err
	}
	return d.opened(ev, "aster_docs_import_read_namespace")
}

// Handle returns the native doc handle.
func (d *Doc) Handle() uint64 { return d.handle }

// AuthorityReport reports the node's authority over this doc's namespace.
func (d *Doc) AuthorityReport(ctx context.Context, node *Node) (AuthorityReport, error) {
	ev, err := startOp(ctx, d.runtime, "aster_doc_authority_report", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_authority_report(C.uint64_t(d.runtime.handle), C.uint64_t(node.handle), C.uint64_t(d.handle), 0, op)
	})
	if err != nil {
		return AuthorityReport{}, err
	}
	return parseAuthority(d.runtime, ev, "aster_doc_authority_report")
}

// SetBytesDurable commits value blob metadata before publishing the document
// record; returns the content hash. After a hard process kill, a surviving record
// cannot name missing locally-authored content. It does not guarantee document
// metadata persistence or survival of sudden machine power loss.
func (d *Doc) SetBytesDurable(ctx context.Context, author AuthorID, key string, value []byte) (BlobID, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_doc_set_bytes_durable", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_set_bytes_durable(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle),
			a.str(string(author)), a.str(key), a.bytes(value), 0, op)
	})
	if err != nil {
		return "", err
	}
	p, err := expect(d.runtime, ev, ASTER_EVENT_DOC_SET, "aster_doc_set_bytes_durable")
	return BlobID(p), err
}

// EntryExists reports whether (author, key) has an entry (tombstones count).
func (d *Doc) EntryExists(ctx context.Context, author AuthorID, key string) (bool, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_doc_entry_exists", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_entry_exists(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle),
			a.str(string(author)), a.str(key), 0, op)
	})
	if err != nil {
		return false, err
	}
	if _, err := expect(d.runtime, ev, ASTER_EVENT_DOC_ENTRY_EXISTS, "aster_doc_entry_exists"); err != nil {
		return false, err
	}
	return ev.Flags != 0, nil
}

// EntryContentStatus reports whether an entry's content is local, and its size.
func (d *Doc) EntryContentStatus(ctx context.Context, contentHash BlobID) (BlobStatus, uint64, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_doc_entry_content_status", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_entry_content_status(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle),
			a.str(string(contentHash)), 0, op)
	})
	if err != nil {
		return 0, 0, err
	}
	p, err := expect(d.runtime, ev, ASTER_EVENT_DOC_CONTENT_STATUS, "aster_doc_entry_content_status")
	if err != nil {
		return 0, 0, err
	}
	var size uint64
	if len(p) >= 8 {
		size = binary.LittleEndian.Uint64(p)
	}
	return BlobStatus(ev.Flags), size, nil
}

// ShutdownForRoleSwap leaves and closes the doc under one deadline; the
// handle is freed immediately.
func (d *Doc) ShutdownForRoleSwap(ctx context.Context, budgetMs uint64) error {
	ev, err := startOp(ctx, d.runtime, "aster_doc_shutdown_for_role_swap", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_shutdown_for_role_swap(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle), C.uint64_t(budgetMs), 0, op)
	})
	if err != nil {
		return err
	}
	_, err = expect(d.runtime, ev, ASTER_EVENT_UNIT_RESULT, "aster_doc_shutdown_for_role_swap")
	return err
}

// ─── Net ────────────────────────────────────────────────────────────────────

// PathKind is a transport path kind.
type PathKind uint32

const (
	PathNone   PathKind = C.ASTER_PATH_KIND_NONE
	PathDirect PathKind = C.ASTER_PATH_KIND_DIRECT
	PathRelay  PathKind = C.ASTER_PATH_KIND_RELAY
	PathOther  PathKind = C.ASTER_PATH_KIND_OTHER
)

// PathInfo describes one transport path of a connection.
type PathInfo struct {
	Kind       PathKind
	IsSelected bool
	PeerHost   string
	PeerPort   uint16
	RelayURL   string
	Other      string
	// RTTMicros is the measured round-trip time, nil if unmeasured.
	RTTMicros *uint64
}

func readPaths(l *C.aster_path_list_t) []PathInfo {
	defer C.aster_path_list_release(l)
	if l.items == nil || l.len == 0 {
		return nil
	}
	items := unsafe.Slice(l.items, int(l.len))
	out := make([]PathInfo, len(items))
	for i, it := range items {
		p := PathInfo{
			Kind: PathKind(it.path_kind), IsSelected: it.is_selected != 0,
			PeerHost: string(viewBytes(it.peer_host)), PeerPort: uint16(it.peer_port),
			RelayURL: string(viewBytes(it.relay_url)), Other: string(viewBytes(it.other)),
		}
		if it.rtt_micros != C.uint64_t(^uint64(0)) {
			v := uint64(it.rtt_micros)
			p.RTTMicros = &v
		}
		out[i] = p
	}
	return out
}

// Paths lists the connection's transport paths.
func (c *Connection) Paths() ([]PathInfo, error) {
	var l C.aster_path_list_t
	l.struct_size = C.uint32_t(unsafe.Sizeof(l))
	if err := wrapStatus("aster_connection_paths", C.aster_connection_paths(C.uint64_t(c.runtime.handle), C.uint64_t(c.handle), &l)); err != nil {
		return nil, err
	}
	return readPaths(&l), nil
}

// SelectedPath returns the currently selected path (false if none).
func (c *Connection) SelectedPath() (PathInfo, bool, error) {
	var l C.aster_path_list_t
	l.struct_size = C.uint32_t(unsafe.Sizeof(l))
	if err := wrapStatus("aster_connection_selected_path", C.aster_connection_selected_path(C.uint64_t(c.runtime.handle), C.uint64_t(c.handle), &l)); err != nil {
		return PathInfo{}, false, err
	}
	paths := readPaths(&l)
	if len(paths) == 0 {
		return PathInfo{}, false, nil
	}
	return paths[0], true, nil
}

// SetPriority sets the stream's send priority (higher is sent first).
func (s *SendStream) SetPriority(ctx context.Context, priority int32) error {
	ev, err := startOp(ctx, s.runtime, "aster_send_stream_set_priority", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_send_stream_set_priority(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), C.int32_t(priority), 0, op)
	})
	if err != nil {
		return err
	}
	_, err = expect(s.runtime, ev, ASTER_EVENT_UNIT_RESULT, "aster_send_stream_set_priority")
	return err
}

// Priority returns the stream's send priority.
func (s *SendStream) Priority(ctx context.Context) (int32, error) {
	ev, err := startOp(ctx, s.runtime, "aster_send_stream_priority", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_send_stream_priority(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), 0, op)
	})
	if err != nil {
		return 0, err
	}
	p, err := expect(s.runtime, ev, ASTER_EVENT_STREAM_PRIORITY, "aster_send_stream_priority")
	if err != nil {
		return 0, err
	}
	if len(p) < 4 {
		return 0, fmt.Errorf("aster_send_stream_priority: short payload: %w", ErrInternal)
	}
	return int32(binary.LittleEndian.Uint32(p)), nil
}
