//go:build cgo

package aster

/*
#include "aster_ffi.h"
*/
import "C"

import (
	"context"
	"encoding/binary"
	"fmt"
	"io"
	"sync/atomic"
)

// DocEvent is a live change. Entry is populated for inserts, From for remote
// inserts, Hash for content-ready, and Peer for neighbor/sync events.
type DocEvent struct {
	Type  DocEventType
	Entry *DocEntry
	From  string
	Hash  BlobID
	Peer  string
}

// DocSubscription owns a native document event receiver. Close it when done.
// Recv calls are serialized by the native receiver; Close wakes pending calls.
type DocSubscription struct {
	runtime *Runtime
	handle  uint64
	closed  atomic.Bool
}

// Subscribe listens for subsequent changes to this document.
func (d *Doc) Subscribe(ctx context.Context) (*DocSubscription, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	ev, err := startOwnedOp(ctx, d.runtime, "aster_doc_subscribe", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_doc_subscribe(C.uint64_t(d.runtime.handle), C.uint64_t(d.handle), 0, op)
	}, func(ev *Event) {
		if ev.Kind == ASTER_EVENT_DOC_SUBSCRIBED && ev.Status == ASTER_STATUS_OK {
			C.aster_doc_event_receiver_free(C.uint64_t(d.runtime.handle), C.uint64_t(ev.Handle))
		}
	})
	if err != nil {
		return nil, err
	}
	if _, err := expectDocEvent(d.runtime, ev, ASTER_EVENT_DOC_SUBSCRIBED, "subscribe"); err != nil {
		return nil, err
	}
	return &DocSubscription{runtime: d.runtime.retain(), handle: ev.Handle}, nil
}

// JoinAndSubscribeWithEvents atomically joins a document and subscribes before
// synchronization starts. The caller owns and must close both returned handles.
func (d *Docs) JoinAndSubscribeWithEvents(ctx context.Context, ticket string) (*Doc, *DocSubscription, error) {
	if err := ctx.Err(); err != nil {
		return nil, nil, err
	}
	var a cArena
	defer a.free()
	ev, err := startOwnedOp(ctx, d.runtime, "aster_docs_join_and_subscribe", func(op *C.aster_operation_t) C.int32_t {
		return C.aster_docs_join_and_subscribe(C.uint64_t(d.runtime.handle), C.uint64_t(d.node.handle), a.str(ticket), 0, op)
	}, func(ev *Event) {
		if ev.Kind == ASTER_EVENT_DOC_JOINED_AND_SUBSCRIBED && ev.Status == ASTER_STATUS_OK {
			C.aster_doc_event_receiver_free(C.uint64_t(d.runtime.handle), C.uint64_t(ev.Related))
			C.aster_doc_free(C.uint64_t(d.runtime.handle), C.uint64_t(ev.Handle))
		}
	})
	if err != nil {
		return nil, nil, err
	}
	if _, err := expectDocEvent(d.runtime, ev, ASTER_EVENT_DOC_JOINED_AND_SUBSCRIBED, "join and subscribe"); err != nil {
		return nil, nil, err
	}
	return &Doc{runtime: d.runtime, handle: ev.Handle}, &DocSubscription{runtime: d.runtime.retain(), handle: ev.Related}, nil
}

// Close releases the receiver and wakes pending Recv calls with io.EOF.
// An in-flight receive racing Close may still return a successful event.
func (s *DocSubscription) Close() error {
	if s.closed.Swap(true) {
		return nil
	}
	defer s.runtime.release()
	status, statusMsg := ffiCall(func() C.int32_t {
		return C.aster_doc_event_receiver_free(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle))
	})
	if status == C.ASTER_STATUS_NOT_FOUND {
		return nil
	}
	return wrapStatusMsg("aster_doc_event_receiver_free", status, statusMsg)
}

// Recv returns the next event or io.EOF when closed. A timeout does not consume
// a subsequent event. If completion wins the cancellation race, Recv returns
// that event, even if the context has just expired.
func (s *DocSubscription) Recv(ctx context.Context) (*DocEvent, error) {
	if s.closed.Load() {
		return nil, io.EOF
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	s.runtime.retain()
	defer s.runtime.release()
	var op C.aster_operation_t
	status, statusMsg := ffiCall(func() C.int32_t {
		return C.aster_doc_event_recv(C.uint64_t(s.runtime.handle), C.uint64_t(s.handle), 0, &op)
	})
	if status == C.ASTER_STATUS_NOT_FOUND {
		return nil, io.EOF
	}
	if err := wrapStatusMsg("aster_doc_event_recv", status, statusMsg); err != nil {
		return nil, err
	}
	ch := make(chan *Event, 1)
	s.runtime.Register(uint64(op), ch)
	var ev *Event
	select {
	case ev = <-ch:
	case <-ctx.Done():
		// Preserve the channel: native cancellation and completion arbitrate
		// one terminal event. Dropping a committed event here would lose it.
		ev = s.runtime.cancelAndCollect(uint64(op), ch)
	}
	if ev == nil {
		if s.closed.Load() {
			return nil, io.EOF
		}
		return nil, ErrAlreadyClosed
	}
	if ev.Kind == ASTER_EVENT_OPERATION_CANCELLED {
		s.runtime.releaseEventBuffer(ev)
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		return nil, ErrCancelled
	}
	if ev.Kind == ASTER_EVENT_DOC_EVENT && ev.Status == ASTER_STATUS_NOT_FOUND {
		s.runtime.releaseEventBuffer(ev)
		return nil, io.EOF
	}
	data, err := expectDocEvent(s.runtime, ev, ASTER_EVENT_DOC_EVENT, "doc event")
	if err != nil {
		return nil, err
	}
	return decodeDocEvent(DocEventType(ev.Flags), data)
}

func decodeDocEvent(kind DocEventType, data []byte) (*DocEvent, error) {
	event := &DocEvent{Type: kind}
	switch kind {
	case DOC_EVENT_INSERT_LOCAL, DOC_EVENT_INSERT_REMOTE:
		// Locate the end of the three length-prefixed metadata fields.
		end := 0
		for i := 0; i < 3; i++ {
			if len(data)-end < 4 {
				return nil, fmt.Errorf("doc event: truncated metadata length")
			}
			n := uint64(binary.LittleEndian.Uint32(data[end:]))
			end += 4
			if n > uint64(len(data)-end) {
				return nil, fmt.Errorf("doc event: truncated metadata")
			}
			end += int(n)
		}
		if len(data)-end < 16 {
			return nil, fmt.Errorf("doc event: truncated metadata counters")
		}
		end += 16
		entries, err := parseDocEntryList(data[:end])
		if err != nil {
			return nil, err
		}
		event.Entry = &entries[0]
		rest := data[end:]
		if kind == DOC_EVENT_INSERT_REMOTE {
			if len(rest) < 4 || uint64(binary.LittleEndian.Uint32(rest)) != uint64(len(rest)-4) {
				return nil, fmt.Errorf("doc event: malformed remote peer")
			}
			event.From = string(rest[4:])
		} else if len(rest) != 0 {
			return nil, fmt.Errorf("doc event: trailing metadata")
		}
	case DOC_EVENT_CONTENT_READY:
		event.Hash = BlobID(data)
	case DOC_EVENT_PENDING_CONTENT:
		if len(data) != 0 {
			return nil, fmt.Errorf("doc event: unexpected pending-content payload")
		}
	case DOC_EVENT_NEIGHBOR_UP, DOC_EVENT_NEIGHBOR_DOWN, DOC_EVENT_SYNC_FINISHED:
		event.Peer = string(data)
	default:
		return nil, fmt.Errorf("doc event: unknown type %d", kind)
	}
	return event, nil
}
