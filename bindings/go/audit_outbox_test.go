//go:build cgo

package aster

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"
)

type auditReceiverProcess struct {
	cmd      *exec.Cmd
	endpoint string
	stopped  bool
}

func auditTestPython() string {
	if configured := os.Getenv("ASTER_TEST_PYTHON"); configured != "" {
		return configured
	}
	if runtime.GOOS == "windows" {
		return "python"
	}
	return "python3"
}

func startAuditReceiver(t *testing.T, database, port string) *auditReceiverProcess {
	t.Helper()
	command := exec.Command(auditTestPython(), "../../tests/audit_outbox/receiver.py", "--database", database, "--port", port)
	pipe, err := command.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err := command.Start(); err != nil {
		t.Fatal(err)
	}
	receiver := &auditReceiverProcess{cmd: command}
	t.Cleanup(receiver.stop)
	result := make(chan string, 1)
	go func() {
		reader := bufio.NewScanner(pipe)
		if reader.Scan() {
			result <- reader.Text()
		} else {
			result <- ""
		}
	}()
	select {
	case receiver.endpoint = <-result:
		if !strings.HasPrefix(receiver.endpoint, "http://127.0.0.1:") {
			t.Fatal("receiver did not report endpoint")
		}
	case <-time.After(10 * time.Second):
		t.Fatal("receiver startup timeout")
	}
	return receiver
}
func (p *auditReceiverProcess) stop() {
	if !p.stopped {
		p.stopped = true
		_ = p.cmd.Process.Kill()
		_ = p.cmd.Wait()
	}
}

func TestAuditOutbox_ReopenDeliveryAndTypedError(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 25*time.Second)
	defer cancel()
	database := filepath.Join(t.TempDir(), "receiver.sqlite")
	receiver := startAuditReceiver(t, database, "0")
	endpoint := receiver.endpoint
	parsed, _ := url.Parse(endpoint)
	receiver.stop()
	rt, err := NewRuntime(ctx, DefaultRuntimeConfig())
	if err != nil {
		t.Fatal(err)
	}
	defer rt.Close()
	cfg := AuditOutboxConfig{Directory: filepath.Join(t.TempDir(), "outbox"), Destination: AuditDestination{Endpoint: endpoint}}
	outbox, err := OpenAuditOutbox(ctx, rt, cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer outbox.Close(context.Background())
	id, err := outbox.Submit(ctx, AuditEvent{TimestampMS: ^uint64(0), Kind: "go.test", Payload: json.RawMessage(`{"value":42}`)})
	if err != nil {
		t.Fatal(err)
	}
	status, err := outbox.Status(ctx)
	if err != nil || status.PendingRecords != 1 {
		t.Fatalf("pending status %+v: %v", status, err)
	}
	_, err = outbox.Submit(ctx, AuditEvent{Kind: "oversize", Payload: json.RawMessage(`{"data":"` + strings.Repeat("x", 70*1024) + `"}`)})
	var auditErr *AuditOutboxError
	if !errors.As(err, &auditErr) || auditErr.Kind != AuditEventTooLarge {
		t.Fatalf("expected EventTooLarge: %v", err)
	}
	if err := outbox.Close(ctx); err != nil {
		t.Fatal(err)
	}
	receiver = startAuditReceiver(t, database, parsed.Port())
	outbox, err = OpenAuditOutbox(ctx, rt, cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer outbox.Close(context.Background())
	if err := outbox.Flush(ctx, 10*time.Second); err != nil {
		t.Fatal(err)
	}
	status, err = outbox.Status(ctx)
	if err != nil || status.PendingRecords != 0 {
		t.Fatalf("drained %+v: %v", status, err)
	}
	receiver.stop()
	check := exec.Command(auditTestPython(), "-c", "import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute('SELECT id FROM events').fetchone()[0])", database)
	found, err := check.Output()
	if err != nil || strings.TrimSpace(string(found)) != id {
		t.Fatalf("receiver ID %q vs %q: %v", found, id, err)
	}
}

func TestAuditOutbox_CloneAndCanceledClose(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	rt, err := NewRuntime(ctx, DefaultRuntimeConfig())
	if err != nil {
		t.Fatal(err)
	}
	defer rt.Close()
	cfg := AuditOutboxConfig{Directory: filepath.Join(t.TempDir(), "outbox"), Destination: AuditDestination{Endpoint: "http://127.0.0.1:1/audit"}}
	outbox, err := OpenAuditOutbox(ctx, rt, cfg)
	if err != nil {
		t.Fatal(err)
	}
	other, err := outbox.Clone()
	if err != nil {
		t.Fatal(err)
	}
	defer other.Close(context.Background())
	stopped, stop := context.WithCancel(ctx)
	stop()
	// Completion can win the race; either result still must initiate shared close.
	if err := outbox.Close(stopped); err != nil && !errors.Is(err, context.Canceled) {
		t.Fatal(err)
	}
	// A subsequent closer waits for the original background shutdown rather
	// than returning success merely because the handle was marked closed.
	if err := outbox.Close(ctx); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(5 * time.Second)
	for {
		reopened, openErr := OpenAuditOutbox(ctx, rt, cfg)
		if openErr == nil {
			_ = reopened.Close(ctx)
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("canceled close left shared lock held: %v", openErr)
		}
		time.Sleep(10 * time.Millisecond)
	}
	if _, err := other.Submit(ctx, AuditEvent{Kind: "closed", Payload: json.RawMessage(`{}`)}); err == nil {
		t.Fatal("shared clone accepted after close")
	}
}

func TestAuditOutbox_BoundedInputsNeverReachNativeAdmission(t *testing.T) {
	ctx := context.Background()
	box := &AuditOutbox{eventLimit: auditDefaultEventBytes}
	large := strings.Repeat("x", 2*auditMaxEventBytes)
	assertKind := func(err error, kind AuditErrorKind) {
		t.Helper()
		var auditErr *AuditOutboxError
		if !errors.As(err, &auditErr) || auditErr.Kind != kind {
			t.Fatalf("expected %v: %v", kind, err)
		}
	}
	input := AuditEvent{Kind: "large", Payload: json.RawMessage(large)}
	allocs := testing.AllocsPerRun(5, func() {
		_, err := box.Submit(ctx, input)
		assertKind(err, AuditEventTooLarge)
	})
	if allocs > 8 {
		t.Fatalf("oversized raw payload was serialized: %.0f allocations", allocs)
	}
	_, err := box.Submit(ctx, AuditEvent{Kind: large, Payload: json.RawMessage(`{}`)})
	assertKind(err, AuditInvalidEvent)
	_, err = box.Submit(ctx, AuditEvent{Kind: "expanded", Payload: json.RawMessage(`{"data":"` + strings.Repeat("<", 13*1024) + `"}`)})
	assertKind(err, AuditEventTooLarge)
	assertKind(box.Quarantine(ctx, "id", large, "operator"), AuditInvalidEvent)
	assertKind(box.Replay(ctx, large), AuditIntegrityConflict)
	_, err = box.Export(ctx, large)
	assertKind(err, AuditIntegrityConflict)
	assertKind(box.Reconfigure(ctx, AuditDestination{Endpoint: "http://127.0.0.1/audit", Authorization: &large}), AuditInvalidConfig)
	_, err = OpenAuditOutbox(ctx, &Runtime{}, AuditOutboxConfig{Directory: large,
		Destination: AuditDestination{Endpoint: "http://127.0.0.1/audit"}})
	assertKind(err, AuditInvalidConfig)
}
