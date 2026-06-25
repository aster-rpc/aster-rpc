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
