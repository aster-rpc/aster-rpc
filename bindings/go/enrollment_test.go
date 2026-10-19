//go:build cgo

package aster

import (
	"bytes"
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"iter"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"sdk.getaster.now/go/internal/testroot"
)

// Enrollment through the high-level path only, mirroring
// aster/tests/enrollment_aster_server.rs: StartServer and StartClient
// configured by AsterConfig's trust fields and [enrollment] section. The
// root is played by internal/testroot (root operations belong to the CLI);
// everything the application writes is configuration and Requires.

const enrollFleet = "mission-control"

func enrollSeed(seed byte) []byte { return bytes.Repeat([]byte{seed}, 32) }

func enrollPub(seed byte) string { return testroot.NodeID(enrollSeed(seed)) }

// The root (seed 1) and the fleet namespace (from a fixed secret).
var (
	enrollRoot   = testroot.New(enrollSeed(1), enrollFleet)
	fleetSecret  = Key(bytes.Repeat([]byte{0x55}, 32))
	enrollFleetN = testroot.Namespace(fleetSecret)
)

// credentialFor is a root-issued credential for seed carrying roles in
// aster.role ("" = none).
func credentialFor(t *testing.T, seed byte, roles string) string {
	t.Helper()
	chain, err := enrollRoot.Credential(enrollPub(seed), testroot.RoleClaims(roles), time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	return chain
}

// enrolledCfg is a loopback node (seed) with the root pinned, credential
// inline and the fleet's [enrollment] section.
func enrolledCfg(seed byte, credential string) AsterConfig {
	cfg := testCfg()
	cfg.SecretKey = enrollSeed(seed)
	root, _ := hex.DecodeString(enrollRoot.PublicKey())
	cfg.RootPubkey = root
	cfg.EnrollmentCredentialChain = credential
	cfg.Enrollment = EnrollmentConfig{Audience: enrollFleet, FleetNamespace: enrollFleetN}
	return cfg
}

// serverCfg is the server (seed 2) with a root-issued credential, durable
// state in dir and a fast reconciliation interval.
func serverCfg(t *testing.T, dir string) AsterConfig {
	cfg := enrolledCfg(2, credentialFor(t, 2, ""))
	cfg.Enrollment.StatePath = filepath.Join(dir, "server-enrollment.json")
	cfg.Enrollment.Limits.ReconciliationIntervalMs = 200
	return cfg
}

type opsRequest struct{ N int32 }

func (opsRequest) AsterWireType() string { return "test.enrollment/OpsRequest" }

type opsReply struct{ Roles string }

func (opsReply) AsterWireType() string { return "test.enrollment/OpsReply" }

// opsService: status requires ops.status, logs ops.logs or ops.admin, ping
// is open. Each echoes the caller's aster.role.
func opsService() *Service {
	echo := func(ctx context.Context, _ *opsRequest) (*opsReply, error) {
		info, _ := CallInfoFromContext(ctx)
		for _, a := range info.Attributes {
			if a.Key == "aster.role" {
				return &opsReply{Roles: a.Value}, nil
			}
		}
		return &opsReply{}, nil
	}
	upload := func(ctx context.Context, reqs iter.Seq2[*opsRequest, error]) (*opsReply, error) {
		n := int32(0)
		for r, err := range reqs {
			if err != nil {
				return nil, err
			}
			n += r.N
		}
		return &opsReply{Roles: fmt.Sprint(n)}, nil
	}
	return NewService("Ops", 1,
		Unary("ping", echo),
		Unary("status", echo, Requires(RequireRole("ops.status"))),
		Unary("logs", echo, Requires(RequireAnyOf("ops.logs", "ops.admin"))),
		ClientStream("upload", upload, Requires(RequireRole("ops.admin"))),
	)
}

func startOps(t *testing.T, ctx context.Context, opts ServerOptions) (*AsterServer, *NodeAddr) {
	t.Helper()
	opts.Services = []*Service{opsService()}
	srv, err := StartServer(ctx, opts)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = srv.Close() })
	addr := addrOf(t, srv.Node())
	return srv, &addr
}

func opsCall(ctx context.Context, cl *AsterClient, server *NodeAddr, method string) (string, error) {
	cctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	rc, err := cl.Connect(cctx, server)
	if err != nil {
		return "", err
	}
	reply, err := Call[opsRequest, opsReply](cctx, rc, "Ops", method, &opsRequest{N: 1})
	if err != nil {
		return "", err
	}
	return reply.Roles, nil
}

func startClient(t *testing.T, ctx context.Context, cfg AsterConfig, opts ...ClientOption) *AsterClient {
	t.Helper()
	cl, err := StartClient(ctx, cfg, opts...)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = cl.Close() })
	return cl
}

func addrOf(t *testing.T, n *Node) NodeAddr {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for {
		addr, err := n.NodeAddr()
		if err != nil {
			t.Fatal(err)
		}
		if len(addr.DirectAddresses) > 0 || time.Now().After(deadline) {
			return *addr
		}
		time.Sleep(50 * time.Millisecond)
	}
}

// The roles in a client's credential are what Requires checks, with no
// application code; a credential without the role is refused, and a node
// with no credential cannot reach the service. A revocation the root
// publishes refuses the next call and closes the connection.
func TestEnrollment_CredentialRolesGateRequiresAndRootRevocationClosesTheConnection(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 120*time.Second)
	defer cancel()
	// No replicas: the server holds the fleet namespace and the root
	// publishes into it, as `aster enroll revoke --peer <server>` does.
	srv, server := startOps(t, ctx, ServerOptions{Config: serverCfg(t, t.TempDir())})
	if srv.Enrollment() == nil || srv.Gate() != nil || srv.Issuer() != nil {
		t.Fatal("an enrolled server has Enrollment, no gate and (unconfigured) no issuer")
	}
	if st, err := srv.Enrollment().Status(); err != nil || st.Side != "server" || st.TrustedRoot != enrollRoot.PublicKey() ||
		st.FleetNamespace != enrollFleetN || st.Credential.Subject != enrollPub(2) {
		t.Fatalf("server status %+v %v", st, err)
	}

	operator := startClient(t, ctx, enrolledCfg(5, credentialFor(t, 5, "ops.status, ops.logs")))
	for _, method := range []string{"ping", "status", "logs"} {
		if roles, err := opsCall(ctx, operator, server, method); err != nil || roles != "ops.logs,ops.status" {
			t.Fatalf("operator %s: %q %v", method, roles, err)
		}
	}
	if operator.IsRenewing() {
		t.Fatal("a root-issued credential has no issuer to renew from")
	}

	ingest := startClient(t, ctx, enrolledCfg(6, credentialFor(t, 6, "ops.ingest")))
	if roles, err := opsCall(ctx, ingest, server, "ping"); err != nil || roles != "ops.ingest" {
		t.Fatalf("ingest ping: %q %v", roles, err)
	}
	for _, method := range []string{"status", "logs"} {
		if _, err := opsCall(ctx, ingest, server, method); !errors.Is(err, ErrRpcPermissionDenied) {
			t.Fatalf("ingest %s: want PERMISSION_DENIED, got %v", method, err)
		}
	}
	// The server sees the verified claims (audit), and maps them itself.
	if ev, err := srv.Enrollment().PeerClaims(enrollPub(6)); err != nil || ev.Attributes["aster.role"].Value != "ops.ingest" {
		t.Fatalf("peer claims: %+v %v", ev, err)
	}

	// No enrollment at all: Gate 0 refuses the connection itself.
	stranger := startClient(t, ctx, testCfg())
	if _, err := opsCall(ctx, stranger, server, "ping"); err == nil {
		t.Fatal("a node without a credential reached the service")
	}

	// The root enrolls with the server (a publication session) and delivers
	// a revocation of the operator into the fleet namespace the server holds.
	revoked := make(chan Revocation, 4)
	go func() {
		for {
			rev, err := srv.Enrollment().NextRevocation(ctx)
			if err != nil {
				return
			}
			revoked <- rev
		}
	}()
	root := startClient(t, ctx, enrolledCfg(1, credentialFor(t, 1, "")))
	if _, err := root.Connect(ctx, server); err != nil {
		t.Fatalf("root enrolls with the server: %v", err)
	}
	fleet, err := root.Node().Docs().ImportWriteNamespace(ctx, fleetSecret)
	if err != nil {
		t.Fatal(err)
	}
	defer fleet.Close()
	if err := fleet.SetBytes(ctx, AuthorID(enrollRoot.PublicKey()), testroot.RegistrationKey(enrollPub(5)),
		enrollRoot.Revocation(enrollPub(5), "decommissioned")); err != nil {
		t.Fatal(err)
	}
	if err := fleet.StartSync(ctx, enrollPub(2)); err != nil {
		t.Fatal(err)
	}
	select {
	case rev := <-revoked:
		if rev.Peer != enrollPub(5) || rev.Reason.Code != "node_revoked" || !rev.Reason.Definite {
			t.Fatalf("revocation %+v", rev)
		}
	case <-time.After(30 * time.Second):
		t.Fatal("the server did not revoke the operator")
	}
	if _, err := opsCall(ctx, operator, server, "status"); err == nil {
		t.Fatal("revoked: the next call must be refused")
	}
	if _, err := opsCall(ctx, operator, server, "ping"); err == nil {
		t.Fatal("revoked: the connection must be closed")
	}
	if _, err := srv.Enrollment().PeerClaims(enrollPub(5)); !errors.Is(err, ErrNotFound) {
		t.Fatalf("revoked peer claims: %v", err)
	}
	var admission *AdmissionError
	if _, err := srv.Enrollment().CheckPeer(ctx, enrollPub(5)); !errors.As(err, &admission) || admission.Kind == AdmissionErrorOther {
		t.Fatalf("revoked peer check: %v", err)
	}
	// Enrolling again as the revoked node: the server's denial keeps its
	// admission kind and PERMISSION_DENIED (it used to be INTERNAL
	// "invalid argument: ...").
	again := startClient(t, ctx, enrolledCfg(5, credentialFor(t, 5, "ops.status")))
	if _, err := again.Connect(ctx, server); !errors.Is(err, ErrPermissionDenied) ||
		!errors.As(err, &admission) || admission.Kind != AdmissionErrorDenied || strings.Contains(err.Error(), "invalid argument") {
		t.Fatalf("re-enrolling a revoked node: want PERMISSION_DENIED (denied), got %v", err)
	}
	// Unaffected peers keep working.
	if roles, err := opsCall(ctx, ingest, server, "ping"); err != nil || roles != "ops.ingest" {
		t.Fatalf("ingest after the revocation: %q %v", roles, err)
	}
}

// A Gate-3 refusal of a client-stream call that sends many items is
// PERMISSION_DENIED, not UNAVAILABLE: the server stops the request side
// before the client has sent everything, and the client must still report
// the server's trailer rather than its own failed send.
func TestEnrollment_ClientStreamRefusalIsPermissionDenied(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	_, server := startOps(t, ctx, ServerOptions{Config: serverCfg(t, t.TempDir())})
	ingest := startClient(t, ctx, enrolledCfg(6, credentialFor(t, 6, "ops.ingest")))
	rc, err := ingest.Connect(ctx, server)
	if err != nil {
		t.Fatal(err)
	}
	items := func(yield func(*opsRequest) bool) {
		for i := 0; i < 100; i++ {
			if !yield(&opsRequest{N: 1}) {
				return
			}
		}
	}
	for attempt := 0; attempt < 5; attempt++ {
		_, err := CallClientStream[opsRequest, opsReply](ctx, rc, "Ops", "upload", items)
		if !errors.Is(err, ErrRpcPermissionDenied) {
			t.Fatalf("attempt %d: want PERMISSION_DENIED, got %v", attempt, err)
		}
	}
	operator := startClient(t, ctx, enrolledCfg(5, credentialFor(t, 5, "ops.admin")))
	orc, err := operator.Connect(ctx, server)
	if err != nil {
		t.Fatal(err)
	}
	if reply, err := CallClientStream[opsRequest, opsReply](ctx, orc, "Ops", "upload", items); err != nil || reply.Roles != "100" {
		t.Fatalf("admin upload: %+v %v", reply, err)
	}
}

// Builder overrides of the policy and role mapping (Rust enrollment_policy /
// enrollment_roles) take precedence over the configuration.
func TestEnrollment_RoleMappingOverride(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	off := ""
	_, server := startOps(t, ctx, ServerOptions{
		Config: serverCfg(t, t.TempDir()),
		// Scopes are roles; the aster.role attribute grants nothing.
		EnrollmentRoles: &RoleMapping{Attribute: &off, ScopesAsRoles: true, Tags: map[string][]string{"oncall": {"ops.logs"}}},
	})
	claims := testroot.RoleClaims("ops.admin")
	claims.Scopes, claims.Tags = []string{"ops.status"}, []string{"oncall"}
	chain, err := enrollRoot.Credential(enrollPub(5), claims, time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	cl := startClient(t, ctx, enrolledCfg(5, chain))
	if roles, err := opsCall(ctx, cl, server, "status"); err != nil || roles != "ops.logs,ops.status" {
		t.Fatalf("mapped roles: %q %v", roles, err)
	}
}

// Misconfiguration stops the start with an error that names the problem;
// nothing runs open.
func TestEnrollment_MisconfigurationFailsClosed(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	dir := t.TempDir()
	refused := func(name string, opts ServerOptions, expect string) {
		t.Helper()
		srv, err := StartServer(ctx, opts)
		if err == nil {
			_ = srv.Close()
			t.Fatalf("%s: started", name)
		}
		if !strings.Contains(err.Error(), expect) {
			t.Fatalf("%s: %q not in %v", name, expect, err)
		}
	}
	good := func() AsterConfig { return serverCfg(t, dir) }

	c := good()
	c.Enrollment.Audience = ""
	refused("no audience", ServerOptions{Config: c}, "audience is required")
	c = good()
	c.RootPubkey = nil
	refused("no root", ServerOptions{Config: c}, "no trusted root")
	c = good()
	c.Enrollment.StatePath = ""
	refused("no state", ServerOptions{Config: c}, "keeps denials durably")
	// A server with no credential, or one that does not verify for its key,
	// starts: only a root registration admits it (aster/tests/
	// enrollment_aster_server.rs registered_server_restarts_without_a_valid_credential).
	c = good()
	c.EnrollmentCredentialChain = "/etc/aster/server.cred"
	refused("a path where the chain belongs", ServerOptions{Config: c}, "ASTER_ENROLLMENT_CREDENTIAL)")
	c = good()
	c.Enrollment.Policy = &Policy{"allow_everyone"}
	refused("unknown policy", ServerOptions{Config: c}, "policy")
	refused("policy override", ServerOptions{Config: good(), EnrollmentPolicy: PolicyAllOf()}, "policy")
	c = good()
	c.Enrollment.FleetNamespace = "not-hex"
	refused("namespace", ServerOptions{Config: c}, "fleet_namespace")
	c = enrolledCfg(2, credentialFor(t, 2, ""))
	c.Enrollment = EnrollmentConfig{Enabled: ptrBool(true)}
	refused("enabled alone", ServerOptions{Config: c}, "audience is required")
	gate, err := NewGate(nil)
	if err != nil {
		t.Fatal(err)
	}
	defer gate.Close()
	if _, err := StartServer(ctx, ServerOptions{Config: good(), Gate: gate}); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("gate and enrollment: %v", err)
	}

	// A client: no credential, and server-only issuer settings.
	c = enrolledCfg(5, "")
	if _, err := StartClient(ctx, c); err == nil || !strings.Contains(err.Error(), "no credential") {
		t.Fatalf("client without a credential: %v", err)
	}
	c = enrolledCfg(5, credentialFor(t, 5, ""))
	c.Enrollment.IssuerDelegationFile = filepath.Join(dir, "x")
	if _, err := StartClient(ctx, c); err == nil || !strings.Contains(err.Error(), "server-only") {
		t.Fatalf("client with an issuer: %v", err)
	}
	// Enrollment off (the root alone does not enable it): an open server.
	open := testCfg()
	open.RootPubkey = enrolledCfg(2, "").RootPubkey
	if open.IsEnrollmentEnabled() {
		t.Fatal("the root alone enabled enrollment")
	}
	srv, err := StartServer(ctx, ServerOptions{Config: open})
	if err != nil {
		t.Fatal(err)
	}
	if srv.Enrollment() != nil {
		t.Fatal("enrollment installed without being configured")
	}
	_ = srv.Close()
}

func ptrBool(v bool) *bool { return &v }

func TestEnrollment_PolicyJSON(t *testing.T) {
	p := PolicyAllOf(
		PolicyAnyOf(PolicyScope("session"), PolicyTag("router")),
		PolicyNoneOf(PolicyIssuedBy(strings.Repeat("0", 64))),
		PolicyAttributeEquals("tier", IntAttribute(2)),
		PolicySourceCIDR("10.0.0.0/8"),
		PolicyAudience("kasm"),
		PolicySynchronizedWithin(time.Minute, true),
		PolicyMaxCredentialAge(time.Hour),
		PolicyRequireRegistration(),
		PolicyTolerateStale(),
	)
	got, err := json.Marshal(p)
	if err != nil {
		t.Fatal(err)
	}
	want := `{"all_of":[{"any_of":[{"scope":"session"},{"tag":"router"}]},{"none_of":[{"issued_by":"` +
		strings.Repeat("0", 64) + `"}]},{"attribute_equals":{"key":"tier","value":{"type":"int","value":2}}},` +
		`{"source_cidr":"10.0.0.0/8"},{"audience":"kasm"},{"policy_synchronized_within":{"max_age_secs":60,"require_root":true}},` +
		`{"max_credential_age_secs":3600},"require_registration","tolerate_stale_policy"]}`
	if string(got) != want {
		t.Fatalf("policy JSON\n got %s\nwant %s", got, want)
	}
	// A policy read from configuration round-trips unchanged.
	var back Policy
	if err := json.Unmarshal(got, &back); err != nil {
		t.Fatal(err)
	}
	if again, _ := json.Marshal(&back); string(again) != want {
		t.Fatalf("round trip %s", again)
	}
	var none *Policy
	if b, _ := json.Marshal(none); string(b) != "null" {
		t.Fatalf("nil policy %s", b)
	}
}
