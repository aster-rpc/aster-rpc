//go:build cgo

package main

// An HTTP receiver for the Aster durable audit protocol v1
// (docs/audit-outbox.md, "HTTP protocol v1"). Pure Go, no Aster dependency:
// a Go service embeds Receiver as an ordinary http.Handler.
//
// The rules it implements:
//
//   - Validate the batch: version 1, 1-64 events, at most 1 MiB of decoded
//     envelopes, a request of at most 1,420,000 bytes, and a batch_id equal to
//     the digest of the ordered event IDs.
//   - Validate each event: a well-formed full ID, canonical padded base64,
//     SHA-256 of the decoded bytes equal to the ID's digest, and an envelope
//     whose outbox_id and sequence match the ID.
//   - Deduplicate on the WHOLE event ID, never on the sequence alone.
//   - Store the exact decoded envelope bytes; never re-serialize parsed JSON.
//   - Commit every event durably BEFORE acknowledging, then answer
//     {"version":1,"batch_id":<batch_id>,"durable":true}.
//   - An identical retry (same ID, same bytes) is acknowledged again.
//   - The same ID with different bytes, or bytes that do not match their ID,
//     is answered 409 {"version":1,"error":"integrity_conflict"}.
//
// Storage here is one file per event in a directory, written to a temporary
// file, fsynced, renamed and the directory fsynced. Replace it with your
// database; keep the commit-before-ACK order.

import (
	"bytes"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"runtime"
	"strconv"
	"sync"
)

const (
	maxEnvelopeBytes = 1024 * 1024
	maxBatchBytes    = 1024 * 1024
	maxRequestBytes  = 1_420_000
	maxBatchEvents   = 64
)

var eventIDPattern = regexp.MustCompile(`^aster-audit-v1:([0-9a-f]{32}):([1-9][0-9]{0,19}):([0-9a-f]{64})$`)

var (
	errInvalid           = errors.New("invalid audit batch")
	errIntegrityConflict = errors.New("integrity conflict")
)

// Receiver stores audit envelopes durably under Dir.
type Receiver struct {
	Dir string

	mu sync.Mutex
	// Attempts counts POSTs that reached validation (for the demo and tests).
	Attempts int
	// FailNextAfterCommit, when set, commits the next batch and then answers
	// 503 instead of the acknowledgement: what a lost reply looks like to the
	// sender, which must retry the identical batch.
	FailNextAfterCommit bool
}

type wireEvent struct {
	EventID        string `json:"event_id"`
	EnvelopeBase64 string `json:"envelope_base64"`
}

type wireBatch struct {
	Version int         `json:"version"`
	BatchID string      `json:"batch_id"`
	Events  []wireEvent `json:"events"`
}

type envelope struct {
	Version  int    `json:"version"`
	OutboxID string `json:"outbox_id"`
	Sequence uint64 `json:"sequence"`
}

type verified struct {
	id    string
	bytes []byte
}

// BatchID is SHA-256 over "aster-audit-batch-v1\x00" followed by, for each
// ordered full event ID, its UTF-8 length as a big-endian uint32 and its bytes.
func BatchID(ids []string) string {
	h := sha256.New()
	h.Write([]byte("aster-audit-batch-v1\x00"))
	var length [4]byte
	for _, id := range ids {
		binary.BigEndian.PutUint32(length[:], uint32(len(id)))
		h.Write(length[:])
		h.Write([]byte(id))
	}
	return hex.EncodeToString(h.Sum(nil))
}

func (r *Receiver) ServeHTTP(w http.ResponseWriter, req *http.Request) {
	if req.Method != http.MethodPost {
		respond(w, http.StatusMethodNotAllowed, map[string]any{"error": "method"})
		return
	}
	if req.ContentLength > maxRequestBytes {
		respond(w, http.StatusRequestEntityTooLarge, map[string]any{"error": "size"})
		return
	}
	body, err := io.ReadAll(io.LimitReader(req.Body, maxRequestBytes+1))
	if err != nil || len(body) > maxRequestBytes {
		respond(w, http.StatusRequestEntityTooLarge, map[string]any{"error": "size"})
		return
	}
	batch, events, err := verifyBatch(body)
	switch {
	case errors.Is(err, errIntegrityConflict):
		respond(w, http.StatusConflict, map[string]any{"version": 1, "error": "integrity_conflict"})
		return
	case errors.Is(err, errTooLarge):
		respond(w, http.StatusRequestEntityTooLarge, map[string]any{"error": "batch size"})
		return
	case err != nil:
		// 422 stalls the sender on this record until an operator acts: only
		// answer it for requests that are really malformed.
		respond(w, http.StatusUnprocessableEntity, map[string]any{"error": "invalid record"})
		return
	}

	r.mu.Lock()
	defer r.mu.Unlock()
	r.Attempts++
	if err := r.commit(events); err != nil {
		if errors.Is(err, errIntegrityConflict) {
			respond(w, http.StatusConflict, map[string]any{"version": 1, "error": "integrity_conflict"})
			return
		}
		respond(w, http.StatusServiceUnavailable, map[string]any{"error": "storage unavailable"})
		return
	}
	// Every event is durable; only now may the batch be acknowledged.
	if r.FailNextAfterCommit {
		r.FailNextAfterCommit = false
		respond(w, http.StatusServiceUnavailable, map[string]any{"error": "injected lost acknowledgement"})
		return
	}
	respond(w, http.StatusOK, map[string]any{"version": 1, "batch_id": batch.BatchID, "durable": true})
}

var errTooLarge = errors.New("batch too large")

func verifyBatch(body []byte) (wireBatch, []verified, error) {
	var batch wireBatch
	decoder := json.NewDecoder(bytes.NewReader(body))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&batch); err != nil {
		return batch, nil, errInvalid
	}
	if batch.Version != 1 || len(batch.Events) < 1 || len(batch.Events) > maxBatchEvents {
		return batch, nil, errInvalid
	}
	events := make([]verified, 0, len(batch.Events))
	ids := make([]string, 0, len(batch.Events))
	total := 0
	for _, event := range batch.Events {
		v, err := verifyEvent(event)
		if err != nil {
			return batch, nil, err
		}
		total += len(v.bytes)
		if total > maxBatchBytes {
			return batch, nil, errTooLarge
		}
		events = append(events, v)
		ids = append(ids, v.id)
	}
	if batch.BatchID != BatchID(ids) {
		return batch, nil, errIntegrityConflict
	}
	return batch, events, nil
}

func verifyEvent(event wireEvent) (verified, error) {
	match := eventIDPattern.FindStringSubmatch(event.EventID)
	if match == nil {
		return verified{}, errInvalid
	}
	sequence, err := strconv.ParseUint(match[2], 10, 64)
	if err != nil {
		return verified{}, errInvalid
	}
	if len(event.EnvelopeBase64) > base64.StdEncoding.EncodedLen(maxEnvelopeBytes) {
		return verified{}, errTooLarge
	}
	raw, err := base64.StdEncoding.Strict().DecodeString(event.EnvelopeBase64)
	if err != nil || base64.StdEncoding.EncodeToString(raw) != event.EnvelopeBase64 {
		return verified{}, errInvalid
	}
	digest := sha256.Sum256(raw)
	if hex.EncodeToString(digest[:]) != match[3] {
		return verified{}, errIntegrityConflict
	}
	var header envelope
	if err := json.Unmarshal(raw, &header); err != nil || header.Version != 1 {
		return verified{}, errInvalid
	}
	if header.OutboxID != match[1] || header.Sequence != sequence {
		return verified{}, errIntegrityConflict
	}
	return verified{id: event.EventID, bytes: raw}, nil
}

// path names an event's file by a digest of its WHOLE ID, so two IDs that
// share a sequence never share a file.
func (r *Receiver) path(id string) string {
	sum := sha256.Sum256([]byte(id))
	return filepath.Join(r.Dir, hex.EncodeToString(sum[:])+".envelope")
}

// commit checks every event against what is stored, then durably writes the
// new ones. A conflict anywhere writes nothing for this batch.
func (r *Receiver) commit(events []verified) error {
	if err := os.MkdirAll(r.Dir, 0o700); err != nil {
		return err
	}
	var fresh []verified
	for _, event := range events {
		stored, err := os.ReadFile(r.path(event.id))
		switch {
		case err == nil && bytes.Equal(stored, event.bytes):
			// Identical retry: already durable, acknowledge again.
		case err == nil:
			return errIntegrityConflict
		case errors.Is(err, os.ErrNotExist):
			fresh = append(fresh, event)
		default:
			return err
		}
	}
	for _, event := range fresh {
		if err := writeDurably(r.Dir, r.path(event.id), event.bytes); err != nil {
			return err
		}
	}
	return syncDir(r.Dir)
}

// Envelope returns the exact stored bytes for a full event ID.
func (r *Receiver) Envelope(id string) ([]byte, error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	return os.ReadFile(r.path(id))
}

func writeDurably(dir, path string, data []byte) error {
	file, err := os.CreateTemp(dir, ".incoming-*")
	if err != nil {
		return err
	}
	tmp := file.Name()
	defer os.Remove(tmp)
	if _, err := file.Write(data); err != nil {
		file.Close()
		return err
	}
	if err := file.Sync(); err != nil {
		file.Close()
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

func syncDir(dir string) error {
	if runtime.GOOS == "windows" {
		return nil // Windows cannot fsync a directory handle; rename is the commit point.
	}
	d, err := os.Open(dir)
	if err != nil {
		return err
	}
	defer d.Close()
	if err := d.Sync(); err != nil {
		return fmt.Errorf("sync %s: %w", dir, err)
	}
	return nil
}

func respond(w http.ResponseWriter, status int, payload map[string]any) {
	body, _ := json.Marshal(payload)
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Content-Length", strconv.Itoa(len(body)))
	w.WriteHeader(status)
	_, _ = w.Write(body)
}
