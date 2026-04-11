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
	"context"
	"encoding/binary"
	"fmt"
	"strings"
	"unsafe"
)

// AuthorID is a 32-byte author key for content-addressed documents.
type AuthorID string

// DocID is a document identifier.
type DocID string

// QueryMode for document queries.
type QueryMode uint32

// Query mode constants.
const (
	QUERY_MODE_AUTHOR QueryMode = 0
	QUERY_MODE_ALL    QueryMode = 1
	QUERY_MODE_PREFIX QueryMode = 2
)

// DocEventType for document events.
type DocEventType uint32

// Doc event type constants.
const (
	DOC_EVENT_INSERT_LOCAL       DocEventType = 0
	DOC_EVENT_INSERT_REMOTE      DocEventType = 1
	DOC_EVENT_CONTENT_READY     DocEventType = 2
	DOC_EVENT_PENDING_CONTENT    DocEventType = 3
	DOC_EVENT_NEIGHBOR_UP        DocEventType = 4
	DOC_EVENT_NEIGHBOR_DOWN      DocEventType = 5
	DOC_EVENT_SYNC_FINISHED     DocEventType = 6
)

// DocEntry represents an entry in a document.
type DocEntry struct {
	Key         string
	Author      AuthorID
	ContentHash BlobID
	Value       []byte
}

// Doc represents a document handle.
type Doc struct {
	runtime *Runtime
	handle  uint64
}

// Docs provides document operations for an Iroh node.
// Get an instance via Node.Docs().
type Docs struct {
	node     *Node
	runtime *Runtime
}

// Node returns the parent node.
func (d *Docs) Node() *Node {
	return d.node
}

// Create creates a new document.
func (d *Docs) Create(ctx context.Context) (*Doc, error) {
	var opID C.iroh_operation_t

	r := C.iroh_docs_create(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.node.handle),
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_docs_create: %w", Error(r))
	}

	ev, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("create poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_DOC_CREATED {
		return nil, fmt.Errorf("create: unexpected event %d", ev.Kind)
	}

	return &Doc{runtime: d.runtime, handle: ev.Handle}, nil
}

// CreateAuthor creates a new author.
func (d *Docs) CreateAuthor(ctx context.Context) (AuthorID, error) {
	var opID C.iroh_operation_t

	r := C.iroh_docs_create_author(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.node.handle),
		0,
		&opID,
	)
	if r != 0 {
		return "", fmt.Errorf("iroh_docs_create_author: %w", Error(r))
	}

	ev, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return "", fmt.Errorf("create author poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_AUTHOR_CREATED {
		return "", fmt.Errorf("create author: unexpected event %d", ev.Kind)
	}

	authorHex := C.GoStringN((*C.char)(ev.DataPtr), C.int(ev.DataLen))
	return AuthorID(strings.TrimSpace(authorHex)), nil
}

// Join joins a document from a ticket.
func (d *Docs) Join(ctx context.Context, ticket string) (*Doc, error) {
	var opID C.iroh_operation_t

	ticketBytes := []byte(ticket)
	ticketC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&ticketBytes[0])),
		len: C.uintptr_t(len(ticketBytes)),
	}

	r := C.iroh_docs_join(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.node.handle),
		ticketC,
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_docs_join: %w", Error(r))
	}

	ev, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("join poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_DOC_JOINED {
		return nil, fmt.Errorf("join: unexpected event %d", ev.Kind)
	}

	return &Doc{runtime: d.runtime, handle: ev.Handle}, nil
}

// JoinAndSubscribe joins and subscribes to a document atomically.
func (d *Docs) JoinAndSubscribe(ctx context.Context, ticket string) (*Doc, error) {
	var opID C.iroh_operation_t

	ticketBytes := []byte(ticket)
	ticketC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&ticketBytes[0])),
		len: C.uintptr_t(len(ticketBytes)),
	}

	r := C.iroh_docs_join_and_subscribe(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.node.handle),
		ticketC,
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_docs_join_and_subscribe: %w", Error(r))
	}

	ev, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("join and subscribe poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_DOC_JOINED_AND_SUBSCRIBED {
		return nil, fmt.Errorf("join and subscribe: unexpected event %d", ev.Kind)
	}

	return &Doc{runtime: d.runtime, handle: ev.Handle}, nil
}

// Close closes this document and frees its handle.
func (d *Doc) Close() error {
	r := C.iroh_doc_free(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle))
	if r != 0 {
		return fmt.Errorf("iroh_doc_free: %w", Error(r))
	}
	return nil
}

// SetBytes sets content in the document.
func (d *Doc) SetBytes(ctx context.Context, author AuthorID, key string, value []byte) error {
	var opID C.iroh_operation_t

	authorBytes := []byte(author)
	keyBytes := []byte(key)

	authorC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&authorBytes[0])),
		len: C.uintptr_t(len(authorBytes)),
	}
	keyC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&keyBytes[0])),
		len: C.uintptr_t(len(keyBytes)),
	}
	valueC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&value[0])),
		len: C.uintptr_t(len(value)),
	}

	r := C.iroh_doc_set_bytes(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.handle),
		authorC,
		keyC,
		valueC,
		0,
		&opID,
	)
	if r != 0 {
		return fmt.Errorf("iroh_doc_set_bytes: %w", Error(r))
	}

	ev, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return fmt.Errorf("set bytes poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_DOC_SET {
		return fmt.Errorf("set bytes: unexpected event %d", ev.Kind)
	}

	return nil
}

// GetExact gets an exact entry from the document.
func (d *Doc) GetExact(ctx context.Context, author AuthorID, key string) (*DocEntry, error) {
	var opID C.iroh_operation_t

	authorBytes := []byte(author)
	keyBytes := []byte(key)

	authorC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&authorBytes[0])),
		len: C.uintptr_t(len(authorBytes)),
	}
	keyC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&keyBytes[0])),
		len: C.uintptr_t(len(keyBytes)),
	}

	r := C.iroh_doc_get_exact(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.handle),
		authorC,
		keyC,
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_doc_get_exact: %w", Error(r))
	}

	ev, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("get exact poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_DOC_GET {
		return nil, fmt.Errorf("get exact: unexpected event %d", ev.Kind)
	}
	if ev.Status == IROH_STATUS_NOT_FOUND {
		return nil, nil
	}

	return parseDocEntry(ev.DataPtr, int(ev.DataLen))
}

// Query queries entries from the document.
func (d *Doc) Query(ctx context.Context, mode QueryMode, keyPrefix string) ([]DocEntry, error) {
	var opID C.iroh_operation_t

	keyBytes := []byte(keyPrefix)
	keyC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&keyBytes[0])),
		len: C.uintptr_t(len(keyBytes)),
	}

	r := C.iroh_doc_query(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.handle),
		C.uint32_t(mode),
		keyC,
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_doc_query: %w", Error(r))
	}

	ev, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("query poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_DOC_QUERY {
		return nil, fmt.Errorf("query: unexpected event %d", ev.Kind)
	}

	return parseDocEntryList(ev.DataPtr, int(ev.DataLen))
}

// ReadEntryContent reads entry content from the document.
func (d *Doc) ReadEntryContent(ctx context.Context, contentHash BlobID) ([]byte, error) {
	var opID C.iroh_operation_t

	hashBytes := []byte(contentHash)
	hashC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&hashBytes[0])),
		len: C.uintptr_t(len(hashBytes)),
	}

	r := C.iroh_doc_read_entry_content(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.handle),
		hashC,
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_doc_read_entry_content: %w", Error(r))
	}

	ev, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("read entry content poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_DOC_GET {
		return nil, fmt.Errorf("read entry content: unexpected event %d", ev.Kind)
	}

	data := cDataToGoBytes(ev.DataPtr, ev.DataLen)
	if ev.Buffer != 0 {
		d.runtime.ReleaseBuffer(ev.Buffer)
	}

	return data, nil
}

// Share shares the document.
func (d *Doc) Share(ctx context.Context, mode uint32) (string, error) {
	var opID C.iroh_operation_t

	r := C.iroh_doc_share(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.handle),
		C.uint32_t(mode),
		0,
		&opID,
	)
	if r != 0 {
		return "", fmt.Errorf("iroh_doc_share: %w", Error(r))
	}

	ev, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return "", fmt.Errorf("share poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_DOC_SHARED {
		return "", fmt.Errorf("share: unexpected event %d", ev.Kind)
	}

	ticket := C.GoStringN((*C.char)(ev.DataPtr), C.int(ev.DataLen))
	return strings.TrimSpace(ticket), nil
}

// StartSync starts syncing the document.
func (d *Doc) StartSync(ctx context.Context) error {
	var opID C.iroh_operation_t

	// Empty peers list
	peersC := C.struct_iroh_bytes_list_t{
		items: nil,
		len:   0,
	}

	r := C.iroh_doc_start_sync(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.handle),
		peersC,
		0,
		&opID,
	)
	if r != 0 {
		return fmt.Errorf("iroh_doc_start_sync: %w", Error(r))
	}

	_, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return fmt.Errorf("start sync poll: %w", err)
	}

	return nil
}

// Leave stops syncing the document.
func (d *Doc) Leave(ctx context.Context) error {
	var opID C.iroh_operation_t

	r := C.iroh_doc_leave(
		C.uint64_t(d.runtime.handle),
		C.uint64_t(d.handle),
		0,
		&opID,
	)
	if r != 0 {
		return fmt.Errorf("iroh_doc_leave: %w", Error(r))
	}

	_, err := d.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return fmt.Errorf("leave poll: %w", err)
	}

	return nil
}

// parseDocEntry parses a doc entry from C data.
// Format: author_hex (64) + key_len (4) + key + content_hash_hex (64) + content_len (8) + value
func parseDocEntry(dataPtr unsafe.Pointer, dataLen int) (*DocEntry, error) {
	if dataPtr == nil || dataLen == 0 {
		return nil, fmt.Errorf("nil doc entry data")
	}

	data := C.GoBytes(dataPtr, C.int(dataLen))
	offset := 0

	// Read author (64 hex chars)
	if len(data) < offset+64 {
		return nil, fmt.Errorf("data too short for author")
	}
	author := strings.TrimSpace(string(data[offset : offset+64]))
	offset += 64

	// Read key length
	if len(data) < offset+4 {
		return nil, fmt.Errorf("data too short for key length")
	}
	keyLen := binary.LittleEndian.Uint32(data[offset : offset+4])
	offset += 4

	// Read key
	if len(data) < offset+int(keyLen) {
		return nil, fmt.Errorf("data too short for key")
	}
	key := string(data[offset : offset+int(keyLen)])
	offset += int(keyLen)

	// Read content hash (64 hex chars)
	if len(data) < offset+64 {
		return nil, fmt.Errorf("data too short for content hash")
	}
	contentHash := strings.TrimSpace(string(data[offset : offset+64]))
	offset += 64

	// Read content length
	if len(data) < offset+8 {
		return nil, fmt.Errorf("data too short for content length")
	}
	contentLen := binary.LittleEndian.Uint64(data[offset : offset+8])
	offset += 8

	// Read value
	if len(data) < offset+int(contentLen) {
		return nil, fmt.Errorf("data too short for value")
	}
	value := data[offset : offset+int(contentLen)]

	return &DocEntry{
		Key:         key,
		Author:      AuthorID(author),
		ContentHash: BlobID(contentHash),
		Value:       value,
	}, nil
}

// parseDocEntryList parses multiple doc entries from C data.
func parseDocEntryList(dataPtr unsafe.Pointer, dataLen int) ([]DocEntry, error) {
	if dataPtr == nil || dataLen == 0 {
		return nil, nil
	}

	data := C.GoBytes(dataPtr, C.int(dataLen))
	entries := make([]DocEntry, 0)
	offset := 0

	for offset < len(data) {
		// Check minimum size for header
		if len(data) < offset+72 { // 64 (author) + 4 (key_len) + 64 (hash) + 8 (content_len)
			break
		}

		// Read author
		author := strings.TrimSpace(string(data[offset : offset+64]))
		offset += 64

		// Read key length
		keyLen := binary.LittleEndian.Uint32(data[offset : offset+4])
		offset += 4

		// Check for key
		if len(data) < offset+int(keyLen) {
			break
		}
		key := string(data[offset : offset+int(keyLen)])
		offset += int(keyLen)

		// Read content hash
		if len(data) < offset+64 {
			break
		}
		contentHash := strings.TrimSpace(string(data[offset : offset+64]))
		offset += 64

		// Read content length
		if len(data) < offset+8 {
			break
		}
		contentLen := binary.LittleEndian.Uint64(data[offset : offset+8])
		offset += 8

		// Read value
		if len(data) < offset+int(contentLen) {
			break
		}
		value := data[offset : offset+int(contentLen)]
		offset += int(contentLen)

		entries = append(entries, DocEntry{
			Key:         key,
			Author:      AuthorID(author),
			ContentHash: BlobID(contentHash),
			Value:       value,
		})
	}

	return entries, nil
}
