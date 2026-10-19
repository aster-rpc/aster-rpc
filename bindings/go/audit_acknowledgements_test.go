//go:build cgo

package aster

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"
	"time"
)

// ackingReceiver acknowledges every batch by echoing its digest. It does not
// store anything: these tests exercise the sender's acknowledgement log.
func ackingReceiver(t *testing.T) string {
	t.Helper()
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var batch struct {
			BatchID string `json:"batch_id"`
		}
		if err := json.NewDecoder(r.Body).Decode(&batch); err != nil {
			w.WriteHeader(http.StatusUnprocessableEntity)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{"version": 1, "batch_id": batch.BatchID, "durable": true})
	}))
	t.Cleanup(server.Close)
	return server.URL + "/audit"
}

func TestAuditOutbox_AcknowledgementLogPagesAndWatches(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	rt, err := NewRuntime(ctx, DefaultRuntimeConfig())
	if err != nil {
		t.Fatal(err)
	}
	defer rt.Close()
	entries := uint64(1000)
	outbox, err := OpenAuditOutbox(ctx, rt, AuditOutboxConfig{
		Directory:                    filepath.Join(t.TempDir(), "outbox"),
		Destination:                  AuditDestination{Endpoint: ackingReceiver(t)},
		AcknowledgementLogMaxEntries: &entries,
	})
	if err != nil {
		t.Fatal(err)
	}
	defer outbox.Close(context.Background())

	var invalid *AuditOutboxError
	if _, err := outbox.Acknowledgements(ctx, 0, 0); !errors.As(err, &invalid) || invalid.Kind != AuditInvalidEvent {
		t.Fatalf("limit 0: %v", err)
	}
	if _, err := outbox.WaitAcknowledgements(ctx, 0, 1, 2*time.Hour); !errors.As(err, &invalid) {
		t.Fatalf("wait above one hour: %v", err)
	}
	// short-wait-ok: an empty log must return an empty page; the bound is the idle wait under test
	if page, err := outbox.WaitAcknowledgements(ctx, 0, 10, 20*time.Millisecond); err != nil || len(page) != 0 {
		t.Fatalf("empty wait: %v %v", page, err)
	}

	watchCtx, stopWatch := context.WithCancel(ctx)
	acks, watchErr := outbox.WatchAcknowledgements(watchCtx, 0)
	var ids []string
	for i := 0; i < 3; i++ {
		id, err := outbox.Submit(ctx, AuditEvent{TimestampMS: uint64(time.Now().UnixMilli()), Kind: "go.ack", Payload: json.RawMessage(`{"n":1}`)})
		if err != nil {
			t.Fatal(err)
		}
		ids = append(ids, id)
	}
	if err := outbox.Flush(ctx, 30*time.Second); err != nil {
		t.Fatal(err)
	}
	for i, id := range ids {
		select {
		case ack := <-acks:
			if ack.EventID != id || ack.Cursor != uint64(i+1) || len(ack.BatchID) != 64 || ack.AcknowledgedAt().IsZero() {
				t.Fatalf("watched ack %d: %+v (want %s)", i, ack, id)
			}
		case <-ctx.Done():
			t.Fatal("watch did not deliver every acknowledgement")
		}
	}
	stopWatch()
	for range acks {
	}
	if err := <-watchErr; err != nil {
		t.Fatalf("watch ended with %v", err)
	}

	all, err := outbox.Acknowledgements(ctx, 0, 100)
	if err != nil || len(all) != 3 {
		t.Fatalf("page: %v %v", all, err)
	}
	for i, ack := range all {
		if ack.EventID != ids[i] || ack.Sequence != uint64(i+1) {
			t.Fatalf("ack %d: %+v", i, ack)
		}
	}
	if page, err := outbox.Acknowledgements(ctx, 2, 10); err != nil || len(page) != 1 || page[0].Cursor != 3 {
		t.Fatalf("after cursor 2: %v %v", page, err)
	}
	status, err := outbox.Status(ctx)
	if err != nil || status.PendingRecords != 0 {
		t.Fatalf("status %+v: %v", status, err)
	}

	tooMany := uint64(1_000_001)
	_, err = OpenAuditOutbox(ctx, rt, AuditOutboxConfig{Directory: t.TempDir(),
		Destination: AuditDestination{Endpoint: "http://127.0.0.1:1/audit"}, AcknowledgementLogMaxEntries: &tooMany})
	if !errors.As(err, &invalid) || invalid.Kind != AuditInvalidConfig {
		t.Fatalf("entry bound: %v", err)
	}
}
