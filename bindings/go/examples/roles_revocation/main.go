//go:build cgo

// Example: roles per node id, granted and revoked at runtime.
//
// Three nodes:
//
//   - serving node: serves Clipboard.Read (requires role "clipboard.read")
//     and Control.SetRoles (requires role "roles.admin").
//   - API node: holds "roles.admin" on the serving node and grants or
//     revokes roles for other node ids by calling Control.SetRoles.
//   - session node: calls Clipboard.Read.
//
// Roles live in memory on the serving node (AsterServer.SetPeerAttributes,
// key "aster.role"). Gate 3 checks them on every call before the handler
// runs, so a revocation applies from the session node's next call, on the
// same connection. A call already in flight is not interrupted.
//
// Run (see README.md):
//
//	go run ./examples/roles_revocation
package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log"
	"os"
	"strings"
	"time"

	aster "sdk.getaster.now/go"
)

// ─── wire types ───────────────────────────────────────────────────────────

type ReadRequest struct {
	ClipboardID string
	// DelayMs holds the handler this long (to show an in-flight call).
	DelayMs int32
}

func (ReadRequest) AsterWireType() string { return "examples.roles/ReadRequest" }

type ReadReply struct{ Text string }

func (ReadReply) AsterWireType() string { return "examples.roles/ReadReply" }

type SetRolesRequest struct {
	// Peer is the 64-hex node id whose roles are replaced.
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
	roleKey           = "aster.role" // the attribute Gate 3 reads, comma-separated
)

// ─── serving node ─────────────────────────────────────────────────────────

// Services builds the two services. srv is read when a handler runs (it is
// set once StartServer returns); protected are node ids whose roles
// SetRoles must not change (the API node's own grant).
func Services(srv **aster.AsterServer, protected map[string]bool) []*aster.Service {
	clipboard := aster.NewService("Clipboard", 1,
		aster.Unary("Read", func(ctx context.Context, r *ReadRequest) (*ReadReply, error) {
			if r.DelayMs > 0 {
				select {
				case <-time.After(time.Duration(r.DelayMs) * time.Millisecond):
				case <-ctx.Done():
					return nil, ctx.Err()
				}
			}
			return &ReadReply{Text: "contents of " + r.ClipboardID}, nil
		}, aster.Requires(aster.RequireRole(RoleClipboardRead))),
	)
	control := aster.NewService("Control", 1,
		aster.Unary("SetRoles", func(ctx context.Context, r *SetRolesRequest) (*SetRolesReply, error) {
			info, _ := aster.CallInfoFromContext(ctx)
			if info == nil || !info.IrohAuthenticated {
				return nil, aster.NewRpcError(aster.StatusPermissionDenied, "caller identity not authenticated")
			}
			peer := strings.ToLower(strings.TrimSpace(r.Peer))
			if len(peer) != 64 {
				return nil, aster.NewRpcError(aster.StatusInvalidArgument, "peer must be a 64-hex node id")
			}
			if protected[peer] {
				return nil, aster.NewRpcError(aster.StatusPermissionDenied, "roles of %s are not managed over RPC", peer)
			}
			roles := normalizeRoles(r.Roles)
			attrs := map[string]string{}
			if roles != "" {
				attrs[roleKey] = roles
			}
			// Empty map clears: the peer has no roles from the next call on.
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

// loopback is a relay-free local config for the demo.
func loopback() aster.AsterConfig {
	cfg := aster.DefaultConfig()
	cfg.RelayMode, cfg.BindAddr = "disabled", "127.0.0.1:0"
	return cfg
}

// ─── demo ─────────────────────────────────────────────────────────────────

// Run runs the whole scenario and returns an error if any step does not
// behave as described.
func Run(ctx context.Context, out io.Writer) error {
	say := func(format string, args ...any) { fmt.Fprintf(out, format+"\n", args...) }

	// The API and session nodes (clients have their own node ids).
	api, err := aster.StartClient(ctx, loopback())
	if err != nil {
		return err
	}
	defer api.Close()
	sess, err := aster.StartClient(ctx, loopback())
	if err != nil {
		return err
	}
	defer sess.Close()
	apiID, _ := api.Node().NodeID()
	sessID, _ := sess.Node().NodeID()

	// The serving node. Gate 0 (who may connect at all) is separate from
	// roles: both nodes are admitted, neither has a role yet.
	var srv *aster.AsterServer
	srv, err = aster.StartServer(ctx, aster.ServerOptions{
		Config:   loopback(),
		Services: Services(&srv, map[string]bool{apiID: true}),
	})
	if err != nil {
		return err
	}
	defer srv.Close()
	for _, id := range []string{apiID, sessID} {
		if err := srv.Gate().Admit(id); err != nil {
			return err
		}
	}
	// Bootstrap: the serving node's own configuration trusts the API node.
	if err := srv.SetPeerAttributes(apiID, map[string]string{roleKey: RoleAdmin}); err != nil {
		return err
	}
	srvID, _ := srv.NodeID()
	say("serving node %s", short(srvID))
	say("API node     %s (role %s)", short(apiID), RoleAdmin)
	say("session node %s (no roles)", short(sessID))

	ticket, err := srv.Ticket(ctx)
	if err != nil {
		return err
	}
	apiConn, err := api.ConnectTicket(ctx, ticket)
	if err != nil {
		return err
	}
	sessConn, err := sess.ConnectTicket(ctx, ticket)
	if err != nil {
		return err
	}

	read := func(delay time.Duration) (*ReadReply, error) {
		return aster.Call[ReadRequest, ReadReply](ctx, sessConn, "Clipboard", "Read",
			&ReadRequest{ClipboardID: "desk-7", DelayMs: int32(delay.Milliseconds())})
	}
	setRoles := func(conn *aster.RpcConnection, peer, roles string) error {
		_, err := aster.Call[SetRolesRequest, SetRolesReply](ctx, conn, "Control", "SetRoles",
			&SetRolesRequest{Peer: peer, Roles: roles})
		return err
	}
	expectDenied := func(step string, err error) error {
		if !errors.Is(err, aster.ErrRpcPermissionDenied) {
			return fmt.Errorf("%s: want PERMISSION_DENIED, got %v", step, err)
		}
		say("   -> PERMISSION_DENIED (%v)", err)
		return nil
	}

	say("\n1. session reads before any grant")
	_, err = read(0)
	if err := expectDenied("read before grant", err); err != nil {
		return err
	}

	say("\n2. API grants %q to the session node", RoleClipboardRead+",clipboard.write")
	if err := setRoles(apiConn, sessID, RoleClipboardRead+", clipboard.write"); err != nil {
		return fmt.Errorf("grant: %w", err)
	}
	r, err := read(0)
	if err != nil {
		return fmt.Errorf("read after grant: %w", err)
	}
	say("   session read -> %q", r.Text)

	say("\n3. session tries to grant itself %s", RoleAdmin)
	if err := expectDenied("self-grant", setRoles(sessConn, sessID, RoleAdmin)); err != nil {
		return err
	}

	say("\n4. session starts a slow read (1.5 s); API revokes while it runs")
	type result struct {
		r   *ReadReply
		err error
	}
	inflight := make(chan result, 1)
	go func() {
		r, err := read(1500 * time.Millisecond)
		inflight <- result{r, err}
	}()
	time.Sleep(300 * time.Millisecond) // let the call pass Gate 3
	start := time.Now()
	if err := setRoles(apiConn, sessID, ""); err != nil {
		return fmt.Errorf("revoke: %w", err)
	}
	say("   API revoked all roles (SetRoles round trip %v)", time.Since(start).Round(time.Millisecond))

	say("\n5. session's next call, same connection")
	_, err = read(0)
	if err := expectDenied("read after revoke", err); err != nil {
		return err
	}

	res := <-inflight
	if res.err != nil {
		return fmt.Errorf("in-flight read should complete after revoke, got %v", res.err)
	}
	say("\n6. the read that was already in flight completed: %q", res.r.Text)

	say("\n7. the API node's own grant is not changeable over RPC")
	if err := expectDenied("change API roles", setRoles(apiConn, apiID, "")); err != nil {
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

func main() {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	if err := Run(ctx, os.Stdout); err != nil {
		log.Fatal(err)
	}
}
