# Aster CLI Identity / Publish Checklist

**Status:** Active working checklist  
**Last updated:** 2026-04-09  
**Scope:** CLI and shell work for identity, join, publish, and directory UX

This checklist is intentionally implementation-oriented. It tracks what is
already landed in the CLI versus what still depends on the `@aster` service,
framework, and follow-up CLI integration.

## Completed

### Foundation

- [x] Added shared config/profile helpers for active profile access and mutation
- [x] Added shared `@aster` CLI helper layer for:
  - [x] service address resolution
  - [x] root key loading
  - [x] runtime typed-client generation from live manifests
  - [x] signed request envelope construction
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
- [x] Registered top-level `aster discover`
- [x] Registered top-level `aster visibility`
- [x] Registered top-level `aster update-service`
- [x] Registered top-level `aster access grant`
- [x] Registered top-level `aster access revoke`
- [x] Registered top-level `aster access list`
- [x] Registered top-level `aster access delegation`
- [x] Implemented local-only `status` / `whoami`
- [x] Implemented preview `join --demo`
- [x] Implemented preview `verify --demo`
- [x] Implemented preview `verify --resend --demo`
- [x] Switched `aster join` to real networked flow by default
- [x] Switched `aster verify` to real networked flow by default
- [x] Switched `aster publish` to real networked flow by default
- [x] Switched `aster unpublish` to real networked flow by default
- [x] Added real networked `aster discover`
- [x] Kept explicit preview/demo mode for `join`, `verify`, `publish`, and `unpublish`
- [x] Added Day 0 publish flags for description, visibility, delegation mode, token TTL, rate limit, endpoint TTL, and endpoint identity override

### Shell Integration

- [x] Wired shell commands:
  - [x] `join`
  - [x] `verify`
  - [x] `whoami`
  - [x] `status`
  - [x] `publish`
  - [x] `unpublish`
  - [x] `discover`
  - [x] `access`
  - [x] `grant`
  - [x] `revoke`
  - [x] `visibility`
  - [x] `update-service`
  - [x] `delegation`
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
- [x] Added targeted CLI tests for:
  - [x] signed envelope construction
  - [x] service address resolution
  - [x] real command dispatch without `--demo`
  - [x] publish argument validation
- [x] Updated shell command registration expectations
- [x] Kept shell test suite green after wiring new commands
- [x] Verified `uv run pytest tests/python/test_aster_cli_day0.py -v`
- [x] Verified `uv run pytest tests/python/test_shell.py -v`
- [x] Smoke-tested:
  - [x] `aster status`
  - [x] `aster join --demo`
  - [x] `aster verify --demo`
- [x] Smoke-tested generated client `PublicationService.discover(...)` against live Day 0 `@aster`
- [x] Smoke-tested live CLI flow:
  - [x] `aster join` against a dev `@aster` node
  - [x] `aster status` against a dev `@aster` node
  - [x] `aster discover` against a dev `@aster` node
  - [x] `aster publish` against a dev `@aster` node
  - [x] `aster update-service` against a dev `@aster` node
  - [x] `aster visibility` against a dev `@aster` node
  - [x] `aster access delegation` against a dev `@aster` node
  - [x] `aster access list` against a dev `@aster` node
  - [x] `aster access grant` against a dev `@aster` node
  - [x] `aster access revoke` against a dev `@aster` node

## Todo

### CLI: Real Join / Verify Networking

- [x] Replace preview-only `join --demo` flow with real `@aster` RPC client flow
- [x] Implement registry/service client resolver for `@aster`
- [x] Call `check_availability` before handle claim
- [x] Handle “taken” / “reserved” / “invalid” responses with correct CLI UX
- [x] Sign `join` payloads with the root key
- [x] Call real `join`
- [x] Persist pending state from real server response
- [x] Sign `verify` payloads with the root key
- [x] Call real `verify`
- [x] Call real `resend_verification`
- [ ] Surface server error cases:
  - [ ] invalid code
  - [ ] code expired
  - [ ] attempts remaining
  - [ ] invalid handle
  - [ ] service unavailable / timeout

### CLI: Root-Key and Signing UX

- [x] Reuse canonical signed-request encoding expected by the `@aster` service
- [x] Add nonce generation for signed requests
- [x] Add timestamp generation for signed requests
- [x] Load root private key from keyring/file in one shared helper for identity commands
- [x] Make `aster join` auto-create the root key in the final non-demo flow
- [x] Decide whether `aster join` should ever require an explicit `--demo`, or infer preview mode when the service client is absent

### CLI: Status / Whoami

- [ ] Add non-blocking `handle_status` call on shell startup
- [ ] Cache `handle_status` locally with TTL
- [x] Show online/offline indicator from real `@aster` reachability
- [x] Distinguish local-only state from confirmed remote state in output
- [x] Optionally add `--json` structured output for the new identity commands

### Shell UX

- [x] Refresh shell banner/VFS state after `join`
- [x] Refresh shell banner/VFS state after `verify`
- [x] Refresh shell banner/VFS state after `publish` / `unpublish`
- [x] Add `discover` command in the shell
- [x] Add top-level CLI `aster discover`
- [x] Add shell commands for access grant/list/revoke and owner mutations
- [x] Add richer shell banners for all documented states: (these could even look like badges or on/off switches as similar to 'mode' in the producer server banner)
  - [x] State A: no root key
  - [x] State B: root key, unregistered
  - [x] State C: pending verification
  - [x] State D: verified
  - [x] State E: offline
  - [x] State F: air-gapped
- [ ] Decide whether preview/demo commands should print via shell display rather than plain stdout

### Publish: Real Integration

- [x] Replace local preview publish marker flow with real `publish` RPC
- [x] Reuse `aster contract gen` pipeline end-to-end for publish requests
- [x] Build signed publish payload
- [x] Post manifest to `@aster`
- [x] Store returned producer token locally for producer startup
- [x] Implement real `set_visibility` RPC
- [x] Implement real `update_service` RPC
- [ ] Store returned `delegation_pubkey`
- [x] Implement real `unpublish` RPC
- [x] Remove stored producer token on unpublish
- [ ] Show first-publish recovery code guidance if that behavior remains in scope
- [ ] Add real shell refresh of `/aster/<handle>` after publish/unpublish

### Access Control Commands

- [x] Add top-level CLI `aster access grant`
- [x] Add top-level CLI `aster access revoke`
- [x] Add top-level CLI `aster access list`
- [x] Add top-level CLI `aster access delegation`
- [x] Add top-level CLI `aster access public`
- [x] Add top-level CLI `aster access private`
- [ ] Wire shell commands:
  - [x] `grant`
  - [x] `revoke`
  - [x] `access`
  - [x] `public`
  - [x] `private`
- [x] Integrate those commands with real `@aster` RPC methods

### VFS / Directory

- [x] Build real `/aster/<handle>` VFS integration backed by directory responses
- [x] Merge local + published services correctly outside demo mode for `@currentuser`
- [x] Show `● published` / `⬡ local` consistently in listings
- [x] Resolve and browse other handles from real directory data
- [x] Cache discovered handles/services for the active directory session
- [x] Make `refresh` invalidate and repopulate real `@aster` cache data

### Contract / Resolution

- [x] Extend contract/client generation to accept `@handle/ServiceName`
- [ ] Implement handle-based resolution for `aster shell`
- [ ] Implement handle-based resolution for `aster call` if/when that command is added
- [x] Support manifest fetch from `@aster`

### Cleanup / Refactor

- [ ] Decide whether preview-only behavior should stay in `join.py` / `publish.py` or move to a dedicated preview module
- [ ] Avoid shell importing command implementations that print directly to stdout
- [ ] Consolidate identity-state formatting into shared helpers used by both CLI and shell
- [ ] Add targeted tests for new config serialization
- [ ] Add targeted tests for `join.py` and `publish.py`

## Cross-Agent Dependencies

- [ ] `@aster` service methods available and stable:
  - [x] `check_availability`
  - [x] `join`
  - [x] `verify`
  - [x] `resend_verification`
  - [x] `handle_status`
  - [x] `publish`
  - [x] `set_visibility`
  - [x] `update_service`
  - [x] `unpublish`
  - [x] `discover`
  - [x] `grant_access`
  - [x] `revoke_access`
  - [x] `list_access`
  - [x] `update_delegation`
- [ ] Framework support available and stable:
  - [x] signed request helpers
  - [ ] delegated enrollment / `delegation_pubkey`
  - [ ] admission-handler integration for `@aster`-signed tokens
  - [ ] handle-based connect / resolution path if owned by framework

## Notes

- Current `join` / `verify` / `publish` / `unpublish` implementations now target the live `@aster` service by default and keep `--demo` as an explicit fallback.
- `status` now opportunistically syncs against remote `handle_status` and updates local state.
- `discover` now talks to the live `PublicationService`.
- `publish` now derives the Day 0 directory `contract_id` from canonical manifest JSON, matching the live service validator.
- `grant_access` now includes `scope_node_id: null` in signed payloads so signature verification matches server-side dataclass canonicalization.
- Current shell banner work is local-state driven; it does not yet talk to `@aster`.
- `aster shell <peer> --demo2` now uses the live `list_directory_handles` endpoint when a peer is provided, and falls back to the old offline directory demo only when no peer is passed.
- Direct `cd /aster/@handle` now works even if that handle is not in the current root listing.
