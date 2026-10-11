//go:build cgo

package aster

/*
#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"encoding/binary"
	"encoding/hex"
	"fmt"
)

// ReadEntryContentsIfComplete reads content hashes in one local native batch,
// preserving order and duplicates. A nil slot means missing or partial at the
// initial check; a non-nil empty slice means complete empty content. No fetch,
// tag or retention is added. This is not a global snapshot: GC/deletion before
// a large blob's validated export may fail the whole batch. Use a custody scope
// when stable protection is needed during host verification.
func (d *Doc) ReadEntryContentsIfComplete(ctx context.Context, hashes []BlobID) ([][]byte, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if uint64(len(hashes)) > uint64(^uint32(0)) {
		return nil, fmt.Errorf("document batch: too many hashes: %w", ErrInvalidArgument)
	}
	input := make([]string, len(hashes))
	for i, hash := range hashes {
		if len(hash) != 64 {
			return nil, fmt.Errorf("document batch: hash %d must be 64 hex characters: %w", i, ErrInvalidArgument)
		}
		if _, err := hex.DecodeString(string(hash)); err != nil {
			return nil, fmt.Errorf("document batch: invalid hash %d: %w", i, ErrInvalidArgument)
		}
		input[i] = string(hash)
	}
	rt := d.runtime.retain()
	defer rt.release()
	var a cArena
	defer a.free()
	ev, err := startOp(ctx, rt, "aster_doc_read_entry_contents_if_complete", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_read_entry_contents_if_complete(C.uint64_t(rt.handle), C.uint64_t(d.handle), a.list(input), 0, op)
	})
	if err != nil {
		return nil, err
	}
	payload, err := expectDocEvent(rt, ev, ASTER_EVENT_DOC_CONTENTS_BATCH, "read entry contents if complete")
	if err != nil {
		return nil, err
	}
	return decodeDocContentsBatch(payload, len(hashes))
}

// Wire: u32 count, then u8 presence; present slots have u64 length + bytes.
func decodeDocContentsBatch(payload []byte, expectedCount int) ([][]byte, error) {
	invalid := func(reason string) ([][]byte, error) {
		return nil, fmt.Errorf("document batch: %s: %w", reason, ErrInternal)
	}
	if len(payload) < 4 {
		return invalid("truncated count")
	}
	count := binary.LittleEndian.Uint32(payload)
	rest := payload[4:]
	if expectedCount < 0 || uint64(count) != uint64(expectedCount) {
		return invalid("count does not match submitted hashes")
	}
	// Every slot needs at least a presence byte; bound allocation by payload.
	if uint64(count) > uint64(len(rest)) {
		return invalid("truncated slots")
	}
	out := make([][]byte, int(count))
	for i := range out {
		if len(rest) == 0 {
			return invalid("truncated presence")
		}
		present := rest[0]
		rest = rest[1:]
		switch present {
		case 0:
		case 1:
			if len(rest) < 8 {
				return invalid("truncated content length")
			}
			length := binary.LittleEndian.Uint64(rest)
			rest = rest[8:]
			if length > uint64(len(rest)) {
				return invalid("truncated or oversized content")
			}
			out[i] = make([]byte, int(length))
			copy(out[i], rest[:int(length)])
			rest = rest[int(length):]
		default:
			return invalid("invalid presence marker")
		}
	}
	if len(rest) != 0 {
		return invalid("trailing bytes")
	}
	return out, nil
}
