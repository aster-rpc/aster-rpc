# aster.site — Service Directory & Access Control Platform

**Status:** Concept / Pre-design  
**Date:** 2026-04-07 (revised)  
**Target:** Post-0.1 release  

---

## Executive Summary

aster.site is a hosted service directory and access control platform for Aster services. It's the **GitHub for distributed services** — publish, discover, and manage access to P2P services through a web UI and CLI, while all RPC traffic flows peer-to-peer.

**Core analogy:** Git is open source, GitHub is the platform. Aster protocol is open source (Apache 2.0), aster.site is the platform.

| Git/GitHub | Aster/aster.site |
|-----------|------------------|
| Git protocol is free | Aster RPC protocol is free (Apache 2.0) |
| Anyone can self-host a git repo | Anyone can run an Aster service |
| GitHub adds discovery, access control, collaboration | aster.site adds discovery, access control, enrollment |
| GitHub free for public, paid for private/teams | Same |
| Lock-in is convenience + network effects, not protocol | Same |

The protocol being open source **helps** — it grows the ecosystem that feeds the platform.

---

## How It Works

### The Key Mechanism: Delegated Enrollment

Today, the Aster trust model works like this:

```
Service owner → generates enrollment credentials → hands them to consumers manually
```

With aster.site as a **delegated enrollment issuer**:

```
Service owner → publishes to aster.site → grants access via web/CLI
                                              ↓
Consumer → finds service on aster.site → gets enrollment token from aster.site → connects P2P
```

The service's admission handler already verifies enrollment credentials. The integration adds: "also accept tokens signed by aster.site's key, if I've opted into aster.site publishing."

**Properties:**
- **No traffic flows through aster.site** — enrollment tokens are issued once, then it's P2P
- **Service owner can revoke at any time** — remove the delegation, go back to self-managed enrollment
- **aster.site never sees the RPC traffic** — it's the identity/access layer, not a proxy
- **Capital-efficient** — costs scale with directory size, not call volume

### Technical Integration

The service's consumer admission handler gains one new check:

```python
# Pseudocode: existing admission handler with aster.site delegation
async def admit_consumer(self, credential):
    # Existing checks (self-issued credentials)
    if self.verify_credential(credential):
        return admit(credential.roles)

    # NEW: if service is published to aster.site, also accept platform-issued tokens
    if self.aster_site_delegation_enabled:
        if verify_aster_site_token(credential, self.aster_site_pubkey):
            return admit(credential.roles)

    return reject()
```

### Roles Serialization

If a service contract defines roles (from the capability model in the trust spec), aster.site stores them. The web UI lets the owner assign roles to consumers. The enrollment token issued by aster.site includes the granted roles. The service's Gate 2 (capability check) just works — it sees the roles in the credential and enforces them as normal.

**Centralized access management UI for decentralized services.** That's the product.

---

## Data Model

### What's stored per published service

```
services/<contract_id>/
├── owner_pubkey          # root public key (operator identity)
├── contract.manifest     # the contract manifest (methods, types, version)
├── endpoints/            # live node IDs hosting this service
│   ├── <node_id_1>       # with health/TTL
│   └── <node_id_2>
├── roles/                # if service defines roles, serialized here
│   ├── admin
│   └── reader
├── access/               # who has access (for private services)
│   ├── <consumer_pubkey_1> → {roles: [admin], granted_by: owner}
│   └── <consumer_pubkey_2> → {roles: [reader], granted_by: owner}
└── visibility            # public | private
```

---

## User Journeys

### Publishing (Producer)

```bash
# One-time: link your root key to your aster.site account
aster auth link

# Publish (public by default, like a public GitHub repo)
aster publish myservice:TaskManager
# → uploads contract manifest + current node IDs to aster.site

# Make it private
aster publish myservice:TaskManager --private

# Grant access via CLI
aster access grant --service TaskManager --consumer <pubkey> --role admin

# Or via web UI at aster.site/myhandle/TaskManager/settings
```

### Discovery (Consumer)

```bash
# Search
aster discover TaskManager
# → shows: myhandle/TaskManager, 2 live endpoints, public

# Connect (public service — enrollment token auto-issued)
client = AsterClient.from_registry("myhandle/TaskManager")
result = await client.submit_task(...)

# For private services — request access, owner approves via web panel
aster access request --service myhandle/TaskManager
# Owner gets notification, grants via web panel or CLI
```

### Web UI

- Browse `aster.site/<handle>/` — see published contracts, methods, types, live endpoint count
- Service detail page — contract methods, type definitions, live endpoints, documentation
- Settings panel — visibility toggle, access grants, role assignments
- Notification center — access requests, endpoint health alerts

---

## What to Build First (Minimal Viable Platform)

Don't build the marketplace. Build the **GitHub for services** — just directory + access control:

1. **`aster publish`** — registers contract + endpoints in hosted directory
2. **Web UI** — browse services, view contracts, see live endpoints
3. **Public/private toggle** — default public
4. **Access grants** — web UI + CLI
5. **Enrollment token issuance** — aster.site issues tokens for published services

That's it. No billing, no marketplace fees, no analytics. Just make it easy to share a service with someone.

---

## Revenue Model

### Tier 1: Free (Grow the Network)
- Public services, unlimited
- Basic discovery and search
- Handle registration
- Community support

### Tier 2: Pro ($X/month per handle)
- Private services (visible only to authorized consumers)
- Team access management (org handles, team roles)
- Verified badge / priority listing
- Usage analytics dashboard
- Custom domains (myapi.example.com → aster.site resolution)

### Tier 3: Enterprise
- SSO / SAML integration for consumer enrollment
- Private registry instances (on-prem or hosted)
- Audit logging
- SLA guarantees on directory availability

### Future: Marketplace Cut
- Publishers set pricing on services
- aster.site handles billing, takes X% (target: 5-15%, undercutting RapidAPI's 20% and app stores' 15-30%)
- Consumers pay per-call or subscription
- Payment settled off-band; authorization tokens issued on-band via enrollment credentials
- **Don't build this yet.** Let the free directory prove the model first.

---

## Why This Is Defensible

### Network effects (the real moat)
Every published service makes the directory more valuable. Consumers go where the services are. Services publish where the consumers are. This flywheel is the moat, not code.

### Convenience lock-in
Self-hosting enrollment is possible but painful (like self-hosting git). The web UI for access control is sticky. Teams won't switch once their access policies are configured.

### Enrollment delegation = switching cost
Once a service owner relies on aster.site for enrollment, switching means migrating all their consumers' access grants. High switching cost without lock-in resentment (they can always self-host).

### Data advantage
You see which services are popular, which are growing, what the ecosystem looks like. This informs product decisions and proves the market to investors.

---

## Initial Wedge: AI Agent Services

AI agent-to-agent communication is the strongest initial market:

- **Agents need discovery:** "Find me a service that can summarize documents" — contract-based search
- **Agents need trust:** Capability-gated access prevents rogue agents from calling arbitrary services
- **Session-scoped services fit perfectly:** Agent conversations are stateful, sequential, cancellable — exactly what Aster sessions provide
- **No incumbent:** There's no standard for agent-to-agent RPC discovery. MCP is client-server, not peer-to-peer.

Example flow:
1. Agent A publishes `DocumentSummarizer` to aster.site
2. Agent B discovers it via `aster discover --tag ai.summarization`
3. Agent B gets enrollment token from aster.site (public service, auto-issued)
4. Agent B connects P2P, opens session, sends documents, gets summaries
5. aster.site never touches the traffic

---

## Competitive Landscape

| Platform | Model | How We Differ |
|----------|-------|---------------|
| **AWS API Gateway** | Centralized traffic proxy | We're a directory, not a proxy. P2P transport, no per-request cost |
| **Kong / Apigee** | API management proxy | Same — centralized proxies; we're identity + discovery |
| **RapidAPI** | API marketplace + proxy | Centralized, HTTP-only, high fees (20%). We're P2P, protocol-agnostic, lower fees |
| **gRPC Server Reflection** | Point-to-point schema discovery | No directory, no marketplace, no access control |
| **npm / PyPI** | Package registry | Packages, not live services; no runtime discovery |
| **Docker Hub** | Container registry | Containers, not service contracts; no direct invocation |
| **GitHub** | Code hosting + collaboration | Closest analogy — we're this but for live services, not source code |

**Unique position:** Content-addressed contracts (hash IS the API) + P2P transport (NAT traversal built in) + built-in trust model (three-gate) + hosted access control. No other platform combines all four.

---

## Implementation Roadmap

### Prerequisites (Part of 0.1 Release)
- [ ] RPC wire protocol (ASTER_PLAN Phases 1-3)
- [ ] Service definition decorators (Phase 4)
- [ ] Server + client stubs (Phases 5-6)
- [ ] Interceptors (Phase 7)
- [ ] Trust model (enrollment, admission, capabilities)

### Phase A: Publish + Directory (MVP)
- [ ] `aster publish` CLI command
- [ ] Contract manifest upload to hosted directory
- [ ] Endpoint registration (node IDs + health/TTL)
- [ ] Handle registration (account system)
- [ ] Public/private visibility toggle
- [ ] Web UI: browse and search published services

### Phase B: Access Control
- [ ] `aster access grant/revoke` CLI
- [ ] Web UI: access management panel
- [ ] Role serialization from contract → directory
- [ ] Enrollment token issuance (aster.site signs tokens on behalf of owner)
- [ ] Admission handler integration (accept aster.site-issued tokens)

### Phase C: Consumer Experience
- [ ] `aster discover` / `aster search` CLI
- [ ] `AsterClient.from_registry()` — programmatic resolution
- [ ] Contract detail pages (methods, types, live endpoints, docs)
- [ ] `aster access request` — consumer-initiated access flow

### Phase D: Pro Tier
- [ ] Team/org handles
- [ ] Usage analytics dashboard
- [ ] Custom domains
- [ ] Billing integration (Stripe)

### Phase E: Enterprise
- [ ] SSO / SAML for consumer enrollment
- [ ] Private registry instances
- [ ] Audit logging
- [ ] On-prem deployment option

### Phase F: Marketplace (Future)
- [ ] Publisher pricing controls
- [ ] Consumer payment flow
- [ ] Usage metering + settlement
- [ ] Revenue share model

---

## Open Questions

- **Namespace governance:** How to handle disputes, squatting, trademark claims on handles?
- **Hybrid architecture:** Hosted authoritative registry vs. P2P-syncable via Iroh docs? Likely hybrid: aster.site is authoritative for handle ownership, but contract data is syncable for resilience/caching.
- **Offline-first:** Can users discover and cache contracts locally for air-gapped environments?
- **Schema evolution:** How to surface compatibility reports between contract versions in the UI?
- **Delegation revocation:** When an owner removes aster.site delegation, how are already-issued tokens handled? TTL-based expiry? Active revocation list?
- **Multi-region:** How to distribute the hosted registry globally for low-latency discovery?
- **Legal:** Terms of service for published services. Liability model. Takedown process.

---

## References

- [Aster-ContractIdentity.md](../../ffi_spec/Aster-ContractIdentity.md) — Contract hashing, canonical encoding, publication procedures
- [Aster-trust-spec.md](../../ffi_spec/Aster-trust-spec.md) — Three-gate trust model
- [Aster-session-scoped-services.md](../../ffi_spec/Aster-session-scoped-services.md) — Stateful session model
- [Aster-SPEC.md](../../ffi_spec/Aster-SPEC.md) — Full RPC specification
- [ASTER_PLAN.md](../../ffi_spec/ASTER_PLAN.md) — Python implementation phases
