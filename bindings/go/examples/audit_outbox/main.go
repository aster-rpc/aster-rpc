//go:build cgo

// Example: durable audit events from Go, with an HTTP receiver and the
// sender's acknowledgement log.
//
//	go run . receiver -listen 127.0.0.1:8787 -dir ./state/receiver
//	go run . sender -endpoint http://127.0.0.1:8787/audit -dir ./state/outbox
//	go run .                       # demo: both in one process
//
// The sender opens the outbox (aster.OpenAuditOutbox), submits daily usage
// reports, flushes, and reads which events the receiver acknowledged
// (AuditOutbox.Acknowledgements / WatchAcknowledgements). The receiver
// (receiver.go) implements the receiver rules of docs/audit-outbox.md.
package main

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/signal"
	"time"

	aster "sdk.getaster.now/go"
)

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()
	var err error
	args := os.Args[1:]
	command := "demo"
	if len(args) > 0 {
		command, args = args[0], args[1:]
	}
	switch command {
	case "receiver":
		err = receiverMain(ctx, args)
	case "sender":
		err = senderMain(ctx, args)
	case "demo":
		err = Run(ctx, os.Stdout)
	default:
		err = fmt.Errorf("usage: audit_outbox [demo|receiver|sender] [flags]")
	}
	if err != nil && !errors.Is(err, context.Canceled) {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func receiverMain(ctx context.Context, args []string) error {
	flags := flag.NewFlagSet("receiver", flag.ExitOnError)
	listen := flags.String("listen", "127.0.0.1:8787", "address to listen on")
	dir := flags.String("dir", "./state/receiver", "directory for stored envelopes")
	_ = flags.Parse(args)
	server := &http.Server{Addr: *listen, Handler: routes(&Receiver{Dir: *dir}), ReadHeaderTimeout: 5 * time.Second}
	go func() {
		<-ctx.Done()
		_ = server.Close()
	}()
	fmt.Printf("receiver: http://%s/audit, storing in %s\n", *listen, *dir)
	if err := server.ListenAndServe(); !errors.Is(err, http.ErrServerClosed) {
		return err
	}
	return nil
}

func senderMain(ctx context.Context, args []string) error {
	flags := flag.NewFlagSet("sender", flag.ExitOnError)
	endpoint := flags.String("endpoint", "http://127.0.0.1:8787/audit", "receiver endpoint")
	dir := flags.String("dir", "./state/outbox", "outbox directory (a persistent volume in production)")
	count := flags.Int("count", 3, "usage reports to submit")
	after := flags.Uint64("after", 0, "print acknowledgements after this cursor")
	_ = flags.Parse(args)
	return send(ctx, os.Stdout, *endpoint, *dir, *count, *after)
}

func routes(receiver *Receiver) http.Handler {
	mux := http.NewServeMux()
	mux.Handle("/audit", receiver)
	return mux
}

// send is the sender side: open, submit, flush, then read the acknowledgement log.
func send(ctx context.Context, out io.Writer, endpoint, dir string, count int, after uint64) error {
	rt, err := aster.NewRuntime(ctx, aster.DefaultRuntimeConfig())
	if err != nil {
		return err
	}
	defer rt.Close()
	outbox, err := aster.OpenAuditOutbox(ctx, rt, aster.AuditOutboxConfig{
		Directory:   dir,
		Destination: aster.AuditDestination{Endpoint: endpoint},
	})
	if err != nil {
		return err
	}
	defer outbox.Close(context.Background())
	if _, err := submitReports(ctx, out, outbox, count); err != nil {
		return err
	}
	// Flush returns once the receiver acknowledged everything submitted so far.
	if err := outbox.Flush(ctx, 30*time.Second); err != nil {
		return fmt.Errorf("flush: %w", err)
	}
	acks, err := outbox.Acknowledgements(ctx, after, aster.AuditAcknowledgementPageMax)
	if err != nil {
		return err
	}
	printAcknowledgements(out, acks)
	return nil
}

func submitReports(ctx context.Context, out io.Writer, outbox *aster.AuditOutbox, count int) ([]string, error) {
	var ids []string
	for i := 1; i <= count; i++ {
		payload, _ := json.Marshal(map[string]any{"report": "daily-usage", "day": i, "sessions": 40 + i})
		// Submit returns once the event is durable locally, not delivered.
		id, err := outbox.Submit(ctx, aster.AuditEvent{
			TimestampMS: uint64(time.Now().UnixMilli()),
			Kind:        "usage.report",
			Payload:     payload,
		})
		if err != nil {
			return nil, fmt.Errorf("submit: %w", err)
		}
		fmt.Fprintf(out, "submitted %s\n", short(id))
		ids = append(ids, id)
	}
	return ids, nil
}

func printAcknowledgements(out io.Writer, acks []aster.AuditAcknowledgement) {
	fmt.Fprintf(out, "acknowledgement log (%d entries):\n", len(acks))
	for _, ack := range acks {
		fmt.Fprintf(out, "  cursor=%d seq=%d id=%s batch=%s at=%s\n",
			ack.Cursor, ack.Sequence, short(ack.EventID), ack.BatchID[:12], ack.AcknowledgedAt().UTC().Format(time.RFC3339Nano))
	}
}

func short(id string) string {
	if len(id) > 60 {
		return id[:60] + "..."
	}
	return id
}

// Run is the demo: receiver and sender in one process, with a lost
// acknowledgement (the sender retries the identical batch and the receiver
// acknowledges it again), then a forged retry carrying different bytes under
// a stored ID, which the receiver refuses with 409 integrity_conflict.
func Run(ctx context.Context, out io.Writer) error {
	state, err := os.MkdirTemp("", "aster-audit-example-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(state)
	receiver := &Receiver{Dir: state + "/receiver", FailNextAfterCommit: true}
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return err
	}
	server := &http.Server{Handler: routes(receiver), ReadHeaderTimeout: 5 * time.Second}
	go func() { _ = server.Serve(listener) }()
	defer server.Close()
	endpoint := "http://" + listener.Addr().String() + "/audit"
	fmt.Fprintf(out, "receiver listening on %s\n", endpoint)

	rt, err := aster.NewRuntime(ctx, aster.DefaultRuntimeConfig())
	if err != nil {
		return err
	}
	defer rt.Close()
	outbox, err := aster.OpenAuditOutbox(ctx, rt, aster.AuditOutboxConfig{
		Directory:   state + "/outbox",
		Destination: aster.AuditDestination{Endpoint: endpoint},
	})
	if err != nil {
		return err
	}
	defer outbox.Close(context.Background())

	// A live consumer of acknowledgements, e.g. to update an admin view.
	watchCtx, stopWatch := context.WithCancel(ctx)
	defer stopWatch()
	watched, watchErr := outbox.WatchAcknowledgements(watchCtx, 0)

	ids, err := submitReports(ctx, out, outbox, 3)
	if err != nil {
		return err
	}
	if err := outbox.Flush(ctx, 30*time.Second); err != nil {
		return fmt.Errorf("flush: %w", err)
	}
	for range ids {
		select {
		case ack, ok := <-watched:
			if !ok {
				return fmt.Errorf("acknowledgement watch stopped: %v", <-watchErr)
			}
			fmt.Fprintf(out, "watch: receiver acknowledged %s (cursor %d)\n", short(ack.EventID), ack.Cursor)
		case <-ctx.Done():
			return ctx.Err()
		}
	}
	acks, err := outbox.Acknowledgements(ctx, 0, aster.AuditAcknowledgementPageMax)
	if err != nil {
		return err
	}
	printAcknowledgements(out, acks)
	if len(acks) != len(ids) {
		return fmt.Errorf("acknowledgement log has %d entries, want %d", len(acks), len(ids))
	}
	for i, ack := range acks {
		if ack.EventID != ids[i] {
			return fmt.Errorf("acknowledgement %d is %s, want %s", i, ack.EventID, ids[i])
		}
	}
	receiver.mu.Lock()
	attempts := receiver.Attempts
	receiver.mu.Unlock()
	if attempts < 2 {
		return fmt.Errorf("receiver saw %d attempts; the lost acknowledgement was not retried", attempts)
	}
	fmt.Fprintf(out, "lost acknowledgement: receiver saw %d attempts, stored each event once\n", attempts)

	// The receiver holds the exact envelope bytes. Replaying the identical
	// batch is acknowledged again; the same ID with other bytes is refused.
	stored, err := receiver.Envelope(ids[0])
	if err != nil {
		return err
	}
	status, body, err := post(ctx, endpoint, ids[0], stored)
	if err != nil {
		return err
	}
	want := fmt.Sprintf(`{"batch_id":%q,"durable":true,"version":1}`, BatchID(ids[:1]))
	if status != http.StatusOK || body != want {
		return fmt.Errorf("identical retry: %d %s", status, body)
	}
	fmt.Fprintf(out, "identical retry: %d %s\n", status, body)
	forged := bytes.Replace(stored, []byte(`"daily-usage"`), []byte(`"forged-usage"`), 1)
	status, body, err = post(ctx, endpoint, ids[0], forged)
	if err != nil {
		return err
	}
	if status != http.StatusConflict || body != `{"error":"integrity_conflict","version":1}` {
		return fmt.Errorf("conflicting retry: %d %s", status, body)
	}
	fmt.Fprintf(out, "same ID, different bytes: %d %s\n", status, body)
	return nil
}

// post sends a one-event batch the way the outbox does.
func post(ctx context.Context, endpoint, id string, envelope []byte) (int, string, error) {
	batch, _ := json.Marshal(map[string]any{
		"version":  1,
		"batch_id": BatchID([]string{id}),
		"events":   []map[string]string{{"event_id": id, "envelope_base64": base64.StdEncoding.EncodeToString(envelope)}},
	})
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, bytes.NewReader(batch))
	if err != nil {
		return 0, "", err
	}
	req.Header.Set("Content-Type", "application/json")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return 0, "", err
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(io.LimitReader(resp.Body, 4096))
	return resp.StatusCode, string(body), err
}
