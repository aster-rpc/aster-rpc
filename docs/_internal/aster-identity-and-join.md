# @aster Identity and Join

**Status:** Implemented Day 0 core  
**Date:** 2026-04-08  
**Companion:** [aster-service-api.md](/Users/emrul/dev/aster/aster-app/docs/aster-service-api.md)

## Terminology

- `@aster` is the producer application in this repository.
- The replicated Aster producer registry is a separate runtime concern and is
  not the same thing as `@aster`.
- In these docs, "directory" means `@aster`'s handle and published-service
  directory, not the underlying runtime registry.

## Identity Model

`@aster` binds a verified handle to a signing public key.

Day 0 identity state:

- local root/signing key material exists outside `@aster`
- `@aster` stores handle claim and verification state
- email is used for verification and recovery-related UX, not as the primary
  identity
- a verified handle is the human-facing identity used across publish, access,
  and enrollment flows

The implemented handle states are:

- `unregistered`
- `pending`
- `verified`

## Join Flow

The implemented identity loop is:

1. `check_availability`
2. `join`
3. `verify`
4. `resend_verification`
5. `handle_status`

All mutating identity methods use signed requests with:

- `action`
- `timestamp`
- `nonce`

Day 0 request protections:

- signature verification against the caller's signing key
- timestamp skew check
- nonce deduplication
- handle and email validation

## Handle Rules

The current validation rules are:

| Rule | Value |
|------|-------|
| Length | 3-39 chars |
| Characters | lowercase `a-z`, digits `0-9`, hyphen `-` |
| Start/end | alphanumeric |
| No consecutive hyphens | enforced |
| Reserved handles | `admin`, `aster`, `support`, `help`, `root`, `api`, `www` |

## Email Verification

Day 0 verification behavior:

| Property | Value |
|----------|-------|
| Code format | 6 digits |
| Code TTL | 15 minutes |
| Verification window | 24 hours |
| Attempts per code | 5 |
| Resend cooldown | 60 seconds |
| Max resends | 5 |
| Code hashing | Argon2id |

Join rejects:

- invalid handle
- invalid email
- taken handle
- pending verification collision for the same email
- verified email already in use

## Profile Metadata

Day 0 now includes lightweight public profile metadata.

Signed owner update:

```python
update_profile(SignedRequest[UpdateProfilePayload]) -> UpdateProfileResult
```

Public read:

```python
get_profile(GetProfileRequest) -> PublicProfileResult
```

Stored profile fields:

- `display_name` up to 64 chars
- `bio` up to 500 chars
- `url` as a single `http://` or `https://` link

`handle_status` also returns:

- `display_name`
- `bio`
- `url`

`get_profile` intentionally omits email and returns only:

- handle
- display name
- bio
- url
- services published
- registered at

## Rate Limiting

Current Day 0 service-side rate limiting:

- `check_availability`: `30/1m`
- `join`: `3/1h` per signing pubkey and `3/1h` per email hash
- `verify`: `10/5m`
- `resend_verification`: `5/1h`

This is implemented in application code today. Peer/IP-aware framework-backed
limits are still future work.

## Day 0 Storage Shape

Current identity state is stored in the authoritative local store used by
`@aster`:

- pending and verified handles
- verification-code state
- public profile metadata
- nonce deduplication state

The current production-shaped implementation uses SQLite as the authoritative
local store. The in-memory store mirrors the same behavior for tests and local
development.

## Explicit Day 0 Non-Goals

These identity features are not implemented in the current service package:

- recovery-code issuance and recovery wizard
- automatic leader election
- replicated Iroh projection of identity state
- framework admission-handler wiring

Those remain documented elsewhere as follow-on work, not part of the current
`@aster` core identity loop.
