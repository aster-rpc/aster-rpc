# @aster Service API

**Status:** Implemented Day 0 core  
**Date:** 2026-04-08  
**Companion:** [aster-day0-technical-design.md](/Users/emrul/dev/aster/aster-app/docs/aster-day0-technical-design.md)

## Terminology

- `@aster` is the producer application implemented in this repository.
- The underlying replicated producer registry provided by the Aster runtime is
  a separate system concern.
- This document describes the `@aster` RPC surface as it exists now.

## Runtime Shape

The current implementation exposes multiple wire-facing services backed by one
shared `AsterApp` state object:

- `ProfileService`
- `PublicationService`
- `AccessService`
- `EnrollmentService`
- `AuditService`

The compatibility alias `AsterService` still exists in code, but the runnable
app registers the split services on the wire.

## Access Model

The current logical role model used by `@aster` is:

- `PUBLIC`
- `REGISTERED`
- `OWNER`
- `ADMIN`

In practice, Day 0 enforcement is a mix of:

- unsigned public methods for read-only public lookups
- signed mutation methods for caller-authenticated changes
- owner checks against the verified-handle mapping in the authoritative store

`@aster` itself is intended to run with:

- `allow_all_consumers=True`
- `@service(public=True)` on the exposed services

That is intentional. Public reachability is separated from per-method mutation
validation and ownership checks.

## ProfileService

### Public methods

```python
check_availability(CheckAvailabilityRequest) -> CheckAvailabilityResult
get_profile(GetProfileRequest) -> PublicProfileResult
```

### Signed methods

```python
join(SignedRequest[JoinPayload]) -> JoinResult
verify(SignedRequest[VerifyPayload]) -> VerifyResult
resend_verification(SignedRequest[ResendPayload]) -> ResendResult
handle_status(SignedRequest[HandleStatusPayload]) -> HandleStatusResult
update_profile(SignedRequest[UpdateProfilePayload]) -> UpdateProfileResult
```

### Important payload/result details

`JoinPayload`

- `handle`
- `email`
- `announcements`
- `timestamp`
- `nonce`

`JoinResult`

- `handle`
- `status = "pending_verification"`
- `verification_expires_at`
- `code_expires_at`
- `email_delivery_status`

`HandleStatusResult`

- `handle`
- `status`
- `email_masked`
- `display_name`
- `bio`
- `url`
- `registered_at`
- `services_published`
- `recovery_codes_remaining`

`UpdateProfilePayload`

- `display_name: str | None`
- `bio: str | None`
- `url: str | None`

`PublicProfileResult`

- `handle`
- `display_name`
- `bio`
- `url`
- `services_published`
- `registered_at`

### Current validation rules

- handles: 3-39 chars, lowercase plus digits and hyphens
- verification codes: 6 digits
- profile `display_name`: max 64 chars
- profile `bio`: max 500 chars
- profile `url`: single `http://` or `https://` URL

## PublicationService

### Public methods

```python
discover(DiscoverRequest) -> DiscoverResult
get_manifest(GetManifestRequest) -> GetManifestResult
resolve(ResolveRequest) -> ResolveResult
list_services(ListServicesRequest) -> ListServicesResult
```

### Signed owner methods

```python
publish(SignedRequest[PublishPayload]) -> PublishResult
set_visibility(SignedRequest[SetVisibilityPayload]) -> VisibilityResult
update_service(SignedRequest[UpdateServicePayload]) -> UpdateServiceResult
unpublish(SignedRequest[UnpublishPayload]) -> UnpublishResult
```

### Important payload/result details

`PublishPayload`

- `handle`
- `service_name`
- `contract_id`
- `manifest_json`
- `description` required, max 280 chars
- `status` in `experimental | stable | deprecated`
- `endpoints`
- `delegation`
- `timestamp`
- `nonce`

`DiscoverEntry`

- `handle`
- `service_name`
- `version`
- `contract_id`
- `description`
- `status`
- `method_count`
- `endpoint_count`
- `visibility`
- `delegation_mode`
- `published_at`

`UpdateServicePayload`

- `handle`
- `service_name`
- `description: str | None`
- `status: str | None`
- `replacement: str | None`
- `timestamp`
- `nonce`

`UpdateServiceResult`

- `handle`
- `service_name`
- `description`
- `status`
- `replacement`

### Current publication behavior

- contract IDs are re-verified as `blake3(canonical_manifest_json)`
- at least one endpoint is required
- visibility is stored separately from delegation mode
- exact `get_manifest` and `resolve` remain available even for private services
- endpoint freshness is enforced from per-endpoint `registered_at + ttl`

## AccessService

Signed owner methods:

```python
update_delegation(SignedRequest[UpdateDelegationPayload]) -> UpdateDelegationResult
grant_access(SignedRequest[GrantAccessPayload]) -> GrantAccessResult
revoke_access(SignedRequest[RevokeAccessPayload]) -> RevokeAccessResult
list_access(SignedRequest[ListAccessPayload]) -> ListAccessResult
```

### Delegation model

`DelegationStatement`

- `authority = "consumer"` in Day 0
- `mode` in `open | closed`
- `token_ttl` in seconds
- `rate_limit: str | None`
- `roles: list[str]`

Current validation:

- `token_ttl` must be `60..86400`
- roles must be non-empty
- rate limits must parse as `N/Nm` or `N/Nh`

## EnrollmentService

Methods:

```python
get_attestation(GetAttestationRequest) -> GetAttestationResult
enroll(SignedRequest[EnrollPayload]) -> EnrollResult
```

Current behavior:

- `get_attestation` returns the active signing-key attestation and optional
  previous attestation
- `enroll` issues delegated enrollment tokens for published services using the
  current `@aster` signing material
- token verification helpers exist in the service package
- ALPN/runtime admission-handler wiring is out of scope for this repository

## AuditService

Signed owner method:

```python
audit_log(SignedRequest[AuditLogPayload]) -> AuditLogResult
```

Current audit coverage includes:

- service published
- service updated
- service unpublished
- visibility changed
- delegation updated
- access granted
- access revoked
- enrollment issued

Identity-side audit coverage is still lighter than the publication and access
surface.

## Current Day 0 Storage Model

The implemented storage model is:

- leader-local authoritative ACID store
- currently SQLite in the production-shaped path
- in-memory mirror for tests and local-only flows

Not yet implemented in this repository:

- Iroh-backed replicated projection
- leader election
- authoritative write proxying
- automatic reconciliation between canonical state and projected state

## Explicit Gaps

These are still outside the implemented Day 0 service core:

- recovery code lifecycle
- framework-level peer/IP-aware rate limiting
- backup/export tooling
- runtime/CLI integration for delegated admission
