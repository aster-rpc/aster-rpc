# Capability Interception

How role-based access control flows from admission through to per-method
dispatch. The full chain: credential attributes → PeerAttributeStore →
CallContext.attributes → CapabilityInterceptor → allow/deny.

**Spec:** Aster-trust-spec.md §4 (Gate 3)

## The attribute flow

```
1. Admission                    2. PeerAttributeStore          3. RPC Dispatch
   ┌─────────────┐                ┌──────────────┐              ┌─────────────────┐
   │ credential   │──admit()──▶   │ endpointId → │──getAttrs()▶ │ CallContext      │
   │ attributes:  │               │ { aster.role: │              │ .attributes:    │
   │   aster.role │               │   "ops,admin"}│              │   { aster.role: │
   │   = "ops,    │               └──────────────┘              │     "ops,admin" }│
   │     admin"   │                                              └────────┬────────┘
   └─────────────┘                                                        │
                                                                          ▼
                                                               CapabilityInterceptor
                                                               .on_request(ctx, null)
                                                                          │
                                                                  requires: "admin"
                                                                  caller has: {ops, admin}
                                                                  → ALLOW
```

### Step 1: Admission records attributes

On successful consumer admission, the server stores the credential's
`attributes` dict in `PeerAttributeStore`:

```python
peer_store.admit(peer_endpoint_id, cred.attributes)
```

**Python:** `PeerAttributeStore` at `peer_store.py`.
**TypeScript:** `PeerAttributeStore` at `peer-store.ts`.

### Step 2: RPC dispatch reads attributes

When an RPC arrives, the server looks up the peer's attributes and
injects them into `CallContext`:

```python
attributes = peer_store.get_attributes(peer_endpoint_id)
call_ctx = build_call_context(..., attributes=attributes)
```

### Step 3: Interceptor evaluates

`CapabilityInterceptor.on_request(ctx, null)` runs **before** reading the
request payload. It reads `ctx.attributes['aster.role']`, splits on comma,
and checks against the method's `requires` declaration.

## Where auth fires

Auth interceptors run **BEFORE** pattern dispatch, not inside the handler.
The call is:

```python
await apply_request_interceptors(interceptors, call_ctx, None)
```

The request is `None` at this point — only the CallContext (with metadata
and attributes) is available. This guarantees auth checks fire on every
pattern, including bidi/client streams that might never produce a request
frame.

### Session-scoped services

For session-scoped services, auth fires **per CALL**, not per session.
An auth denial writes an error trailer but continues the session loop
(doesn't kill the session). The client can make other calls that pass auth.

The `SessionServer` must receive the `PeerAttributeStore` so it can look
up attributes for each CALL. Without this, `ctx.attributes` is empty and
all role checks fail (this was a bug found by the QA agent).

### Attribute expiry

Attributes have a TTL. The `PeerAttributeStore` lazily evicts entries
whose credential `expires_at` has passed or whose server-side TTL
(`ASTER_PEER_TTL_S`, default 24h) has elapsed. If a peer's attributes
expire between calls, the next call will see empty attributes and all
role checks will fail. The peer must re-admit to get fresh attributes.

```python
# session.py _session_loop, after decoding CallHeader:
try:
    await apply_request_interceptors(self._interceptors, call_ctx, None)
except RpcError as auth_err:
    await _write_trailer(send, codec, auth_err.code, auth_err.message)
    continue  # session stays open
```

## Requires declaration

The `requires` parameter on `@service` and `@rpc` accepts two forms:

### Bare string (shorthand)

```python
@rpc(requires="admin")
```

Equivalent to `{ kind: 'role', roles: ['admin'] }`. The interceptor
normalises this via `_normalize_requirement()` / `normaliseRequirement()`.

### Structured requirement

```python
@rpc(requires=anyOf("admin", "ops.ingest"))
```

Produces `{ kind: 'any_of', roles: ['admin', 'ops.ingest'] }`.

Three kinds are supported:

| Kind | Semantics |
|------|-----------|
| `role` | Caller must have this exact role |
| `any_of` | Caller must have at least one of the listed roles |
| `all_of` | Caller must have all of the listed roles |

### Evaluation

Service-level and method-level requirements are **both** checked
(conjunction). A call must satisfy the service requirement AND the method
requirement.

## Comma-separated roles

`aster.role` is a single string with comma-separated values:

```
"ops.status,ops.ingest"
```

The interceptor splits on comma and trims whitespace to build the caller's
role set:

```python
roles = {r.strip() for r in ctx.attributes.get("aster.role", "").split(",")}
```

Without this split, `callerRoles.has('ops.status')` would fail on the
full string.

## Open-gate mode (dev mode)

When `allow_all_consumers=True`, peers are auto-admitted with **empty
attributes**. CapabilityInterceptor still fires but `aster.role` is empty,
so all role checks fail. Only methods with no `requires` pass through.

This means dev mode + auth-decorated services = all guarded methods denied.
This is intentional: it forces developers to either disable auth decorators
or configure real credentials.

## PeerAttributeStore wiring

The store must be connected to **both** consumer admission and the RPC
server. Two separate wiring points:

1. **Admission handler:** calls `peerStore.admit(peerId, attributes)` on
   success.
2. **RPC server:** reads `peerStore.getAttributes(peerId)` when building
   CallContext.

If either side is not wired, attributes are empty and all role checks fail.

## Performance notes

- The interceptor is stateless per call — it reads from CallContext, not
  from a database. The expensive part (signature verification) happened
  at admission time.
- `normaliseRequirement()` should be called once at registration time,
  not on every request.
- Avoid dynamic imports. `from aster.interceptors.capability import ...`
  should be module-level.

## Invariants confirmed by chaos tests

- All 58/58 cross-language matrix tests pass with auth enabled
- Scope mismatch fires before capability check (correct ordering)
- Session-scoped auth denial doesn't kill the session

## Implementation checklist for new bindings

- [ ] PeerAttributeStore: `admit(peerId, attrs)` and `getAttributes(peerId)`
- [ ] Wire PeerAttributeStore to admission handler AND RPC server
- [ ] Thread attributes into CallContext on every RPC
- [ ] CapabilityInterceptor: read `aster.role`, split on comma
- [ ] Handle both string and structured `requires` declarations
- [ ] Normalise bare string requires to `{ kind: 'role', roles: [s] }`
- [ ] Evaluate service-level AND method-level requirements (conjunction)
- [ ] Fire auth BEFORE pattern dispatch (request is null)
- [ ] Session: per-CALL auth, not per-session; deny continues session
- [ ] Open-gate mode: empty attributes → all role checks fail

## Key files

| Binding | File | Entry point |
|---------|------|-------------|
| Python | `interceptors/capability.py:45` | `CapabilityInterceptor` |
| Python | `interceptors/capability.py:25` | `_normalize_requirement()` |
| Python | `trust/rcan.py:40` | `evaluate_capability()` |
| Python | `peer_store.py` | `PeerAttributeStore` |
| TS | `interceptors/capability.ts:31` | `CapabilityInterceptor` |
| TS | `interceptors/capability.ts:20` | `normaliseRequirement()` |
| TS | `peer-store.ts` | `PeerAttributeStore` |
