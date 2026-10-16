//go:build cgo

package aster

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestConfig_DefaultsEnvTOML(t *testing.T) {
	d := DefaultConfig()
	if d.RelayMode != "default" || d.HookFailureMode != HookFailClosed || d.EnableHooks || d.IsEnrollmentEnabled() || d.LogLevel != "info" {
		t.Fatalf("defaults %+v", d)
	}

	t.Setenv("ASTER_RELAY_MODE", "disabled")
	t.Setenv("ASTER_BIND_ADDR", "127.0.0.1:0")
	t.Setenv("ASTER_LOG_LEVEL", "DEBUG")
	t.Setenv("ASTER_ENROLLMENT_AUDIENCE", "mission-control")
	t.Setenv("ASTER_ENROLLMENT_CREDENTIAL", "agent.cred")
	t.Setenv("ASTER_ENROLLMENT_REPLICAS", "aa, bb")
	t.Setenv("ASTER_ENROLLMENT_POLICY", `{"scope":"ops"}`)
	t.Setenv("ASTER_ENROLLMENT_SYNC_TIMEOUT_MS", "2000")
	t.Setenv("ASTER_ENROLLMENT_ISSUER_LIFETIME", "6h")
	c, err := ConfigFromEnv()
	if err != nil {
		t.Fatal(err)
	}
	if c.RelayMode != "disabled" || c.BindAddr != "127.0.0.1:0" || c.LogLevel != "debug" {
		t.Fatalf("from env %+v", c)
	}
	e := c.Enrollment
	policy, _ := json.Marshal(e.Policy)
	if !c.IsEnrollmentEnabled() || e.Audience != "mission-control" || c.EnrollmentCredentialFile != "agent.cred" ||
		strings.Join(e.Replicas, ",") != "aa,bb" || string(policy) != `{"scope":"ops"}` ||
		e.Limits.SyncTimeoutMs != 2000 || e.IssuerLifetimeSecs != 6*3600 || *e.Roles.Attribute != "aster.role" {
		t.Fatalf("enrollment from env %+v", e)
	}
	t.Setenv("ASTER_ENROLLMENT_SYNC_TIMEOUT_MS", "soon")
	if _, err := ConfigFromEnv(); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("malformed variable: %v", err)
	}
	for _, k := range []string{"AUDIENCE", "CREDENTIAL", "REPLICAS", "POLICY", "SYNC_TIMEOUT_MS", "ISSUER_LIFETIME"} {
		t.Setenv("ASTER_ENROLLMENT_"+k, "")
	}

	dir := t.TempDir()
	path := filepath.Join(dir, "aster.toml")
	toml := "[network]\nrelay_mode = \"staging\"\nenable_monitoring = true\n\n[trust]\nroot_pubkey = \"" +
		strings.Repeat("ab", 32) + "\"\nnext_root_commitment = \"" + strings.Repeat("cd", 32) + "\"\nroot_seq = 2\n" +
		"enrollment_credential_chain = \"aster-chain-v1~x\"\n\n[logging]\nformat = \"JSON\"\n\n" +
		"[enrollment]\naudience = \"mc\"\nfleet_namespace = \"" + strings.Repeat("ef", 32) + "\"\n" +
		"policy = { all_of = [{ scope = \"ops\" }, \"tolerate_stale_policy\"] }\n\n" +
		"[enrollment.roles]\nattribute = \"\"\nscopes_as_roles = true\n[enrollment.roles.tags]\nadmin = \"ops.admin, ops.logs\"\n\n" +
		"[enrollment.issuer]\ndelegation_file = \"issuer.chain\"\nlifetime = \"2h\"\n"
	if err := os.WriteFile(path, []byte(toml), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("ASTER_RELAY_MODE", "")
	c, err = ConfigFromTOML(path)
	if err != nil {
		t.Fatal(err)
	}
	if c.RelayMode != "staging" || !c.EnableMonitoring || c.LogFormat != "json" ||
		len(c.ResolveRootPubkey()) != 32 || c.BindAddr != "127.0.0.1:0" {
		t.Fatalf("from toml %+v", c)
	}
	e = c.Enrollment
	policy, _ = json.Marshal(e.Policy)
	if c.NextRootCommitment != strings.Repeat("cd", 32) || c.RootSeq != 2 || c.EnrollmentCredentialChain != "aster-chain-v1~x" ||
		e.Audience != "mc" || string(policy) != `{"all_of":[{"scope":"ops"},"tolerate_stale_policy"]}` ||
		*e.Roles.Attribute != "" || !e.Roles.ScopesAsRoles || strings.Join(e.Roles.Tags["admin"], ",") != "ops.admin,ops.logs" ||
		e.IssuerDelegationFile != "issuer.chain" || e.IssuerLifetimeSecs != 7200 {
		t.Fatalf("enrollment from toml %+v", e)
	}
	// Retired consumer-admission keys are refused, naming the replacement.
	retired := filepath.Join(dir, "retired.toml")
	if err := os.WriteFile(retired, []byte("[trust]\nallow_all_consumers = false\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := ConfigFromTOML(retired); !errors.Is(err, ErrInvalidArgument) ||
		!strings.Contains(err.Error(), "allow_all_consumers") || !strings.Contains(err.Error(), "enrollment_credential") {
		t.Fatalf("retired keys: %v", err)
	}
	if _, err := ConfigFromTOML(filepath.Join(dir, "missing.toml")); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("missing file: %v", err)
	}
	c.SecretKey = bytes.Repeat([]byte{0xab}, 32)
	if s := c.String(); strings.Contains(s, "abab") || !strings.Contains(s, "<redacted>") {
		t.Fatalf("String leaks the secret key: %s", s)
	}
}

func TestConfig_NativeRoundTrip(t *testing.T) {
	mtu := uint32(1300)
	idle := uint64(9000)
	fair := true
	c := DefaultConfig()
	c.RelayMode = "https://relay.example"
	c.SecretKey = bytes.Repeat([]byte{7}, 32)
	c.ALPNs = []string{"a", "b"}
	c.Transport = TransportConfig{InitialMtu: &mtu, MaxIdleTimeoutMs: &idle, SendFairness: &fair}
	var a cArena
	defer a.free()
	n, err := c.toNative(&a, []string{"b", "c"})
	if err != nil {
		t.Fatal(err)
	}
	var back AsterConfig
	back.fromNative(&n)
	if back.RelayMode != "custom" || len(back.RelayURLs) != 1 || back.RelayURLs[0] != "https://relay.example" ||
		!bytes.Equal(back.SecretKey, c.SecretKey) || strings.Join(back.ALPNs, ",") != "a,b,c" ||
		back.Transport.InitialMtu == nil || *back.Transport.InitialMtu != 1300 ||
		back.Transport.MaxIdleTimeoutMs == nil || *back.Transport.MaxIdleTimeoutMs != 9000 ||
		back.Transport.SendFairness == nil || !*back.Transport.SendFairness || back.Transport.SendWindow != nil {
		t.Fatalf("round trip %+v", back)
	}
	c.SecretKey = []byte{1, 2}
	if _, err := c.toNative(&a, nil); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("short secret key: %v", err)
	}
}

func TestNode_StartTicketMetrics(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	secret, err := GenerateSecretKey()
	if err != nil {
		t.Fatal(err)
	}
	cfg := testCfg()
	cfg.SecretKey = secret[:]
	node, err := StartNode(ctx, cfg, NodeOptions{ExtraALPNs: []string{"test/1"}})
	if err != nil {
		t.Fatal(err)
	}
	defer node.Close()
	id, err := node.NodeID()
	if err != nil {
		t.Fatal(err)
	}
	pub, _ := SecretKeyPublicKey(secret)
	if want, _ := FormatID(IDNodeID, pub); id != want {
		t.Fatalf("node id %s, want %s from the configured secret", id, want)
	}
	ticket, err := node.Ticket(ctx, 5*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	addr, err := NodeAddrFromTicket(ticket)
	if err != nil || addr.EndpointID != id || len(addr.DirectAddresses) == 0 {
		t.Fatalf("ticket %s -> %+v %v", ticket, addr, err)
	}
	if node.Admissions() != nil {
		t.Fatal("no admissions channel without EnableHooks")
	}
	if err := node.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}
	if err := node.Close(); err != nil {
		t.Fatalf("second close: %v", err)
	}
}

func TestNode_AdmissionEvents(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	cfg := testCfg()
	cfg.EnableHooks = true
	cfg.HookTimeoutMs = 10_000
	server, err := StartNode(ctx, cfg, NodeOptions{ExtraALPNs: []string{"test/1"}})
	if err != nil {
		t.Fatal(err)
	}
	defer server.Close()
	client, err := StartNode(ctx, testCfg(), NodeOptions{ExtraALPNs: []string{"test/1"}})
	if err != nil {
		t.Fatal(err)
	}
	defer client.Close()
	addr, _ := server.NodeAddr()
	clientID, _ := client.NodeID()

	// First connection: deny with a code and reason.
	denied := make(chan struct{})
	go func() {
		for req := range server.Admissions() {
			if req.Inbound {
				if req.PeerID != clientID || req.ALPN != "test/1" {
					t.Errorf("admission %+v", req)
				}
				if err := req.Deny(451, "nope"); err != nil {
					t.Errorf("deny: %v", err)
				}
				if err := req.Allow(); err == nil {
					t.Error("second answer accepted")
				}
				close(denied)
				return
			}
			_ = req.Allow()
		}
	}()
	conn, err := client.Connect(ctx, addr, "test/1")
	if err == nil {
		// The handshake completes before the after-connect decision closes it.
		cctx, ccancel := context.WithTimeout(ctx, 10*time.Second)
		err = conn.OnClosed(cctx)
		ccancel()
		if err != nil {
			t.Fatalf("expected the denied connection to close: %v", err)
		}
	}
	select {
	case <-denied:
	case <-time.After(10 * time.Second):
		t.Fatal("no inbound admission event")
	}
	// Second connection: allow, and the server accepts it.
	go func() {
		for req := range server.Admissions() {
			_ = req.Allow()
		}
	}()
	accepted := make(chan error, 1)
	go func() {
		c, err := server.Accept(ctx)
		if err == nil {
			_ = c.Close("")
		}
		accepted <- err
	}()
	conn, err = client.Connect(ctx, addr, "test/1")
	if err != nil {
		t.Fatalf("allowed connect: %v", err)
	}
	defer conn.Close("")
	if err := <-accepted; err != nil {
		t.Fatalf("accept: %v", err)
	}
}

func TestGate_Policy(t *testing.T) {
	g, err := NewGate(nil)
	if err != nil {
		t.Fatal(err)
	}
	defer g.Close()
	sk, _ := GenerateSecretKey()
	pk, _ := SecretKeyPublicKey(sk)
	peer, _ := FormatID(IDNodeID, pk)
	if ok, _ := g.ShouldAllow(peer, "aster/1"); ok {
		t.Fatal("protected gate allowed an un-admitted peer")
	}
	if ok, _ := g.ShouldAllow(peer, "aster.producer_admission"); !ok {
		t.Fatal("admission ALPN should be open")
	}
	// The retired consumer admission ALPN is not in the default set (5.1).
	if ok, _ := g.ShouldAllow(peer, "aster.consumer_admission"); ok {
		t.Fatal("the retired consumer admission ALPN is open")
	}
	if err := g.SetAdmissionALPNs(); err != nil {
		t.Fatal(err)
	}
	if ok, _ := g.ShouldAllow(peer, "aster.producer_admission"); ok {
		t.Fatal("empty admission set should close the admission ALPN")
	}
	if err := g.Admit(strings.ToUpper(peer)); err != nil {
		t.Fatal(err)
	}
	if ok, _ := g.ShouldAllow(peer, "aster/1"); !ok {
		t.Fatal("admitted peer rejected")
	}
	if err := g.Revoke(peer); err != nil {
		t.Fatal(err)
	}
	if ok, _ := g.IsAdmitted(peer); ok {
		t.Fatal("revoked peer still admitted")
	}
	if err := g.SetTrustMode(TrustOpenDev); err != nil {
		t.Fatal(err)
	}
	if ok, _ := g.ShouldAllow(peer, "aster/1"); !ok {
		t.Fatal("open-dev gate rejected")
	}
	if err := g.SetReject(403, "go away"); err != nil {
		t.Fatal(err)
	}
	if err := g.Admit("not-hex"); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("bad peer: %v", err)
	}
}

func TestPure_IDsHPKEAttestation(t *testing.T) {
	sk, err := GenerateSecretKey()
	if err != nil {
		t.Fatal(err)
	}
	pk, _ := SecretKeyPublicKey(sk)
	text, err := FormatID(IDNodeID, pk)
	if err != nil || len(text) != 64 {
		t.Fatalf("format %q %v", text, err)
	}
	back, err := ParseID(IDNodeID, strings.ToUpper(text))
	if err != nil || back != pk {
		t.Fatalf("parse: %v", err)
	}
	if err := ValidateID(IDNodeID, pk); err != nil {
		t.Fatal(err)
	}
	if _, err := ParseID(IDNodeID, "zz"); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("bad id: %v", err)
	}
	ns, _ := GenerateSecretKey()
	if _, err := NamespaceSecretID(ns); err != nil {
		t.Fatal(err)
	}

	priv, pub, err := HPKEGenerateKeypair()
	if err != nil {
		t.Fatal(err)
	}
	if p2, _ := HPKEPublicKeyFromPrivate(priv); p2 != pub {
		t.Fatal("public key mismatch")
	}
	env, err := HPKESeal(pub, []byte("aad"), []byte("secret"))
	if err != nil {
		t.Fatal(err)
	}
	if pt, err := HPKEOpen(priv, []byte("aad"), env); err != nil || string(pt) != "secret" {
		t.Fatalf("open: %v %q", err, pt)
	}
	if _, err := HPKEOpen(priv, []byte("other"), env); !errors.Is(err, ErrVerificationFailed) {
		t.Fatalf("wrong aad: %v", err)
	}
	// Identity-derived keys agree whichever side derives them.
	xs, _ := X25519SecretFromIdentity(sk)
	xp1, _ := HPKEPublicKeyFromPrivate(xs)
	xp2, _ := X25519PublicFromIdentity(pk)
	xp3, _ := X25519PublicFromNodeID(text)
	if xp1 != xp2 || xp2 != xp3 {
		t.Fatal("identity X25519 derivations disagree")
	}

}

func TestGrants_SealOpenRoundTrip(t *testing.T) {
	sk, _ := GenerateSecretKey()
	pk, _ := SecretKeyPublicKey(sk)
	recipient, _ := FormatID(IDNodeID, pk)
	ctx := GrantContext{App: "app/v1", Granter: recipient, Recipient: recipient, Resource: []byte("r"), Path: "/p", Role: "write"}
	env, err := GrantSeal(ctx, []byte("payload"))
	if err != nil {
		t.Fatal(err)
	}
	if pt, err := GrantOpen(sk, ctx, env); err != nil || string(pt) != "payload" {
		t.Fatalf("open %v %q", err, pt)
	}
	ns, _ := GenerateSecretKey()
	nenv, err := NamespaceGrantSeal(ctx, NamespaceCapability{Access: AccessWrite, Key: ns})
	if err != nil {
		t.Fatal(err)
	}
	rec, err := EncodeGrantRecord(GrantRecord{State: GrantActive, ContextVersion: 1, Role: "write", Envelope: nenv})
	if err != nil {
		t.Fatal(err)
	}
	out, err := ReconcileNamespaceGrant(sk, ctx, rec)
	if err != nil || out.Kind != OutcomeActive || out.Access != AccessWrite || !bytes.Equal(out.Payload, ns[:]) {
		t.Fatalf("reconcile %+v %v", out, err)
	}
	if out, _ := ReconcileGrant(sk, ctx, nil); out.Kind != OutcomeAbsent {
		t.Fatalf("absent: %+v", out)
	}
	if out, _ := ReconcileGrant(sk, ctx, []byte{}); out.Kind != OutcomeUnopenable {
		t.Fatalf("empty record: %+v", out)
	}
	// A read role cannot carry a write capability.
	ctx.Role = "read"
	if _, err := NamespaceGrantSeal(ctx, NamespaceCapability{Access: AccessWrite, Key: ns}); !errors.Is(err, ErrVerificationFailed) {
		t.Fatalf("role rule: %v", err)
	}
}
