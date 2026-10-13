# Mission Control (Java): the auth chapter

The Java port of [Python Mission Control](../../../examples/python/mission_control)
and the [Rust auth chapter](../../../examples/rust/mission-control): a shared
`MissionControl` service and a session-scoped `AgentSession` service covering
all four RPC patterns, with the same method names and role names as Python,
TypeScript, Rust and Go. Everything comes from configuration
(`AsterConfig.fromEnv()` or an `aster.toml`).

| Service / method | Pattern | Requires (auth chapter) |
|------------------|---------|-------------------------|
| `MissionControl.getStatus` | unary | `ops.status` |
| `MissionControl.submitLog` | unary | any enrolled node |
| `MissionControl.tailLogs` | server-stream | `ops.logs` or `ops.admin` |
| `MissionControl.ingestMetrics` | client-stream | `ops.ingest` |
| `AgentSession.register` | unary (session) | `ops.ingest` |
| `AgentSession.runCommand` | bidi-stream (session) | `ops.admin` |

Nothing in the code reads a credential. With enrollment configured the server
admits only nodes holding a credential from your root, and each peer's
`aster.role` claim becomes the role set every method's `requires` is checked
against, in Rust, on every call. The role names are in
[`Role.java`](src/main/java/site/aster/examples/missioncontrol/Role.java):
`ops.status`, `ops.logs`, `ops.admin`, `ops.ingest`.

## Run (open)

Build the native library (`cargo build -p aster_transport_ffi`) and, from
`bindings/java`:

```sh
mvn -q install -DskipTests
mvn -pl aster-examples-mission-control exec:java \
  -Dexec.mainClass=site.aster.examples.missioncontrol.Server        # prints its aster1… address
mvn -pl aster-examples-mission-control exec:java \
  -Dexec.mainClass=site.aster.examples.missioncontrol.Agent -Dexec.args=aster1…
```

Without enrollment configured the server is open: there is no "allow all"
switch.

## With auth

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

A credential is bound to one node key: run each node with the key file it was
issued for.

**2. Start the server** with those files:

```bash
export ASTER_ROOT_PUBKEY=$ROOT
export ASTER_ENROLLMENT_AUDIENCE=mission-control
export ASTER_ENROLLMENT_FLEET_NAMESPACE=$FLEET

ASTER_SECRET_KEY_FILE=server.key \
ASTER_ENROLLMENT_CREDENTIAL=server.cred \
ASTER_ENROLLMENT_STATE_PATH=./mc-enrollment.json \
mvn -pl aster-examples-mission-control exec:java \
  -Dexec.mainClass=site.aster.examples.missioncontrol.ServerAuth   # prints its aster1… address
```

or the same as an `aster.toml` (`-Dexec.args="--config aster.toml"`):

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

In code that is all there is:

```java
AsterServer server = AsterServer.builder()
    .config(AsterConfig.fromEnv())          // or AsterConfig.fromFile("aster.toml")
    .service(new MissionControl(), new MissionControlAuthDispatcher())
    .sessionService(AgentSession.class, AgentSession::new, new AgentSessionAuthDispatcher())
    .build().get();
```

**3. Run the agent** with its own key and credential:

```bash
ASTER_SECRET_KEY_FILE=agent.key ASTER_ENROLLMENT_CREDENTIAL=agent.cred \
mvn -pl aster-examples-mission-control exec:java \
  -Dexec.mainClass=site.aster.examples.missioncontrol.Agent -Dexec.args=aster1…
```

The client enrolls with the server before its first call (the server verifies
the agent's credential and the agent verifies the server's). `getStatus` and
`ingestMetrics` succeed. A credential without those roles gets
`PERMISSION_DENIED`, and a node with no credential cannot connect.

**4. Revoke** the agent as the root; the server holds the fleet namespace, so
the root's publication session delivers to it:

```bash
aster enroll revoke --subject $AGENT --fleet-only --fleet-namespace $FLEET \
  --fleet-secret-file fleet.secret --audience mission-control --peer $SERVER
```

The agent's next call is refused and its connection closes.

## Test

```sh
mvn -pl aster-examples-mission-control test -Dtest=MissionControlAuthE2ETest
```

[`MissionControlAuthE2ETest`](src/test/java/site/aster/examples/missioncontrol/MissionControlAuthE2ETest.java)
runs this chapter (the Java version of the Rust `tests/auth.rs`): a server
configured from an `aster.toml`, and operator, agent, admin and role-less
clients, each reaching exactly the methods its roles allow; a node with no
credential is refused. A test root (`TestRoot`) signs the credentials the CLI
would.

The cross-binding matrix (`tests/integration/mission_control/run_matrix.sh`
with `ASTER_MATRIX_INCLUDE_JAVA=1`) runs `ServerAuth` and the Kotlin guide
client (`aster-examples-mission-control-guide`) against the files
`setup_auth.sh` produces with the CLI.

## Differences from the Rust example

- No HTTP transport here; in auth mode the Rust example serves Iroh only too.
- `runCommand` lives on the session-scoped `AgentSession`, as in Python, and
  echoes the command; it does not run a shell.
