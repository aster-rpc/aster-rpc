# Aster CLI Identity / Publish Checklist

**Status:** Active working checklist  
**Last updated:** 2026-04-08  
**Scope:** CLI and shell work for identity, join, publish, and directory UX

This checklist is intentionally implementation-oriented. It tracks what is
already landed in the CLI versus what still depends on the `@aster` service,
framework, and follow-up CLI integration.

## Completed

### Foundation

- [x] Added shared config/profile helpers for active profile access and mutation
- [x] Extended local config handling for profile-level handle state:
  - [x] `handle`
  - [x] `handle_status`
  - [x] `handle_claimed_at`
  - [x] `email`
  - [x] `published_services`
- [x] Extended local config handling for `[aster_service]` defaults:
  - [x] `enabled`
  - [x] `node_id`
  - [x] `relay`
  - [x] `offline_banner`
- [x] Added client-side handle validation module
- [x] Added reserved handle enforcement
- [x] Added pattern checks:
  - [x] lowercase only
  - [x] length 3-39
  - [x] no consecutive hyphens
  - [x] must start/end alphanumeric
  - [x] no purely numeric handles
  - [x] reserved `aster-` / `admin-` prefixes

### CLI Commands

- [x] Registered top-level `aster status`
- [x] Registered top-level `aster whoami`
- [x] Registered top-level `aster join`
- [x] Registered top-level `aster verify`
- [x] Registered top-level `aster publish`
- [x] Registered top-level `aster unpublish`
- [x] Implemented local-only `status` / `whoami`
- [x] Implemented preview `join --demo`
- [x] Implemented preview `verify --demo`
- [x] Implemented preview `verify --resend --demo`
- [x] Implemented preview `publish`
- [x] Implemented preview `unpublish`

### Shell Integration

- [x] Wired shell commands:
  - [x] `join`
  - [x] `verify`
  - [x] `whoami`
  - [x] `status`
  - [x] `publish`
  - [x] `unpublish`
- [x] Added `--air-gapped` to `aster shell`
- [x] Added shell startup identity banner for local states
- [x] Added air-gapped banner treatment at shell startup

### Directory Demo / UX

- [x] Reused documented shell plugin path instead of creating parallel command plumbing
- [x] Extended `--demo2` directory mode to derive current user identity from local config
- [x] Merged local manifest-backed services into `/aster/<current-user>` in directory demo
- [x] Reflected local published/unpublished preview state in directory demo service entries

### Tests / Verification

- [x] Added test coverage for handle validation
- [x] Updated shell command registration expectations
- [x] Kept shell test suite green after wiring new commands
- [x] Verified `uv run pytest tests/python/test_shell.py -v`
- [x] Smoke-tested:
  - [x] `aster status`
  - [x] `aster join --demo`
  - [x] `aster verify --demo`

## Todo

### CLI: Real Join / Verify Networking

- [ ] Replace preview-only `join --demo` flow with real `@aster` RPC client flow
- [ ] Implement registry/service client resolver for `@aster`
- [ ] Call `check_availability` before handle claim
- [ ] Handle “taken” / “reserved” / “invalid” responses with correct CLI UX
- [ ] Sign `join` payloads with the root key
- [ ] Call real `join`
- [ ] Persist pending state from real server response
- [ ] Sign `verify` payloads with the root key
- [ ] Call real `verify`
- [ ] Call real `resend_verification`
- [ ] Surface server error cases:
  - [ ] invalid code
  - [ ] code expired
  - [ ] attempts remaining
  - [ ] invalid handle
  - [ ] service unavailable / timeout

### CLI: Root-Key and Signing UX

- [ ] Reuse canonical signed-request encoding expected by the `@aster` service
- [ ] Add nonce generation for signed requests
- [ ] Add timestamp generation for signed requests
- [ ] Load root private key from keyring/file in one shared helper for identity commands
- [ ] Make `aster join` auto-create the root key in the final non-demo flow
- [ ] Decide whether `aster join` should ever require an explicit `--demo`, or infer preview mode when the service client is absent

### CLI: Status / Whoami

- [ ] Add non-blocking `handle_status` call on shell startup
- [ ] Cache `handle_status` locally with TTL
- [ ] Show online/offline indicator from real `@aster` reachability
- [ ] Distinguish local-only state from confirmed remote state in output
- [ ] Optionally add `--json` structured output for the new identity commands

### Shell UX

- [ ] Refresh shell banner immediately after `join`
- [ ] Refresh shell banner immediately after `verify`
- [ ] Refresh shell banner after `publish` / `unpublish`
- [ ] Add `discover` command in the shell
- [ ] Add top-level CLI `aster discover`
- [ ] Add richer shell banners for all documented states:
  - [ ] State A: no root key
  - [ ] State B: root key, unregistered
  - [ ] State C: pending verification
  - [ ] State D: verified
  - [ ] State E: offline
  - [ ] State F: air-gapped
- [ ] Decide whether preview/demo commands should print via shell display rather than plain stdout

### Publish: Real Integration

- [ ] Replace local preview publish marker flow with real `publish` RPC
- [ ] Reuse `aster contract gen` pipeline end-to-end for publish requests
- [ ] Build signed publish payload
- [ ] Post manifest to `@aster`
- [ ] Store returned `delegation_pubkey`
- [ ] Implement real `unpublish` RPC
- [ ] Show first-publish recovery code guidance if that behavior remains in scope
- [ ] Add real shell refresh of `/aster/<handle>` after publish/unpublish

### Access Control Commands

- [ ] Add top-level CLI `aster access grant`
- [ ] Add top-level CLI `aster access revoke`
- [ ] Add top-level CLI `aster access list`
- [ ] Add top-level CLI `aster access public`
- [ ] Add top-level CLI `aster access private`
- [ ] Wire shell commands:
  - [ ] `grant`
  - [ ] `revoke`
  - [ ] `access`
- [ ] Integrate those commands with real `@aster` RPC methods

### VFS / Directory

- [ ] Build real `/aster/<handle>` VFS integration backed by registry responses
- [ ] Merge local + published services correctly outside demo mode
- [ ] Show `● published` / `⬡ local` consistently in listings
- [ ] Resolve and browse other handles from real directory data
- [ ] Cache discovered handles/services with TTL
- [ ] Make `refresh` invalidate and repopulate real `@aster` cache data

### Contract / Resolution

- [ ] Extend contract/client generation to accept `@handle/ServiceName`
- [ ] Implement handle-based resolution for `aster shell`
- [ ] Implement handle-based resolution for `aster call` if/when that command is added
- [ ] Support manifest fetch from `@aster`

### Cleanup / Refactor

- [ ] Decide whether preview-only behavior should stay in `join.py` / `publish.py` or move to a dedicated preview module
- [ ] Avoid shell importing command implementations that print directly to stdout
- [ ] Consolidate identity-state formatting into shared helpers used by both CLI and shell
- [ ] Add targeted tests for new config serialization
- [ ] Add targeted tests for `join.py` and `publish.py`

## Cross-Agent Dependencies

- [ ] `@aster` service methods available and stable:
  - [ ] `check_availability`
  - [ ] `join`
  - [ ] `verify`
  - [ ] `resend_verification`
  - [ ] `handle_status`
  - [ ] `publish`
  - [ ] `unpublish`
  - [ ] `discover`
  - [ ] `grant_access`
  - [ ] `revoke_access`
  - [ ] `list_access`
- [ ] Framework support available and stable:
  - [ ] signed request helpers
  - [ ] delegated enrollment / `delegation_pubkey`
  - [ ] admission-handler integration for `@aster`-signed tokens
  - [ ] handle-based connect / resolution path if owned by framework

## Notes

- Current `join` / `verify` implementation is deliberately preview-only unless `--demo` is used.
- Current `publish` / `unpublish` implementation updates local preview state only.
- Current shell banner work is local-state driven; it does not yet talk to `@aster`.
