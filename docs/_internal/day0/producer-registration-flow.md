# Producer Endpoint Registration

**Status:** Implemented  
**Date:** 2026-04-09

## Problem

When a consumer browses `@aster` and finds `@emrul/TaskManager`, they
need to know where to connect. The service manifest tells them the API
shape, but not which node is hosting it right now. Without endpoint
registration, `resolve(@emrul/TaskManager)` returns `endpoints=[]`.

## Flow

```
                  Operator (offline)
                       │
        aster publish TaskManager
                       │
                       ▼
    ┌──────────────────────────────────────┐
    │  CLI connects to @aster              │
    │  (signed with operator root key)     │
    │                                      │
    │  → publish(manifest, contract_id)    │
    │  ← ProducerServiceToken              │
    │    {root_pubkey, service, contract,  │
    │     issued_at, sig(@aster)}          │
    │                                      │
    │  CLI stores token in .aster-identity │
    │  [published_services.TaskManager]    │
    └──────────────────────────────────────┘

                  Producer startup
                       │
        AsterServer loads .aster-identity
        finds published_services with tokens
                       │
                       ▼
    ┌──────────────────────────────────────┐
    │  AsterServer background task         │
    │                                      │
    │  → connects to @aster                │
    │  → register_endpoint(                │
    │      producer_token,                 │
    │      node_id, relay, addrs, ttl)     │
    │  ← @aster verifies token signature  │
    │  ← stores endpoint record            │
    │                                      │
    │  re-registers every 75% of TTL       │
    └──────────────────────────────────────┘

                  Consumer connects
                       │
        aster shell @emrul/TaskManager
                       │
                       ▼
    ┌──────────────────────────────────────┐
    │  Shell calls @aster.resolve(         │
    │    handle="emrul",                   │
    │    service="TaskManager")            │
    │  ← endpoints: [{node_id, relay}]     │
    │                                      │
    │  Shell connects to producer node     │
    │  → consumer admission                │
    │  → invoke RPCs                       │
    └──────────────────────────────────────┘
```

## ProducerServiceToken

The token is the authorization for endpoint registration. It is:

- **Issued by `@aster`** (signed with `@aster`'s signing key)
- **Scoped to one service** (service_name + contract_id)
- **Bound to an operator** (root_pubkey), NOT to a specific node

```
ProducerServiceToken {
    root_pubkey: str        # operator identity (ed25519 hex)
    service_name: str       # "TaskManager"
    contract_id: str        # BLAKE3 hash of the service contract
    issued_at: int          # epoch ms
    signature: str          # @aster's signing key signs the above
}
```

## Security model

### What it protects against

**Unauthorized endpoint registration.** Without a valid token, a
malicious node cannot register endpoints for `@emrul/TaskManager`.
Consumers who resolve that service will only get endpoints from nodes
that the operator explicitly published through.

**Token theft across operators.** A token for `@emrul/TaskManager`
cannot be used to register endpoints for `@alice/OtherService` — the
token is scoped to the operator's root_pubkey and the specific service.

**Contract spoofing.** The token contains the contract_id. If a
malicious node registers with a mismatched contract_id, `@aster`
rejects the registration. Consumers are guaranteed to reach a node
serving the exact contract they expect.

### What it deliberately allows

**Multi-node scaling.** The token is NOT bound to a node_id. The
operator can start 10 nodes all serving `TaskManager`, and each node
registers its own endpoint using the same token. This is how horizontal
scaling works — the operator controls which nodes serve the service,
not `@aster`.

**Node replacement.** If a node goes down and the operator starts a new
one, the new node uses the same token. No re-publish needed.

### What it does NOT protect against

**Token leakage.** If the token file (`.aster-identity`) is
compromised, an attacker can register fake endpoints. Mitigation:

- Tokens are stored in the operator's identity file (file permissions)
- The operator can revoke by calling `unpublish` (removes the token
  from `@aster`'s records, invalidating it)
- Future: token rotation via `aster publish --rotate`

**Malicious @aster operator.** `@aster` signs the tokens. A compromised
`@aster` could issue tokens for any operator. This is inherent in the
delegated trust model — `@aster` is a trusted third party for the
directory service. Mitigation: operators can self-host `@aster`.

## Storage

### Operator side (.aster-identity)

```toml
[published_services.TaskManager]
producer_token = "eyJ..."     # base64 or JSON-encoded token
contract_id = "abc123..."
```

The `AsterServer` reads this on startup from the `published_services`
section of the identity file. Each entry maps a service name to its
token and contract_id.

### @aster side (SQLite)

```sql
-- Extends the existing service_endpoints table
INSERT INTO service_endpoints (
    handle, service_name, node_id, relay, ttl, registered_at
) VALUES (
    'emrul', 'TaskManager', '<node_hex>', 'relay.url', 300, <now>
);
```

Endpoints are TTL-gated. The producer re-registers before expiry.
Stale endpoints are filtered out by `resolve()` queries.

## TTL and heartbeat

- Default TTL: 300 seconds (5 minutes)
- Re-registration interval: 225 seconds (75% of TTL)
- If a producer stops, its endpoints expire naturally
- `resolve()` filters `WHERE registered_at + ttl >= now()`

## AsterServer integration

The registration runs as a background task alongside the accept loop:

```python
async with AsterServer(services=[TaskManager()]) as srv:
    await srv.serve()
    # ↑ this also starts _aster_registration_loop() if tokens exist
```

The task:
1. Loads tokens from `.aster-identity`
2. Resolves `@aster` address (env var or identity file)
3. Connects as a consumer
4. Calls `register_endpoint` for each published service
5. Sleeps 75% of TTL
6. Repeats until shutdown

No developer action needed — if tokens exist, registration is automatic.

## CLI commands

| Command | What it does |
|---------|-------------|
| `aster publish TaskManager` | Connects to @aster, uploads manifest, receives token, stores in .aster-identity |
| `aster unpublish TaskManager` | Connects to @aster, calls unpublish, removes token from .aster-identity |
| `aster discover <query>` | Searches @aster directory (public, no auth) |

## Relationship to consumer enrollment

The producer registration model mirrors consumer enrollment:

| | Consumer enrollment | Producer registration |
|---|---|---|
| **Who issues** | Operator (root key) | @aster (signing key) |
| **Who presents** | Consumer on connect | Producer on register |
| **Bound to** | Consumer's root_pubkey | Operator's root_pubkey |
| **Scoped to** | Service + role | Service + contract_id |
| **Stored in** | .cred file | .aster-identity |

Both are delegation tokens. Consumer enrollment delegates "this consumer
can access this service." Producer registration delegates "this operator
can register endpoints for this service."
