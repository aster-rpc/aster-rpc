//go:build cgo

// Example: roles from enrollment credentials, revoked by the root.
//
// A serving node runs StartServer from an aster.toml that enables
// Enrollment. Every other node is a StartClient configured with its own key
// and credential. Nothing in the code reads a credential:
//
//   - The serving node admits only nodes holding a credential from the
//     configured root, and the roles in each credential's aster.role claim
//     are what Requires checks, on every call.
//   - A session node whose credential carries "clipboard.read" reads the
//     clipboard; a viewer whose credential carries no role is refused; a node
//     with no credential cannot connect at all.
//   - The root revokes the session node (aster enroll revoke): the serving
//     node closes its connections, and its next call is refused.
//   - Secondary: roles that change mid-session without re-issuing a
//     credential, through a small control RPC (Control.SetRoles, itself gated
//     by the roles.admin role from the API node's credential).
//
// The root operations (keys, credentials, the revocation) are what the aster
// CLI does; this self-contained demo plays the root in process with
// internal/testroot. Run (see README.md):
//
//	go run ./examples/roles_revocation
package main

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"log"
	"os"
	"path/filepath"
	"strings"
	"time"

	aster "sdk.getaster.now/go"
	"sdk.getaster.now/go/internal/testroot"
)

// ─── wire types ───────────────────────────────────────────────────────────

type ReadRequest struct{ ClipboardID string }

func (ReadRequest) AsterWireType() string { return "examples.roles/ReadRequest" }

type ReadReply struct{ Text string }

func (ReadReply) AsterWireType() string { return "examples.roles/ReadReply" }

type SetRolesRequest struct {
	// Peer is the 64-hex node id whose runtime roles are replaced.
	Peer string
	// Roles is the complete comma-separated role set; "" revokes all.
	Roles string
}

func (SetRolesRequest) AsterWireType() string { return "examples.roles/SetRolesRequest" }

type SetRolesReply struct{ Roles string }

func (SetRolesReply) AsterWireType() string { return "examples.roles/SetRolesReply" }

const (
	RoleClipboardRead = "clipboard.read"
	RoleAdmin         = "roles.admin"
	audience          = "kasm-sessions"
)

// ─── serving node ─────────────────────────────────────────────────────────

// Services builds Clipboard (Read requires clipboard.read) and Control
// (SetRoles requires roles.admin). srv is read when a handler runs (it is
// set once StartServer returns).
func Services(srv **aster.AsterServer) []*aster.Service {
	clipboard := aster.NewService("Clipboard", 1,
		aster.Unary("Read", func(ctx context.Context, r *ReadRequest) (*ReadReply, error) {
			return &ReadReply{Text: "contents of " + r.ClipboardID}, nil
		}, aster.Requires(aster.RequireRole(RoleClipboardRead))),
	)
	control := aster.NewService("Control", 1,
		aster.Unary("SetRoles", func(ctx context.Context, r *SetRolesRequest) (*SetRolesReply, error) {
			peer := strings.ToLower(strings.TrimSpace(r.Peer))
			if len(peer) != 64 {
				return nil, aster.NewRpcError(aster.StatusInvalidArgument, "peer must be a 64-hex node id")
			}
			roles := normalizeRoles(r.Roles)
			attrs := map[string]string{}
			if roles != "" {
				attrs["aster.role"] = roles
			}
			// Runtime roles (the escape hatch). Roles from a peer's
			// credential win on a clash, so these apply to peers whose
			// credentials carry no roles.
			if err := (*srv).SetPeerAttributes(peer, attrs); err != nil {
				return nil, err
			}
			return &SetRolesReply{Roles: roles}, nil
		}, aster.Requires(aster.RequireRole(RoleAdmin))),
	)
	return []*aster.Service{clipboard, control}
}

func normalizeRoles(s string) string {
	var out []string
	for r := range strings.SplitSeq(s, ",") {
		if r = strings.TrimSpace(r); r != "" {
			out = append(out, r)
		}
	}
	return strings.Join(out, ",")
}

// ─── the operator's files (what the aster CLI produces) ───────────────────

type fleet struct {
	dir    string
	root   *testroot.Root
	secret [32]byte // aster enroll namespace create --out-secret
}

func newFleet(dir string) (*fleet, error) {
	root, err := testroot.Generate(audience) // aster key root generate
	if err != nil {
		return nil, err
	}
	f := &fleet{dir: dir, root: root}
	_, err = rand.Read(f.secret[:])
	return f, err
}

// node writes a key (aster key node generate --out <name>.key) and a
// credential carrying roles (aster enroll node --attributes roles.json
// --out <name>.cred), and returns the key file, credential file and node id.
func (f *fleet) node(name, roles string) (keyFile, credFile, id string, err error) {
	seed := make([]byte, 32)
	if _, err = rand.Read(seed); err != nil {
		return
	}
	return f.nodeWithSeed(name, seed, roles)
}

func (f *fleet) nodeWithSeed(name string, seed []byte, roles string) (keyFile, credFile, id string, err error) {
	id = testroot.NodeID(seed)
	keyFile = filepath.Join(f.dir, name+".key")
	if err = os.WriteFile(keyFile, []byte(hex.EncodeToString(seed)+"\n"), 0o600); err != nil {
		return
	}
	chain, err := f.root.Credential(id, testroot.RoleClaims(roles), time.Hour)
	if err != nil {
		return
	}
	credFile = filepath.Join(f.dir, name+".cred")
	err = os.WriteFile(credFile, []byte(chain+"\n"), 0o600)
	return
}

// clientConfig is what a node's environment would say (ASTER_SECRET_KEY_FILE,
// ASTER_ROOT_PUBKEY, ASTER_ENROLLMENT_CREDENTIAL, ASTER_ENROLLMENT_*), here as
// the struct; credFile "" = no enrollment.
func (f *fleet) clientConfig(keyFile, credFile string) aster.AsterConfig {
	cfg := aster.DefaultConfig()
	cfg.RelayMode, cfg.BindAddr, cfg.SecretKeyFile = "disabled", "127.0.0.1:0", keyFile
	if credFile != "" {
		cfg.RootPubkey, _ = hex.DecodeString(f.root.PublicKey())
		cfg.EnrollmentCredentialFile = credFile
		cfg.Enrollment = aster.EnrollmentConfig{Audience: audience, FleetNamespace: testroot.Namespace(f.secret)}
	}
	return cfg
}

// serverTOML is the serving node's aster.toml.
func (f *fleet) serverTOML(keyFile, credFile string) (string, error) {
	path := filepath.Join(f.dir, "aster.toml")
	text := fmt.Sprintf(`[trust]
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
`, f.root.PublicKey(), credFile, keyFile, audience, testroot.Namespace(f.secret), filepath.Join(f.dir, "enrollment.json"))
	return path, os.WriteFile(path, []byte(text), 0o600)
}

// revoke is `aster enroll revoke --subject <id> --fleet-only --peer
// <server>`: the root enrolls with the serving node (it holds the fleet
// namespace) and publishes a revocation record into it, staying connected
// until delivered reports the server has it.
func (f *fleet) revoke(ctx context.Context, server *aster.NodeAddr, subject string, delivered func() error) error {
	keyFile, credFile, _, err := f.nodeWithSeed("root", f.root.Seed(), "")
	if err != nil {
		return err
	}
	root, err := aster.StartClient(ctx, f.clientConfig(keyFile, credFile))
	if err != nil {
		return err
	}
	defer root.Close()
	if _, err := root.Connect(ctx, server); err != nil {
		return err
	}
	doc, err := root.Node().Docs().ImportWriteNamespace(ctx, aster.Key(f.secret))
	if err != nil {
		return err
	}
	defer doc.Close()
	if err := doc.SetBytes(ctx, aster.AuthorID(f.root.PublicKey()), testroot.RegistrationKey(subject),
		f.root.Revocation(subject, "session ended")); err != nil {
		return err
	}
	if err := doc.StartSync(ctx, server.EndpointID); err != nil {
		return err
	}
	return delivered()
}

// ─── demo ─────────────────────────────────────────────────────────────────

// Run runs the whole scenario and returns an error if any step does not
// behave as described.
func Run(ctx context.Context, out io.Writer) error {
	say := func(format string, args ...any) { fmt.Fprintf(out, format+"\n", args...) }
	dir, err := os.MkdirTemp("", "roles-revocation-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(dir)
	f, err := newFleet(dir)
	if err != nil {
		return err
	}

	// The serving node: StartServer from its aster.toml.
	srvKey, srvCred, _, err := f.node("server", "")
	if err != nil {
		return err
	}
	toml, err := f.serverTOML(srvKey, srvCred)
	if err != nil {
		return err
	}
	cfg, err := aster.ConfigFromTOML(toml)
	if err != nil {
		return err
	}
	var srv *aster.AsterServer
	srv, err = aster.StartServer(ctx, aster.ServerOptions{Config: cfg, Services: Services(&srv)})
	if err != nil {
		return err
	}
	defer srv.Close()
	ticket, err := srv.Ticket(ctx)
	if err != nil {
		return err
	}
	server, err := aster.NodeAddrFromTicket(ticket)
	if err != nil {
		return err
	}
	revocations := make(chan aster.Revocation, 4)
	go func() {
		for {
			rev, err := srv.Enrollment().NextRevocation(ctx)
			if err != nil {
				return
			}
			revocations <- rev
		}
	}()

	// Each node: its key, its credential, StartClient.
	start := func(name, roles string, enrolled bool) (*aster.AsterClient, string, error) {
		key, cred, id, err := f.node(name, roles)
		if err != nil {
			return nil, "", err
		}
		if !enrolled {
			cred = ""
		}
		cl, err := aster.StartClient(ctx, f.clientConfig(key, cred))
		return cl, id, err
	}
	session, sessID, err := start("session", RoleClipboardRead, true)
	if err != nil {
		return err
	}
	defer session.Close()
	viewer, viewerID, err := start("viewer", "", true)
	if err != nil {
		return err
	}
	defer viewer.Close()
	api, _, err := start("api", RoleAdmin, true)
	if err != nil {
		return err
	}
	defer api.Close()
	srvID, _ := srv.NodeID()
	say("serving node %s (aster.toml, enrollment on)", short(srvID))
	say("session node %s (credential roles: %s)", short(sessID), RoleClipboardRead)
	say("viewer node  %s (credential roles: none)", short(viewerID))

	read := func(cl *aster.AsterClient) (*ReadReply, error) {
		cctx, cancel := context.WithTimeout(ctx, 10*time.Second)
		defer cancel()
		conn, err := cl.Connect(cctx, server)
		if err != nil {
			return nil, err
		}
		return aster.Call[ReadRequest, ReadReply](cctx, conn, "Clipboard", "Read", &ReadRequest{ClipboardID: "desk-7"})
	}
	expectDenied := func(step string, err error) error {
		if !errors.Is(err, aster.ErrRpcPermissionDenied) {
			return fmt.Errorf("%s: want PERMISSION_DENIED, got %v", step, err)
		}
		say("   -> PERMISSION_DENIED (%v)", err)
		return nil
	}

	say("\n1. the session node reads: its credential carries %s", RoleClipboardRead)
	r, err := read(session)
	if err != nil {
		return fmt.Errorf("session read: %w", err)
	}
	say("   -> %q", r.Text)

	say("\n2. the viewer reads: enrolled, but its credential carries no role")
	_, err = read(viewer)
	if err := expectDenied("viewer read", err); err != nil {
		return err
	}

	say("\n3. a node with no credential tries to connect")
	stranger, _, err := start("stranger", "", false)
	if err != nil {
		return err
	}
	defer stranger.Close()
	if _, err := read(stranger); err == nil {
		return errors.New("a node without a credential reached the service")
	} else {
		say("   -> refused at admission (%v)", firstLine(err))
	}

	say("\n4. the root revokes the session node (aster enroll revoke --peer <server>)")
	err = f.revoke(ctx, server, sessID, func() error {
		select {
		case rev := <-revocations:
			say("   serving node revoked %s (%s) and closed its connections", short(rev.Peer), rev.Reason.Code)
			return nil
		case <-time.After(30 * time.Second):
			return errors.New("the serving node did not revoke the session node")
		}
	})
	if err != nil {
		return fmt.Errorf("revoke: %w", err)
	}

	say("\n5. the session node's next call")
	if _, err := read(session); err == nil {
		return errors.New("a revoked node's call succeeded")
	} else {
		say("   -> refused (%v)", firstLine(err))
	}
	if _, err := session.Reconnect(ctx, server); err == nil {
		if _, err := read(session); err == nil {
			return errors.New("a revoked node reconnected and read")
		}
	}
	say("   -> and it cannot come back: the revocation is the root's, durable on the serving node")

	say("\n6. mid-session change without a new credential: the API node (%s in its credential) grants the viewer %s",
		RoleAdmin, RoleClipboardRead)
	conn, err := api.Connect(ctx, server)
	if err != nil {
		return err
	}
	setRoles := func(roles string) error {
		_, err := aster.Call[SetRolesRequest, SetRolesReply](ctx, conn, "Control", "SetRoles", &SetRolesRequest{Peer: viewerID, Roles: roles})
		return err
	}
	if err := setRoles(RoleClipboardRead); err != nil {
		return fmt.Errorf("grant: %w", err)
	}
	if r, err = read(viewer); err != nil {
		return fmt.Errorf("viewer read after the grant: %w", err)
	}
	say("   viewer read -> %q", r.Text)
	if err := setRoles(""); err != nil {
		return fmt.Errorf("revoke runtime roles: %w", err)
	}
	_, err = read(viewer)
	if err := expectDenied("viewer read after the runtime revoke", err); err != nil {
		return err
	}
	say("\nOK")
	return nil
}

func short(id string) string {
	if len(id) > 12 {
		return id[:12] + "..."
	}
	return id
}

func firstLine(err error) string {
	s := err.Error()
	if i := strings.IndexByte(s, '\n'); i >= 0 {
		s = s[:i]
	}
	if len(s) > 100 {
		s = s[:100] + "..."
	}
	return s
}

func main() {
	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()
	if err := Run(ctx, os.Stdout); err != nil {
		log.Fatal(err)
	}
}
