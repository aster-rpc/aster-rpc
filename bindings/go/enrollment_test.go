//go:build cgo

package aster

import (
	"bytes"
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// Enrollment through the C ABI (5.1) with real nodes on loopback. Root
// operations stay in the Rust CLI, so this file plays the root itself with a
// test-only signer: it signs the delegation and root credentials as compact
// Ed25519 JWTs (crypto/ed25519) and writes root records into the fleet
// namespace with the generic Docs API. None of it is public API, and Rust
// verifying links signed here doubles as a cross-implementation check.

const enrollFleet = "go-enrollment"

func enrollSeed(seed byte) []byte { return bytes.Repeat([]byte{seed}, 32) }

func enrollKey(seed byte) ed25519.PrivateKey { return ed25519.NewKeyFromSeed(enrollSeed(seed)) }

func enrollPub(seed byte) string {
	return hex.EncodeToString(enrollKey(seed).Public().(ed25519.PublicKey))
}

// testRoot signs links as the root (seed 1).
type testRoot struct{ key ed25519.PrivateKey }

var b64 = base64.RawURLEncoding

func (r testRoot) link(t *testing.T, typ, subject string, lifetime time.Duration, extension string, body any) string {
	t.Helper()
	sub, err := hex.DecodeString(subject)
	if err != nil {
		t.Fatal(err)
	}
	jti := make([]byte, 16)
	if _, err := rand.Read(jti); err != nil {
		t.Fatal(err)
	}
	now := time.Now().Unix()
	payload, err := json.Marshal(map[string]any{
		"iss": hex.EncodeToString(r.key.Public().(ed25519.PublicKey)),
		"sub": subject,
		"cnf": map[string]any{"jwk": map[string]string{"kty": "OKP", "crv": "Ed25519", "x": b64.EncodeToString(sub)}},
		"aud": enrollFleet,
		"jti": b64.EncodeToString(jti),
		"iat": now,
		"exp": now + int64(lifetime/time.Second),
		extension: body,
	})
	if err != nil {
		t.Fatal(err)
	}
	header, _ := json.Marshal(map[string]string{"alg": "EdDSA", "typ": typ})
	input := b64.EncodeToString(header) + "." + b64.EncodeToString(payload)
	return input + "." + b64.EncodeToString(ed25519.Sign(r.key, []byte(input)))
}

// credential enrolls subject as the root, with unknown claims.
func (r testRoot) credential(t *testing.T, subject string) string {
	return "aster-chain-v1~" + r.link(t, "aster-enrollment-v1+jwt", subject, time.Hour, "aster_enrollment_v1",
		map[string]any{"scopes": nil, "tags": nil, "attributes": nil, "mandatory": []string{}, "registration": "ephemeral"})
}

// delegation lets subject enroll ephemeral "build" nodes with a "pod" attribute.
func (r testRoot) delegation(t *testing.T, subject string) string {
	return "aster-chain-v1~" + r.link(t, "aster-delegation-v1+jwt", subject, time.Hour, "aster_delegation_v1",
		map[string]any{
			"may_issue": []string{"enrollment"}, "audiences": []string{enrollFleet}, "scopes": []string{"build"},
			"tags": []string{}, "attribute_keys": []string{"pod"}, "max_child_lifetime": 3600, "max_depth": 0,
			"subjects": nil, "operations": []string{}, "mandatory": []string{}, "may_register": false,
		})
}

// revocation is a root NodeRegistrationV1 record revoking subject.
func (r testRoot) revocation(subject string) []byte {
	record, _ := json.Marshal(map[string]any{
		"version": 1, "issuer": hex.EncodeToString(r.key.Public().(ed25519.PublicKey)), "subject": subject,
		"audience": enrollFleet, "revision": 1, "issued_at": time.Now().Unix() - 60, "status": "revoked",
		"claims": map[string]any{"scopes": []string{"build"}, "tags": []string{}, "attributes": map[string]any{}},
		"mandatory": []string{}, "reauthorizes_revision": nil, "reason": "decommissioned",
	})
	return record
}

func writeFile(path, content string) error { return os.WriteFile(path, []byte(content), 0o600) }

func enrollCfg(seed byte) AsterConfig {
	cfg := testCfg()
	cfg.SecretKey = enrollSeed(seed)
	return cfg
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

func TestEnrollment_IssuerRenewalClaimsAndRevocation(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 120*time.Second)
	defer cancel()
	dir := t.TempDir()
	root := testRoot{enrollKey(1)}
	nsSecret := Key(bytes.Repeat([]byte{0x55}, 32))
	fleet := hex.EncodeToString(ed25519.NewKeyFromSeed(nsSecret[:]).Public().(ed25519.PublicKey))
	fast := 200 * time.Millisecond

	// The root: an enrolled server holding the fleet namespace. Its root key
	// comes from the node config (RootPubkey), not the EnrollmentConfig.
	rootCfg := enrollCfg(1)
	rootCfg.RootPubkey = root.key.Public().(ed25519.PublicKey)
	rootSrv, err := StartServer(ctx, ServerOptions{Config: rootCfg, Enrollment: &EnrollmentConfig{
		Audience: enrollFleet, FleetNamespace: fleet, StatePath: filepath.Join(dir, "root.json"),
		Credential: root.credential(t, enrollPub(1)), Policy: PolicyTolerateStale(), ReconciliationInterval: fast,
	}})
	if err != nil {
		t.Fatal(err)
	}
	defer rootSrv.Close()
	if rootSrv.Gate() != nil || rootSrv.Enrollment() == nil {
		t.Fatal("an enrolled server has Enrollment instead of a gate")
	}
	fleetDoc, err := rootSrv.Node().Docs().ImportWriteNamespace(ctx, nsSecret)
	if err != nil {
		t.Fatal(err)
	}
	defer fleetDoc.Close()
	ticket, err := fleetDoc.ShareWithAddr(ctx, DOC_SHARE_READ)
	if err != nil {
		t.Fatal(err)
	}

	// The issuer: enrolled by the root, synchronized, serving renewals. Its
	// credential comes from the node config's EnrollmentCredentialFile.
	issuerCfg := enrollCfg(2)
	issuerCfg.EnrollmentCredentialFile = filepath.Join(dir, "issuer.chain")
	if err := writeFile(issuerCfg.EnrollmentCredentialFile, root.credential(t, enrollPub(2))); err != nil {
		t.Fatal(err)
	}
	policy := PolicyAnyOf(PolicyScope("build"), PolicyTolerateStale())
	issuerSrv, err := StartServer(ctx, ServerOptions{
		Config: issuerCfg,
		Enrollment: &EnrollmentConfig{
			Root: root.key.Public().(ed25519.PublicKey), Audience: enrollFleet, FleetNamespace: fleet,
			StatePath: filepath.Join(dir, "issuer.json"), Policy: policy, ReconciliationInterval: fast,
		},
		EnrollmentIssuer: &IssuerConfig{Delegation: root.delegation(t, enrollPub(2)), Lifetime: 6 * time.Second},
	})
	if err != nil {
		t.Fatal(err)
	}
	defer issuerSrv.Close()
	enr := issuerSrv.Enrollment()
	ready, _, err := enr.Connect(ctx, addrOf(t, rootSrv.Node()))
	if err != nil {
		t.Fatal(err)
	}
	if ready.Peer != enrollPub(1) || ready.Source != "chain" {
		t.Fatalf("readiness %+v", ready)
	}
	if ns, err := enr.Synchronize(ctx, ready.Peer, ticket, 20*time.Second); err != nil || ns != fleet {
		t.Fatalf("synchronize: %q %v", ns, err)
	}
	status, err := enr.Status()
	if err != nil || status.TrustedRoot != enrollPub(1) || status.Credential.Subject != enrollPub(2) {
		t.Fatalf("status %+v %v", status, err)
	}
	issuer := issuerSrv.EnrollmentIssuer()
	if info, err := issuer.Info(); err != nil || info.PublicKey != enrollPub(2) {
		t.Fatalf("issuer info %+v %v", info, err)
	}

	// Claims beyond the delegation are refused with a code.
	_, err = issuer.Issue(enrollPub(7), EnrollmentClaims{Scopes: []string{"deploy"}, Tags: []string{}}, RegistrationEphemeral)
	var refusal *IssuerRefusal
	if !errors.As(err, &refusal) || refusal.Code != "invalid_credential" || !errors.Is(err, ErrPermissionDenied) {
		t.Fatalf("wider claims: %v", err)
	}

	// The orchestrator authenticated pod node 7 out of band: issue for it.
	issued, err := issuer.Issue(enrollPub(7), EnrollmentClaims{
		Scopes: []string{"build"}, Tags: []string{}, Attributes: map[string]Attribute{"pod": StringAttribute("p-1")},
	}, RegistrationEphemeral)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(issued.Chain, "aster-chain-v1~") || issued.ExpiresAt > uint64(time.Now().Unix())+6 {
		t.Fatalf("issued %+v", issued)
	}

	revocations := make(chan Revocation, 1)
	go func() {
		if rev, err := enr.NextRevocation(ctx); err == nil {
			revocations <- rev
		}
	}()

	// Node 7 enrolls with the issuer on the issued credential.
	pod, err := StartNode(ctx, enrollCfg(7), NodeOptions{Enrollment: &EnrollmentConfig{
		Root: root.key.Public().(ed25519.PublicKey), Audience: enrollFleet, FleetNamespace: fleet,
		StatePath: filepath.Join(dir, "pod.json"), Credential: issued.Chain, Policy: policy,
	}})
	if err != nil {
		t.Fatal(err)
	}
	defer pod.Close()
	issuerAddr := addrOf(t, issuerSrv.Node())
	if ready, _, err := pod.Enrollment().Connect(ctx, issuerAddr); err != nil || ready.Peer != enrollPub(2) {
		t.Fatalf("pod connect: %+v %v", ready, err)
	}

	// The issuer reads the pod's verified claims and maps them to a role.
	claims, err := enr.PeerClaims(enrollPub(7))
	if err != nil {
		t.Fatal(err)
	}
	if claims.Subject != enrollPub(7) || claims.Root != enrollPub(1) || claims.Issuer != enrollPub(2) ||
		claims.Source != "chain" || !claims.HasScope("build") || claims.Attributes["pod"].Value != "p-1" ||
		claims.Registration != RegistrationEphemeral || claims.Registered() || claims.ExpiresAt == nil {
		t.Fatalf("claims %+v", claims)
	}
	if err := issuerSrv.SetPeerAttributes(claims.Subject, map[string]string{"aster.role": "build"}); err != nil {
		t.Fatal(err)
	}
	if _, err := enr.PeerClaims(enrollPub(9)); !errors.Is(err, ErrNotFound) {
		t.Fatalf("unknown peer claims: %v", err)
	}
	if _, err := enr.CheckPeer(ctx, enrollPub(7)); err != nil {
		t.Fatal(err)
	}
	if ev, err := enr.VerifyCredential(enrollPub(7), issued.Chain); err != nil || ev.CredentialID != issued.CredentialID {
		t.Fatalf("verify: %+v %v", ev, err)
	}
	if _, err := enr.VerifyCredential(enrollPub(9), issued.Chain); !errors.Is(err, ErrVerificationFailed) {
		t.Fatalf("verify for another subject: %v", err)
	}

	// Renewal keeps the pod on fresh credentials from the issuer.
	renewal, err := pod.Enrollment().StartRenewal(RenewalConfig{Issuer: issuerAddr, Retry: fast})
	if err != nil {
		t.Fatal(err)
	}
	// The pod switches to the renewed credential first, then presents it to
	// the issuer: wait for both.
	deadline := time.Now().Add(20 * time.Second)
	for {
		st, err := pod.Enrollment().Status()
		if err != nil {
			t.Fatal(err)
		}
		seen, err := enr.PeerClaims(enrollPub(7))
		if err != nil {
			t.Fatal(err)
		}
		if st.Credential.CredentialID != issued.CredentialID && seen.CredentialID == st.Credential.CredentialID {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("renewal did not move the pod and the issuer's session to a new credential: %+v", seen)
		}
		time.Sleep(fast)
	}

	// The root revokes the pod; the issuer revokes its session and reports it.
	rootID := AuthorID(enrollPub(1))
	if err := fleetDoc.SetBytes(ctx, rootID, "aster/enrollment/v1/nodes/"+enrollPub(7), root.revocation(enrollPub(7))); err != nil {
		t.Fatal(err)
	}
	_ = fleetDoc.StartSync(ctx, enrollPub(2))
	select {
	case rev := <-revocations:
		if rev.Peer != enrollPub(7) || rev.Reason.Code != "node_revoked" || !rev.Reason.Definite {
			t.Fatalf("revocation %+v", rev)
		}
	case <-time.After(30 * time.Second):
		t.Fatal("no revocation event")
	}
	var admission *AdmissionError
	if _, err := enr.CheckPeer(ctx, enrollPub(7)); !errors.As(err, &admission) || admission.Kind != AdmissionErrorDenied ||
		!errors.Is(err, ErrPermissionDenied) {
		t.Fatalf("revoked peer check: %v", err)
	}
	if _, err := enr.PeerClaims(enrollPub(7)); !errors.Is(err, ErrNotFound) {
		t.Fatalf("revoked peer claims: %v", err)
	}
	outcome, err := renewal.Wait(ctx)
	if err != nil || (outcome.Outcome != "refused" && outcome.Outcome != "expired") {
		t.Fatalf("renewal outcome %+v %v", outcome, err)
	}
	if outcome.Outcome == "refused" && outcome.Refusal.Code != "subject_denied" {
		t.Fatalf("refusal %+v", outcome.Refusal)
	}
}

func TestEnrollment_ConfigurationErrors(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	dir := t.TempDir()
	fleet := strings.Repeat("ab", 32)
	base := EnrollmentConfig{Audience: enrollFleet, FleetNamespace: fleet, StatePath: filepath.Join(dir, "s.json"), Policy: PolicyTolerateStale()}

	// No root anywhere: refused before the node starts.
	if _, err := StartNode(ctx, enrollCfg(3), NodeOptions{Enrollment: &base}); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("no root: %v", err)
	}
	cfg := enrollCfg(3)
	cfg.RootPubkeyFile = filepath.Join(dir, "missing.pub")
	if _, err := StartNode(ctx, cfg, NodeOptions{Enrollment: &base}); err == nil {
		t.Fatal("unreadable RootPubkeyFile accepted")
	}
	cfg.RootPubkeyFile = filepath.Join(dir, "root.json")
	if err := writeFile(cfg.RootPubkeyFile, `{"public_key": "`+enrollPub(1)+`"}`); err != nil {
		t.Fatal(err)
	}
	// A policy Rust rejects, and a Gate next to Enrollment.
	bad := base
	bad.Policy = PolicyAllOf()
	if _, err := StartNode(ctx, cfg, NodeOptions{Enrollment: &bad}); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("empty all_of: %v", err)
	}
	gate, err := NewGate(nil)
	if err != nil {
		t.Fatal(err)
	}
	defer gate.Close()
	if _, err := StartNode(ctx, cfg, NodeOptions{Enrollment: &base, Gate: gate}); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("gate and enrollment: %v", err)
	}
	if _, err := StartServer(ctx, ServerOptions{Config: cfg, EnrollmentIssuer: &IssuerConfig{}}); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("issuer without enrollment: %v", err)
	}

	// A pure verifier (root from RootPubkeyFile, no credential) installs, and
	// the node refuses a gate afterwards.
	n, err := StartNode(ctx, cfg, NodeOptions{Enrollment: &base})
	if err != nil {
		t.Fatal(err)
	}
	defer n.Close()
	st, err := n.Enrollment().Status()
	if err != nil || st.Credential != nil || st.TrustedRoot != enrollPub(1) || st.FleetNamespace != fleet {
		t.Fatalf("status %+v %v", st, err)
	}
	sameRuntime, err := NewGate(n.Runtime())
	if err != nil {
		t.Fatal(err)
	}
	defer sameRuntime.Close()
	if err := n.SetGate(sameRuntime); !errors.Is(err, ErrUnsupported) {
		t.Fatalf("set gate after enrollment: %v", err)
	}
	// A delegation that does not name this node is refused.
	other := testRoot{enrollKey(1)}
	if _, err := n.Enrollment().NewIssuer(IssuerConfig{Delegation: other.delegation(t, enrollPub(2))}); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("foreign delegation: %v", err)
	}
	if err := n.Enrollment().Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := n.Enrollment().Status(); !errors.Is(err, ErrAlreadyClosed) {
		t.Fatalf("closed enrollment: %v", err)
	}
}

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
}
