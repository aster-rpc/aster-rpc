# @aster Service API — Day 0 Specification

**Status:** Specification (build from this)  
**Date:** 2026-04-08  
**Companion:** [aster-day0-technical-design.md](aster-day0-technical-design.md)

---

## Service Identity & Access Model

```python
@service(name="AsterService", version=1, scoped="shared", public=True)
class AsterService:
    ...
```

The `@aster` service runs with:
- `@service(public=True)` — suppresses "no auth" warnings; this service intentionally accepts unauthenticated peers
- `allow_all_consumers=True` on the `Server` — any peer can connect without a credential
- No `AsterRole` enum — access control uses the framework's existing mechanisms:
  - `requires=None` on a method means **anyone can call it** (no Gate 2 check)
  - `requires=CapabilityRequirement(ROLE, ["registered"])` means Gate 2 denies peers without that role — but since unauthenticated peers have empty attributes, they'll be denied. Authenticated identity is checked per-call via the `SignatureVerificationInterceptor` on the signed payload.

### How it works end-to-end

1. Peer connects to `@aster` with no credential → admitted via `allow_all_consumers=True` with empty attributes
2. Calls a method with `requires=None` (e.g., `discover`) → Gate 2 skips check (line 51 of `CapabilityInterceptor`) → **allowed**
3. Calls a method with `requires=CapabilityRequirement(ROLE, ["registered"])` (e.g., `publish`) → Gate 2 evaluates against empty attributes → **denied**
4. For REGISTERED methods: the request itself is a `SignedRequest[T]` — the `SignatureVerificationInterceptor` verifies the ed25519 signature, checks timestamp/nonce, and attaches the verified pubkey to the context. If the pubkey maps to a verified handle, the method proceeds.
5. For OWNER methods: the `OwnershipInterceptor` additionally checks that the verified pubkey's handle matches the resource being accessed.

### Method access levels

Three levels, enforced by different mechanisms:

| Level | `requires=` | Additional check | Example methods |
|-------|-------------|-----------------|-----------------|
| **Open** | `None` | None (or signature check on `SignedRequest` payloads for mutating calls) | `check_availability`, `discover`, `get_manifest`, `resolve`, `list_services`, `get_attestation`, `join`, `verify`, `resend_verification` |
| **Registered** | `CapabilityRequirement(ROLE, ["registered"])` | `SignatureVerificationInterceptor` verifies signed payload; pubkey must map to a verified handle | `handle_status`, `publish`, `enroll` |
| **Owner** | `CapabilityRequirement(ROLE, ["registered"])` | `SignatureVerificationInterceptor` + `OwnershipInterceptor` (handle must own the resource) | `unpublish`, `update_delegation`, `grant_access`, `revoke_access`, `list_access`, `audit_log` |

**Why no role enum?** The `@aster` service doesn't assign roles via credentials — it uses `allow_all_consumers=True` (everyone in) and then checks identity per-call via signed payloads. The framework's `requires=None` vs `requires=CapabilityRequirement(...)` is sufficient to distinguish open methods from authenticated ones. The interceptors handle the rest.

**Note on `join` and `verify`:** These are open methods (`requires=None`) because the caller may not have a handle yet — that's what they're creating. But they still accept `SignedRequest[T]` payloads, and the `SignatureVerificationInterceptor` verifies the signature. The difference from REGISTERED methods is that the interceptor doesn't require the pubkey to map to an existing verified handle.

---

## Methods

### 1. `check_availability`

Check if a handle is available for registration.

```python
@unary(requires=None)  # PUBLIC
async def check_availability(self, request: CheckAvailabilityRequest) -> CheckAvailabilityResult:
```

**Request:**
```python
@wire_type
@dataclass
class CheckAvailabilityRequest:
    handle: str
```

**Result:**
```python
@wire_type
@dataclass
class CheckAvailabilityResult:
    available: bool
    reason: str         # "available", "taken", "reserved", "invalid"
    # Does NOT distinguish "taken+verified" from "taken+pending" (prevents enumeration)
```

**Validations:**
- Handle format: 3-39 chars, lowercase `a-z0-9-`, no consecutive hyphens, starts/ends with alphanumeric
- Reserved word check (hardcoded list)
- Rate limit: 30/min per peer

**Logging:** None (high frequency, low value).

---

### 2. `join`

Reserve a handle and send a verification email.

```python
@unary(requires=None)  # PUBLIC (but signed payload required)
async def join(self, request: SignedRequest[JoinPayload]) -> JoinResult:
```

**Payload:**
```python
@wire_type
@dataclass
class JoinPayload:
    action: str             # must be "join"
    handle: str
    email: str
    announcements: bool     # opt-in to service announcements
    timestamp: int          # epoch seconds
    nonce: str              # 32 hex chars
```

**Result:**
```python
@wire_type
@dataclass
class JoinResult:
    handle: str
    status: str                     # "pending_verification"
    verification_expires_at: str    # ISO 8601 (24h from now)
    code_expires_at: str            # ISO 8601 (15 min from now)
```

**Validations:**
- Signature verification (interceptor)
- Timestamp within ±5 min of server time
- Nonce not seen in last 10 min
- Handle format valid
- Handle not taken (including pending handles)
- Handle not reserved word
- Email format valid (RFC 5322)
- Email not already associated with a verified handle → reject with generic message (no privacy leak)
- Email has pending handle → inform user, offer resend

**Side effects:**
- Store handle record: `{pubkey, email_hash, handle, status: "pending", claimed_at}`
- Generate 6-digit verification code, store `argon2(code)` with TTL
- Send verification email (plain text, no tracking)
- If root key has no existing identity: create it (auto `aster init`)

**Rate limit:** 3/hour per email, 3/hour per peer.

**Logging:** `handle_claimed` event with handle, pubkey (no email in log).

---

### 3. `verify`

Confirm email ownership with a verification code.

```python
@unary(requires=None)  # PUBLIC (but signed payload required)
async def verify(self, request: SignedRequest[VerifyPayload]) -> VerifyResult:
```

**Payload:**
```python
@wire_type
@dataclass
class VerifyPayload:
    action: str         # must be "verify"
    handle: str
    code: str           # 6-digit code from email
    timestamp: int
    nonce: str
```

**Result:**
```python
@wire_type
@dataclass
class VerifyResult:
    handle: str
    status: str         # "verified"
    pubkey: str         # confirmed root pubkey
```

**Validations:**
- Signature verification (interceptor)
- Handle exists and is pending
- Signing pubkey matches the pubkey that claimed this handle
- Code matches stored hash (argon2_verify)
- Code not expired (15 min TTL)
- Attempts not exhausted (5 per code)

**Side effects:**
- Update handle status: `"pending"` → `"verified"`
- Delete verification code record
- Decrement attempts on failure; invalidate code after 5 failures

**Error responses:**
- Invalid code → `{"error": "invalid_code", "attempts_remaining": N}`
- Code expired → `{"error": "code_expired"}`
- Handle not found / not pending → `{"error": "invalid_handle"}`

**Rate limit:** 5 attempts per code, then code invalidated.

**Logging:** `handle_verified` event on success. `verification_failed` event on failure (with attempt count).

---

### 4. `resend_verification`

Resend the verification email with a new code.

```python
@unary(requires=None)  # PUBLIC (but signed payload required)
async def resend_verification(self, request: SignedRequest[ResendPayload]) -> ResendResult:
```

**Payload:**
```python
@wire_type
@dataclass
class ResendPayload:
    action: str         # must be "resend_verification"
    handle: str
    timestamp: int
    nonce: str
```

**Result:**
```python
@wire_type
@dataclass
class ResendResult:
    code_expires_at: str        # ISO 8601
    resends_remaining: int      # out of 5 per 24h
```

**Validations:**
- Signature verification (interceptor)
- Handle exists and is pending
- Signing pubkey matches
- Resend cooldown: 60 seconds since last send
- Max 5 resends per 24h per handle

**Side effects:**
- Generate new 6-digit code, replace old one
- Send verification email

**Logging:** `verification_resent` event.

---

### 5. `handle_status`

Get current handle state for the caller's pubkey.

```python
@unary(requires=CapabilityRequirement(CapabilityKind.ROLE, ["registered"]))
async def handle_status(self, request: SignedRequest[HandleStatusPayload]) -> HandleStatusResult:
```

**Payload:**
```python
@wire_type
@dataclass
class HandleStatusPayload:
    action: str         # must be "handle_status"
    timestamp: int
    nonce: str
```

**Result:**
```python
@wire_type
@dataclass
class HandleStatusResult:
    handle: str
    status: str                 # "pending" | "verified"
    email_masked: str           # "em***@example.com"
    registered_at: str          # ISO 8601
    services_published: int
    recovery_codes_remaining: int   # 0 if none generated yet
```

**Validations:**
- Signature verification
- Look up handle by signing pubkey

**Rate limit:** 10/min per peer.

**Logging:** None.

---

### 6. `get_attestation`

Fetch the current signing key attestation.

```python
@unary(requires=None)  # PUBLIC
async def get_attestation(self) -> GetAttestationResult:
```

**Result:**
```python
@wire_type
@dataclass
class GetAttestationResult:
    attestation: SigningKeyAttestation
    # If during rotation overlap, may include both:
    previous_attestation: SigningKeyAttestation | None
```

No input needed. Returns the current (and optionally previous) signing key attestation. Services and consumers use this to verify enrollment tokens.

**Rate limit:** 30/min per peer.

**Logging:** None.

---

### 7. `publish`

Publish a service to the `@aster` directory.

```python
@unary(requires=CapabilityRequirement(CapabilityKind.ROLE, ["registered"]))
async def publish(self, request: SignedRequest[PublishPayload]) -> PublishResult:
```

**Payload:**
```python
@wire_type
@dataclass
class PublishPayload:
    action: str                 # must be "publish"
    handle: str                 # must match signing key's handle
    service_name: str
    contract_id: str            # BLAKE3 hash claimed by publisher
    manifest_json: str          # full contract manifest as JSON
    endpoints: list[EndpointInfo]
    delegation: DelegationStatement
    timestamp: int
    nonce: str

@wire_type
@dataclass
class EndpointInfo:
    node_id: str                # Iroh endpoint ID (hex)
    relay: str                  # relay URL
    ttl: int                    # seconds (default 3600)

@wire_type
@dataclass
class DelegationStatement:
    authority: str              # "consumer" (Day 0). Future: "producer" | "both"
    mode: str                   # "open" | "closed"
    token_ttl: int              # seconds (default 300 = 5 min)
    rate_limit: str | None      # e.g., "1/60m" or None
    roles: list[str]            # roles @aster can grant. Day 0: all from contract.
```

**Result:**
```python
@wire_type
@dataclass
class PublishResult:
    handle: str
    service_name: str
    version: int
    contract_id: str
    aster_root_pubkey: str          # trust anchor for verifying tokens
    current_attestation: SigningKeyAttestation   # so publisher can configure immediately
    endpoints_registered: int
    first_publish: bool             # true if this handle has never published before
    recovery_codes: list[str] | None  # only on first-ever publish for this handle
```

**Validations:**
- Signature verification (interceptor)
- Handle is verified
- `handle` in payload matches signing key's registered handle (OWNER check)
- Contract ID re-verification: `blake3(canonical(manifest_json))` must equal claimed `contract_id`
- Manifest JSON parseable and well-formed
- Service name valid (same rules as handle: 3-39 chars, alphanumeric + hyphens)
- At least one endpoint
- Delegation statement well-formed:
  - `authority` must be `"consumer"` (Day 0)
  - `mode` must be `"open"` or `"closed"`
  - `token_ttl` must be 60..86400 (1 min to 24 hours)
  - `rate_limit` if present must parse as `"<int>/<duration>"` where duration is `Nm` or `Nh`
  - `roles` must be non-empty and match roles defined in the manifest

**Side effects:**
- Store service record in iroh-doc
- Store manifest as iroh-blob (content-addressed, deduped)
- Store endpoint records
- Store delegation statement
- If first publish for this handle: generate 8 recovery codes, store argon2 hashes, return plaintext codes in result
- Broadcast `service_published` event on gossip

**Rate limit:** 10/hour per handle.

**Logging:** `service_published` event with handle, service_name, contract_id, version, mode.

---

### 8. `unpublish`

Remove a service from the directory.

```python
@unary(requires=CapabilityRequirement(CapabilityKind.ROLE, ["registered"]))
async def unpublish(self, request: SignedRequest[UnpublishPayload]) -> UnpublishResult:
```

**Payload:**
```python
@wire_type
@dataclass
class UnpublishPayload:
    action: str             # must be "unpublish"
    handle: str
    service_name: str
    timestamp: int
    nonce: str
```

**Result:**
```python
@wire_type
@dataclass
class UnpublishResult:
    handle: str
    service_name: str
    removed: bool
```

**Validations:**
- Signature verification
- Handle matches signing key (OWNER check)
- Service exists and is published by this handle

**Side effects:**
- Remove service record, endpoints, access grants, delegation from iroh-docs
- Blob (manifest) remains (content-addressed, may be shared). Tag removed.
- Broadcast `service_unpublished` event

**Logging:** `service_unpublished` event.

---

### 9. `update_delegation`

Update delegation settings (mode, TTL, rate limit) without re-publishing.

```python
@unary(requires=CapabilityRequirement(CapabilityKind.ROLE, ["registered"]))
async def update_delegation(self, request: SignedRequest[UpdateDelegationPayload]) -> UpdateDelegationResult:
```

**Payload:**
```python
@wire_type
@dataclass
class UpdateDelegationPayload:
    action: str                 # must be "update_delegation"
    handle: str
    service_name: str
    delegation: DelegationStatement     # new delegation settings
    timestamp: int
    nonce: str
```

**Result:**
```python
@wire_type
@dataclass
class UpdateDelegationResult:
    handle: str
    service_name: str
    delegation: DelegationStatement     # confirmed new settings
```

**Validations:**
- Signature verification
- Handle matches (OWNER)
- Service exists
- DelegationStatement well-formed (same validation as publish)

**Side effects:**
- Replace delegation in iroh-doc
- Broadcast `delegation_updated` event

**Logging:** `delegation_updated` event with old and new settings.

---

### 10. `grant_access`

Grant a handle access to a closed service with a specific role.

```python
@unary(requires=CapabilityRequirement(CapabilityKind.ROLE, ["registered"]))
async def grant_access(self, request: SignedRequest[GrantAccessPayload]) -> GrantAccessResult:
```

**Payload:**
```python
@wire_type
@dataclass
class GrantAccessPayload:
    action: str             # must be "grant_access"
    handle: str             # service owner
    service_name: str
    consumer_handle: str    # who gets access
    role: str               # must be in the contract's defined roles
    scope: str              # "handle" (Day 0). Future: "node"
    scope_node_id: str | None   # only when scope="node" (Day 2, ignored Day 0)
    timestamp: int
    nonce: str
```

**Result:**
```python
@wire_type
@dataclass
class GrantAccessResult:
    consumer_handle: str
    service_name: str
    role: str
    scope: str
    granted_at: str         # ISO 8601
```

**Validations:**
- Signature verification
- Handle matches (OWNER)
- Service exists and is published
- Consumer handle exists and is verified
- Role is defined in the service's contract manifest
- Scope must be `"handle"` (Day 0)
- Scope `"node"` → reject with "not yet supported"

**Side effects:**
- Store access grant in iroh-doc
- If grant already exists for this consumer+service, update the role (upsert)
- Broadcast `access_granted` event on audit gossip topic

**Logging:** `access_granted` event with owner, service, consumer, role.

---

### 11. `revoke_access`

Remove a handle's access to a service.

```python
@unary(requires=CapabilityRequirement(CapabilityKind.ROLE, ["registered"]))
async def revoke_access(self, request: SignedRequest[RevokeAccessPayload]) -> RevokeAccessResult:
```

**Payload:**
```python
@wire_type
@dataclass
class RevokeAccessPayload:
    action: str             # must be "revoke_access"
    handle: str             # service owner
    service_name: str
    consumer_handle: str
    timestamp: int
    nonce: str
```

**Result:**
```python
@wire_type
@dataclass
class RevokeAccessResult:
    consumer_handle: str
    service_name: str
    revoked: bool           # false if no grant existed
```

**Validations:**
- Signature verification
- Handle matches (OWNER)
- Service exists

**Side effects:**
- Remove access grant from iroh-doc
- Broadcast `access_revoked` event on audit gossip topic
- Note: already-issued tokens remain valid until their TTL expires

**Logging:** `access_revoked` event.

---

### 12. `list_access`

List all access grants for a service.

```python
@unary(requires=CapabilityRequirement(CapabilityKind.ROLE, ["registered"]))
async def list_access(self, request: SignedRequest[ListAccessPayload]) -> ListAccessResult:
```

**Payload:**
```python
@wire_type
@dataclass
class ListAccessPayload:
    action: str             # must be "list_access"
    handle: str
    service_name: str
    timestamp: int
    nonce: str
```

**Result:**
```python
@wire_type
@dataclass
class ListAccessResult:
    grants: list[AccessGrantEntry]

@wire_type
@dataclass
class AccessGrantEntry:
    consumer_handle: str
    role: str
    scope: str
    granted_at: str
```

**Validations:**
- Signature verification
- Handle matches (OWNER)
- Service exists

**Logging:** None.

---

### 13. `enroll`

Consumer requests an enrollment token for a published service.

```python
@unary(requires=CapabilityRequirement(CapabilityKind.ROLE, ["registered"]))
async def enroll(self, request: SignedRequest[EnrollPayload]) -> EnrollResult:
```

**Payload:**
```python
@wire_type
@dataclass
class EnrollPayload:
    action: str             # must be "enroll"
    consumer_handle: str    # the caller's handle
    target_handle: str      # service owner
    target_service: str     # service name
    timestamp: int
    nonce: str
```

**Result:**
```python
@wire_type
@dataclass
class EnrollResult:
    token: str                          # base64-encoded EnrollmentToken (signed by signing key)
    attestation: SigningKeyAttestation   # so consumer can pass it to the service
    expires_at: str                     # ISO 8601
    role: str                           # granted role
```

**Validations:**
- Signature verification
- Consumer handle is verified (REGISTERED check)
- `consumer_handle` matches signing key's handle (you can only enroll yourself)
- Target service exists and is published
- Access check based on delegation mode:
  - **Open:** consumer has a verified handle → allowed. Check rate limit if configured.
  - **Closed:** consumer has an explicit access grant → allowed with granted role. No grant → reject.
- Rate limit check (if publisher configured one)

**Token issuance (on success):**
1. Build `EnrollmentToken`:
   - `consumer_handle`, `consumer_pubkey` (from handle record)
   - `target_handle`, `target_service`, `target_contract_id` (from service record)
   - `roles`: for open services, all roles from delegation. For closed, the granted role.
   - `issued_at`: now
   - `expires_at`: now + delegation's `token_ttl`
   - `signing_key_id`: current signing key's `key_id`
2. Sign token with current signing key
3. Return token + current attestation

**Error responses:**
- Service not found → `{"error": "service_not_found"}`
- Access denied (closed + no grant) → `{"error": "access_denied"}`
- Rate limited → `{"error": "rate_limited", "retry_after": N}` (seconds)

**Logging:** `token_issued` event on audit gossip topic with consumer_handle, target_service, role, expires_at. Also written to audit iroh-doc for history.

---

### 14. `discover`

Search for published services.

```python
@unary(requires=None)  # PUBLIC
async def discover(self, request: DiscoverRequest) -> DiscoverResult:
```

**Request:**
```python
@wire_type
@dataclass
class DiscoverRequest:
    query: str              # service name substring, or "@handle" to list a handle's services
    limit: int              # max results (default 20, max 100)
    offset: int             # pagination offset (default 0)
```

**Result:**
```python
@wire_type
@dataclass
class DiscoverResult:
    services: list[DiscoverEntry]
    total: int              # total matching (for pagination)

@wire_type
@dataclass
class DiscoverEntry:
    handle: str
    service_name: str
    version: int
    contract_id: str
    method_count: int
    endpoint_count: int
    visibility: str         # "open" | "closed"
```

**Validations:**
- Query non-empty, max 100 chars
- Limit 1-100
- Offset >= 0

**Search logic (Day 0):**
- If query starts with `@`: list services for that handle (prefix match on `svc:<handle>:`)
- Otherwise: substring match on service name across all handles
- Only return services with visibility "open" or where caller has an access grant (if caller is authenticated)

**Rate limit:** 30/min per peer.

**Logging:** None.

---

### 15. `get_manifest`

Fetch a published service's contract manifest.

```python
@unary(requires=None)  # PUBLIC
async def get_manifest(self, request: GetManifestRequest) -> GetManifestResult:
```

**Request:**
```python
@wire_type
@dataclass
class GetManifestRequest:
    handle: str
    service_name: str
```

**Result:**
```python
@wire_type
@dataclass
class GetManifestResult:
    handle: str
    service_name: str
    manifest_json: str          # full contract manifest
    contract_id: str
    version: int
    endpoint_count: int
    published_at: str           # ISO 8601
```

**Validations:**
- Service exists and is published
- If closed: manifest still public (you can see what it offers, just can't connect without a grant)

**Rate limit:** 30/min per peer.

**Logging:** None.

---

### 16. `resolve`

Resolve a service to its live endpoints.

```python
@unary(requires=None)  # PUBLIC
async def resolve(self, request: ResolveRequest) -> ResolveResult:
```

**Request:**
```python
@wire_type
@dataclass
class ResolveRequest:
    handle: str
    service_name: str
```

**Result:**
```python
@wire_type
@dataclass
class ResolveResult:
    handle: str
    service_name: str
    endpoints: list[EndpointInfo]
    contract_id: str
```

**Validations:**
- Service exists
- Filter out stale endpoints (past TTL) — return them with a `stale: true` flag? Or omit? Day 0: omit stale.

**Rate limit:** 60/min per peer.

**Logging:** None.

---

### 17. `list_services`

List all published services for a handle.

```python
@unary(requires=None)  # PUBLIC
async def list_services(self, request: ListServicesRequest) -> ListServicesResult:
```

**Request:**
```python
@wire_type
@dataclass
class ListServicesRequest:
    handle: str
```

**Result:**
```python
@wire_type
@dataclass
class ListServicesResult:
    handle: str
    services: list[DiscoverEntry]   # reuse DiscoverEntry
```

**Validations:**
- Handle exists

**Rate limit:** 30/min per peer.

**Logging:** None.

---

### 18. `audit_log`

Fetch audit history for a service.

```python
@unary(requires=CapabilityRequirement(CapabilityKind.ROLE, ["registered"]))
async def audit_log(self, request: SignedRequest[AuditLogPayload]) -> AuditLogResult:
```

**Payload:**
```python
@wire_type
@dataclass
class AuditLogPayload:
    action: str             # must be "audit_log"
    handle: str
    service_name: str
    since: int              # epoch seconds (return events after this time)
    limit: int              # max events (default 50, max 500)
    timestamp: int
    nonce: str
```

**Result:**
```python
@wire_type
@dataclass
class AuditLogResult:
    events: list[AuditEvent]
    has_more: bool

@wire_type
@dataclass
class AuditEvent:
    event_type: str         # "token_issued", "access_granted", "access_revoked",
                            # "delegation_updated", "service_published", "service_unpublished"
    consumer_handle: str | None
    role: str | None
    details: str            # JSON string with event-specific data
    timestamp: str          # ISO 8601
```

**Validations:**
- Signature verification
- Handle matches (OWNER)
- Service exists

**Logging:** None (it IS the log).

---

## Interceptors

### `SignatureVerificationInterceptor`

Applied to all methods that accept `SignedRequest[T]`.

```python
@interceptor
class SignatureVerificationInterceptor:
    async def intercept(self, ctx, request, next):
        if hasattr(request, 'payload') and hasattr(request, 'signature'):
            # 1. Verify ed25519 signature
            canonical = canonical_json(request.payload)
            if not ed25519_verify(request.pubkey, canonical, request.signature):
                raise AsterError("INVALID_SIGNATURE", "Signature verification failed")

            # 2. Check timestamp freshness (±5 min)
            if abs(time.time() - request.payload.timestamp) > 300:
                raise AsterError("STALE_REQUEST", "Request timestamp outside acceptable window")

            # 3. Check nonce uniqueness (10 min sliding window)
            if self.nonce_store.seen(request.payload.nonce):
                raise AsterError("REPLAYED_NONCE", "Nonce has been used before")
            self.nonce_store.record(request.payload.nonce)

            # 4. Attach verified pubkey to context for downstream use
            ctx.attributes["aster.verified_pubkey"] = request.pubkey

        return await next(ctx, request)
```

### `RateLimitInterceptor`

Applied per-method with configurable limits.

```python
@interceptor
class RateLimitInterceptor:
    async def intercept(self, ctx, request, next):
        peer_id = ctx.peer  # endpoint ID of the caller
        method = ctx.method_name
        limit = self.limits.get(method)

        if limit and self.is_rate_limited(peer_id, method, limit):
            raise AsterError("RATE_LIMITED", f"Rate limit exceeded for {method}")

        return await next(ctx, request)
```

### `OwnershipInterceptor`

For OWNER-gated methods, verifies the signing key's handle matches the resource.

```python
@interceptor
class OwnershipInterceptor:
    async def intercept(self, ctx, request, next):
        if self.requires_ownership(ctx.method_name):
            pubkey = ctx.attributes.get("aster.verified_pubkey")
            handle_in_request = request.payload.handle
            registered_handle = self.storage.get_handle_by_pubkey(pubkey)

            if not registered_handle or registered_handle != handle_in_request:
                raise AsterError("NOT_OWNER", "You don't own this resource")

        return await next(ctx, request)
```

---

## Error Codes

Standardized error codes returned as `AsterError`:

| Code | Meaning |
|------|---------|
| `INVALID_SIGNATURE` | Ed25519 signature verification failed |
| `STALE_REQUEST` | Timestamp outside ±5 min window |
| `REPLAYED_NONCE` | Nonce already seen |
| `RATE_LIMITED` | Too many requests |
| `NOT_OWNER` | Caller doesn't own the resource |
| `HANDLE_TAKEN` | Handle already claimed |
| `HANDLE_RESERVED` | Handle is a reserved word |
| `HANDLE_INVALID` | Handle format invalid |
| `EMAIL_TAKEN` | Email already associated with an account |
| `EMAIL_INVALID` | Email format invalid |
| `INVALID_CODE` | Verification code incorrect |
| `CODE_EXPIRED` | Verification code past TTL |
| `RESEND_COOLDOWN` | Must wait before resending |
| `HANDLE_NOT_VERIFIED` | Handle exists but not yet verified |
| `SERVICE_NOT_FOUND` | Published service doesn't exist |
| `ACCESS_DENIED` | No access grant for closed service |
| `CONTRACT_HASH_MISMATCH` | Claimed contract_id doesn't match computed hash |
| `INVALID_DELEGATION` | Delegation statement malformed |
| `INVALID_ROLE` | Role not defined in service contract |
| `NOT_SUPPORTED` | Feature not available (e.g., node-scope grants) |

---

## Gossip Topics

| Topic | Derived from | Purpose |
|-------|-------------|---------|
| `aster-events` | `blake3(b"aster-events")` | Cross-node internal events (handle registered, service published) |
| `aster-audit:<handle>:<service>` | `blake3(b"aster-audit:<handle>:<service>")` | Per-service audit stream (token issued, access granted/revoked) |

---

## Storage Keys (iroh-docs)

### Handles doc

```
b"handle:<handle>"              → {pubkey, email_hash, status, registered_at, announcements}
b"email:<sha256(email)>"        → {handle}  (reverse lookup for uniqueness)
b"pubkey:<pubkey>"              → {handle}  (reverse lookup for status checks)
b"recovery:<handle>:<n>"        → {code_hash}  (argon2, n=0..7)
```

### Services doc

```
b"svc:<handle>:<service>"       → {version, contract_id, visibility, published_at, token_ttl}
b"ep:<handle>:<service>:<nid>"  → {relay, ttl, registered_at}
b"deleg:<handle>:<service>"     → {authority, mode, token_ttl, rate_limit, roles}  (DelegationStatement)
```

### Access doc

```
b"grant:<handle>:<service>:<consumer>" → {role, scope, scope_node_id, granted_at, granted_by}
```

### Audit doc

```
b"audit:<handle>:<service>:<ts>:<event_type>:<id>" → {AuditEvent JSON}
```

### Blobs

```
tag: "manifest:<handle>:<service>" → blob hash of manifest JSON
```
