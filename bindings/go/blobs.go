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
	"encoding/json"
	"fmt"
	"unsafe"
)

// BlobID is a 32-byte blob identifier, displayed as a 64-character hex string.
type BlobID string

// BlobStatus represents the download status of a blob.
type BlobStatus uint32

// Blob status constants.
const (
	BLOB_STATUS_NOT_FOUND BlobStatus = 0
	BLOB_STATUS_PARTIAL   BlobStatus = 1
	BLOB_STATUS_COMPLETE BlobStatus = 2
)

// String returns a human-readable string for the blob status.
func (s BlobStatus) String() string {
	switch s {
	case BLOB_STATUS_NOT_FOUND:
		return "NOT_FOUND"
	case BLOB_STATUS_PARTIAL:
		return "PARTIAL"
	case BLOB_STATUS_COMPLETE:
		return "COMPLETE"
	default:
		return "UNKNOWN"
	}
}

// BlobFormat represents the format of a blob ticket.
type BlobFormat uint32

// Blob format constants.
const (
	BLOB_FORMAT_RAW     BlobFormat = 0
	BLOB_FORMAT_HASH_SEQ BlobFormat = 1
)

// BlobTicket represents a ticket for sharing or downloading blobs.
type BlobTicket string

// BlobEntry represents an entry in a blob collection.
type BlobEntry struct {
	Name string
	Hash BlobID
	Size int64
}

// BlobCollection represents a collection of named blobs.
type BlobCollection struct {
	Entries []BlobEntry
}

// BlobInfo represents local information about a blob.
type BlobInfo struct {
	Hash   BlobID
	Size   int64
	Status BlobStatus
}

// Blobs provides blob storage operations for an Iroh node.
// Get an instance via Node.Blobs().
type Blobs struct {
	node     *Node
	runtime *Runtime
}

// Node returns the parent node.
func (b *Blobs) Node() *Node {
	return b.node
}

// AddBytes stores bytes in the blob store.
func (b *Blobs) AddBytes(ctx context.Context, data []byte) (BlobID, error) {
	var opID C.iroh_operation_t

	bytes := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&data[0])),
		len: C.uintptr_t(len(data)),
	}

	r := C.iroh_blobs_add_bytes(
		C.uint64_t(b.runtime.handle),
		C.uint64_t(b.node.handle),
		bytes,
		0,
		&opID,
	)
	if r != 0 {
		return "", fmt.Errorf("iroh_blobs_add_bytes: %w", Error(r))
	}

	ev, err := b.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return "", fmt.Errorf("add bytes poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_BLOB_ADDED {
		return "", fmt.Errorf("add bytes: unexpected event %d", ev.Kind)
	}

	hashHex := C.GoStringN((*C.char)(ev.DataPtr), C.int(ev.DataLen))
	return BlobID(hashHex), nil
}

// AddBytesAsCollection stores bytes as a named entry in a collection.
func (b *Blobs) AddBytesAsCollection(ctx context.Context, data []byte, name string) (BlobID, error) {
	var opID C.iroh_operation_t

	nameBytes := []byte(name)
	dataBytes := data

	bytes := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&dataBytes[0])),
		len: C.uintptr_t(len(dataBytes)),
	}
	nameC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&nameBytes[0])),
		len: C.uintptr_t(len(nameBytes)),
	}

	r := C.iroh_blobs_add_bytes_as_collection(
		C.uint64_t(b.runtime.handle),
		C.uint64_t(b.node.handle),
		nameC,
		bytes,
		0,
		&opID,
	)
	if r != 0 {
		return "", fmt.Errorf("iroh_blobs_add_bytes_as_collection: %w", Error(r))
	}

	ev, err := b.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return "", fmt.Errorf("add bytes as collection poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_BLOB_ADDED {
		return "", fmt.Errorf("add bytes as collection: unexpected event %d", ev.Kind)
	}

	hashHex := C.GoStringN((*C.char)(ev.DataPtr), C.int(ev.DataLen))
	return BlobID(hashHex), nil
}

// AddCollection stores a multi-file collection.
// entriesJson is a UTF-8 JSON string: [[name, base64data], ...]
func (b *Blobs) AddCollection(ctx context.Context, entriesJson string) (BlobID, error) {
	var opID C.iroh_operation_t

	jsonBytes := []byte(entriesJson)
	bytes := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&jsonBytes[0])),
		len: C.uintptr_t(len(jsonBytes)),
	}

	r := C.iroh_blobs_add_collection(
		C.uint64_t(b.runtime.handle),
		C.uint64_t(b.node.handle),
		bytes,
		0,
		&opID,
	)
	if r != 0 {
		return "", fmt.Errorf("iroh_blobs_add_collection: %w", Error(r))
	}

	ev, err := b.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return "", fmt.Errorf("add collection poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_BLOB_COLLECTION_ADDED {
		return "", fmt.Errorf("add collection: unexpected event %d", ev.Kind)
	}

	hashHex := C.GoStringN((*C.char)(ev.DataPtr), C.int(ev.DataLen))
	return BlobID(hashHex), nil
}

// Read retrieves blob data by hash.
func (b *Blobs) Read(ctx context.Context, hashHex string) ([]byte, error) {
	var opID C.iroh_operation_t

	hashBytes := []byte(hashHex)
	hashC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&hashBytes[0])),
		len: C.uintptr_t(len(hashBytes)),
	}

	r := C.iroh_blobs_read(
		C.uint64_t(b.runtime.handle),
		C.uint64_t(b.node.handle),
		hashC,
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_blobs_read: %w", Error(r))
	}

	ev, err := b.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("read poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_BLOB_READ {
		return nil, fmt.Errorf("read: unexpected event %d", ev.Kind)
	}

	data := cDataToGoBytes(ev.DataPtr, ev.DataLen)
	if ev.Buffer != 0 {
		b.runtime.ReleaseBuffer(ev.Buffer)
	}

	return data, nil
}

// Download downloads a blob from a ticket.
func (b *Blobs) Download(ctx context.Context, ticket BlobTicket) (BlobID, error) {
	var opID C.iroh_operation_t

	ticketBytes := []byte(ticket)
	ticketC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&ticketBytes[0])),
		len: C.uintptr_t(len(ticketBytes)),
	}

	r := C.iroh_blobs_download(
		C.uint64_t(b.runtime.handle),
		C.uint64_t(b.node.handle),
		ticketC,
		0,
		&opID,
	)
	if r != 0 {
		return "", fmt.Errorf("iroh_blobs_download: %w", Error(r))
	}

	ev, err := b.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return "", fmt.Errorf("download poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_BLOB_DOWNLOADED {
		return "", fmt.Errorf("download: unexpected event %d", ev.Kind)
	}

	hashHex := C.GoStringN((*C.char)(ev.DataPtr), C.int(ev.DataLen))
	return BlobID(hashHex), nil
}

// Status returns the status of a blob.
func (b *Blobs) Status(hashHex string) (BlobStatus, int64, error) {
	hashBytes := []byte(hashHex)
	var status C.uint32_t
	var size C.uint64_t

	r := C.iroh_blobs_status(
		C.uint64_t(b.runtime.handle),
		C.uint64_t(b.node.handle),
		(*C.uint8_t)(unsafe.Pointer(&hashBytes[0])),
		C.uintptr_t(len(hashBytes)),
		&status,
		&size,
	)
	if r != 0 {
		return BLOB_STATUS_NOT_FOUND, 0, fmt.Errorf("iroh_blobs_status: %w", Error(r))
	}

	return BlobStatus(status), int64(size), nil
}

// Has returns true if the blob is completely stored locally.
func (b *Blobs) Has(hashHex string) (bool, error) {
	hashBytes := []byte(hashHex)
	var has C.uint32_t

	r := C.iroh_blobs_has(
		C.uint64_t(b.runtime.handle),
		C.uint64_t(b.node.handle),
		(*C.uint8_t)(unsafe.Pointer(&hashBytes[0])),
		C.uintptr_t(len(hashBytes)),
		&has,
	)
	if r != 0 {
		return false, fmt.Errorf("iroh_blobs_has: %w", Error(r))
	}

	return has != 0, nil
}

// ObserveComplete waits until a blob is fully downloaded.
func (b *Blobs) ObserveComplete(ctx context.Context, hashHex string) error {
	var opID C.iroh_operation_t

	hashBytes := []byte(hashHex)

	r := C.iroh_blobs_observe_complete(
		C.uint64_t(b.runtime.handle),
		C.uint64_t(b.node.handle),
		(*C.uint8_t)(unsafe.Pointer(&hashBytes[0])),
		C.uintptr_t(len(hashBytes)),
		0,
		&opID,
	)
	if r != 0 {
		return fmt.Errorf("iroh_blobs_observe_complete: %w", Error(r))
	}

	_, err := b.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return fmt.Errorf("observe complete poll: %w", err)
	}

	return nil
}

// ObserveSnapshot returns a snapshot of blob download progress.
func (b *Blobs) ObserveSnapshot(hashHex string) (bool, int64, error) {
	hashBytes := []byte(hashHex)
	var isComplete C.uint32_t
	var size C.uint64_t

	r := C.iroh_blobs_observe_snapshot(
		C.uint64_t(b.runtime.handle),
		C.uint64_t(b.node.handle),
		(*C.uint8_t)(unsafe.Pointer(&hashBytes[0])),
		C.uintptr_t(len(hashBytes)),
		&isComplete,
		&size,
	)
	if r != 0 {
		return false, 0, fmt.Errorf("iroh_blobs_observe_snapshot: %w", Error(r))
	}

	return isComplete != 0, int64(size), nil
}

// ListCollection lists entries in a collection.
func (b *Blobs) ListCollection(ctx context.Context, hashHex string) (*BlobCollection, error) {
	var opID C.iroh_operation_t

	hashBytes := []byte(hashHex)
	hashC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&hashBytes[0])),
		len: C.uintptr_t(len(hashBytes)),
	}

	r := C.iroh_blobs_list_collection(
		C.uint64_t(b.runtime.handle),
		C.uint64_t(b.node.handle),
		hashC,
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_blobs_list_collection: %w", Error(r))
	}

	ev, err := b.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("list collection poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_BLOB_READ {
		return nil, fmt.Errorf("list collection: unexpected event %d", ev.Kind)
	}

	jsonStr := C.GoStringN((*C.char)(ev.DataPtr), C.int(ev.DataLen))
	if ev.Buffer != 0 {
		b.runtime.ReleaseBuffer(ev.Buffer)
	}

	return parseBlobCollectionJSON(jsonStr)
}

// parseBlobCollectionJSON parses JSON in format: [[name, hash, size], ...]
func parseBlobCollectionJSON(jsonStr string) (*BlobCollection, error) {
	var entries [][]interface{}
	if err := json.Unmarshal([]byte(jsonStr), &entries); err != nil {
		return nil, fmt.Errorf("parse blob collection json: %w", err)
	}

	collection := &BlobCollection{
		Entries: make([]BlobEntry, 0, len(entries)),
	}

	for _, entry := range entries {
		if len(entry) < 3 {
			continue
		}
		name, ok := entry[0].(string)
		if !ok {
			continue
		}
		hash, ok := entry[1].(string)
		if !ok {
			continue
		}
		size, ok := entry[2].(float64)
		if !ok {
			continue
		}
		collection.Entries = append(collection.Entries, BlobEntry{
			Name: name,
			Hash: BlobID(hash),
			Size: int64(size),
		})
	}

	return collection, nil
}

// CreateTicket creates a ticket for sharing a blob.
func (b *Blobs) CreateTicket(hashHex string, format BlobFormat) (BlobTicket, error) {
	hashBytes := []byte(hashHex)
	var buf [1024]byte
	var outLen C.uintptr_t

	hashC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&hashBytes[0])),
		len: C.uintptr_t(len(hashBytes)),
	}

	r := C.iroh_blobs_create_ticket(
		C.uint64_t(b.runtime.handle),
		C.uint64_t(b.node.handle),
		hashC,
		(*C.uint8_t)(unsafe.Pointer(&buf[0])),
		1024,
		&outLen,
	)
	if r != 0 {
		return "", fmt.Errorf("iroh_blobs_create_ticket: %w", Error(r))
	}

	ticket := BlobTicket(C.GoStringN((*C.char)(unsafe.Pointer(&buf[0])), C.int(outLen)))
	return ticket, nil
}

// CreateCollectionTicket creates a ticket for sharing a collection.
func (b *Blobs) CreateCollectionTicket(hashHex string, names []string) (BlobTicket, error) {
	hashBytes := []byte(hashHex)
	var buf [1024]byte
	var outLen C.uintptr_t

	hashC := C.struct_iroh_bytes_t{
		ptr: (*C.uint8_t)(unsafe.Pointer(&hashBytes[0])),
		len: C.uintptr_t(len(hashBytes)),
	}

	r := C.iroh_blobs_create_collection_ticket(
		C.uint64_t(b.runtime.handle),
		C.uint64_t(b.node.handle),
		hashC,
		(*C.uint8_t)(unsafe.Pointer(&buf[0])),
		1024,
		&outLen,
	)
	if r != 0 {
		return "", fmt.Errorf("iroh_blobs_create_collection_ticket: %w", Error(r))
	}

	ticket := BlobTicket(C.GoStringN((*C.char)(unsafe.Pointer(&buf[0])), C.int(outLen)))
	return ticket, nil
}

// LocalInfo returns local information about a blob.
func (b *Blobs) LocalInfo(hashHex string) (*BlobInfo, error) {
	hashBytes := []byte(hashHex)
	var isComplete C.uint32_t
	var localBytes C.uint64_t

	r := C.iroh_blobs_local_info(
		C.uint64_t(b.runtime.handle),
		C.uint64_t(b.node.handle),
		(*C.uint8_t)(unsafe.Pointer(&hashBytes[0])),
		C.uintptr_t(len(hashBytes)),
		&isComplete,
		&localBytes,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_blobs_local_info: %w", Error(r))
	}

	status := BLOB_STATUS_PARTIAL
	if isComplete != 0 {
		status = BLOB_STATUS_COMPLETE
	}

	return &BlobInfo{
		Hash:   BlobID(hashHex),
		Size:   int64(localBytes),
		Status: status,
	}, nil
}
