//go:build cgo

package aster

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"strings"
	"testing"
	"time"

	"sdk.getaster.now/go/internal/testroot"
)

// Gateway identity (C ABI 7.7) on the high-level path only: StartServer and
// StartClient configured by AsterConfig.Identity / RemoteConfig and the
// credentials, mirroring aster/tests/enrollment_gateway_identity.rs,
// enrollment_provider_tokens.rs and node_config.rs. The root is
// internal/testroot.

type whoRequest struct{ N int32 }

func (whoRequest) AsterWireType() string { return "test.gateway/WhoRequest" }

type whoReply struct {
	Principal string
	Peer      string
	Actor     string
	Amr       string
	Roles     string
	Issuer    string
	Tier      int64
	Name      string
}

func (whoReply) AsterWireType() string { return "test.gateway/WhoReply" }

func who(ctx context.Context) *whoReply {
	info, _ := CallInfoFromContext(ctx)
	r := &whoReply{Principal: info.Principal, Peer: info.PeerID, Actor: info.Actor,
		Amr: strings.Join(info.AuthMethods, ",")}
	for _, a := range info.Attributes {
		if a.Key == "aster.role" {
			r.Roles = a.Value
		}
	}
	if info.OnBehalfOf != nil {
		r.Issuer = info.OnBehalfOf.Issuer
	}
	if tier, ok := info.NodeAttribute("tier"); ok {
		r.Tier, _ = tier.Int()
	}
	r.Name = info.NodeProperties().Name
	return r
}

// notesService: whoami is open, read needs notes.read, publish needs
// notes.read and a passkey sign-in.
func notesService() *Service {
	whoami := func(ctx context.Context, _ *whoRequest) (*whoReply, error) { return who(ctx), nil }
	publish := func(ctx context.Context, _ *whoRequest) (*whoReply, error) {
		info, _ := CallInfoFromContext(ctx)
		if err := info.RequireAuthMethod(AMRWebAuthn); err != nil {
			return nil, err
		}
		return who(ctx), nil
	}
	return NewService("Notes", 1,
		Unary("whoami", whoami),
		Unary("read", whoami, Requires(RequireRole("notes.read"))),
		Unary("publish", publish, Requires(RequireRole("notes.read"))),
	)
}

// claimsWith carries roles in aster.role and, when given, an issuer
// delegation.
func claimsWith(roles, issuable string) testroot.Claims {
	c := testroot.RoleClaims(roles)
	if issuable != "" {
		c.Attributes[IssuableRolesAttribute] = map[string]any{"type": "string", "value": issuable}
	}
	return c
}

func credentialWith(t *testing.T, seed byte, claims testroot.Claims) string {
	t.Helper()
	chain, err := enrollRoot.Credential(enrollPub(seed), claims, time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	return chain
}

func startNotes(t *testing.T, ctx context.Context, cfg AsterConfig, opts ServerOptions) (*AsterServer, *NodeAddr) {
	t.Helper()
	opts.Config = cfg
	opts.Services = []*Service{notesService()}
	srv, err := StartServer(ctx, opts)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = srv.Close() })
	addr := addrOf(t, srv.Node())
	return srv, &addr
}

func notesCall(ctx context.Context, rc *RpcConnection, method string, opts ...CallOption) (*whoReply, error) {
	cctx, cancel := context.WithTimeout(ctx, 20*time.Second)
	defer cancel()
	return Call[whoRequest, whoReply](cctx, rc, "Notes", method, &whoRequest{N: 1}, opts...)
}

func wantCode(t *testing.T, err error, code StatusCode, what string) {
	t.Helper()
	var rpc *RpcError
	if !errors.As(err, &rpc) || rpc.Code != code {
		t.Fatalf("%s: want %s, got %v", what, code, err)
	}
}

func TestIdentity_ConfigFromEnvRoundTrips(t *testing.T) {
	t.Setenv("ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF", "true")
	t.Setenv("ASTER_IDENTITY_TRUSTED_ISSUERS", `[{"iss":"https://idp.test","audiences":["notes"],"roles_claim":"groups","role_map":{"readers":"notes.read"}}]`)
	t.Setenv("ASTER_IDENTITY_CLOCK_LEEWAY_SECS", "0")
	t.Setenv("ASTER_IDENTITY_PUBLISHER", `{"issuers":["https://idp.test"],"jwks_url":{"https://idp.test":"https://idp.test/jwks"},"refresh_secs":600}`)
	t.Setenv("ASTER_REMOTE_CONFIG_LOCK", "identity")
	cfg, err := ConfigFromEnv()
	if err != nil {
		t.Fatal(err)
	}
	id := cfg.Identity
	if !id.AcceptOnBehalfOf || len(id.TrustedIssuers) != 1 || id.TrustedIssuers[0].RoleMap["readers"] != "notes.read" {
		t.Fatalf("identity %+v", id)
	}
	if id.ClockLeewaySecs == nil || *id.ClockLeewaySecs != 0 || id.EdgeTokenMaxLifetimeSecs != nil {
		t.Fatalf("limits %+v", id)
	}
	if id.Publisher == nil || id.Publisher.RefreshSecs != 600 || cfg.RemoteConfig.Lock[0] != "identity" || cfg.RemoteConfig.Disabled {
		t.Fatalf("publisher / remote config %+v %+v", id.Publisher, cfg.RemoteConfig)
	}
	if id.sources[0] != 3 || id.sources[1] != 3 || id.sources[2] != 0 {
		t.Fatalf("sources %v", id.sources)
	}
	// Round trip through the native config, sources included.
	var a cArena
	defer a.free()
	n, err := cfg.toNative(&a, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer releaseNative(&n)
	var back AsterConfig
	back.fromNative(&n)
	if !back.Identity.AcceptOnBehalfOf || *back.Identity.ClockLeewaySecs != 0 || back.Identity.sources != id.sources ||
		back.Identity.Publisher.Issuers[0] != "https://idp.test" || back.RemoteConfig.Lock[0] != "identity" {
		t.Fatalf("round trip %+v", back.Identity)
	}
	// A value changed in code no longer claims to come from the environment.
	cfg.Identity.AcceptOnBehalfOf = false
	n2, err := cfg.toNative(&a, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer releaseNative(&n2)
	back.fromNative(&n2)
	if back.Identity.sources[0] != 0 || back.Identity.sources[1] != 3 {
		t.Fatalf("sources after a code change %v", back.Identity.sources)
	}
}

func TestIdentity_BearerToken(t *testing.T) {
	if tok, ok := BearerToken("Bearer abc.def"); !ok || tok != "abc.def" {
		t.Fatal(tok)
	}
	if _, ok := BearerToken("Basic x"); ok {
		t.Fatal("basic")
	}
}

// An edge holding aster.gateway mints a token for a user; the Go handler
// sees the user, the edge as the actor and the passkey sign-in. The edge,
// holding aster.config-admin, changes the server through NodeConfig: the
// handler sees the attributes, and turning accept_on_behalf_of off refuses
// the entry.
func TestGatewayIdentity_EdgeMintedTokenAndNodeConfig(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 120*time.Second)
	defer cancel()
	cfg := serverCfg(t, t.TempDir())
	cfg.Identity.AcceptOnBehalfOf = true
	srv, server := startNotes(t, ctx, cfg, ServerOptions{})
	edge := startClient(t, ctx, enrolledCfg(5, credentialWith(t, 5,
		claimsWith(GatewayRole+","+ConfigAdminRole, "notes.read"))))
	rc, err := edge.Connect(ctx, server)
	if err != nil {
		t.Fatal(err)
	}
	edgeID := enrollPub(5)

	me, err := notesCall(ctx, rc, "whoami")
	if err != nil || me.Principal != edgeID || me.Actor != "" || me.Amr != "" {
		t.Fatalf("edge as itself: %+v %v", me, err)
	}
	_, err = notesCall(ctx, rc, "read")
	wantCode(t, err, StatusPermissionDenied, "the edge itself lacks notes.read")

	token, err := edge.MintUserToken("alice", []string{"notes.read"}, []string{AMRWebAuthn}, time.Minute)
	if err != nil {
		t.Fatal(err)
	}
	alice, err := notesCall(ctx, rc, "publish", WithOnBehalfOf(token))
	if err != nil {
		t.Fatal(err)
	}
	if alice.Principal != edgeID+"|alice" || alice.Peer != edgeID || alice.Actor != edgeID ||
		alice.Amr != AMRWebAuthn || alice.Roles != "notes.read" || alice.Issuer != edgeID {
		t.Fatalf("forwarded: %+v", alice)
	}
	pwd, err := edge.MintUserToken("bob", []string{"notes.read"}, []string{AMRPassword}, time.Minute)
	if err != nil {
		t.Fatal(err)
	}
	_, err = notesCall(ctx, rc, "publish", WithOnBehalfOf(pwd))
	wantCode(t, err, StatusPermissionDenied, "publish needs a passkey")
	if _, err := edge.MintUserToken("bob", []string{"notes.admin"}, []string{AMRPassword}, time.Minute); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("minting outside the delegation: %v", err)
	}
	if err := edge.Enrollment().NoticeKid("https://idp.test", "k1"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("no publisher: %v", err)
	}

	// NodeConfig: get, set, ack, clear from the admin edge.
	admin := NewNodeConfigClient(rc)
	view, err := admin.Get(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if s := view.Setting("identity.accept_on_behalf_of"); view.NodeID != enrollPub(2) || s == nil || s.Value != true || s.Source != "code" {
		t.Fatalf("view %+v", view)
	}
	off := false
	ack, err := admin.Set(ctx, NodeConfigSetRequest{
		Name: "notes-1", Tags: []string{"eu"},
		Attributes: map[string]NodeAttribute{"tier": AttrInt(3)},
		Identity:   &IdentityOverride{AcceptOnBehalfOf: &off},
	})
	if err != nil || !ack.Complete || ack.Generation != 1 {
		t.Fatalf("set: %+v %v", ack, err)
	}
	me, err = notesCall(ctx, rc, "whoami")
	if err != nil || me.Tier != 3 || me.Name != "notes-1" {
		t.Fatalf("handler sees the properties: %+v %v", me, err)
	}
	if props, err := srv.NodeProperties(); err != nil || props.Generation != 1 || !props.HasTag("eu") {
		t.Fatalf("server properties %+v %v", props, err)
	}
	_, err = notesCall(ctx, rc, "read", WithOnBehalfOf(token))
	wantCode(t, err, StatusPermissionDenied, "accept_on_behalf_of = false refuses the entry")
	if ack, err := admin.Ack(ctx, 1, time.Second); err != nil || !ack.Complete {
		t.Fatalf("ack: %+v %v", ack, err)
	}
	_, err = admin.Set(ctx, NodeConfigSetRequest{Sections: map[string]json.RawMessage{"network": json.RawMessage(`{"bind_port":1}`)}})
	wantCode(t, err, StatusPermissionDenied, "a section that is not overridable")
	if ack, err := admin.Clear(ctx, 0); err != nil || !ack.Complete {
		t.Fatalf("clear: %+v %v", ack, err)
	}
	if alice, err := notesCall(ctx, rc, "read", WithOnBehalfOf(token)); err != nil || alice.Actor != edgeID || alice.Tier != 0 {
		t.Fatalf("after clear: %+v %v", alice, err)
	}

	// A peer without aster.config-admin is refused by Gate 3.
	plain := startClient(t, ctx, enrolledCfg(6, credentialFor(t, 6, "notes.read")))
	prc, err := plain.Connect(ctx, server)
	if err != nil {
		t.Fatal(err)
	}
	_, err = NewNodeConfigClient(prc).Get(ctx)
	wantCode(t, err, StatusPermissionDenied, "NodeConfig needs aster.config-admin")
	// And a peer without aster.gateway cannot forward.
	_, err = notesCall(ctx, prc, "read", WithOnBehalfOf(token))
	wantCode(t, err, StatusPermissionDenied, "forwarding needs aster.gateway")
}

// ServerOptions.AcceptOnBehalfOf overrides the config: off, the entry is
// refused with PERMISSION_DENIED rather than ignored.
func TestGatewayIdentity_AcceptOffRefusesTheEntry(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	cfg := serverCfg(t, t.TempDir())
	cfg.Identity.AcceptOnBehalfOf = true
	off := false
	_, server := startNotes(t, ctx, cfg, ServerOptions{AcceptOnBehalfOf: &off})
	edge := startClient(t, ctx, enrolledCfg(5, credentialWith(t, 5, claimsWith(GatewayRole+",notes.read", "notes.read"))))
	rc, err := edge.Connect(ctx, server)
	if err != nil {
		t.Fatal(err)
	}
	token, err := edge.MintUserToken("alice", []string{"notes.read"}, []string{AMRPassword}, time.Minute)
	if err != nil {
		t.Fatal(err)
	}
	if me, err := notesCall(ctx, rc, "read"); err != nil || me.Actor != "" {
		t.Fatalf("as itself: %+v %v", me, err)
	}
	_, err = notesCall(ctx, rc, "read", WithOnBehalfOf(token))
	wantCode(t, err, StatusPermissionDenied, "accept_on_behalf_of off")
}

// A provider's ES256 token, forwarded unchanged by an edge, verifies with
// the issuer keys a node holding aster.identity-publisher wrote into the
// fleet namespace; its groups map to roles.
func TestGatewayIdentity_ProviderTokenFromTrustedIssuer(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 120*time.Second)
	defer cancel()
	const iss = "https://idp.test"
	cfg := serverCfg(t, t.TempDir())
	cfg.Identity.AcceptOnBehalfOf = true
	cfg.Identity.TrustedIssuers = []TrustedIssuer{{Iss: iss, Audiences: []string{"notes"},
		RolesClaim: "groups", RoleMap: map[string]string{"readers": "notes.read"}}}
	_, server := startNotes(t, ctx, cfg, ServerOptions{})

	idp := newTestIdP(t, iss)
	// The publisher: writes the key set into the fleet namespace as its own
	// node, carrying its credential, and syncs it to the server.
	publisherChain := credentialFor(t, 8, IdentityPublisherRole)
	publisher := startClient(t, ctx, enrolledCfg(8, publisherChain))
	if _, err := publisher.Connect(ctx, server); err != nil {
		t.Fatal(err)
	}
	fleet, err := publisher.Node().Docs().ImportWriteNamespace(ctx, fleetSecret)
	if err != nil {
		t.Fatal(err)
	}
	defer fleet.Close()
	record, _ := json.Marshal(map[string]any{
		"version": 1, "iss": iss, "source_url": iss + "/jwks", "fetched_at": time.Now().Unix(),
		"jwks": map[string]any{"keys": []any{idp.jwk()}}, "credential": publisherChain,
	})
	key := "aster/identity/v1/issuers/" + base64.RawURLEncoding.EncodeToString([]byte(iss)) + "/jwks"
	if err := fleet.SetBytes(ctx, AuthorID(enrollPub(8)), key, record); err != nil {
		t.Fatal(err)
	}
	if err := fleet.StartSync(ctx, enrollPub(2)); err != nil {
		t.Fatal(err)
	}

	edge := startClient(t, ctx, enrolledCfg(5, credentialFor(t, 5, GatewayRole)))
	rc, err := edge.Connect(ctx, server)
	if err != nil {
		t.Fatal(err)
	}
	token := idp.token(t, map[string]any{"sub": "alice", "aud": "notes", "groups": []string{"readers", "other"}, "amr": []string{"pwd"}})
	deadline := time.Now().Add(30 * time.Second)
	for {
		alice, err := notesCall(ctx, rc, "read", WithOnBehalfOf(token))
		if err == nil {
			if alice.Principal != iss+"|alice" || alice.Actor != enrollPub(5) || alice.Roles != "notes.read" ||
				alice.Issuer != iss || alice.Amr != "pwd" {
				t.Fatalf("provider token: %+v", alice)
			}
			break
		}
		var rpc *RpcError
		if !errors.As(err, &rpc) || rpc.Code != StatusUnauthenticated || time.Now().After(deadline) {
			t.Fatalf("provider token: %v", err)
		}
		time.Sleep(200 * time.Millisecond) // the key set is still syncing
	}
	// A token for another audience, or signed by another key, is refused.
	_, err = notesCall(ctx, rc, "read", WithOnBehalfOf(idp.token(t, map[string]any{"sub": "alice", "aud": "other"})))
	wantCode(t, err, StatusUnauthenticated, "wrong audience")
	forged := newTestIdP(t, iss).token(t, map[string]any{"sub": "alice", "aud": "notes"})
	_, err = notesCall(ctx, rc, "read", WithOnBehalfOf(forged))
	wantCode(t, err, StatusUnauthenticated, "another key with the same kid")
}

// testIdP signs ES256 ID tokens with one P-256 key (kid "k1").
type testIdP struct {
	iss string
	key *ecdsa.PrivateKey
}

func newTestIdP(t *testing.T, iss string) *testIdP {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	return &testIdP{iss: iss, key: key}
}

func (p *testIdP) jwk() map[string]any {
	pub, err := p.key.PublicKey.ECDH()
	if err != nil {
		panic(err)
	}
	raw := pub.Bytes() // 0x04 || X || Y
	return map[string]any{"kty": "EC", "crv": "P-256", "kid": "k1", "alg": "ES256", "use": "sig",
		"x": base64.RawURLEncoding.EncodeToString(raw[1:33]), "y": base64.RawURLEncoding.EncodeToString(raw[33:])}
}

func (p *testIdP) token(t *testing.T, claims map[string]any) string {
	t.Helper()
	now := time.Now().Unix()
	claims["iss"], claims["iat"], claims["exp"] = p.iss, now, now+300
	header, _ := json.Marshal(map[string]any{"alg": "ES256", "typ": "JWT", "kid": "k1"})
	body, _ := json.Marshal(claims)
	input := base64.RawURLEncoding.EncodeToString(header) + "." + base64.RawURLEncoding.EncodeToString(body)
	digest := sha256.Sum256([]byte(input))
	r, s, err := ecdsa.Sign(rand.Reader, p.key, digest[:])
	if err != nil {
		t.Fatal(err)
	}
	sig := make([]byte, 64)
	r.FillBytes(sig[:32])
	s.FillBytes(sig[32:])
	return input + "." + base64.RawURLEncoding.EncodeToString(sig)
}
