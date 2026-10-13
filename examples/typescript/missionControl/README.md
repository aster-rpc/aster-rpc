# Mission Control Example (TypeScript)

A control plane for managing remote agents, demonstrating all four Aster
RPC patterns, session-scoped services, and capability-based auth.

**Full walkthrough on the docs site:** [Mission Control quickstart](https://sdk.getaster.now/docs/quickstart/mission-control) (switch to the TypeScript tab) — seven chapters covering all four RPC patterns, session-scoped services, capability-based auth, contract gen-client, and cross-language interop. The code in this directory is the runnable TypeScript implementation that the walkthrough builds up to chapter by chapter.

## Quick start

```bash
# Install dependencies
npm install

# Generate type metadata (required before first run)
npx aster-gen

# Start the server
bun run server.ts

# In another terminal: connect an agent
bun run agent.ts aster1...

# In another terminal: tail logs
bun run operator.ts aster1...
```

The `npx aster-gen` step reads your `tsconfig.json`, discovers all
`@Service` / `@WireType` decorated classes, and emits `aster-rpc.generated.ts`
with wire type metadata, field shapes, and contract identity hashes.
Re-run it after adding or changing wire types or RPC methods.

See the [TypeScript Build Setup](https://sdk.getaster.now/docs/guides/typescript-build-setup)
guide for Vite/Webpack plugin integration and other options.

## Cross-language (Chapter 7)

The wire types use the same `mission/` tags as the Python example.
Start the TypeScript server, then connect with a Python client:

```bash
# TS server
npx aster-gen && bun run server.ts

# Python client
aster call aster1... MissionControl.getStatus '{"agent_id": "py-worker"}'
```

## With auth (Chapter 5)

`bun run server.ts --auth` serves [`services-auth.ts`](services-auth.ts):
the same services with `requires` on each method, using the roles in
[`roles.ts`](roles.ts) (identical to Python's `roles.py` and Rust's
`roles.rs`):

| Method | Requires |
|--------|----------|
| `getStatus` | `ops.status` |
| `submitLog` | any enrolled node |
| `tailLogs` | `ops.logs` or `ops.admin` |
| `ingestMetrics` | `ops.ingest` |
| `runCommand` | `ops.admin` |

Nothing in the code reads a credential. The configuration
(`AsterConfig.fromEnv()`, or an `aster.toml` with `--config`) switches
enrollment on: the server admits only nodes holding a credential from your
root, and each peer's `aster.role` claim becomes the role set `requires` is
checked against, on every call. Auth mode serves Iroh only (an HTTP caller
has no credential).

**1. Produce the files with the CLI** (the Rust `aster` CLI, as the root
operator):

```bash
aster key root generate
ROOT=$(aster key root show --json | jq -r .root_pubkey)
FLEET=$(aster enroll namespace create --out-secret fleet.secret --json | jq -r .namespace)
SERVER=$(aster key node generate --out server.key --json | jq -r .node_id)
AGENT=$(aster key node generate --out agent.key --json | jq -r .node_id)
OPERATOR=$(aster key node generate --out operator.key --json | jq -r .node_id)

aster enroll node --subject $SERVER --audience mission-control --out server.cred
echo '{"aster.role":{"type":"string","value":"ops.status,ops.ingest"}}' > agent-roles.json
aster enroll node --subject $AGENT --audience mission-control \
  --attributes agent-roles.json --out agent.cred
echo '{"aster.role":{"type":"string","value":"ops.status,ops.logs"}}' > operator-roles.json
aster enroll node --subject $OPERATOR --audience mission-control \
  --attributes operator-roles.json --out operator.cred
```

Add `--persistent` for long-lived nodes the root later promotes; see
`aster enroll delegate` to run an online issuer (`enrollment.issuerDelegationFile`,
`ASTER_ENROLLMENT_ISSUER_DELEGATION_FILE`) instead of the root signing every
credential. Clients whose credential the server signed renew it from the
server automatically.

**2. Start the server** with those files:

```bash
export ASTER_ROOT_PUBKEY=$ROOT
export ASTER_ENROLLMENT_AUDIENCE=mission-control
export ASTER_ENROLLMENT_FLEET_NAMESPACE=$FLEET

ASTER_SECRET_KEY_FILE=server.key \
ASTER_ENROLLMENT_CREDENTIAL=server.cred \
ASTER_ENROLLMENT_STATE_PATH=./mc-enrollment.json \
bun run server.ts --auth          # prints its aster1… address
```

or the same as an `aster.toml` (`bun run server.ts --auth --config aster.toml`):

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

The same settings in code are `AsterConfig` fields:
`{ rootPubkey, enrollmentCredentialFile, secretKeyFile, enrollment: { audience, fleetNamespace, statePath } }`.
A missing or malformed value stops the server with an error naming it; it
never starts open.

**3. Run the agent and the operator** with their own keys and credentials
(the same `ASTER_ROOT_PUBKEY` / `ASTER_ENROLLMENT_*` exports):

```bash
ASTER_SECRET_KEY_FILE=agent.key ASTER_ENROLLMENT_CREDENTIAL=agent.cred \
bun run agent.ts aster1…

ASTER_SECRET_KEY_FILE=operator.key ASTER_ENROLLMENT_CREDENTIAL=operator.cred \
bun run operator.ts aster1…
```

The agent's `getStatus`, `submitLog` and `ingestMetrics` succeed and the
operator tails logs. A credential without the required role gets
`PERMISSION_DENIED`, and a node with no credential cannot connect.

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

`tests/typescript/integration/enrollment-aster-server.test.ts` runs these
behaviours as tests (roles reaching `requires`, a missing role refused, no
credential refused, revocation, issuer renewal, misconfiguration).

> Upgrading from the consumer-admission auth chapter (`aster trust keygen`,
> `aster enroll node --role consumer`, `.aster-identity` credentials,
> `allowAllConsumers`)? See the Breaking change in
> [`bindings/typescript/packages/aster/CHANGELOG.md`](../../../bindings/typescript/packages/aster/CHANGELOG.md).

## Files

| File | Purpose |
|------|---------|
| `types.ts` | Wire types (request/response classes) |
| `roles.ts` | Capability roles (Chapter 5) |
| `services.ts` | Services without auth (Chapters 1-4) |
| `services-auth.ts` | Services with requires= (Chapter 5) |
| `server.ts` | Producer entry point |
| `agent.ts` | Example agent (unary + client streaming) |
| `operator.ts` | Example operator (server streaming) |
| `aster-rpc.generated.ts` | Auto-generated metadata (run `npx aster-gen`) |
| `tsconfig.json` | TypeScript config (used by aster-gen scanner) |
