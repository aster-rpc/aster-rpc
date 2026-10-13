# Roles per node id, revoked at runtime

An API node grants a session node a set of roles on a serving node, then
revokes them. After the revocation, the session node's next method call is
refused with `PERMISSION_DENIED`.

| node | role on the serving node | what it does |
|------|--------------------------|--------------|
| serving node | (n/a) | serves `Clipboard.Read` (requires `clipboard.read`) and `Control.SetRoles` (requires `roles.admin`) |
| API node | `roles.admin` (set from the serving node's own config at boot) | calls `Control.SetRoles` to grant or revoke roles for any other node id |
| session node | whatever the API node grants | calls `Clipboard.Read` |

## How it works

- `aster.Requires(aster.RequireRole("clipboard.read"))` puts the
  requirement in the method contract. Rust enforces it (Gate 3) **before**
  the Go handler runs. A caller without the role never reaches your code.
- Gate 3 checks the caller's attributes on the serving node. You set them
  with `AsterServer.SetPeerAttributes(peerNodeID, map)`. Roles go under the
  key `aster.role` as a comma-separated list. An empty or nil map clears
  them.
- Those attributes live in memory **on the serving node**. Another node
  changes them through a small control RPC on the serving node, which calls
  `SetPeerAttributes`. Here that RPC is `Control.SetRoles`:
  - It requires `roles.admin`. Only the API node has that role, granted by
    the serving node's own configuration. The session node is refused.
  - It refuses to change the API node's own entry, so an admin cannot lock
    itself (or a co-admin) out over RPC.
  - It takes the complete role set for a peer. `""` revokes everything.
  - The peer identity Gate 3 checks is the QUIC-authenticated node id. Roles
    are bound to the node's key, not to anything the caller claims.
- Gate 0 (admission: who may connect at all) is separate from roles. The
  demo runs the server in the default protected trust mode and admits the
  API and session nodes with `Gate().Admit`. Removing roles leaves the
  session node connected but powerless. Use `Gate().Revoke` if it should
  not connect at all.

## Revocation semantics

- **Latency: the next call.** Gate 3 evaluates the requirement on every
  call against the current attributes. A revocation applies to every call
  that starts after `SetPeerAttributes` returns, including calls on an
  already open connection. There is no cache and no reconnect. The time from
  the API node's decision to enforcement is one `Control.SetRoles` round trip
  (2 ms on loopback in the run below).
- **In-flight calls are not interrupted.** A call that already passed Gate
  3 runs to completion (step 4/6 below). For long-running streams or
  session-scoped services, re-check inside the handler if you need that:
  `CallInfoFromContext(ctx).Attributes` is the snapshot taken when the call
  started. Alternatively, close the connection to cut the peer off.
- **Not durable.** The attributes are in-memory state of one serving node.
  After a restart they are gone, and they are not replicated to other
  replicas. The API node (or the serving node's config) must re-apply them.
  For example, on startup the serving node pulls the current role table
  from the API, or the API pushes it to every replica.
- **Coming later: Enrollment claims.** Roles carried as claims in an
  Enrollment credential (verified by the serving node, with no control RPC
  and no per-node push) are being added to the Go binding separately. That
  path survives restarts and scales to many serving nodes. Revocation then
  happens by re-issuing or denying enrollment, not by `SetPeerAttributes`.

## Run

From `bindings/go`, with the native library built (see the binding README,
"CGO_CFLAGS/CGO_LDFLAGS"):

```sh
export CGO_CFLAGS="-I$(pwd)/../../ffi"
export CGO_LDFLAGS="-L$(pwd)/../../target/debug -laster_transport_ffi -Wl,-rpath,$(pwd)/../../target/debug"

go run ./examples/roles_revocation
go test ./examples/roles_revocation     # the same scenario as a test
```

Output from a real run:

```
serving node 7315ffb42c95...
API node     06996a49a51f... (role roles.admin)
session node 82d6c559ec57... (no roles)

1. session reads before any grant
   -> PERMISSION_DENIED ([PERMISSION_DENIED] capability check failed for 'Clipboard.Read')

2. API grants "clipboard.read,clipboard.write" to the session node
   session read -> "contents of desk-7"

3. session tries to grant itself roles.admin
   -> PERMISSION_DENIED ([PERMISSION_DENIED] capability check failed for 'Control.SetRoles')

4. session starts a slow read (1.5 s); API revokes while it runs
   API revoked all roles (SetRoles round trip 2ms)

5. session's next call, same connection
   -> PERMISSION_DENIED ([PERMISSION_DENIED] capability check failed for 'Clipboard.Read')

6. the read that was already in flight completed: "contents of desk-7"

7. the API node's own grant is not changeable over RPC
   -> PERMISSION_DENIED ([PERMISSION_DENIED] roles of 06996a49...30a2 are not managed over RPC)

OK
```
