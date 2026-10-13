//go:build cgo

package aster

import (
	"bytes"
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
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
	return NewService("Ops", 1,
		Unary("ping", echo),
		Unary("status", echo, Requires(RequireRole("ops.status"))),
		Unary("logs", echo, Requires(RequireAnyOf("ops.logs", "ops.admin"))),
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

// The credential sets the maximum; runtime settings can only restrict. A
// credential granting ops.status,ops.logs restricted to ops.status refuses
// logs on the next call; SetPeerAttributes cannot grant an enrolled peer a
// role; clearing restores the credential roles; a peer without a session
// cannot be restricted.
func TestEnrollment_RuntimeRestrictionRefusesTheNextCall(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 120*time.Second)
	defer cancel()
	srv, server := startOps(t, ctx, ServerOptions{Config: serverCfg(t, t.TempDir())})
	client := startClient(t, ctx, enrolledCfg(5, credentialFor(t, 5, "ops.status,ops.logs")))
	peer := enrollPub(5)
	if roles, err := opsCall(ctx, client, server, "logs"); err != nil || roles != "ops.logs,ops.status" {
		t.Fatalf("logs before the restriction: %q %v", roles, err)
	}

	if err := srv.RestrictPeerRoles(peer, []string{"ops.status"}); err != nil {
		t.Fatal(err)
	}
	if roles, err := opsCall(ctx, client, server, "status"); err != nil || roles != "ops.status" {
		t.Fatalf("status after the restriction: %q %v", roles, err)
	}
	if _, err := opsCall(ctx, client, server, "logs"); !errors.Is(err, ErrRpcPermissionDenied) {
		t.Fatalf("logs after the restriction: want PERMISSION_DENIED, got %v", err)
	}

	err := srv.SetPeerAttributes(peer, map[string]string{"aster.role": "ops.admin"})
	if !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("a runtime role grant to an enrolled peer: want INVALID_ARGUMENT, got %v", err)
	}
	if _, err := opsCall(ctx, client, server, "logs"); !errors.Is(err, ErrRpcPermissionDenied) {
		t.Fatalf("logs after the refused grant: want PERMISSION_DENIED, got %v", err)
	}

	if err := srv.ClearPeerRestriction(peer); err != nil {
		t.Fatal(err)
	}
	if roles, err := opsCall(ctx, client, server, "logs"); err != nil || roles != "ops.logs,ops.status" {
		t.Fatalf("logs after clearing: %q %v", roles, err)
	}
	if err := srv.RestrictPeerRoles(enrollPub(9), nil); !errors.Is(err, ErrNotFound) {
		t.Fatalf("restricting a peer without a session: want NOT_FOUND, got %v", err)
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
	// Unaffected peers keep working.
	if roles, err := opsCall(ctx, ingest, server, "ping"); err != nil || roles != "ops.ingest" {
		t.Fatalf("ingest after the revocation: %q %v", roles, err)
	}
}

// A server configured as a delegated issuer serves renewals; a client whose
// credential that server signed renews automatically and stays admitted,
// with its roles, across several short lifetimes. The server synchronizes
// policy from a configured replica (the root).
func TestEnrollment_ConfigDrivenIssuerRenewsClientCredentials(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 120*time.Second)
	defer cancel()
	dir := t.TempDir()

	// The root as a policy replica: it holds the fleet namespace.
	rootCfg := enrolledCfg(1, credentialFor(t, 1, ""))
	rootCfg.Enrollment.StatePath = filepath.Join(dir, "root.json")
	rootSrv, err := StartServer(ctx, ServerOptions{Config: rootCfg})
	if err != nil {
		t.Fatal(err)
	}
	defer rootSrv.Close()
	addrOf(t, rootSrv.Node())
	rootTicket, err := rootSrv.Ticket(ctx)
	if err != nil {
		t.Fatal(err)
	}

	delegation, err := enrollRoot.Delegation(enrollPub(2), []string{}, []string{"aster.role"}, time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	cfg := serverCfg(t, dir)
	cfg.Enrollment.Replicas = []string{rootTicket}
	cfg.Enrollment.IssuerDelegationFile = filepath.Join(dir, "issuer.chain")
	cfg.Enrollment.IssuerLifetimeSecs = 6
	if err := os.WriteFile(cfg.Enrollment.IssuerDelegationFile, []byte(delegation+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	srv, server := startOps(t, ctx, ServerOptions{Config: cfg})
	issuer := srv.Issuer()
	if issuer == nil {
		t.Fatal("configured issuer missing")
	}
	if info, err := issuer.Info(); err != nil || info.PublicKey != enrollPub(2) {
		t.Fatalf("issuer info %+v %v", info, err)
	}
	// Claims beyond the delegation are refused with a code.
	_, err = issuer.Issue(enrollPub(7), EnrollmentClaims{Scopes: []string{"deploy"}, Tags: []string{}}, RegistrationEphemeral)
	var refusal *IssuerRefusal
	if !errors.As(err, &refusal) || refusal.Code != "invalid_credential" || !errors.Is(err, ErrPermissionDenied) {
		t.Fatalf("wider claims: %v", err)
	}
	issued, err := issuer.Issue(enrollPub(7), EnrollmentClaims{
		Scopes: []string{}, Tags: []string{}, Attributes: map[string]Attribute{"aster.role": StringAttribute("ops.status")},
	}, RegistrationEphemeral)
	if err != nil {
		t.Fatal(err)
	}

	client := startClient(t, ctx, enrolledCfg(7, issued.Chain))
	if roles, err := opsCall(ctx, client, server, "status"); err != nil || roles != "ops.status" {
		t.Fatalf("status: %q %v", roles, err)
	}
	if !client.IsRenewing() {
		t.Fatal("the server signed the credential, so the client renews from it")
	}
	deadline := time.Now().Add(3*6*time.Second + 2*time.Second)
	for time.Now().Before(deadline) {
		if roles, err := opsCall(ctx, client, server, "status"); err != nil || roles != "ops.status" {
			t.Fatalf("status across lifetimes: %q %v", roles, err)
		}
		time.Sleep(500 * time.Millisecond)
	}
	current, err := client.Enrollment().Credential()
	if err != nil || current.CredentialID == issued.CredentialID {
		t.Fatalf("the credential was not renewed: %+v %v", current, err)
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
	c = good()
	c.EnrollmentCredentialChain = ""
	c.Enrollment.Enabled = ptrBool(true)
	refused("no credential", ServerOptions{Config: c}, "no credential")
	c = good()
	c.EnrollmentCredentialChain = "/etc/aster/server.cred"
	refused("a path where the chain belongs", ServerOptions{Config: c}, "ASTER_ENROLLMENT_CREDENTIAL)")
	c = good()
	c.EnrollmentCredentialChain = credentialFor(t, 3, "")
	refused("a credential for another key", ServerOptions{Config: c}, "enrollment:")
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
