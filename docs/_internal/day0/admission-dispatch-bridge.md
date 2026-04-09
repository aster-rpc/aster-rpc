# Admission → Dispatch Bridge

**Status:** Design  
**Date:** 2026-04-09

## Problem

Admission and RPC dispatch are currently disconnected. A consumer is
admitted on one ALPN (with attributes like roles), then makes RPC calls
on a different ALPN. The RPC dispatch path has no way to know what roles
the peer was admitted with.

```
Consumer ──→ aster.consumer_admission ──→ admitted with {aster.role: "ops.status"}
Consumer ──→ aster/1 (RPC) ──→ calls getStatus() ──→ CallContext.attributes = {} ← EMPTY
```

The `CapabilityInterceptor` checks `CallContext.attributes["aster.role"]`
but that dict is always empty because nothing bridges the admission result
to the RPC path.

## Current architecture

```
                          ┌─────────────────────┐
                          │   MeshEndpointHook   │
                          │  (Gate 0 allowlist)  │
                          │                      │
                          │  peers: set[str]     │  ← endpoint_ids only
                          │  addPeer(id)         │     no attributes
                          │  removePeer(id)      │
                          └─────────────────────┘
                                    │
                     used by QUIC handshake hook
                          (allow/deny connect)
                                    │
    ┌──────────────────┐           │          ┌──────────────────┐
    │ consumer_admission│           │          │   aster/1 (RPC)  │
    │     ALPN          │           │          │      ALPN        │
    │                   │           │          │                  │
    │ → verify cred     │           │          │ → accept stream  │
    │ → hook.addPeer()  │           │          │ → read header    │
    │ → return services │           │          │ → dispatch       │
    │   + attributes    │           │          │ → build_ctx()    │
    │   (but nobody     │           │          │   attributes={}  │
    │    stores them)   │           │          │                  │
    └──────────────────┘           │          └──────────────────┘
```

The admission handler returns attributes in the response (the consumer
sees them), but they're never stored server-side. The RPC server can't
look them up.

## Design: PeerAttributeStore

Add a shared in-memory store that both admission handlers write to and
the RPC server reads from.

```
                     ┌──────────────────────┐
                     │  PeerAttributeStore  │
                     │                      │
                     │  store: dict[         │
                     │    endpoint_id →      │
                     │    PeerAdmission      │
                     │  ]                    │
                     └──────────────────────┘
                        ▲               ▲
                  write │               │ read
                        │               │
    ┌──────────────┐    │    ┌──────────────────┐
    │  admission   │────┘    │   Server (RPC)   │
    │  handlers    │         │                  │
    │              │         │  _build_ctx()    │
    │  consumer_   │         │    → store.get() │
    │  admission   │         │    → attributes  │
    │              │         │                  │
    │  aster.      │         └──────────────────┘
    │  admission   │
    └──────────────┘
```

### PeerAdmission record

```python
@dataclass
class PeerAdmission:
    endpoint_id: str
    handle: str                          # consumer handle (or "")
    attributes: dict[str, str]           # includes aster.role
    admitted_at: float                   # time.time()
    admission_path: str                  # "consumer_admission" | "aster.admission"
```

### PeerAttributeStore

```python
class PeerAttributeStore:
    """Thread-safe store mapping peer endpoint_id to admission attributes."""

    def __init__(self) -> None:
        self._peers: dict[str, PeerAdmission] = {}

    def admit(self, admission: PeerAdmission) -> None:
        """Record a successful admission."""
        self._peers[admission.endpoint_id] = admission

    def get(self, endpoint_id: str) -> PeerAdmission | None:
        """Look up admission attributes for a peer."""
        return self._peers.get(endpoint_id)

    def remove(self, endpoint_id: str) -> None:
        """Remove a peer (on disconnect or revocation)."""
        self._peers.pop(endpoint_id, None)

    def get_attributes(self, endpoint_id: str) -> dict[str, str]:
        """Convenience: get attributes dict, or empty if not admitted."""
        admission = self._peers.get(endpoint_id)
        return dict(admission.attributes) if admission else {}
```

### Where each admission path writes

**Consumer admission** (`handle_consumer_admission_rpc`):

```python
# After successful admission:
if peer_store is not None:
    peer_store.admit(PeerAdmission(
        endpoint_id=peer_node_id,
        handle=cred.attributes.get("aster.name", ""),
        attributes=result.attributes,  # includes aster.role
        admitted_at=time.time(),
        admission_path="consumer_admission",
    ))
```

**Delegated admission** (`aster.admission` handler):

```python
# After successful token verification + proof of possession:
if peer_store is not None:
    peer_store.admit(PeerAdmission(
        endpoint_id=peer_node_id,
        handle=token.consumer_handle,
        attributes={"aster.role": ",".join(token.roles)},
        admitted_at=time.time(),
        admission_path="aster.admission",
    ))
```

### Where the RPC server reads

In `Server._build_ctx` (or the new `build_call_context`):

```python
def _build_ctx(self, conn: IrohConnection, header: StreamHeader, method_info) -> CallContext:
    peer = conn.remote_id()

    # Look up admission attributes for this peer
    attributes = {}
    if self._peer_store is not None:
        attributes = self._peer_store.get_attributes(peer)

    return build_call_context(
        service=header.service,
        method=header.method,
        ...
        attributes=attributes,
    )
```

`build_call_context` gains an `attributes` parameter:

```python
def build_call_context(
    *,
    service: str,
    method: str,
    ...
    attributes: dict[str, str] | None = None,
) -> CallContext:
    return CallContext(
        ...
        attributes=dict(attributes or {}),
    )
```

### Wiring in AsterServer

`AsterServer` creates the store and passes it to both the admission
handlers and the RPC server:

```python
class AsterServer:
    def __init__(self, ...):
        self._peer_store = PeerAttributeStore()

    async def start(self):
        # Pass store to Server
        self._server = Server(
            ...,
            peer_store=self._peer_store,
        )

    # In the consumer admission accept loop:
    async def _handle_consumer_admission(self, conn, ...):
        response = await handle_consumer_admission_rpc(
            ...,
            peer_store=self._peer_store,  # NEW
        )

    # In the aster.admission accept loop:
    async def _handle_delegated_admission(self, conn, ...):
        # verify token → proof of possession → store attributes
        self._peer_store.admit(PeerAdmission(...))
```

### Open gate (allow_all_consumers)

When `allow_all_consumers=True`, there's no admission step — the
consumer connects directly on the RPC ALPN. In this case:

- `PeerAttributeStore` has no entry for the peer
- `get_attributes()` returns `{}`
- `CapabilityInterceptor` sees no roles
- Methods with `requires=...` will be denied (correct — open gate
  doesn't grant capabilities)
- Methods without `requires=...` pass through (correct — no auth
  needed)

This is the right behavior: open gate means "anyone can connect" but
NOT "anyone has all capabilities."

### Cleanup

When the `MeshEndpointHook` removes a peer (disconnect or revocation),
the `PeerAttributeStore` should also be cleaned up:

```python
# In MeshEndpointHook.removePeer():
self._peers.discard(endpoint_id)
if self._peer_store:
    self._peer_store.remove(endpoint_id)
```

### The complete admission → dispatch chain

```
Consumer connects on consumer_admission ALPN
  → handle_consumer_admission_rpc
  → verify credential (signature, expiry, nonce)
  → cred.attributes contains {"aster.role": "ops.status,ops.logs"}
  → peer_store.admit(PeerAdmission(attributes=cred.attributes))
  → hook.addPeer(endpoint_id)  # Gate 0 allowlist

Consumer connects on aster.admission ALPN
  → verify_admission_request (token + attestation)
  → create_admission_challenge → verify_admission_proof
  → token.roles = ["ops.status", "ops.logs"]
  → peer_store.admit(PeerAdmission(attributes={"aster.role": "ops.status,ops.logs"}))
  → hook.addPeer(endpoint_id)  # Gate 0 allowlist

Consumer connects on aster/1 (RPC) ALPN
  → Gate 0: hook allows (peer in allowlist)
  → Server accepts stream
  → Server reads StreamHeader (service, method)
  → Server calls _build_ctx
  → _build_ctx reads peer_store.get_attributes(endpoint_id)
  → CallContext.attributes = {"aster.role": "ops.status,ops.logs"}
  → CapabilityInterceptor checks method.requires against ctx.attributes
  → ROLE("ops.status") → "ops.status" in roles? → YES → allow
```

### Changes required

| Component | Change |
|-----------|--------|
| `interceptors/base.py` | `build_call_context` gains `attributes` param |
| `interceptors/base.py` | New `PeerAdmission` dataclass |
| `interceptors/base.py` or new `peer_store.py` | New `PeerAttributeStore` class |
| `server.py` | `Server.__init__` accepts `peer_store` |
| `server.py` | `Server._build_ctx` reads from `peer_store` |
| `trust/consumer.py` | `handle_consumer_admission_rpc` writes to `peer_store` |
| `high_level.py` | `AsterServer` creates `PeerAttributeStore`, wires it |
| NEW `trust/delegated.py` | `aster.admission` ALPN handler + token verification |
| `high_level.py` | `AsterServer.serve()` starts `aster.admission` accept loop |

### What does NOT change

- `CapabilityInterceptor` — already reads `ctx.attributes["aster.role"]`
- `evaluate_capability` — already works with the attributes dict
- Gate 0 (`MeshEndpointHook`) — still manages the allowlist
- Gate 2 (interceptor chain) — still runs per-call
- Self-issued credential path — works as before, now also writes to store
- `@service(requires=...)` / `@rpc(requires=...)` — unchanged
