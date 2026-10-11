//go:build cgo

package aster

import (
	"context"
	"errors"
	"io"
	"testing"
	"time"
)

func TestDocSubscriptionLifecycle(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 30*time.Second)
	defer cancel()
	node := startTestNode(t, ctx)
	doc, err := node.Docs().Create(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer doc.Close()
	author, err := node.Docs().CreateAuthor(ctx)
	if err != nil {
		t.Fatal(err)
	}
	sub, err := doc.Subscribe(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer sub.Close()
	timed, stop := context.WithTimeout(ctx, 30*time.Millisecond)
	defer stop()
	if _, err := sub.Recv(timed); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("idle receive: %v", err)
	}
	if err := doc.SetBytes(ctx, author, "after-timeout", []byte("value")); err != nil {
		t.Fatal(err)
	}
	event, err := sub.Recv(ctx)
	if err != nil || event.Type != DOC_EVENT_INSERT_LOCAL || event.Entry == nil || event.Entry.Key != "after-timeout" || event.Entry.ContentLen != 5 {
		t.Fatalf("event after timeout = %+v, %v", event, err)
	}
	done := make(chan error, 1)
	go func() { _, err := sub.Recv(ctx); done <- err }()
	if err := sub.Close(); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-done:
		if !errors.Is(err, io.EOF) {
			t.Fatalf("pending recv close: %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("close stranded receive")
	}
	if err := sub.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := sub.Recv(ctx); !errors.Is(err, io.EOF) {
		t.Fatalf("closed recv: %v", err)
	}
}

func TestDocJoinWithEvents(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 30*time.Second)
	defer cancel()
	node := startTestNode(t, ctx)
	remote := startTestNode(t, ctx)
	doc, err := node.Docs().Create(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer doc.Close()
	author, err := node.Docs().CreateAuthor(ctx)
	if err != nil {
		t.Fatal(err)
	}
	ticket, err := doc.ShareWithAddr(ctx, DOC_SHARE_WRITE)
	if err != nil {
		t.Fatal(err)
	}
	joined, events, err := remote.Docs().JoinAndSubscribeWithEvents(ctx, ticket)
	if err != nil {
		t.Fatal(err)
	}
	defer joined.Close()
	defer events.Close()
	if err := doc.SetBytes(ctx, author, "remote-key", []byte("remote-value")); err != nil {
		t.Fatal(err)
	}
	for {
		event, err := events.Recv(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if event.Type == DOC_EVENT_INSERT_REMOTE && event.Entry.Key == "remote-key" {
			if event.From == "" || event.Entry.Author != author || event.Entry.ContentLen != 12 {
				t.Fatalf("remote event: %+v", event)
			}
			break
		}
	}
}

func TestDocEventRejectsMalformed(t *testing.T) {
	for _, kind := range []DocEventType{DOC_EVENT_INSERT_LOCAL, DOC_EVENT_INSERT_REMOTE} {
		for _, data := range [][]byte{nil, {1, 0, 0, 0}, {255, 255, 255, 255}} {
			if _, err := decodeDocEvent(kind, data); err == nil {
				t.Fatalf("accepted malformed %d: %x", kind, data)
			}
		}
	}
	if _, err := decodeDocEvent(99, nil); err == nil {
		t.Fatal("accepted unknown event")
	}
	if _, err := decodeDocEvent(DOC_EVENT_PENDING_CONTENT, []byte{1}); err == nil {
		t.Fatal("accepted unexpected payload")
	}
}

func TestDocSubscriptionRuntimeShutdown(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 10*time.Second)
	defer cancel()
	node := startTestNode(t, ctx)
	doc, err := node.Docs().Create(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer doc.Close()
	events, err := doc.Subscribe(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer events.Close()
	done := make(chan error, 1)
	go func() { _, err := events.Recv(ctx); done <- err }()
	// Ensure the receive is registered and genuinely idle before shutdown.
	for {
		node.runtime.mu.Lock()
		registered := len(node.runtime.ops) > 0
		node.runtime.mu.Unlock()
		if registered {
			break
		}
		select {
		case <-ctx.Done():
			t.Fatal("receive did not register")
		case <-time.After(time.Millisecond):
		}
	}
	if err := node.runtime.Close(); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-done:
		if !errors.Is(err, ErrAlreadyClosed) {
			t.Fatalf("shutdown receive: %v", err)
		}
	case <-time.After(time.Second):
		t.Fatal("runtime shutdown stranded receive")
	}
}
