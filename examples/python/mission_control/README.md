# Mission Control Example

A control plane for managing remote agents, demonstrating all four Aster
RPC patterns, session-scoped services, and capability-based auth.

**Full walkthrough on the docs site:** [Mission Control quickstart](https://sdk.getaster.now/docs/quickstart/mission-control) — seven chapters covering all four RPC patterns, session-scoped services, capability-based auth, contract gen-client, and cross-language interop. The code in this directory is the runnable Python implementation that the walkthrough builds up to chapter by chapter.

## Quick start

```bash
# Terminal 1: start the server
cd examples/python
python -m mission_control

# Terminal 2: connect an agent
python -m mission_control.agent aster1...

# Terminal 3: tail logs
python -m mission_control.operator aster1...
```

Without enrollment configured the server is open: any node can connect.

## With auth (Chapter 5)

`python -m mission_control --auth` serves [`services_auth.py`](services_auth.py):
the same services with `requires=` on each method, using the roles in
[`roles.py`](roles.py) (identical to the Rust example's `roles.rs`):

| Method | Requires |
|--------|----------|
| `getStatus` | `ops.status` |
| `submitLog` | any enrolled node |
| `tailLogs` | `ops.logs` or `ops.admin` |
| `ingestMetrics` | `ops.ingest` |
| `runCommand` (`AgentSession`) | `ops.admin` |

Nothing in the code reads a credential. `AsterConfig.from_env()` (or
`--config aster.toml`) switches Enrollment on: the server admits only nodes
holding a credential from your root, and each peer's `aster.role` claim
becomes the role set `requires=` is checked against, evaluated on every call.
`--auth` refuses to start without enrollment configured, and any missing or
malformed enrollment setting stops the server with an error naming it.

**1. Produce the files with the CLI** (as the root operator, with the Rust
`aster` CLI from `aster-cli`):

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
every credential (the server then serves renewals with
`[enrollment.issuer] delegation_file`). `aster trust init` adds a root
rotation pin to the profile (`[trust] next_root_commitment`, `root_seq`).

**2. Start the server** with those files:

```bash
export ASTER_ROOT_PUBKEY=$ROOT
export ASTER_ENROLLMENT_AUDIENCE=mission-control
export ASTER_ENROLLMENT_FLEET_NAMESPACE=$FLEET

ASTER_SECRET_KEY_FILE=server.key \
ASTER_ENROLLMENT_CREDENTIAL=server.cred \
ASTER_ENROLLMENT_STATE_PATH=./mc-enrollment.json \
python -m mission_control --auth          # prints its aster1… address
```

or the same as an `aster.toml` (`python -m mission_control --auth --config aster.toml`):

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
python -m mission_control.agent aster1…
```

`getStatus` and `ingestMetrics` succeed. A credential without those roles
gets `PERMISSION_DENIED`, and a node with no credential cannot connect
(`AdmissionDeniedError`).

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

`tests/python/test_mission_control_auth.py` runs this chapter as a test: a
server configured from an `aster.toml`, and agent, operator, admin and
role-less clients, each reaching exactly the methods its roles allow.

## Files

| File | Purpose |
|------|---------|
| `types.py` | Wire types (request/response dataclasses) |
| `roles.py` | Capability roles (Chapter 5) |
| `services.py` | Services without auth (Chapters 1-4) |
| `services_auth.py` | Services with requires= (Chapter 5) |
| `server.py` | Producer entry point (`--auth`, `--config`) |
| `agent.py` | Example agent (unary + client streaming) |
| `operator.py` | Example operator (server streaming) |
