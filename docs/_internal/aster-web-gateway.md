# Aster Web Gateway — Design

## Goal

Let browsers and ordinary HTTP clients call Aster services. Aster endpoints live
on iroh QUIC with NodeId-based admission and are not directly reachable from a
browser. A web gateway ("the proxy") terminates HTTP on a public domain, handles
end-user authentication via OIDC, and forwards calls to Aster service producers
over iroh on behalf of that user.

Target URL shape:

```
https://<handle>.example.com/<ServiceName>/v<N>/<methodName>
```

- `<handle>` — the service publisher's registered handle (wildcard DNS /
  SNI-routed at the gateway).
- `<ServiceName>` — an Aster service the publisher has exposed through the gateway.
- `v<N>` — Aster service version.
- `<methodName>` — an RPC method on that service.

## Architecture

```
Browser ──HTTPS/H3──▶ Web Gateway ──iroh QUIC──▶ Aster Producer
                          │                          │
                          │                          └─ ServiceRegistry
                          ├─ OIDC (Google/MS/GitHub)
                          ├─ Publisher route registry
                          └─ Delegated admission credentials
```

The gateway is a single Rust service (Salvo or similar) that:

1. Accepts browser requests on `*.example.com`.
2. Runs the end user through OIDC.
3. Holds a long-lived Aster client connection to each publisher's producer,
   admitted via a delegated `EnrollmentToken` issued by @aster and signed into
   the proxy's NodeId.
4. Translates HTTP method+path+body into an Aster RPC call, injecting end-user
   identity as call metadata.

### Why this works transport-wise

WebTransport (W3C) and iroh both expose bidi streams + datagrams over QUIC, so
the semantic gap between "HTTP request from browser" and "Aster RPC call" is
small. For an MVP we can stay with plain HTTPS/JSON at the browser edge — we do
not need browser WebTransport to make this architecture work. We can upgrade to
WebTransport later for streaming RPCs without changing the trust or routing
model.

The `web-transport` crates under `aster/web-transport` are relevant only if and
when we want streaming semantics on the browser leg. For unary RPCs, a normal
HTTP handler is enough.

## Publisher onboarding

1. Publisher registers a handle with the gateway's control plane. Control plane
   allocates `<handle>.example.com` and stores publisher metadata (NodeId,
   contact, billing, etc.).
2. Publisher runs their Aster producer normally.
3. Publisher mints a delegated `EnrollmentToken`
   (see `aster/trust/delegated.py:47`) scoped to:
   - `consumer_pubkey` — the gateway's NodeId
   - `target_handle`, `target_service`, `target_contract_id`
   - `roles` — the ceiling of capabilities the publisher is willing to delegate
     to the gateway (e.g. `["ops.status", "ops.logs"]`)
4. Publisher hands the token to the gateway via the control plane API.
5. The gateway performs the 6-step proof-of-possession handshake on ALPN
   `aster.admission` (`aster/trust/delegated.py:240-390`) and caches the
   admission in its `PeerAttributeStore`.
6. Publisher registers the set of services + methods they want exposed through
   the gateway (name, version, request/response schema, auth requirements).
   This populates the gateway's route table. Aster has no runtime introspection
   RPC, so this step is explicit.

Route registration is out-of-band (publisher → gateway control plane), not
discovered over the Aster wire protocol. A future improvement is an
introspection service on Aster itself, at which point step 6 can be automated.

## Request flow

```
1. Browser → GET https://alice.example.com/MissionControl/v1/listTasks
2. Gateway resolves SNI → publisher record → Aster client connection
3. Gateway checks session cookie:
     - no session → redirect to OIDC provider, return
     - session present → decode, load end-user claims
4. Gateway looks up route: (MissionControl, v1, listTasks) → MethodInfo
5. Gateway constructs CallContext.metadata:
     end_user_sub    = "google-oauth2|1234567890"
     end_user_claims = {"email": "...", "groups": [...], "roles": [...]}
     end_user_token  = "<raw JWT from IdP>"
6. Gateway invokes the Aster RPC over its delegated connection
7. Producer-side auth interceptor reads metadata, computes effective roles,
   authorizes, dispatches
8. Response marshalled back to JSON and returned to the browser
```

## Trust model

Two independent trust layers:

**Layer 1 — transport admission (existing).** Gateway's NodeId is admitted to
the producer via a delegated `EnrollmentToken`. This is connection-scoped and
unchanged from today's Aster. The publisher controls the ceiling of
capabilities the gateway can ever exercise against the service via the token's
`roles` field.

**Layer 2 — per-call end-user identity (new).** Each RPC carries
`end_user_sub` / `end_user_claims` / `end_user_token` in `CallContext.metadata`.
The producer's auth interceptor uses these to authorize the specific call.

### Effective roles are an intersection

```
effective_roles = proxy_roles ∩ user_roles_from_claims
```

Where `proxy_roles` is what the publisher delegated in the EnrollmentToken, and
`user_roles_from_claims` is what the end user's OIDC claims say they can do.
The gateway can never grant an end user more than the publisher delegated to
the gateway, regardless of what the claims say. This gives publishers a crisp
mental model: "I gave the gateway `ops.*`; nothing the gateway relays will
exceed that."

### Default: trust the proxy

The default interceptor accepts `end_user_claims` at face value because the
connection itself is already authenticated at the transport layer — the
producer knows the calls come from the gateway's NodeId, which it has chosen
to trust by accepting the EnrollmentToken. This is the same model as every
mature API gateway (Envoy ext_authz, Kong OIDC, Istio RequestAuthentication).

### Paranoid mode: verify the JWT

The producer-side auth interceptor can be configured to verify
`end_user_token` as a JWT against the OIDC provider's JWKS. This raises the
bar: a compromised gateway would have to also forge tokens signed by Google /
Microsoft / GitHub to impersonate users, which it cannot.

Caveats publishers must understand:

- The JWT's `aud` is the gateway's OIDC client ID, not the producer's service.
  Verification proves "a trusted IdP issued this token to the gateway," not
  "issued to me." That is usually enough, but document it.
- `iss` must be pinned to an allow-list of accepted IdPs.
- Clock skew tolerance should be small (<60s).
- Token lifetime bounds how long a revoked user can keep hitting the service.
  Gateway should use short IdP token lifetimes (5–15 min) and refresh at the
  gateway when it has a refresh token.

If a publisher needs stronger guarantees than this, the gateway can be
extended to re-sign an internal assertion (short-lived, `aud=<service>`,
signed by a key the publisher can fetch via a well-known URL). Not in scope
for v1.

## CallContext changes

Today `CallContext` has `peer` (NodeId-level identity) and `metadata` (a bag).
Rather than have every interceptor and app handler fish magic keys out of the
bag, add a typed field:

```python
@dataclass
class EndUserPrincipal:
    sub: str                    # e.g. "google-oauth2|1234567890"
    claims: dict[str, Any]      # decoded OIDC claims
    source_token: str           # raw JWT, for optional verification
    verified: bool              # True iff the interceptor verified the JWT
    iss: str                    # pinned issuer
    issued_at: datetime
    expires_at: datetime

class CallContext:
    peer: PeerAttributes                    # existing, gateway's NodeId
    metadata: Mapping[str, str]             # existing, app headers
    end_user: Optional[EndUserPrincipal]    # NEW, set by auth interceptor
```

The auth interceptor populates `end_user` once per call by reading the three
metadata keys. Downstream interceptors and handlers read the typed field.
Stringly-typed access to `end_user_sub` etc. is an internal detail of the
interceptor and should not leak into app code.

## Implementation punch list

### Aster core (Python binding first, then port to others)

1. `aster/trust/` — add an `EndUserPrincipal` dataclass and an `end_user` field
   on `CallContext`. Default `None`.
2. New interceptor: `EndUserAuthInterceptor`. Config:
   - `trust_proxy: bool` (default True) — accept claims without JWT verification.
   - `verify_jwt: bool` (default False) — verify `end_user_token` against JWKS.
   - `accepted_issuers: list[str]` — `iss` allow-list.
   - `jwks_cache_ttl: int` — seconds.
   - `clock_skew: int` — seconds, default 30.
3. Role-check helpers updated to compute
   `effective = proxy_roles ∩ user_roles` rather than read one side.
4. Audit logging: every dispatch logs `peer.node_id` AND `end_user.sub` (when
   present) so audit trails capture both the gateway and the end user.
5. Tests: end-user trust, JWT verification success + failure modes, role
   intersection, revoked issuer, expired token, missing claims.

### Gateway service (new Rust crate, not yet in the tree)

1. SNI-based tenant routing on `*.example.com`.
2. OIDC integration — start with Google, Microsoft, GitHub. Session cookie with
   server-side store, short TTL, refresh at the edge.
3. Route table: `(handle, service, version, method) → MethodInfo`, populated
   via publisher control plane API. Hot-reloadable.
4. Aster client pool: one long-lived delegated connection per publisher,
   reconnect with backoff, surface admission failures to the control plane.
5. HTTP → Aster adapter: JSON body → Aster request message, metadata
   injection, response marshalling.
6. Per-call metadata population:
   - `end_user_sub` — OIDC `sub` claim
   - `end_user_claims` — canonical subset (email, groups, roles, custom claims)
   - `end_user_token` — raw JWT
   - Any existing app metadata from request headers (allow-listed)
7. Observability: per-route latency, per-tenant error rate, admission health.
8. Streaming RPCs: initially unary only. Phase 2 adds WebTransport or
   Server-Sent Events on the browser leg for server streaming, using the
   `web-transport` crates already under `aster/`.

### Publisher control plane (new, minimal for v1)

1. Handle registration + DNS wildcard binding.
2. EnrollmentToken intake (stores the proxy's admission credentials).
3. Service/method route registration API — publishers POST route manifests.
4. Publisher-facing dashboard: activity, errors, revocation.

## What is explicitly out of scope for v1

- **Browser-side WebTransport.** Plain HTTPS/JSON is enough to start. Revisit
  when we need streaming.
- **Runtime introspection RPC in Aster.** Routes are registered out-of-band.
  Auto-discovery is a later improvement.
- **Gateway-issued internal assertions.** JWT pass-through is sufficient for
  v1. If a publisher needs stronger binding to their service, they turn on
  JWT verification with `aud` pinning at the interceptor.
- **Per-user sub-delegated Aster credentials.** A cleaner long-term story is
  for the gateway to mint short-lived rcan-style tokens bound to an end-user
  subject, threaded through as per-call credentials the producer cryptographically
  verifies. This is a protocol change to Aster and should wait until the
  trust-the-proxy model is shown to be insufficient in practice.

## Open questions

1. **Refresh token handling.** Does the gateway hold the IdP refresh token, or
   do we force re-auth at session expiry? Holding refresh tokens is a
   significant threat-model upgrade for the gateway — it becomes a high-value
   target. Probably fine, but decide explicitly.
2. **Tenant isolation.** One gateway process serving many publishers means a
   process-level compromise exposes all delegated admissions at once. Do we
   want per-tenant gateway processes for high-value publishers, or rely on
   in-process isolation?
3. **Rate limiting and abuse.** Per-user, per-route, per-publisher. Whose
   quota gets charged when an end user floods a producer? Probably the
   publisher's, but the gateway needs to enforce it before forwarding.
4. **Content negotiation.** JSON in/out is obvious; do we also want to accept
   the Aster wire codec directly for clients that speak it? Would let a
   non-browser client bypass the JSON translation cost while still getting
   OIDC identity injection.
5. **Revocation signal.** When a publisher revokes the gateway's
   EnrollmentToken, connected sessions need to terminate. Is there a push
   revocation channel, or does the gateway rediscover on next reconnect?
