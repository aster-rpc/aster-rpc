# Roles from enrollment credentials, revoked by the root

A serving node admits only nodes enrolled by your root, and the roles in each
node's credential decide which methods it may call. The root revokes a node
and the serving node closes its connections; its next call is refused.
Nothing in the code reads a credential: it is all configuration.

| node | credential | what it does |
|------|-----------|--------------|
| serving node | from the root, no roles | `StartServer` from an `aster.toml` with `[trust]` and `[enrollment]`; serves `Clipboard.Read` (requires `clipboard.read`) and `Control.SetRoles` (requires `roles.admin`) |
| session node | roles `clipboard.read` | reads the clipboard; then the root revokes it |
| viewer node | no roles | enrolled, so it connects, but `Clipboard.Read` refuses it |
| API node | roles `roles.admin` | changes roles at runtime through `Control.SetRoles` |
| stranger | none | cannot connect at all |

This is the same path as the [Mission Control auth chapter](../mission_control):
`StartServer` / `StartClient` from configuration, design §15
(`docs/_internal/working_ideas/aster-enrollment.md`).

## How it works

- The serving node's `aster.toml` names the root (`[trust] root_pubkey`), its
  own key and credential, the fleet audience and namespace, and a durable
  state path (`[enrollment]`). That enables Enrollment: the node admits only
  peers that complete the enrollment handshake with a credential from that
  root (Gate 0), so the stranger never reaches `Clipboard`.
- Each client is `StartClient` with its key, its credential, the root and the
  same `[enrollment]` audience and namespace. Before its first connection it
  enrolls with the server over the reciprocal handshake.
- A role is a comma-separated value of the `aster.role` claim attribute in
  the credential (`aster enroll node --attributes roles.json` with
  `{"aster.role": {"type": "string", "value": "clipboard.read"}}`). The server
  maps it to the attribute Gate 3 checks `Requires` against, **on every
  call** from the peer's live enrollment session.
- `aster.Requires(aster.RequireRole("clipboard.read"))` puts the requirement
  in the method contract; Rust enforces it before the Go handler runs.
- Revocation is the root's: `aster enroll revoke --subject <id> --fleet-only
  --peer <server>` publishes a revocation record into the fleet namespace the
  serving node holds. The serving node revokes the session, closes every
  connection of that node, and refuses it from then on, durably (it survives
  restarts in `state_path`) and without any code in the application.

## Runtime role changes (secondary)

Credentials carry the roles a node has for the credential's lifetime. For
roles that change mid-session without issuing a new credential, a control
RPC on the serving node can call `AsterServer.SetPeerAttributes`, the escape
hatch. Here `Control.SetRoles` requires `roles.admin`, which the API node has
**in its credential**, so the control plane itself is gated by enrollment.

- Runtime roles live in memory on the serving node: they are lost on
  restart and not replicated. Durable roles belong in credentials.
- Roles from a credential win on a clash: a runtime grant applies to nodes
  whose credentials carry no roles (the viewer here).
- A runtime revoke applies from the next call; a call already past Gate 3
  completes. A root revocation, by contrast, closes the node's connections.

## Run

From `bindings/go`, with the native library built (see the binding README,
"CGO_CFLAGS/CGO_LDFLAGS"):

```sh
export CGO_CFLAGS="-I$(pwd)/../../ffi"
export CGO_LDFLAGS="-L$(pwd)/../../target/debug -laster_transport_ffi -Wl,-rpath,$(pwd)/../../target/debug"

go run ./examples/roles_revocation
go test ./examples/roles_revocation     # the same scenario as a test
```

The demo is self-contained: it plays the root in process
(`internal/testroot`, which signs what `aster key root generate`,
`aster enroll node` and `aster enroll revoke` produce) and writes the key,
credential and `aster.toml` files to a temporary directory. In production
the operator runs the CLI and each process reads its files from
configuration. Output from a real run (Rust log lines on stderr omitted):

```
serving node 18c41b8ba72e... (aster.toml, enrollment on)
session node eb6f1cf03efe... (credential roles: clipboard.read)
viewer node  0ea438866bf3... (credential roles: none)

1. the session node reads: its credential carries clipboard.read
   -> "contents of desk-7"

2. the viewer reads: enrolled, but its credential carries no role
   -> PERMISSION_DENIED ([PERMISSION_DENIED] capability check failed for 'Clipboard.Read')

3. a node with no credential tries to connect
   -> refused at admission ([UNAVAILABLE] stream ended before response)

4. the root revokes the session node (aster enroll revoke --peer <server>)
   serving node revoked eb6f1cf03efe... (node_revoked) and closed its connections

5. the session node's next call
   -> refused ([UNAVAILABLE] connection lost)
   -> and it cannot come back: the revocation is the root's, durable on the serving node

6. mid-session change without a new credential: the API node (roles.admin in its credential) grants the viewer clipboard.read
   viewer read -> "contents of desk-7"
   -> PERMISSION_DENIED ([PERMISSION_DENIED] capability check failed for 'Clipboard.Read')

OK
```
