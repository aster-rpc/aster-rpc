//go:build cgo

package aster

/*
#include <stdint.h>
#include <stddef.h>

#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"encoding/binary"
	"fmt"
	"unsafe"
)

// AuthorID is a 32-byte author key encoded as hexadecimal.
type AuthorID string

// DocID is a document identifier encoded as hexadecimal.
type DocID string

// QueryMode selects a document query.
type QueryMode uint32

const (
	QUERY_MODE_KEY_EXACT     QueryMode = C.ASTER_DOC_QUERY_KEY_EXACT
	QUERY_MODE_KEY_PREFIX    QueryMode = C.ASTER_DOC_QUERY_KEY_PREFIX
	QUERY_MODE_LATEST_EXACT  QueryMode = C.ASTER_DOC_QUERY_LATEST_EXACT
	QUERY_MODE_LATEST_PREFIX QueryMode = C.ASTER_DOC_QUERY_LATEST_PREFIX
)

// Document share modes accepted by Share and ShareWithAddr.
const (
	DOC_SHARE_READ  uint32 = C.ASTER_DOC_SHARE_READ
	DOC_SHARE_WRITE uint32 = C.ASTER_DOC_SHARE_WRITE
)

// DocEventType identifies a live document event.
type DocEventType uint32

const (
	DOC_EVENT_INSERT_LOCAL    DocEventType = 0
	DOC_EVENT_INSERT_REMOTE   DocEventType = 1
	DOC_EVENT_CONTENT_READY   DocEventType = 2
	DOC_EVENT_PENDING_CONTENT DocEventType = 3
	DOC_EVENT_NEIGHBOR_UP     DocEventType = 4
	DOC_EVENT_NEIGHBOR_DOWN   DocEventType = 5
	DOC_EVENT_SYNC_FINISHED   DocEventType = 6
)

// DocEntry contains document metadata or an exact read's value.
// Query populates all metadata fields and leaves Value nil. GetExact populates
// Key, Author, ContentLen and Value; its ABI does not return ContentHash or Timestamp.
type DocEntry struct {
	Key         string
	Author      AuthorID
	ContentHash BlobID
	ContentLen  uint64
	// Timestamp is the document entry timestamp in microseconds since the Unix epoch.
	Timestamp uint64
	Value     []byte
}

// Doc represents a document handle.
type Doc struct {
	runtime *Runtime
	handle  uint64
}

// Docs provides document operations for a node; obtain it through Node.Docs().
type Docs struct {
	node    *Node
	runtime *Runtime
}

// Node returns the parent node.
func (d *Docs) Node() *Node { return d.node }

// Create creates a new document.
func (d *Docs) Create(ctx context.Context) (*Doc, error) {
	ev, err := startOp(ctx, d.runtime, "aster_docs_create", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_docs_create(C.uint64_t(d.runtime.handle), C.uint64_t(d.node.handle), 0, op)
	})
	if err != nil {
		return nil, err
	}
	if _, err := expectDocEvent(d.runtime, ev, ASTER_EVENT_DOC_CREATED, "create"); err != nil {
		return nil, err
	}
	return &Doc{runtime: d.runtime, handle: ev.Handle}, nil
}

// CreateAuthor creates a new author.
func (d *Docs) CreateAuthor(ctx context.Context) (AuthorID, error) {
	ev, err := startOp(ctx, d.runtime, "aster_docs_create_author", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_docs_create_author(C.uint64_t(d.runtime.handle), C.uint64_t(d.node.handle), 0, op)
	})
	if err != nil {
		return "", err
	}
	p, err := expectDocEvent(d.runtime, ev, ASTER_EVENT_AUTHOR_CREATED, "create author")
	return AuthorID(p), err
}

// Join joins a document from a ticket.
func (d *Docs) Join(ctx context.Context, ticket string) (*Doc, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_docs_join", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_docs_join(C.uint64_t(d.runtime.handle), C.uint64_t(d.node.handle), a.str(ticket), 0, op)
	})
	if err != nil {
		return nil, err
	}
	if _, err := expectDocEvent(d.runtime, ev, ASTER_EVENT_DOC_JOINED, "join"); err != nil {
		return nil, err
	}
	return &Doc{runtime: d.runtime, handle: ev.Handle}, nil
}

// JoinAndSubscribe joins a document. Use JoinAndSubscribeWithEvents to retain
// the atomic subscription and receive events.
// Deprecated: use Join or JoinAndSubscribeWithEvents.
func (d *Docs) JoinAndSubscribe(ctx context.Context, ticket string) (*Doc, error) {
	doc, events, err := d.JoinAndSubscribeWithEvents(ctx, ticket)
	if err != nil {
		return nil, err
	}
	if err := events.Close(); err != nil {
		_ = doc.Close()
		return nil, err
	}
	return doc, nil
}

// Close closes this document and frees its handle.
func (d *Doc) Close() error {
	return wrapCall("aster_doc_free", func() C.int32_t { return C.aster_doc_free(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle)) })
}

// ID returns this document's namespace identifier.
func (d *Doc) ID() (DocID, error) {
	var buf [64]byte
	n := C.uintptr_t(len(buf))
	if err := wrapCall("aster_doc_id", func() C.int32_t {
		return C.aster_doc_id(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle), (*C.uint8_t)(unsafe.Pointer(&buf[0])), &n)
	}); err != nil {
		return "", err
	}
	if n != C.uintptr_t(len(buf)) {
		return "", fmt.Errorf("doc id: invalid length %d", n)
	}
	return DocID(buf[:]), nil
}

// SetBytes sets content in the document.
func (d *Doc) SetBytes(ctx context.Context, author AuthorID, key string, value []byte) error {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_doc_set_bytes", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_set_bytes(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle), a.str(string(author)), a.str(key), a.bytes(value), 0, op)
	})
	if err != nil {
		return err
	}
	_, err = expectDocEvent(d.runtime, ev, ASTER_EVENT_DOC_SET, "set bytes")
	return err
}

// GetExact reads an author's exact key, returning nil when it does not exist.
// The native payload is the raw value, including empty and arbitrary binary values.
// ContentHash and Timestamp are unavailable here; use Query for metadata.
func (d *Doc) GetExact(ctx context.Context, author AuthorID, key string) (*DocEntry, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_doc_get_exact", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_get_exact(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle), a.str(string(author)), a.str(key), 0, op)
	})
	if err != nil {
		return nil, err
	}
	if ev.Kind == ASTER_EVENT_DOC_GET && ev.Status == ASTER_STATUS_NOT_FOUND {
		d.runtime.releaseEventBuffer(ev)
		return nil, nil
	}
	p, err := expectDocEvent(d.runtime, ev, ASTER_EVENT_DOC_GET, "get exact")
	if err != nil {
		return nil, err
	}
	return &DocEntry{Key: key, Author: author, ContentLen: uint64(len(p)), Value: nonNil(p)}, nil
}

// Query returns metadata without reading content blobs. limit caps KEY_EXACT,
// KEY_PREFIX and LATEST_PREFIX queries (0 means unbounded).
func (d *Doc) Query(ctx context.Context, mode QueryMode, key string, limit uint64) ([]DocEntry, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_doc_query", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_query(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle), C.uint32_t(mode), a.str(key), C.uint64_t(limit), 0, op)
	})
	if err != nil {
		return nil, err
	}
	p, err := expectDocEvent(d.runtime, ev, ASTER_EVENT_DOC_QUERY, "query")
	if err != nil {
		return nil, err
	}
	entries, err := parseDocEntryList(p)
	if err != nil {
		return nil, err
	}
	if uint64(len(entries)) != uint64(ev.Flags) {
		return nil, fmt.Errorf("query: entry count %d differs from payload count %d", ev.Flags, len(entries))
	}
	return entries, nil
}

// ReadEntryContent reads a content blob identified by Query metadata.
func (d *Doc) ReadEntryContent(ctx context.Context, contentHash BlobID) ([]byte, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_doc_read_entry_content", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_read_entry_content(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle), a.str(string(contentHash)), 0, op)
	})
	if err != nil {
		return nil, err
	}
	p, err := expectDocEvent(d.runtime, ev, ASTER_EVENT_BLOB_READ, "read entry content")
	return nonNil(p), err
}

// Del deletes this author's entries whose keys start with prefix and returns
// the number removed. An empty prefix deletes all of the author's entries.
func (d *Doc) Del(ctx context.Context, author AuthorID, prefix string) (uint64, error) {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_doc_del", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_del(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle), a.str(string(author)), a.str(prefix), 0, op)
	})
	if err != nil {
		return 0, err
	}
	p, err := expectDocEvent(d.runtime, ev, uint32(C.ASTER_EVENT_DOC_DEL), "delete")
	if err != nil {
		return 0, err
	}
	if len(p) != 8 {
		return 0, fmt.Errorf("delete: invalid removed count payload length %d", len(p))
	}
	return binary.LittleEndian.Uint64(p), nil
}

// Share shares the document. mode is DOC_SHARE_READ or DOC_SHARE_WRITE.
func (d *Doc) Share(ctx context.Context, mode uint32) (string, error) {
	ev, err := startOp(ctx, d.runtime, "aster_doc_share", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_share(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle), C.uint32_t(mode), 0, op)
	})
	if err != nil {
		return "", err
	}
	p, err := expectDocEvent(d.runtime, ev, ASTER_EVENT_DOC_SHARED, "share")
	return string(p), err
}

// ShareWithAddr shares a ticket including relay and direct address hints.
// mode is DOC_SHARE_READ or DOC_SHARE_WRITE.
func (d *Doc) ShareWithAddr(ctx context.Context, mode uint32) (string, error) {
	ev, err := startOp(ctx, d.runtime, "aster_doc_share_with_addr", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_share_with_addr(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle), C.uint32_t(mode), 0, op)
	})
	if err != nil {
		return "", err
	}
	p, err := expectDocEvent(d.runtime, ev, ASTER_EVENT_DOC_SHARED, "share with addr")
	return string(p), err
}

// StartSync starts syncing with explicit endpoint-ID hex strings. Omitting
// peers preserves the previous behavior of starting with an empty peer list.
// Peer addresses can be supplied through Node.AddNodeAddr before starting sync.
func (d *Doc) StartSync(ctx context.Context, peers ...string) error {
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, d.runtime, "aster_doc_start_sync", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_start_sync(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle), a.list(peers), 0, op)
	})
	if err != nil {
		return err
	}
	_, err = expectDocEvent(d.runtime, ev, ASTER_EVENT_UNIT_RESULT, "start sync")
	return err
}

// Leave stops syncing the document.
func (d *Doc) Leave(ctx context.Context) error {
	ev, err := startOp(ctx, d.runtime, "aster_doc_leave", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_leave(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle), 0, op)
	})
	if err != nil {
		return err
	}
	_, err = expectDocEvent(d.runtime, ev, ASTER_EVENT_UNIT_RESULT, "leave")
	return err
}

// expectDocEvent checks both kind and status and releases every completion buffer.
func expectDocEvent(rt *Runtime, ev *Event, kind uint32, what string) ([]byte, error) {
	if ev.Kind != kind || ev.Status != ASTER_STATUS_OK {
		return nil, rt.eventError(ev, what)
	}
	return rt.takePayload(ev), nil
}

// parseDocEntryList decodes the ABI's concatenated metadata records:
// u32-length-prefixed author, key and hash, then u64 content length and timestamp.
// Value bytes are stored in blobs and never appear in this payload.
func parseDocEntryList(data []byte) ([]DocEntry, error) {
	entries := make([]DocEntry, 0)
	for len(data) > 0 {
		var entry DocEntry
		next := func(field string) ([]byte, error) {
			if len(data) < 4 {
				return nil, fmt.Errorf("doc entry: truncated %s length", field)
			}
			n := uint64(binary.LittleEndian.Uint32(data))
			data = data[4:]
			if n > uint64(len(data)) {
				return nil, fmt.Errorf("doc entry: truncated %s", field)
			}
			p := data[:int(n)]
			data = data[int(n):]
			return p, nil
		}
		author, err := next("author")
		if err != nil {
			return nil, err
		}
		entry.Author = AuthorID(author)
		key, err := next("key")
		if err != nil {
			return nil, err
		}
		entry.Key = string(key)
		hash, err := next("content hash")
		if err != nil {
			return nil, err
		}
		entry.ContentHash = BlobID(hash)
		if len(data) < 16 {
			return nil, fmt.Errorf("doc entry: truncated content length or timestamp")
		}
		entry.ContentLen = binary.LittleEndian.Uint64(data[:8])
		entry.Timestamp = binary.LittleEndian.Uint64(data[8:16])
		data = data[16:]
		entries = append(entries, entry)
	}
	return entries, nil
}
