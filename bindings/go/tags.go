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
	"fmt"
	"strings"
	"unsafe"
)

// TagFormat represents the format of a tagged blob.
type TagFormat uint32

// Tag format constants.
const (
	TAG_FORMAT_RAW      TagFormat = 0
	TAG_FORMAT_HASH_SEQ TagFormat = 1
)

// TagEntry represents information about a named tag in the blob store.
type TagEntry struct {
	Name   string
	Hash   BlobID
	Format TagFormat
}

// Tags provides tag operations for an Iroh node.
// Get an instance via Node.Tags().
type Tags struct {
	node    *Node
	runtime *Runtime
}

// Node returns the parent node.
func (t *Tags) Node() *Node {
	return t.node
}

// Set sets a named tag for a blob.
func (t *Tags) Set(ctx context.Context, name string, hash BlobID, format TagFormat) error {
	var opID C.iroh_operation_t

	nameBytes := []byte(name)
	hashBytes := []byte(hash)

	r := C.iroh_tags_set(
		C.uint64_t(t.runtime.handle),
		C.uint64_t(t.node.handle),
		(*C.uint8_t)(unsafe.Pointer(&nameBytes[0])),
		C.uintptr_t(len(nameBytes)),
		(*C.uint8_t)(unsafe.Pointer(&hashBytes[0])),
		C.uintptr_t(len(hashBytes)),
		C.uint32_t(format),
		0,
		&opID,
	)
	if r != 0 {
		return fmt.Errorf("iroh_tags_set: %w", Error(r))
	}

	ev, err := t.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return fmt.Errorf("set poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_TAG_SET {
		return fmt.Errorf("set: unexpected event %d", ev.Kind)
	}

	return nil
}

// Get retrieves a tag by name.
func (t *Tags) Get(ctx context.Context, name string) (*TagEntry, error) {
	var opID C.iroh_operation_t

	nameBytes := []byte(name)

	r := C.iroh_tags_get(
		C.uint64_t(t.runtime.handle),
		C.uint64_t(t.node.handle),
		(*C.uint8_t)(unsafe.Pointer(&nameBytes[0])),
		C.uintptr_t(len(nameBytes)),
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_tags_get: %w", Error(r))
	}

	ev, err := t.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("get poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_TAG_GET {
		return nil, fmt.Errorf("get: unexpected event %d", ev.Kind)
	}
	if ev.Status == IROH_STATUS_NOT_FOUND {
		return nil, nil
	}

	return parseTagEntry(ev.DataPtr, ev.DataLen)
}

// Delete deletes a tag by name.
func (t *Tags) Delete(ctx context.Context, name string) (int, error) {
	var opID C.iroh_operation_t

	nameBytes := []byte(name)

	r := C.iroh_tags_delete(
		C.uint64_t(t.runtime.handle),
		C.uint64_t(t.node.handle),
		(*C.uint8_t)(unsafe.Pointer(&nameBytes[0])),
		C.uintptr_t(len(nameBytes)),
		0,
		&opID,
	)
	if r != 0 {
		return 0, fmt.Errorf("iroh_tags_delete: %w", Error(r))
	}

	ev, err := t.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return 0, fmt.Errorf("delete poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_TAG_DELETED {
		return 0, fmt.Errorf("delete: unexpected event %d", ev.Kind)
	}

	return int(ev.Flags), nil
}

// ListPrefix lists tags matching a prefix.
func (t *Tags) ListPrefix(ctx context.Context, prefix string) ([]TagEntry, error) {
	var opID C.iroh_operation_t

	prefixBytes := []byte(prefix)

	r := C.iroh_tags_list_prefix(
		C.uint64_t(t.runtime.handle),
		C.uint64_t(t.node.handle),
		(*C.uint8_t)(unsafe.Pointer(&prefixBytes[0])),
		C.uintptr_t(len(prefixBytes)),
		0,
		&opID,
	)
	if r != 0 {
		return nil, fmt.Errorf("iroh_tags_list_prefix: %w", Error(r))
	}

	ev, err := t.runtime.Poll(ctx, uint64(opID))
	if err != nil {
		return nil, fmt.Errorf("list prefix poll: %w", err)
	}
	if ev.Kind != IROH_EVENT_TAG_LIST {
		return nil, fmt.Errorf("list prefix: unexpected event %d", ev.Kind)
	}

	return parseTagList(ev.DataPtr, ev.DataLen, int(ev.Flags))
}

// ListAll lists all tags.
func (t *Tags) ListAll(ctx context.Context) ([]TagEntry, error) {
	return t.ListPrefix(ctx, "")
}

// parseTagEntry parses a tag entry from C data.
// Format: null-terminated strings: name\0hash_hex\0format\0
func parseTagEntry(dataPtr unsafe.Pointer, dataLen uintptr) (*TagEntry, error) {
	if dataPtr == nil || dataLen == 0 {
		return nil, fmt.Errorf("nil tag data")
	}

	data := C.GoStringN((*C.char)(dataPtr), C.int(dataLen))
	parts := strings.Split(data, "\x00")
	if len(parts) < 3 {
		return nil, fmt.Errorf("invalid tag entry format")
	}

	name := parts[0]
	hashHex := parts[1]
	formatStr := parts[2]

	format := TAG_FORMAT_RAW
	if formatStr == "hash_seq" {
		format = TAG_FORMAT_HASH_SEQ
	}

	return &TagEntry{
		Name:   name,
		Hash:   BlobID(hashHex),
		Format: format,
	}, nil
}

// parseTagList parses a list of tag entries from C data.
// Format: packed null-terminated strings: name\0hash_hex\0format\0name\0hash_hex\0format\0...
func parseTagList(dataPtr unsafe.Pointer, dataLen uintptr, count int) ([]TagEntry, error) {
	if dataPtr == nil || dataLen == 0 || count == 0 {
		return nil, nil
	}

	data := C.GoStringN((*C.char)(dataPtr), C.int(dataLen))
	parts := strings.Split(data, "\x00")

	entries := make([]TagEntry, 0, count)
	for i := 0; i+2 < len(parts) && len(entries) < count; i += 3 {
		name := parts[i]
		hashHex := parts[i+1]
		formatStr := parts[i+2]

		format := TAG_FORMAT_RAW
		if formatStr == "hash_seq" {
			format = TAG_FORMAT_HASH_SEQ
		}

		entries = append(entries, TagEntry{
			Name:   name,
			Hash:   BlobID(hashHex),
			Format: format,
		})
	}

	return entries, nil
}
