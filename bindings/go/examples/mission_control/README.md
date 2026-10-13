# Mission Control (Go): the auth chapter

The Go port of [Rust Mission Control](../../../../examples/rust/mission-control)
and its auth chapter: one `MissionControl` service with all four RPC patterns,
the same method names and role names, served by `StartServer` and called by
an agent built on `StartClient`. Everything comes from configuration
(`aster.ConfigFromEnv()` or an `aster.toml`).

| Method | Pattern | Requires (with `-auth`) |
|--------|---------|-------------------------|
| `getStatus` | unary | `ops.status` |
| `submitLog` | unary | any enrolled node |
| `tailLogs` | server-stream | `ops.logs` or `ops.admin` |
| `ingestMetrics` | client-stream | `ops.ingest` |
| `runCommand` | bidi-stream | `ops.admin` |

Nothing in the code reads a credential. With enrollment configured the server
admits only nodes holding a credential from your root, and each peer's
`aster.role` claim becomes the role set `Requires` is checked against, on
every call. The roles are in [`mc/mc.go`](mc/mc.go), identical to the Rust
`roles.rs` and Python `roles.py`.

## Run

Build the native library and set the cgo flags (binding README), then from
`bindings/go`:

```sh
go run ./examples/mission_control/server            # open (no enrollment)
go run ./examples/mission_control/agent aster1...
```

## With auth

**1. Produce the files with the CLI** (as the root operator):

```bash
aster key root generate
ROOT=$(aster key root show --json | jq -r .root_pubkey)
FLEET=$(aster enroll namespace create --out-secret fleet.secret --json | jq -r .namespace)
SERVER=$(aster key node generate --out server.key --json | jq -r .node_id)
AGENT=$(aster key node generate --out agent.key --json | jq -r .node_id)

aster enroll node --subject $SERVER --audience mission-control --out server.cred
echo '{"aster.role":{"type":"string","value":"ops.status,ops.ingest"}}' > agent-roles.json
aster enroll node --subject $AGENT --audience mission-control \
  --attributes agent-roles.json --out agent.cred
```

**2. Start the server** with those files:

```bash
export ASTER_ROOT_PUBKEY=$ROOT
export ASTER_ENROLLMENT_AUDIENCE=mission-control
export ASTER_ENROLLMENT_FLEET_NAMESPACE=$FLEET

ASTER_SECRET_KEY_FILE=server.key \
ASTER_ENROLLMENT_CREDENTIAL=server.cred \
ASTER_ENROLLMENT_STATE_PATH=./mc-enrollment.json \
go run ./examples/mission_control/server -auth     # prints its aster1… address
```

or the same as an `aster.toml` (`-config aster.toml`):

```toml
[trust]
root_pubkey = "<ROOT>"
enrollment_credential = "server.cred"

[network]
secret_key_file = "server.key"

[enrollment]
audience = "mission-control"
fleet_namespace = "<FLEET>"
state_path = "./mc-enrollment.json"
```

**3. Run the agent** with its own key and credential:

```bash
ASTER_SECRET_KEY_FILE=agent.key ASTER_ENROLLMENT_CREDENTIAL=agent.cred \
go run ./examples/mission_control/agent aster1…
```

`getStatus` and `ingestMetrics` succeed. A credential without those roles
gets `PERMISSION_DENIED`, and a node with no credential cannot connect.

**4. Revoke** the agent as the root; the server holds the fleet namespace, so
the root's publication session delivers to it:

```bash
aster enroll revoke --subject $AGENT --fleet-only --fleet-namespace $FLEET \
  --fleet-secret-file fleet.secret --audience mission-control --peer $SERVER
```

The agent's next call is refused and its connection closes.

## Test

```sh
go test ./examples/mission_control/...
```

[`mc/auth_test.go`](mc/auth_test.go) runs this chapter (the Go version of the
Rust `tests/auth.rs`): a server configured from an `aster.toml`, and
operator, agent, admin and role-less clients, each reaching exactly the
methods its roles allow. A test root (`internal/testroot`) signs the
credentials the CLI would.

## Differences from the Rust example

- No HTTP transport here; in auth mode the Rust example serves Iroh only too.
- `runCommand` echoes the command; it does not run a shell.
- The wire type and method names are the Rust example's; cross-binding calls
  against the Rust server are not part of this example's tests.
