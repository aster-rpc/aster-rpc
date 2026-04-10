# Capability Interception

**Status:** Stub -- to be filled after chaos tests confirm invariants.

**Reference:** `bindings/python/aster/server.py` lines 485-497 + `bindings/typescript/packages/aster/src/interceptors/capability.ts`

## What this flow covers

How role-based access control is enforced from admission through to
per-method dispatch. The full chain: credential attributes -> PeerAttributeStore
-> CallContext.attributes -> CapabilityInterceptor -> allow/deny.

## Sections to write

### 1. Attribute flow: admission to dispatch
- Consumer presents credential with `attributes: { "aster.role": "ops.status,ops.ingest" }`
- Server's admission handler verifies credential, extracts attributes
- On success: `PeerAttributeStore.admit()` records `endpointId -> attributes`
- On each RPC: server reads `peerStore.getAttributes(peerId)`, injects into CallContext
- CapabilityInterceptor reads `ctx.attributes['aster.role']`, splits on comma, checks against method's `requires`

### 2. Where auth fires
- BEFORE pattern dispatch, not inside the handler
- `applyRequestInterceptors(interceptors, callCtx, null)` -- request is null at this point
- This guarantees auth checks fire on every pattern including bidi/client streams that might never produce a request frame
- For session-scoped services: auth fires per CALL, not per session -- a denied call writes an error trailer but the session continues

### 3. Requires normalization
- `@Rpc({ requires: Role.ADMIN })` -- bare string, shorthand for single role
- `@Rpc({ requires: anyOf(Role.LOGS, Role.ADMIN) })` -- structured `{ kind: 'any_of', roles: [...] }`
- CapabilityInterceptor must handle BOTH forms
- `normaliseRequirement()` converts strings to `{ kind: 'role', roles: [s] }`
- **Known gap fixed:** TS interceptor only handled structured form, bare strings were silently no-ops

### 4. PeerAttributeStore wiring
- Must be connected to BOTH consumer admission AND the RPC server
- Consumer admission: `opts.peerStore.admit(...)` on success
- RPC server: reads `this.peerStore.getAttributes(peerId)` when building CallContext
- **Known gap fixed:** TS consumer admission handler didn't call peerStore.admit(), so attributes were always empty

### 5. Open-gate mode (dev mode)
- `allowAllConsumers=true` auto-admits peers with empty attributes
- CapabilityInterceptor still fires but `aster.role` is empty -> all role checks fail
- EXCEPT methods with no `requires` -> pass
- This means dev mode + auth-decorated services = all guarded methods denied

### 6. Comma-separated roles
- `aster.role` is a SINGLE string with comma-separated values: "ops.status,ops.ingest"
- Interceptor splits on comma and trims whitespace to build the caller's role set
- The split is essential -- without it, `callerRoles.has('ops.status')` fails on the full string

## Invariants for new implementations

_(To be confirmed by chaos tests, then documented here)_

## Bugs this flow exposed

- TS CapabilityInterceptor dropped bare string requires (no normalisation)
- TS consumer admission didn't record attributes in PeerAttributeStore
- TS RpcServer didn't read attributes from PeerAttributeStore into CallContext
- TS server's scope mismatch guard (FAILED_PRECONDITION) fires before CapabilityInterceptor -- this is correct behaviour, not a bug
