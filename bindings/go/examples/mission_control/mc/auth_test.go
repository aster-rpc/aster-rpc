//go:build cgo

package mc

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	aster "sdk.getaster.now/go"
	"sdk.getaster.now/go/internal/testroot"
)

// The auth chapter end to end (examples/rust/mission-control/tests/auth.rs
// in Go): the server is configured from an aster.toml exactly as an operator
// writes it; credentials are the files `aster enroll node --attributes`
// produces (signed here by a test root); clients connect with StartClient.
// Each role reaches exactly the methods whose Requires it satisfies.

const audience = "mission-control"

func newKey(t *testing.T, dir, name string) (seed []byte, file string) {
	t.Helper()
	seed = make([]byte, 32)
	if _, err := rand.Read(seed); err != nil {
		t.Fatal(err)
	}
	// `aster key node generate --out <name>.key`: 64 hex.
	file = filepath.Join(dir, name+".key")
	if err := os.WriteFile(file, []byte(hex.EncodeToString(seed)+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	return seed, file
}

// enroll writes what `aster enroll node --subject <id> --audience
// mission-control --attributes roles.json --out <file>` writes.
func enroll(t *testing.T, root *testroot.Root, seed []byte, roles []string, file string) {
	t.Helper()
	chain, err := root.Credential(testroot.NodeID(seed), testroot.RoleClaims(strings.Join(roles, ",")), time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(file, []byte(chain+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
}

func denied(err error) bool { return errors.Is(err, aster.ErrRpcPermissionDenied) }

func TestAuthChapter_RolesInCredentialsGateEachMethod(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 120*time.Second)
	defer cancel()
	dir := t.TempDir()
	// `aster key root generate`, `aster enroll namespace create`.
	root, err := testroot.Generate(audience)
	if err != nil {
		t.Fatal(err)
	}
	var secret [32]byte
	_, _ = rand.Read(secret[:])
	fleet := testroot.Namespace(secret)

	serverSeed, serverKey := newKey(t, dir, "server")
	serverCred := filepath.Join(dir, "server.cred")
	enroll(t, root, serverSeed, nil, serverCred)
	configFile := filepath.Join(dir, "aster.toml")
	toml := fmt.Sprintf(`
[trust]
root_pubkey = %q
enrollment_credential = %q

[network]
secret_key_file = %q
relay_mode = "disabled"
bind_addr = "127.0.0.1:0"

[enrollment]
audience = %q
fleet_namespace = %q
state_path = %q
`, root.PublicKey(), serverCred, serverKey, audience, fleet, filepath.Join(dir, "server-enrollment.json"))
	if err := os.WriteFile(configFile, []byte(toml), 0o600); err != nil {
		t.Fatal(err)
	}
	cfg, err := aster.ConfigFromTOML(configFile)
	if err != nil {
		t.Fatal(err)
	}
	srv, err := aster.StartServer(ctx, aster.ServerOptions{Config: cfg, Services: []*aster.Service{Service(true)}})
	if err != nil {
		t.Fatal(err)
	}
	defer srv.Close()
	address, err := srv.Ticket(ctx)
	if err != nil {
		t.Fatal(err)
	}

	connect := func(name string, roles ...string) *aster.RpcConnection {
		t.Helper()
		seed, key := newKey(t, dir, name)
		credential := filepath.Join(dir, name+".cred")
		enroll(t, root, seed, roles, credential)
		rootKey, _ := hex.DecodeString(root.PublicKey())
		c := aster.DefaultConfig()
		c.RelayMode, c.BindAddr, c.SecretKeyFile = "disabled", "127.0.0.1:0", key
		c.RootPubkey = rootKey
		c.EnrollmentCredentialFile = credential
		c.Enrollment = aster.EnrollmentConfig{Audience: audience, FleetNamespace: fleet}
		client, err := aster.StartClient(ctx, c)
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { _ = client.Close() })
		conn, err := client.ConnectTicket(ctx, address)
		if err != nil {
			t.Fatal(err)
		}
		return conn
	}
	status := func(conn *aster.RpcConnection) (*StatusResponse, error) {
		return aster.Call[StatusRequest, StatusResponse](ctx, conn, "MissionControl", "getStatus", &StatusRequest{AgentID: "a1"})
	}
	tail := func(conn *aster.RpcConnection) error {
		for _, err := range aster.CallServerStream[TailRequest, LogEntry](ctx, conn, "MissionControl", "tailLogs",
			&TailRequest{AgentID: "a1", Level: "info"}) {
			return err // the first item decides
		}
		return nil
	}
	ingest := func(conn *aster.RpcConnection) (*IngestResult, error) {
		return aster.CallClientStream[MetricPoint, IngestResult](ctx, conn, "MissionControl", "ingestMetrics",
			slices.Values([]*MetricPoint{{Name: "cpu", Value: 1}}))
	}
	command := func(conn *aster.RpcConnection) (*CommandResult, error) {
		for r, err := range aster.CallBidiStream[Command, CommandResult](ctx, conn, "MissionControl", "runCommand",
			slices.Values([]*Command{{Command: "uptime"}})) {
			return r, err
		}
		return nil, errors.New("no result")
	}

	// An operator watches the fleet.
	operator := connect("operator", RoleStatus, RoleLogs)
	if r, err := status(operator); err != nil || r.Status != "running" {
		t.Fatalf("operator getStatus: %+v %v", r, err)
	}
	if err := tail(operator); err != nil {
		t.Fatalf("operator tailLogs: %v", err)
	}
	if _, err := ingest(operator); !denied(err) {
		t.Fatalf("operator ingestMetrics: %v", err)
	}
	if _, err := command(operator); !denied(err) {
		t.Fatalf("operator runCommand: %v", err)
	}

	// An agent reports in.
	agent := connect("agent", RoleStatus, RoleIngest)
	if r, err := ingest(agent); err != nil || r.Accepted != 1 {
		t.Fatalf("agent ingestMetrics: %+v %v", r, err)
	}
	if err := tail(agent); !denied(err) {
		t.Fatalf("agent tailLogs: %v", err)
	}

	// An administrator runs commands but has no status role.
	admin := connect("admin", RoleAdmin)
	if r, err := command(admin); err != nil || r.Stdout != "ran: uptime" {
		t.Fatalf("admin runCommand: %+v %v", r, err)
	}
	if err := tail(admin); err != nil {
		t.Fatalf("admin tailLogs: %v", err)
	}
	if _, err := status(admin); !denied(err) {
		t.Fatalf("admin getStatus: %v", err)
	}

	// Every enrolled node may submit logs; no role needed.
	plain := connect("plain")
	if r, err := aster.Call[LogEntry, SubmitLogResult](ctx, plain, "MissionControl", "submitLog", &LogEntry{}); err != nil || !r.Accepted {
		t.Fatalf("plain submitLog: %+v %v", r, err)
	}
	if _, err := status(plain); !denied(err) {
		t.Fatalf("plain getStatus: %v", err)
	}

	// A node with no credential cannot connect at all.
	open := aster.DefaultConfig()
	open.RelayMode, open.BindAddr = "disabled", "127.0.0.1:0"
	stranger, err := aster.StartClient(ctx, open)
	if err != nil {
		t.Fatal(err)
	}
	defer stranger.Close()
	if conn, err := stranger.ConnectTicket(ctx, address); err == nil {
		if _, err := aster.Call[LogEntry, SubmitLogResult](ctx, conn, "MissionControl", "submitLog", &LogEntry{}); err == nil {
			t.Fatal("a node without a credential reached the service")
		}
	}
}
