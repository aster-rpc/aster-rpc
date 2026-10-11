//go:build cgo

package aster

/*
#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"fmt"
	"sync"
)

// BlobCustodyScope temporarily protects multiple Raw or HashSeq roots from GC,
// including roots not yet present locally. It creates no persistent tags.
// Close seals new submissions; accepted scoped operations retain all custody
// guards and their runtime until native work and terminal delivery finish.
type BlobCustodyScope struct {
	runtime *Runtime
	handle  uint64
	mu      sync.Mutex
	closed  bool
}

// NewCustodyScope creates an owned temporary retention scope. Close it when
// host-side verification is finished or persistent named retention is committed.
func (b *Blobs) NewCustodyScope() (*BlobCustodyScope, error) {
	rt := b.runtime.retain()
	var handle C.uint64_t
	if err := wrapStatus("aster_blobs_custody_new", C.aster_blobs_custody_new(C.uint64_t(rt.handle), C.uint64_t(b.node.handle), &handle)); err != nil {
		_ = rt.release()
		return nil, err
	}
	return &BlobCustodyScope{runtime: rt, handle: uint64(handle)}, nil
}

// Close releases this temporary scope. Accepted operations keep their guards;
// new operations fail. A native close error preserves ownership for retry.
func (s *BlobCustodyScope) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return nil
	}
	status := C.aster_blob_custody_free(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle))
	if status != C.ASTER_STATUS_NOT_FOUND && status != C.ASTER_STATUS_ALREADY_CLOSED {
		if err := wrapStatus("aster_blob_custody_free", status); err != nil {
			return err
		}
	}
	s.closed = true
	return s.runtime.release()
}

// awaitCustodyOp keeps registration until native cancellation/commit chooses
// the sole terminal event. Success already committed wins racing cancellation.
// The caller retains the runtime through this payload copy and buffer release.
func awaitCustodyOp(ctx context.Context, rt *Runtime, op uint64, expected EventKind, what string) ([]byte, error) {
	ch := make(chan *Event, 1)
	rt.Register(op, ch)
	var ev *Event
	select {
	case ev = <-ch:
	case <-ctx.Done():
		ev = rt.cancelAndCollect(op, ch)
	}
	if ev == nil {
		return nil, fmt.Errorf("%s: %w", what, ErrAlreadyClosed)
	}
	if ev.Kind == ASTER_EVENT_OPERATION_CANCELLED {
		rt.releaseEventBuffer(ev)
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		return nil, ErrCancelled
	}
	return expect(rt, ev, uint32(expected), what)
}

func (s *BlobCustodyScope) run(ctx context.Context, what string, expected EventKind, submit func(*C.aster_operation_t) C.int32_t) ([]byte, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	s.mu.Lock()
	if s.closed {
		s.mu.Unlock()
		return nil, ErrAlreadyClosed
	}
	rt := s.runtime.retain()
	var op C.aster_operation_t
	err := wrapStatus(what, submit(&op))
	s.mu.Unlock()
	defer rt.release()
	if err != nil {
		return nil, err
	}
	return awaitCustodyOp(ctx, rt, uint64(op), expected, what)
}

// Protect adds a typed root, even before fetching its bytes. HashSeq custody
// includes its graph as children become known; it imposes no Collection schema.
func (s *BlobCustodyScope) Protect(ctx context.Context, hash string, format BlobFormat) error {
	var a cArena
	defer a.free()
	_, err := s.run(ctx, "aster_blob_custody_protect", ASTER_EVENT_BLOB_CUSTODY_DONE, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_blob_custody_protect(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), a.str(hash), C.uint32_t(format), 0, op)
	})
	return err
}

// AddBytes copies bytes into the store under temporary custody, with no
// automatic persistent tag. Retain commits a named root after host verification.
func (s *BlobCustodyScope) AddBytes(ctx context.Context, data []byte, format BlobFormat) (BlobID, error) {
	var a cArena
	defer a.free()
	p, err := s.run(ctx, "aster_blob_custody_add_bytes", ASTER_EVENT_BLOB_ADDED, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_blob_custody_add_bytes(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), a.bytes(data), C.uint32_t(format), 0, op)
	})
	return BlobID(p), err
}

// AddPath copies a file into temporary custody; later edits cannot alter the
// imported bytes. No automatic persistent tag is created.
func (s *BlobCustodyScope) AddPath(ctx context.Context, path string, format BlobFormat) (BlobID, error) {
	var a cArena
	defer a.free()
	p, err := s.run(ctx, "aster_blob_custody_add_path", ASTER_EVENT_BLOB_ADDED, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_blob_custody_add_path(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), a.str(path), C.uint32_t(format), 0, op)
	})
	return BlobID(p), err
}

// Retain commits the exact named tag and typed root, replacing that tag's prior
// root. It creates no extra automatic tag. Custody remains until scope close.
func (s *BlobCustodyScope) Retain(ctx context.Context, hash string, format BlobFormat, tagName string) error {
	var a cArena
	defer a.free()
	_, err := s.run(ctx, "aster_blob_custody_retain", ASTER_EVENT_BLOB_CUSTODY_DONE, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_blob_custody_retain(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), a.str(hash), C.uint32_t(format), a.str(tagName), 0, op)
	})
	return err
}

// Read copies a local root's raw bytes while holding all scope guards. For a
// HashSeq this returns its hash sequence, without interpreting Collection data.
func (s *BlobCustodyScope) Read(ctx context.Context, hash string) ([]byte, error) {
	var a cArena
	defer a.free()
	p, err := s.run(ctx, "aster_blob_custody_read", ASTER_EVENT_BLOB_READ, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_blob_custody_read(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), a.str(hash), 0, op)
	})
	return nonNil(p), err
}

// DownloadHashFrom protects the typed root before fetching from ordered
// providers. Accepted work keeps all guards if Close races the fetch. With
// toStore only the fetch report is returned, with an empty byte slice. Cancellation
// waits for downloader cleanup and can include network drain; it is not a prompt
// deadline for a stalled peer.
func (s *BlobCustodyScope) DownloadHashFrom(ctx context.Context, hash string, providers []string, format BlobFormat, strategy FetchStrategy, toStore bool) ([]byte, FetchReport, error) {
	var a cArena
	defer a.free()
	var flags C.uint32_t
	if toStore {
		flags = C.ASTER_FETCH_TO_STORE
	}
	p, err := s.run(ctx, "aster_blob_custody_download_hash_from", ASTER_EVENT_BLOB_FETCHED, func(op *C.aster_operation_t) C.int32_t {
		return C.aster_blob_custody_download_hash_from(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), a.str(hash), a.list(providers), C.uint32_t(format), C.uint32_t(strategy), flags, 0, op)
	})
	if err != nil {
		return nil, FetchReport{}, err
	}
	report, data, err := parseFetchReport(p)
	return nonNil(data), report, err
}

func (b *Blobs) verified(ctx context.Context, what string, submit func(*C.aster_operation_t) C.int32_t) (BlobID, error) {
	if err := ctx.Err(); err != nil {
		return "", err
	}
	rt := b.runtime.retain()
	defer rt.release()
	var op C.aster_operation_t
	if err := wrapStatus(what, submit(&op)); err != nil {
		return "", err
	}
	p, err := awaitCustodyOp(ctx, rt, uint64(op), ASTER_EVENT_BLOB_ADDED, what)
	return BlobID(p), err
}

// AddBytesVerified imports copied bytes under continuous temporary custody,
// verifies their store-computed hash, then commits exactly tagName on a match.
// A mismatch preserves the old tag; no automatic root is created. Cancellation
// waits for cleanup, or returns actual success/error if tag commit already began.
func (b *Blobs) AddBytesVerified(ctx context.Context, data []byte, expectedHash string, format BlobFormat, tagName string) (BlobID, error) {
	var a cArena
	defer a.free()
	return b.verified(ctx, "aster_blobs_add_bytes_verified", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_blobs_add_bytes_verified(C.uint64_t(b.runtime.handle), C.uint64_t(b.node.handle), a.bytes(data), a.str(expectedHash), C.uint32_t(format), a.str(tagName), 0, op)
	})
}

// AddPathVerified copies a file, verifies its store-computed hash and commits
// exactly tagName on a match. Ownership and cancellation follow AddBytesVerified.
func (b *Blobs) AddPathVerified(ctx context.Context, path, expectedHash string, format BlobFormat, tagName string) (BlobID, error) {
	var a cArena
	defer a.free()
	return b.verified(ctx, "aster_blobs_add_path_verified", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_blobs_add_path_verified(C.uint64_t(b.runtime.handle), C.uint64_t(b.node.handle), a.str(path), a.str(expectedHash), C.uint32_t(format), a.str(tagName), 0, op)
	})
}
