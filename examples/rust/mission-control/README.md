# Mission Control (Rust)

The Rust port of the [Python](../../python/mission_control) /
[TypeScript](../../typescript/missionControl) Mission Control example,
demonstrating all four Aster RPC patterns served over **both** the Iroh
transport and the **HTTP** transport (Salvo) from one shared dispatcher.

One shared `MissionControl` service:

| Method | Pattern |
|--------|---------|
| `get_status` | unary |
| `submit_log` | unary |
| `tail_logs` | server-stream |
| `ingest_metrics` | client-stream |
| `run_command` | bidi-stream |

## Run

```bash
cargo run -p mission-control
```

Serves Iroh RPC (`aster/1`) plus **HTTPS (H1/H2/H3) + WebTransport** on
`127.0.0.1:8443` (dev self-signed cert). Canonical Aster RPC, browser
JSON, and WebTransport all live under `/aster/...`.

## Test (HTTP, typed, all four patterns)

```bash
cargo test -p mission-control
```

`tests/http.rs` drives the service over HTTP with Fory-encoded payloads
(the canonical `application/aster-frames` path), asserting each pattern
round-trips.

## Browser JSON (no Fory, no framing)

The service opts into a generated JSON gateway with
`#[aster::service(..., codecs = ["json"])]`, which emits
`MissionControlProjection`. Register it and serve via `router_with`, and a
browser can POST plain JSON:

```bash
# unary → JSON
curl --insecure -X POST https://127.0.0.1:8443/aster/MissionControl/getStatus \
  -H 'content-type: application/json' -H 'accept: application/json' \
  -d '{"agent_id":"agent-1"}'

# server-stream → NDJSON
curl --insecure -X POST https://127.0.0.1:8443/aster/MissionControl/tailLogs \
  -H 'content-type: application/json' -H 'accept: application/x-ndjson' \
  -d '{"agent_id":"agent-1","level":"info"}'
```

`tests/json.rs` covers unary JSON, server-stream NDJSON, and bidi → `406`
(bidi isn't projected over plain HTTP JSON). The dispatcher still runs Fory
throughout; JSON is transcoded at the edge.

## With auth (Chapter 5)

`cargo run -p mission-control -- --auth` serves [`src/auth.rs`](src/auth.rs):
the same service with `requires` on each method, using the roles in
[`src/roles.rs`](src/roles.rs) (identical to Python's `roles.py`):

| Method | Requires |
|--------|----------|
| `getStatus` | `ops.status` |
| `submitLog` | any enrolled node |
| `tailLogs` | `ops.logs` or `ops.admin` |
| `ingestMetrics` | `ops.ingest` |
| `runCommand` | `ops.admin` |

Nothing in the code reads a credential. `AsterConfig::from_env()` switches
enrollment on: the server admits only nodes holding a credential from your
root, and each peer's `aster.role` claim becomes the role set `requires` is
checked against. Auth mode serves Iroh only (an HTTP caller has no
credential).

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

Add `--persistent` for long-lived nodes the root later promotes; see
`aster enroll delegate` to run an online issuer instead of the root signing
every credential.

**2. Start the server** with those files:

```bash
export ASTER_ROOT_PUBKEY=$ROOT
export ASTER_ENROLLMENT_AUDIENCE=mission-control
export ASTER_ENROLLMENT_FLEET_NAMESPACE=$FLEET

ASTER_SECRET_KEY_FILE=server.key \
ASTER_ENROLLMENT_CREDENTIAL=server.cred \
ASTER_ENROLLMENT_STATE_PATH=./mc-enrollment.json \
cargo run -p mission-control -- --auth          # prints its aster1… address
```

or the same as an `aster.toml` (`AsterConfig::from_file`):

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
cargo run -p mission-control --bin agent -- aster1…
```

`getStatus` and `ingestMetrics` succeed. A credential without those roles
gets `PERMISSION_DENIED`, and a node with no credential cannot connect.

**4. Revoke** the agent as the root. The server holds the fleet namespace, so
the root's publication session delivers to it (the CLI profile needs its own
enrolled publisher identity, `exec_credential_file`, and a route to the
server):

```bash
aster enroll revoke --subject $AGENT --fleet-only --fleet-namespace $FLEET \
  --fleet-secret-file fleet.secret --audience mission-control --peer $SERVER
```

`--peer` takes the server's node id, `<node id>@<ip:port>`, or the `aster1…`
address the server prints. A node id alone is found through the relay; a
server with the relay disabled (`ASTER_RELAY_MODE=disabled`) is reachable
only at its direct address, so pass its `aster1…` address (or set
`exec_direct_addresses` for it in the CLI profile).

The agent's next call is refused and its connection closes.

`tests/auth.rs` runs this chapter as a test: a server configured from an
`aster.toml`, and operator, agent, admin and role-less clients, each reaching
exactly the methods its roles allow.

## Differences from the Python/TS example

- Those split per-agent state into a **session-scoped** `AgentSession`
  service; the Rust crate doesn't support session scope yet, so the bidi
  `run_command` lives on the shared service here.
- `run_command` echoes the command; it does **not** run a shell (the
  Python example does).
- There are two binaries, the server and `agent`; no separate operator.

Wire method names match the Python/TS peers (`getStatus`, `tailLogs`,
`ingestMetrics`, `runCommand`) via `#[rpc(name = "...")]`, so the service
is cross-binding-identical.
- HTTP is plain H1/H2 on TCP (no TLS yet).
