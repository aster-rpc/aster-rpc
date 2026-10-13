# .NET examples

```sh
dotnet run --project bindings/dotnet/examples            # echo: an in-process server and client
dotnet run --project bindings/dotnet/examples -- runtime # native runtime smoke test
```

Build the native library first (`cargo build -p aster_transport_ffi`); the project
copies it from `target/release` or `target/debug`, or from `-p:NativeLibraryPath=…`.

## Mission Control: the auth chapter

The .NET port of [Rust Mission Control](../../../examples/rust/mission-control)
and its auth chapter ([`MissionControl.cs`](MissionControl.cs)): one
`MissionControl` service with all four RPC patterns, the same method, wire type
and role names as Rust, Python, TypeScript and Go, served by `AsterServer` and
called by an agent built on `AsterClient`. Everything comes from configuration
(`AsterConfig.FromEnv()` or an `aster.toml`).

| Method | Pattern | Requires (with `--auth`) |
|--------|---------|--------------------------|
| `getStatus` | unary | `ops.status` |
| `submitLog` | unary | any enrolled node |
| `tailLogs` | server-stream | `ops.logs` or `ops.admin` |
| `ingestMetrics` | client-stream | `ops.ingest` |
| `runCommand` | bidi-stream | `ops.admin` |

Nothing in the code reads a credential. With enrollment configured the server
admits only nodes holding a credential from your root, and each peer's
`aster.role` claim becomes the role set the requirements are checked against,
on every call.

### Run open

```sh
dotnet run --project bindings/dotnet/examples -- mc-server          # prints its aster1… address
dotnet run --project bindings/dotnet/examples -- mc-agent aster1…
```

### With auth

**1. Produce the files with the Rust `aster` CLI** (as the root operator):

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
dotnet run --project bindings/dotnet/examples -- mc-server --auth   # prints its aster1… address
```

or the same as an `aster.toml` (`mc-server --auth --config aster.toml`):

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
dotnet run --project bindings/dotnet/examples -- mc-agent aster1…
```

`getStatus` and `ingestMetrics` succeed. A credential without those roles gets
`PermissionDenied`, and a node with no credential cannot connect
(`enrollment required`).

**4. Revoke** the agent as the root. The server holds the fleet namespace, so
the root's publication session delivers to it (the CLI profile needs its own
enrolled publisher identity, `exec_credential_file`, and a route to the
server, e.g. `[profiles.<name>.exec_direct_addresses]`):

```bash
aster enroll revoke --subject $AGENT --fleet-only --fleet-namespace $FLEET \
  --fleet-secret-file fleet.secret --audience mission-control --peer $SERVER
```

The agent's next call is refused and its connection closes; enrolling again is
denied, and the denial survives a server restart (`state_path`).

### Tests

`src/Aster.Tests/EnrollmentTests.cs` runs these scenarios through
`AsterServer` / `AsterClient` (the .NET version of the Rust
`aster/tests/enrollment_aster_server.rs`), with a test-only root
(`src/Aster.Tests/TestRoot.cs`) signing the credentials the CLI would.

### Differences from the Rust example

- No HTTP transport; in auth mode the Rust example serves Iroh only too.
- `runCommand` echoes the command; it does not run a shell.
- Cross-binding calls against the Rust server are not part of this example's tests.
