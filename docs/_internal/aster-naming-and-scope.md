# Aster Naming & Day 0 Scope

**Date:** 2026-04-08

---

## Naming

| Name | What it is |
|------|-----------|
| **aster-rpc** | The open-source framework (Apache 2.0). Transport, codec, decorators, trust model, CLI tooling. This repo. |
| **@aster** | The hosted service we run. Identity, directory, access control, enrollment delegation. An aster-rpc service itself. |

The framework is the adoption engine. `@aster` is the service layer on top.

## Day 0 Scope (Build This Week)

The complete loop: identity → publish → discover → connect → govern → expose to agents.

| Command | What it does |
|---------|-------------|
| `aster join` | Create identity + claim handle on `@aster` (auto-creates root key if missing) |
| `aster verify` | Confirm email with 6-digit code |
| `aster publish` | Publish service to `@aster` directory (manifest + endpoints + roles) |
| `aster discover` | Search `@aster` for services |
| `aster access grant/revoke/list` | Control who can use your service, by handle + role |
| `aster call @handle/Service.method '{}'` | Call a published service method (curl-equivalent for Aster) |
| `aster contract gen @handle/Service` | Generate typed client from published contract |
| `AsterClient.connect("@handle/Service")` | Resolve + auto-enroll + connect P2P |
| `aster mcp --discover` | Expose published services as MCP tools |
| `aster whoami` / `aster status` | Show identity state |
| `aster shell --air-gapped` | Work without `@aster` (relays still work for P2P) |

## Join Rules

- **No join needed** to use aster-rpc locally, connect by node ID, or run services P2P.
- **Join needed** to publish, to get handle-based access to private services, to appear in the directory.
- **Join mandatory for publishers**, optional for consumers of public services.

## Relay Note

If `@aster` gets popular, we host our own Iroh relays. Relayed (non-direct) connections may have bandwidth limits. Direct P2P connections (hole-punched) are unlimited — relays are a fallback, not the primary path. This is a future operational concern, not a Day 0 build item.

## What's NOT Day 0

- Monetization, billing, pricing tiers
- Private registries / self-hosted `@aster`
- Team/org handles
- SSO/SAML
- Analytics dashboard
- Endpoint heartbeat
- Recovery & key rotation
- Marketplace / paid services
- Version channels/tags
