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

Serves Iroh RPC (`aster/1`) and HTTP on `127.0.0.1:8080`. Over HTTP, POST
Aster-framed bodies to `/aster/MissionControl/<method>`.

## Test (HTTP, typed, all four patterns)

```bash
cargo test -p mission-control
```

`tests/http.rs` drives the service over HTTP with Fory-encoded payloads
(what an HTTP client binding does), asserting each pattern round-trips.

## Differences from the Python/TS example

- Those split per-agent state into a **session-scoped** `AgentSession`
  service; the Rust crate doesn't support session scope yet, so the bidi
  `run_command` lives on the shared service here.
- `run_command` echoes the command; it does **not** run a shell (the
  Python example does).

Wire method names match the Python/TS peers (`getStatus`, `tailLogs`,
`ingestMetrics`, `runCommand`) via `#[rpc(name = "...")]`, so the service
is cross-binding-identical.
- HTTP is plain H1/H2 on TCP (no TLS yet).
