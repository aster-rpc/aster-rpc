# Producer Accept Loop

**Status:** Reference implementation complete (Python, TypeScript). Blueprint for Java, Go, etc.

**Reference:**
- Python: `bindings/python/aster/high_level.py` → `_accept_loop()` (line 735)
- TypeScript: `bindings/typescript/packages/aster/src/high-level.ts` → `_acceptLoop()`

---

## Overview

When an AsterServer calls `serve()`, it enters a single accept loop that
pulls connections from the iroh endpoint and dispatches them by ALPN tag
to the correct handler. This is the central connection routing mechanism
for any Aster producer.

```
┌──────────────────────────────────────────────────────────┐
│                   AsterServer.serve()                     │
│                                                          │
│  loop:                                                   │
│    conn = accept_aster()     ← blocks until connection   │
│    alpn = conn.alpn                                      │
│                                                          │
│    match alpn:                                           │
│      "aster/1"                    → RPC handler          │
│      "aster.consumer_admission"   → consumer admission   │
│      "aster.producer_admission"   → producer admission   │
│      "aster.admission"            → delegated admission  │
│      _                            → reject (unknown)     │
└──────────────────────────────────────────────────────────┘
```

---

## Why a Single Loop

### The problem with multiple loops

A naive implementation might create separate accept loops per ALPN:

```
loop_1: while true { conn = accept(); handle_admission(conn) }
loop_2: while true { conn = accept(); handle_rpc(conn) }
```

This **does not work** because all aster ALPNs share a single connection
queue in iroh's Router. If `loop_2` pulls an admission connection, it
tries to parse binary RPC framing from a JSON admission stream — causing
protocol mismatch, empty responses, or crashes.

### The correct pattern

One loop, one queue, ALPN-based dispatch:

```
loop: while true {
    conn = accept()           ← pulls from shared queue
    alpn = conn.alpn          ← read the ALPN tag
    dispatch(alpn, conn)      ← route to correct handler
}
```

Each handler runs as a concurrent task (fire-and-forget with error
logging). The accept loop never blocks on handler completion.

---

## ALPN Registry

The producer registers these ALPNs on the iroh endpoint at startup:

| ALPN | Purpose | Handler | When registered |
|------|---------|---------|----------------|
| `aster/1` | RPC calls (unary, streaming, bidi) | `RpcServer.handleConnection()` | Always |
| `aster.consumer_admission` | Consumer credential presentation + service discovery | `handleConsumerAdmissionConnection()` | Always |
| `aster.producer_admission` | Producer-to-producer mesh admission (mutual auth between producers) | `handleProducerAdmissionConnection()` | When `allow_all_producers=False` |
| `aster.admission` | Delegated admission (aster-issued enrollment tokens) | `handleDelegatedAdmissionConnection()` | When delegation policies are configured |

**Consumer vs Producer admission:** Consumer admission is how clients
(consumers) connect to a service. Producer admission is how sibling
producers in a mesh authenticate to each other — they exchange mesh
state, verify each other's root keys, and establish trust for registry
replication and gossip forwarding.

Non-aster ALPNs (blobs, docs, gossip) are handled by iroh's Router
directly and never appear in the aster accept queue.

---

## Step-by-Step Flow

### Step 1: Register ALPNs at startup

```
alpns = [
    "aster/1",                       // RPC
    "aster.consumer_admission",      // consumer admission
    "aster.admission",               // delegated admission (optional)
]
node = IrohNode.memoryWithAlpns(alpns)
```

All three ALPNs are funnelled into a single bounded channel by the
Router's `AsterQueueHandler`. The handler stores the ALPN tag alongside
each connection.

### Step 2: Accept loop

```
while running:
    conn = node.accept_aster()       // blocks until next connection
    alpn = conn.alpn                 // ALPN tag set by the Router
```

**Implementation notes:**
- `accept_aster()` pulls `(alpn, connection)` from a bounded mpsc channel
- The ALPN is stored on the connection object for the caller to read
- If the channel is closed (node shutting down), accept returns an error

### Step 3: Dispatch by ALPN

```
if alpn == "aster/1":
    // RPC connection — hand to RpcServer
    spawn(rpc_server.handle_connection(conn))

elif alpn == "aster.consumer_admission":
    // Consumer wants to present credentials and discover services
    spawn(handle_consumer_admission(conn, ...))

elif alpn == "aster.producer_admission":
    // Sibling producer wants to join the mesh
    spawn(handle_producer_admission(conn, ...))

elif alpn == "aster.admission":
    // Delegated admission (aster-issued token)
    spawn(handle_delegated_admission(conn, ...))

else:
    // Unknown ALPN — reject
    conn.close(400, "unknown ALPN")
```

Each handler is spawned as an independent concurrent task. The accept
loop returns immediately to pull the next connection.

### Step 4: Error handling

```
try:
    conn = accept_aster()
catch Error:
    if shutting_down: return      // clean exit
    log.error("accept failed")
    continue                       // keep accepting
```

Accept errors are non-fatal — the loop continues. Only a shutdown
signal (close/cancel) terminates the loop.

---

## Connection Lifecycle per ALPN

### RPC (`aster/1`)

```
conn accepted
  └─ RpcServer.handleConnection(conn)
       └─ loop:
            stream = conn.acceptBi()
            read StreamHeader → extract service, method, metadata
            dispatch to service handler
            write response + trailer
            (stream closes)
       └─ (connection stays open for multiplexed streams)
```

Multiple RPC calls can be multiplexed on a single connection via
separate QUIC streams. The connection persists until the client
disconnects.

### Consumer Admission (`aster.consumer_admission`)

```
conn accepted
  └─ handleConsumerAdmissionConnection(conn, ...)
       └─ bi = conn.acceptBi()
       └─ read request JSON (credential + iid_token)
       └─ verify credential (signature, expiry, root key)
       └─ if valid: hook.addPeer(peer_id)        ← admits to Gate 0
       └─ write response JSON (services, registry_namespace, ...)
       └─ send.finish()
       └─ (connection drains naturally — do NOT close)
```

The admission connection is short-lived: one bidi stream, one
request-response, then the QUIC streams drain. The connection object
is not explicitly closed — calling `conn.close()` would send
`CONNECTION_CLOSE` which can kill in-flight data before the client
reads the response.

### Producer Admission (`aster.producer_admission`)

```
conn accepted
  └─ handleProducerAdmissionConnection(conn, ...)
       └─ bi = conn.acceptBi()
       └─ exchange mesh state (own root pubkey, clock drift config)
       └─ verify peer's root pubkey matches expected mesh
       └─ if valid: hook.addPeer(peer_id)
       └─ persist updated mesh state
       └─ (connection drains naturally)
```

Producer admission is mutual: both sides verify each other. This is
used in multi-producer deployments where several instances of the same
service form a mesh for registry replication and gossip. Only registered
when `allow_all_producers=False` (production mode).

### Delegated Admission (`aster.admission`)

```
conn accepted
  └─ handleDelegatedAdmissionConnection(conn, ...)
       └─ bi = conn.acceptBi()
       └─ read attestation + proof-of-possession
       └─ verify token against delegation policy
       └─ if valid: hook.addPeer(peer_id)
       └─ write admission response
       └─ send.finish()
```

Same lifecycle as consumer admission but with a different credential
verification path (aster-issued enrollment tokens vs pre-signed
credentials).

---

## Diagram: Connection Routing

```
                         ┌─────────────┐
    Client A ──QUIC──►   │             │
    (admission ALPN)     │   iroh      │
                         │   Router    │
    Client B ──QUIC──►   │             │    ┌─────────────────┐
    (rpc ALPN)           │  AsterQueue │──► │ mpsc channel    │
                         │  Handler    │    │ (alpn, conn)    │
    Client C ──QUIC──►   │             │    └────────┬────────┘
    (admission ALPN)     │             │             │
                         └─────────────┘             │
                                              ┌──────▼──────┐
                                              │ accept_aster│
                                              │ (single     │
                                              │  consumer)  │
                                              └──────┬──────┘
                                                     │
                                            ┌────────▼────────┐
                                            │  ALPN dispatch  │
                                            └──┬─────┬─────┬──┘
                                               │     │     │
                              ┌────────────────┘     │     └────────────────┐
                              │                      │                      │
                       ┌──────▼──────┐       ┌───────▼───────┐     ┌───────▼───────┐
                       │  Admission  │       │  RPC Server   │     │  Delegated    │
                       │  Handler    │       │  (streams)    │     │  Admission    │
                       └─────────────┘       └───────────────┘     └───────────────┘
```

---

## Gate 0 Interaction

The admission handler and the RPC handler interact through Gate 0
(the `MeshEndpointHook`):

```
1. Client connects on admission ALPN
2. Accept loop dispatches to admission handler
3. Admission handler verifies credential
4. On success: hook.addPeer(client_endpoint_id)   ← updates Gate 0
5. Admission handler sends response (services, registry namespace)

6. Client connects on RPC ALPN
7. Gate 0 fires before_connect hook
8. Hook checks: is client_endpoint_id in admitted set? → YES
9. Connection proceeds
10. Accept loop dispatches to RPC server
```

If the client connects on RPC before admission completes, Gate 0
rejects the connection (peer not in admitted set). The client must
complete admission first, then open RPC connections.

**Timing note:** Gate 0 runs in a separate task (`_run_gate0` /
`run_hook_loop`). The `addPeer()` call from the admission handler
updates the admitted set immediately (in-memory Set), so there is
no propagation delay — the next Gate 0 check will see the peer.

---

## Implementation Checklist (per language)

### Native binding requirements

- [ ] `node.accept_aster() → Connection` with ALPN tag accessible on the connection
- [ ] Connection ALPN readable via `conn.alpn()` or equivalent
- [ ] All aster ALPNs routed through a single accept channel

### Accept loop

- [ ] Single loop calling `accept_aster()` in a while-running loop
- [ ] Read ALPN from accepted connection
- [ ] Route to correct handler by ALPN match
- [ ] Spawn handler as concurrent task (do not block accept loop)
- [ ] Log and continue on accept errors (non-fatal)
- [ ] Clean exit on shutdown signal

### Handlers (concurrent tasks)

- [ ] RPC: delegate to `RpcServer.handleConnection(conn)`
- [ ] Consumer admission: delegate to `handleConsumerAdmissionConnection(conn, opts)`
- [ ] Producer admission: delegate to `handleProducerAdmissionConnection(conn, opts)` (when `allow_all_producers=False`)
- [ ] Delegated admission: delegate to `handleDelegatedAdmissionConnection(conn, opts)`
- [ ] Unknown ALPN: close connection with error code

### Error handling

- [ ] Handler errors logged but do not crash the accept loop
- [ ] Accept errors logged and loop continues
- [ ] Shutdown flag checked after each accept to enable clean exit

---

## Anti-patterns

### DO NOT: Multiple accept loops

```
// WRONG — causes ALPN cross-contamination
admissionLoop: while true { conn = accept(); handleAdmission(conn) }
rpcLoop:       while true { conn = accept(); handleRpc(conn) }
```

Both loops pull from the same queue. An RPC connection can end up in
the admission handler, causing protocol mismatch.

### DO NOT: Close admission connections explicitly

```
// WRONG — kills in-flight response data
await send.writeAll(response);
await send.finish();
conn.close(0, "done");    // ← CONNECTION_CLOSE races with stream data
```

Let QUIC drain the streams naturally. The connection will be cleaned
up by the transport layer after both sides finish.

### DO NOT: Block the accept loop on handler completion

```
// WRONG — serializes all connections
while true {
    conn = accept();
    await handleAdmission(conn);    // ← blocks until this finishes
}
```

Handlers must be spawned as concurrent tasks so the accept loop can
immediately pull the next connection.

---

## Notes for implementers

1. **The ALPN tag is set by the Router, not the client.** The Router
   matches the client's TLS ALPN negotiation to a registered handler
   and tags the connection before enqueuing it.

2. **Bounded channel provides natural backpressure.** If handlers are
   slow, the channel fills up and the Router's per-ALPN handler task
   blocks — but other ALPNs (blobs, docs) remain unaffected.

3. **Gate 0 runs in a separate task.** The hook loop polls for
   before_connect events and responds with allow/deny. It does NOT
   run in the accept loop — it's a parallel task that the iroh
   endpoint invokes during QUIC handshake.

4. **The RPC server's `serve(node)` method has its own accept loop.**
   When using AsterServer, do NOT call `rpcServer.serve(node)` — use
   `rpcServer.handleConnection(conn)` instead. The AsterServer's
   accept loop replaces the RPC server's built-in loop.
