# Aster Publish — Service Directory, Access Control & Discovery

**Status:** Design  
**Date:** 2026-04-08  
**Companion:** [aster-identity-and-join.md](aster-identity-and-join.md)  
**Principle:** Provide immense value instantly. Be easy to build.

---

## Why Publish Exists

Without `@aster`, Aster services already work P2P. You can share a node ID, the consumer connects, fetches the contract, calls methods. So why publish?

**Because P2P alone has three painful problems:**

### Problem 1: Access control is a manual credential ceremony

Today, if you want @alice to use your service with `reader` role:

```bash
# You: generate a credential for alice's specific endpoint ID
aster authorize --root-key ./root.key --producer-id <alice_endpoint_id> --attributes aster.role=reader
# Hand the credential to alice on Slack
# Alice: connect with the credential
# You want to revoke? Hope you tracked which credential belongs to whom
# Alice gets a new laptop? New endpoint ID. Repeat everything.
```

This is manageable for 2 consumers. Unmanageable for 20. Impossible for 200.

**With publish + `@aster` identity:**

```bash
# Grant access by who they are, not which machine they're on
aster access grant @alice --service TaskManager --role reader

# Alice connects — @aster vouches for her identity
client = await AsterClient.connect("@emrul/TaskManager")  # just works

# Revoke — instant, by handle
aster access revoke @alice --service TaskManager
```

`@aster` is the identity layer. You say "let @alice in with reader role." `@aster` issues her an enrollment token. Your service's admission handler verifies it. Alice's laptop, node ID, IP address — none of it matters. Her verified handle is her identity.

### Problem 2: Discoverability is word-of-mouth

Without publish, you find services by someone giving you a node ID. There's no directory, no search, no browsing. You have to already know someone runs a service.

```bash
# With publish: discover by name
$ aster discover TaskManager
  @emrul/TaskManager   v3  3 methods  1 endpoint  public
  @alice/TaskRunner     v1  2 methods  3 endpoints public

# Connect by handle — not by hex string
client = await AsterClient.connect("@emrul/TaskManager")
```

### Problem 3: Your service is invisible to AI agents

The Aster CLI has a working MCP bridge (`aster mcp`). It connects to a peer, fetches the contract, and exposes every method as an MCP tool. Claude, Cursor, VS Code copilots — any MCP consumer can call your service.

But today, the MCP bridge requires a node ID. The agent has to *already know* where your service is.

**Publishing to `@aster` makes your service instantly available to every MCP consumer in the ecosystem:**

```bash
# Agent asks: "find a service that can summarize documents"
# @aster search → @alice/DocumentSummarizer
# MCP bridge connects → 2 tools available:
#   DocumentSummarizer:summarize
#   DocumentSummarizer:streamSummary
# Agent calls the tool. P2P. No proxy.
```

Your Aster service becomes an AI tool the moment you publish. No MCP server setup, no tool registration, no adapter code. The MCP bridge reads the contract from `@aster` and generates tool definitions automatically.

---

## The Three Pillars (What Publish Actually Gives You)

| # | Pillar | Without publish | With publish |
|---|--------|----------------|--------------|
| 1 | **Access control** | Manual credential per endpoint ID, out-of-band handoff, no revocation | `aster access grant @alice --role reader`. Revoke by handle. `@aster` issues enrollment tokens. |
| 2 | **Discoverability** | Share node IDs on Slack | `aster discover`, connect by handle, browse in shell, contract gen from `@handle/Service` |
| 3 | **MCP / Agent availability** | Requires node ID, manual `aster mcp <peer>` | Published services auto-discoverable by MCP consumers. Any AI agent can find and call your service. |

Plus the things that are nice but not the core value:
- Human-readable address (`@emrul/TaskManager`)
- Contract available even when your node is offline (cached on `@aster`)
- Typed client gen from published contracts (`aster contract gen @emrul/TaskManager`)
- Stable CI reference (gen clients in CI from `@handle/Service` instead of local paths)

---

## How Delegated Access Control Works

This is the mechanism that makes Pillar 1 work. It builds on the existing three-gate trust model (Aster-trust-spec.md).

### Today's Trust Flow

```
Service owner ──signs──► EnrollmentCredential(endpoint_id=alice_node, role=reader)
                              │
                              │  handed to alice out-of-band
                              ▼
Alice's node ──presents──► Service admission handler ──verifies──► admitted with reader role
```

The credential is bound to alice's *endpoint ID*. If she changes machines, new credential needed.

### With `@aster` Delegation

```
Service owner ──publishes──► @aster (stores manifest + access grants)
                              │
Owner: aster access grant @alice --role reader
                              │
                              ▼
                         @aster stores: @alice → reader on @emrul/TaskManager
                              │
Alice connects to @emrul/TaskManager:
  1. Alice's client calls @aster: "I'm @alice, I want to access @emrul/TaskManager"
  2. @aster checks: does @alice have access? Yes, role=reader.
  3. @aster issues: EnrollmentToken(consumer=@alice, service=TaskManager, role=reader, ttl=2h)
     Signed by @aster's key (not the service owner's key)
  4. Alice presents the token to the service
  5. Service admission handler: "I trust @aster as a delegated enrollment issuer"
     → verifies @aster's signature → admits alice with reader role
```

### What Changes in the Framework

The service's admission handler gains **one new check** — it already exists in concept (aster-site-marketplace.md §2), now it's Day 0:

```python
async def admit_consumer(self, credential):
    # Existing: verify self-issued credentials
    if self.verify_credential(credential):
        return admit(credential.roles)

    # NEW: if service is published, also accept @aster-issued tokens
    if self.aster_delegation_enabled:
        if verify_aster_token(credential, self.aster_delegation_pubkey):
            return admit(credential.roles)

    return reject()
```

The `aster_delegation_pubkey` is returned when you publish. It's `@aster`'s signing key for enrollment tokens. The service owner opts into this by publishing — publishing is the act of saying "I trust `@aster` to vouch for consumers on my behalf."

### Access Control Commands

```bash
# Grant access (requires verified handle — both publisher and consumer)
aster access grant @alice --service TaskManager --role reader
aster access grant @bob --service TaskManager --role admin

# List who has access
aster access list --service TaskManager
  @alice   reader   granted 2026-04-08
  @bob     admin    granted 2026-04-08

# Revoke
aster access revoke @alice --service TaskManager

# Grant to everyone (public — no enrollment needed)
aster access public --service TaskManager

# Restrict to granted handles only (private — default after first grant)
aster access private --service TaskManager
```

### Token TTL and Revocation

- Tokens issued by `@aster` have a configurable TTL (default: 2 hours)
- When you revoke access, `@aster` stops issuing new tokens immediately
- Already-issued tokens remain valid until TTL expiry
- For tighter revocation, reduce the TTL: `aster publish TaskManager --token-ttl 15m`
- Services can optionally subscribe to a revocation gossip topic for instant invalidation (post Day 0)

### What the Enterprise Sees

This is IAM for distributed services:
- **Who** has access to **what**, with **which roles**, granted **when**, by **whom**
- Audit trail on `@aster` (who was granted, who was revoked, when)
- Centralized access management for decentralized services
- SSO/SAML integration point (future: `@aster` trusts your IdP, your IdP users get Aster handles)

---

## MCP: Publish Makes Your Service an AI Tool

### How It Works Today

```bash
# Manual: point the MCP bridge at a specific peer
aster mcp <node_id> --rcan credential.token
# → exposes that peer's services as MCP tools
```

This works, but requires knowing the node ID and having a credential.

### How It Works After Publish

```bash
# Connect MCP bridge to @aster directory
aster mcp --discover TaskManager
# → finds @emrul/TaskManager, gets enrollment token, connects, exposes tools

# Or: expose all public services matching a pattern
aster mcp --discover "Document*"
# → finds @alice/DocumentSummarizer, exposes as tools
```

For the consumer's Claude Desktop / Cursor config:

```json
{
  "mcpServers": {
    "aster": {
      "command": "aster",
      "args": ["mcp", "--discover", "@emrul/TaskManager"]
    }
  }
}
```

The AI agent gets tools named `TaskManager:submitTask`, `TaskManager:watchProgress`, etc. All methods from the contract, with JSON Schema inputs generated from the wire types. The agent can call them directly.

### Why This Matters

- **No MCP server to build.** Your Aster service is already an MCP tool. Publishing just makes it discoverable.
- **Schema is automatic.** The MCP bridge reads the contract and generates JSON Schema tool definitions. No hand-written tool descriptions.
- **Access control works.** The MCP bridge gets an enrollment token from `@aster` with the appropriate roles. The service sees a credentialed consumer, not a random connection.
- **Any agent, any framework.** MCP is supported by Claude, Cursor, Windsurf, VS Code, and growing. One publish = available to all of them.

### The Agent-to-Agent Future

When agents run their own Aster services (not just consume them), publishing creates a machine-discoverable service mesh:

1. Agent A publishes `DocumentSummarizer` to `@aster`
2. Agent B needs summarization → `aster discover --tag ai.summarization`
3. Agent B gets enrollment token from `@aster`, connects P2P
4. Agent B calls `DocumentSummarizer:summarize` via MCP
5. `@aster` never sees the document content — just brokered the connection

This is the "GitHub for services" vision from the marketplace doc, but the initial wedge is AI agents.

---

## Day 0: What We Build

### The core loop

```
Publisher                           @aster                          Consumer / Agent
   │                                  │                                │
   │  aster publish TaskManager       │                                │
   │ ─────────────────────────────► │                                │
   │  (manifest + endpoints + sig)    │                                │
   │                                  │                                │
   │  aster access grant @alice       │                                │
   │ ─────────────────────────────► │                                │
   │                                  │                                │
   │                                  │   aster discover TaskManager   │
   │                                  │ ◄──────────────────────────── │
   │                                  │   → @emrul/TaskManager         │
   │                                  │                                │
   │                                  │   enroll @alice for TM         │
   │                                  │ ◄──────────────────────────── │
   │                                  │   → enrollment token           │
   │                                  │     (role=reader, ttl=2h)      │
   │                                  │                                │
   │  ◄──────── P2P RPC (with token) ──────────────────────────────── │
   │  admission: verify @aster sig → admit with reader role            │
```

### What gets uploaded

When you run `aster publish TaskManager`, the CLI:

1. **Finds the service definition** — imports the decorated class, same as `aster contract gen`
2. **Generates the manifest** — methods, types, fields, contract ID (BLAKE3 hash), version, roles
3. **Signs the publish request** — root key signature over manifest + handle + timestamp
4. **Sends to `@aster`** — one RPC call: manifest + node endpoints

What `@aster` stores:

```
handles/emrul/services/TaskManager/
├── manifest.json          # contract manifest (methods, types, version, contract_id, roles)
├── version                # integer version
├── contract_id            # BLAKE3 hash (content-addressed identity)
├── roles/                 # roles defined in the contract
│   ├── admin              # from @service decorator or capability definitions
│   └── reader
├── access/                # who has access
│   ├── @alice → reader
│   └── @bob → admin
├── endpoints/             # live node IDs hosting this service
│   └── <node_id>          # with relay URL + TTL
├── visibility             # "public" | "private"
├── token_ttl              # default 2h
└── published_at           # timestamp
```

`@aster` returns a `delegation_pubkey` — the key it uses to sign enrollment tokens. The publisher stores this locally and configures their admission handler to accept tokens signed by this key.

### What we DON'T build on Day 0

- No billing / monetization
- No analytics dashboard
- No endpoint heartbeat (manual refresh via `aster publish` re-run)
- No version channels/tags (just latest version)
- No gossip-based instant revocation (TTL-based is sufficient)
- No SSO/SAML integration

---

## `aster publish` — CLI Flow

### Command Syntax

```bash
# Publish from current project (scans for @service decorators)
aster publish TaskManager

# Publish with explicit module path
aster publish myapp.services:TaskManager

# Re-publish (update endpoints or after code changes)
aster publish TaskManager
```

Also available inside the shell as the `publish` command.

### Interactive Flow

```
$ aster publish TaskManager

  Scanning service definition...
  ✓ Found TaskManager v3 (3 methods, 2 roles: admin, reader)

  Publishing to @emrul...
  ✓ Published @emrul/TaskManager v3

  Delegation key received — your service now accepts @aster-issued tokens.
  Endpoint registered: <your_node_id>

  ┌─────────────────────────────────────────────────┐
  │ @emrul/TaskManager is live.                     │
  │                                                 │
  │ Grant access:                                   │
  │   aster access grant @alice --service            │
  │     TaskManager --role reader                   │
  │                                                 │
  │ Or make it public:                              │
  │   aster access public --service TaskManager     │
  │                                                 │
  │ Consumers can now:                              │
  │   aster discover TaskManager                    │
  │   AsterClient.connect("@emrul/TaskManager")     │
  │                                                 │
  │ AI agents can use it via MCP:                   │
  │   aster mcp --discover @emrul/TaskManager       │
  └─────────────────────────────────────────────────┘
```

**First publish** also triggers recovery code generation (see identity doc §5).

The "Delegation key received" line is the critical moment — your service's admission handler is now configured to accept enrollment tokens signed by `@aster`. This happens automatically; `aster publish` writes the delegation key to your local config.

### What "Scanning" Does

1. Import the module (e.g., `myapp.services`)
2. Find the class decorated with `@service(name="TaskManager")`
3. Extract method signatures, wire types, field definitions
4. Compute contract ID: `blake3(canonical_json(service_contract))`
5. Build manifest JSON (same format as `aster contract gen --out manifest.json`)

This reuses the existing `aster contract gen` pipeline — publish just adds the upload step.

### Re-Publishing

Running `aster publish TaskManager` again:

- **Same contract ID** (no code changes): updates endpoint list only. Instant.
- **Different contract ID** (code changed): publishes new version. Old version remains accessible by contract hash. Latest version is what `@emrul/TaskManager` resolves to.

```
$ aster publish TaskManager

  Scanning service definition...
  ✓ Found TaskManager v4 (4 methods, contract: def456ab)

  Contract changed since last publish (v3 abc123de → v4 def456ab).
  Publishing new version...
  ✓ Published @emrul/TaskManager v4

  Note: consumers using v3 will continue to work until they
  regenerate their client from the new contract.
```

---

## `aster unpublish` — Removal

```bash
$ aster unpublish TaskManager

  Removing @emrul/TaskManager from @aster...
  ✓ Unpublished. Service no longer discoverable.

  Your service still runs locally — existing P2P connections
  are unaffected. Only discovery and contract gen are removed.
```

Unpublish removes the manifest and endpoints from `@aster`. It does NOT stop the service or disconnect existing consumers. It just makes it undiscoverable.

---

## `aster discover` — Finding Services

### Command Syntax

```bash
# Search by name
aster discover TaskManager

# Search by keyword
aster discover "task management"

# List all services by a handle
aster discover @emrul

# Browse in shell
aster shell → cd /aster → ls
```

### Output

```
$ aster discover task

  @emrul/TaskManager     v3  3 methods  1 endpoint   public  abc123de
  @alice/TaskRunner       v1  2 methods  3 endpoints  public  789xyz01
  @acme-corp/TaskQueue    v7  5 methods  2 endpoints  public  456def78

  3 services found. Use 'aster contract gen @handle/Service' to generate a client.
```

### What's Searchable

Day 0: service name (exact match and substring). Simple but useful.

Later: tags, description, method names, type names. Full-text search.

---

## `aster contract gen` from Published Contracts

This is the existing `aster contract gen` command, extended to accept `@handle/ServiceName` as input instead of only `module:ClassName`.

### Current (local only)

```bash
aster contract gen --service myapp:TaskManager --out manifest.json
```

### Extended (from `@aster`)

```bash
# Fetch manifest from @aster, generate typed client
aster contract gen @emrul/TaskManager --lang python --out ./client/

# Just fetch the manifest (for inspection or cross-language gen)
aster contract gen @emrul/TaskManager --out manifest.json
```

### What Gets Generated

For `--lang python`:

```
./client/
├── __init__.py
├── task_manager_client.py    # typed client class
├── types.py                  # @wire_type dataclasses (TaskRequest, TaskResult, etc.)
└── manifest.json             # cached manifest for reference
```

The generated client:

```python
# client/task_manager_client.py (generated)
from aster import AsterClient
from .types import TaskRequest, TaskResult, ProgressReq, ProgressEvent

class TaskManagerClient:
    def __init__(self, client: AsterClient):
        self._client = client

    async def submit_task(self, name: str, priority: int, tags: list[str] | None = None) -> TaskResult:
        """Unary RPC: submit a task."""
        request = TaskRequest(name=name, priority=priority, tags=tags or [])
        return await self._client.call("TaskManager", "submitTask", request, TaskResult)

    async def watch_progress(self, task_id: str) -> AsyncIterator[ProgressEvent]:
        """Server streaming: watch task progress."""
        request = ProgressReq(task_id=task_id)
        async for event in self._client.server_stream("TaskManager", "watchProgress", request, ProgressEvent):
            yield event

    async def cancel_task(self, task_id: str) -> CancelResult:
        """Unary RPC: cancel a task."""
        request = CancelReq(task_id=task_id)
        return await self._client.call("TaskManager", "cancelTask", request, CancelResult)
```

**This is the 30-second-from-discovery-to-calling experience.** Find the service, gen the client, call it. No coordination with the publisher.

---

## `@aster` Service: Publish & Access Methods

Added to the `AsterService` definition from the identity doc:

```python
@service(name="AsterService", version=1)
class AsterService:
    # ... identity methods from aster-identity-and-join.md ...

    # ── Publish (Day 1) ──────────────────────────────

    @unary
    async def publish(self, request: SignedRequest[PublishPayload]) -> PublishResult:
        """Publish a service. Signed by root key. Returns delegation pubkey."""
        ...

    @unary
    async def unpublish(self, request: SignedRequest[UnpublishPayload]) -> UnpublishResult:
        """Remove a service from the directory. Signed by root key."""
        ...

    # ── Access Control (Day 1) ───────────────────────

    @unary
    async def grant_access(self, request: SignedRequest[GrantAccessPayload]) -> GrantResult:
        """Grant a handle access to a service with a role. Signed by owner."""
        ...

    @unary
    async def revoke_access(self, request: SignedRequest[RevokeAccessPayload]) -> RevokeResult:
        """Revoke a handle's access. Signed by owner."""
        ...

    @unary
    async def list_access(self, request: SignedRequest[ListAccessPayload]) -> ListAccessResult:
        """List who has access to a service. Signed by owner."""
        ...

    @unary
    async def set_visibility(self, request: SignedRequest[SetVisibilityPayload]) -> VisibilityResult:
        """Set public/private. Signed by owner."""
        ...

    @unary
    async def enroll(self, request: SignedRequest[EnrollPayload]) -> EnrollResult:
        """Consumer requests enrollment token for a service. Signed by consumer's key.
        @aster checks access grants, issues enrollment token if authorized."""
        ...

    # ── Discovery (Day 1) ────────────────────────────

    @unary
    async def get_manifest(self, handle: str, service: str) -> ManifestResult:
        """Fetch a published service's manifest. Public, no auth."""
        ...

    @unary
    async def discover(self, query: str) -> DiscoverResult:
        """Search for published services. Public, no auth."""
        ...

    @unary
    async def resolve(self, handle: str, service: str) -> ResolveResult:
        """Resolve a service to its endpoints (node IDs). Public, no auth."""
        ...

    @unary
    async def list_services(self, handle: str) -> ListServicesResult:
        """List all published services for a handle. Public, no auth."""
        ...
```

### Wire Types

```python
@wire_type
@dataclass
class PublishPayload:
    action: str             # "publish"
    handle: str             # must match the signing key's registered handle
    service_name: str
    manifest: str           # JSON-encoded contract manifest
    endpoints: list[EndpointInfo]
    timestamp: int
    nonce: str

@wire_type
@dataclass
class EndpointInfo:
    node_id: str            # Iroh endpoint ID (hex)
    relay: str              # relay URL for NAT traversal
    ttl: int                # seconds until this endpoint is considered stale

@wire_type
@dataclass
class UnpublishPayload:
    action: str             # "unpublish"
    handle: str
    service_name: str
    timestamp: int
    nonce: str

@wire_type
@dataclass
class PublishResult:
    handle: str
    service_name: str
    version: int
    contract_id: str
    endpoints_registered: int
    first_publish: bool                 # true = recovery codes included
    recovery_codes: list[str] | None    # only on first-ever publish for this handle

@wire_type
@dataclass
class ManifestResult:
    handle: str
    service_name: str
    manifest: str           # JSON-encoded contract manifest
    contract_id: str
    version: int
    endpoints: list[EndpointInfo]
    published_at: str       # ISO 8601

@wire_type
@dataclass
class DiscoverResult:
    services: list[DiscoverEntry]

@wire_type
@dataclass
class DiscoverEntry:
    handle: str
    service_name: str
    version: int
    contract_id: str
    method_count: int
    endpoint_count: int
    visibility: str         # "public"

@wire_type
@dataclass
class ResolveResult:
    handle: str
    service_name: str
    endpoints: list[EndpointInfo]
    contract_id: str

@wire_type
@dataclass
class ListServicesResult:
    handle: str
    services: list[DiscoverEntry]

# ── Access Control ────────────────────────────────

@wire_type
@dataclass
class GrantAccessPayload:
    action: str             # "grant_access"
    handle: str             # owner's handle
    service_name: str
    consumer_handle: str    # who to grant
    role: str               # e.g., "reader", "admin"
    timestamp: int
    nonce: str

@wire_type
@dataclass
class RevokeAccessPayload:
    action: str             # "revoke_access"
    handle: str
    service_name: str
    consumer_handle: str
    timestamp: int
    nonce: str

@wire_type
@dataclass
class ListAccessPayload:
    action: str             # "list_access"
    handle: str
    service_name: str
    timestamp: int
    nonce: str

@wire_type
@dataclass
class SetVisibilityPayload:
    action: str             # "set_visibility"
    handle: str
    service_name: str
    visibility: str         # "public" | "private"
    timestamp: int
    nonce: str

@wire_type
@dataclass
class EnrollPayload:
    action: str             # "enroll"
    consumer_handle: str    # the consumer requesting enrollment
    target_handle: str      # service owner
    target_service: str     # service name
    timestamp: int
    nonce: str

@wire_type
@dataclass
class GrantResult:
    consumer_handle: str
    service_name: str
    role: str
    granted_at: str

@wire_type
@dataclass
class RevokeResult:
    consumer_handle: str
    service_name: str
    revoked: bool

@wire_type
@dataclass
class ListAccessResult:
    grants: list[AccessGrant]

@wire_type
@dataclass
class AccessGrant:
    consumer_handle: str
    role: str
    granted_at: str

@wire_type
@dataclass
class VisibilityResult:
    service_name: str
    visibility: str

@wire_type
@dataclass
class EnrollResult:
    token: str              # signed enrollment token (base64)
    expires_at: str         # ISO 8601
    role: str               # granted role
```

---

## Server-Side Storage (SQLite, Day 0)

```sql
CREATE TABLE published_services (
    handle          TEXT NOT NULL,
    service_name    TEXT NOT NULL,
    version         INTEGER NOT NULL,
    contract_id     TEXT NOT NULL,
    manifest_json   TEXT NOT NULL,
    published_at    INTEGER NOT NULL,   -- epoch seconds
    visibility      TEXT NOT NULL DEFAULT 'public',
    token_ttl       INTEGER NOT NULL DEFAULT 7200,  -- seconds, default 2h
    PRIMARY KEY (handle, service_name)
);

CREATE TABLE service_endpoints (
    handle          TEXT NOT NULL,
    service_name    TEXT NOT NULL,
    node_id         TEXT NOT NULL,
    relay           TEXT NOT NULL DEFAULT '',
    ttl             INTEGER NOT NULL DEFAULT 3600,
    registered_at   INTEGER NOT NULL,
    PRIMARY KEY (handle, service_name, node_id),
    FOREIGN KEY (handle, service_name) REFERENCES published_services(handle, service_name)
);

CREATE TABLE access_grants (
    handle          TEXT NOT NULL,      -- service owner
    service_name    TEXT NOT NULL,
    consumer_handle TEXT NOT NULL,      -- who has access
    role            TEXT NOT NULL,
    granted_at      INTEGER NOT NULL,   -- epoch seconds
    granted_by      TEXT NOT NULL,      -- pubkey of granter (for audit)
    PRIMARY KEY (handle, service_name, consumer_handle),
    FOREIGN KEY (handle, service_name) REFERENCES published_services(handle, service_name)
);

-- Full-text search (Day 0: name only, later: description, tags)
CREATE INDEX idx_service_name ON published_services(service_name);
CREATE INDEX idx_access_consumer ON access_grants(consumer_handle);
```

---

## Connect by Handle: `AsterClient.connect("@emrul/TaskManager")`

This is the consumer-side resolution. When a consumer writes:

```python
client = await AsterClient.connect("@emrul/TaskManager")
```

Under the hood:

1. Parse `@emrul/TaskManager` → handle `emrul`, service `TaskManager`
2. Call `@aster`'s `resolve("emrul", "TaskManager")` → list of `EndpointInfo`
3. If consumer has a verified handle: call `@aster`'s `enroll()` → get enrollment token with roles
4. Pick an endpoint (first live one, or random for load distribution)
5. Connect P2P via Iroh transport to that node ID, presenting the enrollment token
6. Service admission handler verifies `@aster`'s signature → admits with granted roles
7. Return a connected `AsterClient`

`@aster` is involved in resolution and enrollment token issuance (steps 2-3). All RPC traffic is P2P (step 5+). `@aster` never sees the requests or responses.

For **public services** (no access grants required), step 3 is skipped — the consumer connects without an enrollment token and the service's admission handler accepts unauthenticated consumers for public methods.

### Offline / Air-Gapped Resolution

If `@aster` is unreachable:
- Check local cache for previously resolved endpoints
- If cached and within TTL, use cached endpoint
- If no cache, fail with a clear error: "Can't resolve @emrul/TaskManager — @aster unreachable and no cached endpoints"

### Direct Connection Still Works

```python
# This still works — no @aster involved
client = await AsterClient.connect(node_id="7f3a2bc9de01...", relay="...")
```

Publishing is opt-in. Direct connection by node ID is always available.

---

## The 30-Second Experience (End-to-End)

This is what we're optimizing for. A developer finds a service and is calling it in 30 seconds:

```bash
# 1. Find it (5 seconds)
$ aster discover summarizer
  @alice/DocumentSummarizer  v1  2 methods  1 endpoint  public

# 2. Generate a client (10 seconds)
$ aster contract gen @alice/DocumentSummarizer --lang python --out ./summarizer/
  ✓ Generated DocumentSummarizerClient (2 methods)

# 3. Use it (15 seconds)
$ python
>>> from summarizer import DocumentSummarizerClient
>>> from aster import AsterClient
>>> client = await AsterClient.connect("@alice/DocumentSummarizer")
>>> sc = DocumentSummarizerClient(client)
>>> result = await sc.summarize(text="...", max_length=100)
>>> print(result.summary)
```

No signup required for the consumer. No credential exchange for public services. No shared repo. No hand-written client code. Discover → gen → call.

### The 10-Second Experience (No Code At All)

For someone who just wants to *try* a service without writing any code:

```bash
# aster call is the curl of Aster — works with handle addressing
$ aster call @alice/DocumentSummarizer.summarize '{"text": "...", "max_length": 100}'
{
  "summary": "...",
  "word_count": 42
}
```

`aster call` already exists for direct peer connections by node ID. Day 0 adds handle resolution: `@handle/Service.method` resolves via `@aster`, auto-enrolls if needed, calls, prints the result. This is the fastest way to feel the platform without writing a client.

---

## What This Means for `@aster` as a Business

This section is for internal context, not the docs site.

**The publish + gen loop is the growth engine:**

1. Publisher publishes → service appears in directory
2. Consumer discovers → generates client → calls service
3. Consumer thinks "I should publish my service too" → publishes
4. More services → more consumers → more publishers → flywheel

**Why free makes sense for Day 0:**
- Every publish grows the directory
- Every gen proves the value
- Friction = death at this stage
- Charge later for private services, teams, analytics

**What `@aster` sees (and doesn't):**
- Sees: which services are published, discovery queries, gen requests
- Does NOT see: RPC traffic, payloads, responses, connection frequency
- This is a feature, not a limitation — publishers trust us because we can't spy on their traffic

---

## Implementation Plan (Day 1 items from identity doc, extended)

These are the publish-specific items. They depend on D1-A through D1-E from the identity doc being in place.

### D1-G: Publish (Client Side)

- [ ] `aster publish <ServiceName>` CLI command
- [ ] `aster publish myapp:ServiceName` (explicit module path)
- [ ] Reuse `aster contract gen` pipeline for manifest generation
- [ ] Sign publish request with root key
- [ ] Store returned `delegation_pubkey` in local config
- [ ] Auto-configure admission handler to accept `@aster`-signed tokens
- [ ] Display confirmation with access control + MCP hints
- [ ] `aster unpublish <ServiceName>` CLI command
- [ ] `publish` / `unpublish` shell commands wired into REPL
- [ ] First-publish: display recovery codes

### D1-H: Publish (Server Side)

- [ ] `publish` method on `AsterService` — validate signature, re-verify BLAKE3 contract hash from manifest, store manifest + endpoints, return delegation pubkey
- [ ] `unpublish` method — validate signature, remove from directory
- [ ] `get_manifest` method — return stored manifest (public, no auth)
- [ ] `discover` method — search by service name (substring match)
- [ ] `resolve` method — return endpoint list for a handle/service
- [ ] `list_services` method — return all services for a handle
- [ ] SQLite tables: `published_services`, `service_endpoints`, `access_grants`
- [ ] Ownership check: signing key's handle must match the publish target handle

### D1-I: Access Control (Client + Server)

- [ ] `aster access grant @handle --service S --role R` CLI command
- [ ] `aster access revoke @handle --service S` CLI command
- [ ] `aster access list --service S` CLI command
- [ ] `aster access public --service S` / `aster access private --service S`
- [ ] `grant_access` method on `AsterService` — owner grants handle+role
- [ ] `revoke_access` method — owner revokes
- [ ] `list_access` method — owner lists grants
- [ ] `set_visibility` method — public/private toggle
- [ ] `enroll` method — consumer requests enrollment token; `@aster` checks grants, issues signed token
- [ ] Token signing: `@aster` signs enrollment tokens with its delegation key
- [ ] Token format: standard `ConsumerEnrollmentCredential` with handle, roles, TTL, `@aster` signature
- [ ] Shell commands: `grant`, `revoke`, `access` wired into REPL

### D1-J: Admission Handler Integration (Framework)

- [ ] `delegation_pubkey` config field per published service
- [ ] Admission handler: verify `@aster`-signed tokens as second trust path
- [ ] Roles from token applied to `CallContext.attributes` (existing gate 2 just works)
- [ ] `aster publish` auto-writes delegation config — no manual setup

### D1-K: Contract Gen from `@aster`

- [ ] Extend `aster contract gen` to accept `@handle/ServiceName` as input
- [ ] Fetch manifest from `@aster` via `get_manifest` RPC
- [ ] Generate typed client from fetched manifest (same pipeline as local gen)
- [ ] `--lang python` output: client class + wire type dataclasses

### D1-L: Connect by Handle + `aster call`

- [ ] `AsterClient.connect("@handle/ServiceName")` — parse, resolve, connect
- [ ] Resolution via `@aster`'s `resolve` method
- [ ] Auto-enrollment: if consumer has a verified handle, request enrollment token from `@aster`
- [ ] Endpoint caching with TTL
- [ ] Fallback to cached endpoints when `@aster` is unreachable
- [ ] Direct connection by node ID still works (no change)
- [ ] `aster call @handle/Service.method '{...}'` — handle resolution for the existing `aster call` command
- [ ] `aster shell` method invocation: `./method` works against handle-resolved connections

### D1-M: MCP Discovery Integration

- [ ] `aster mcp --discover @handle/ServiceName` — resolve from `@aster`, connect, expose as MCP tools
- [ ] `aster mcp --discover "pattern"` — search `@aster`, connect to matching services
- [ ] Auto-enrollment: MCP bridge gets enrollment token from `@aster` if consumer has a handle
- [ ] Tools inherit access control: only methods the consumer's role permits are exposed

### D1-N: VFS Integration (from identity doc D1-F)

- [ ] Published services show `● published` in `ls` output
- [ ] `cd /aster/<handle>/<service>` shows method details from published manifest
- [ ] `describe <method>` works against published contracts (not just local)
- [ ] Other handles' published services browsable via `cd /aster/<other-handle>`

---

## Open Questions

1. **Manifest size limit.** Services with many types could have large manifests. What's the cap? 1MB? 10MB? (Most will be <100KB.)

2. **Contract ID verification.** ~~Should `@aster` recompute the contract ID from the manifest and verify it matches?~~ **Decided: yes.** `@aster` recomputes the BLAKE3 hash from the submitted manifest and rejects if it doesn't match the claimed `contract_id`. No trust-the-client for identity hashes. Added to D1-H.

3. **Endpoint TTL and staleness.** Day 0 has no heartbeat. Endpoints could go stale. What's the default TTL? 1 hour? 24 hours? How visible is staleness in `aster discover`?

4. **Multi-endpoint services.** A service can run on multiple nodes. `resolve` returns all endpoints. How does the client pick one? Random? Round-robin? Latency-based? (Day 0: first one. Later: configurable.)

5. **Cross-language gen.** `--lang typescript` is the next language. How much of the gen pipeline is language-agnostic vs. language-specific? (The manifest fetch is shared; the code generation is per-language.)

6. **Contract evolution.** What happens when v4 is published but consumers have v3 clients? They still connect and call — the wire format is versioned. But we should surface a "new version available" hint somewhere. Where?

7. **Manifest caching.** Should `aster contract gen` cache fetched manifests locally? (Yes — makes re-gen fast and works offline after first fetch.)

8. **Token TTL defaults.** 2 hours feels right for most cases. Should `aster publish` accept `--token-ttl`? Should it be per-grant (different TTL for different consumers)?

9. **Public services and MCP.** If a service is public (no access grants needed), should the MCP bridge be able to connect without any enrollment at all? (Probably yes — lowers the bar for AI agent discovery.)

10. **Monetization hook.** The access grant system is the natural place to attach billing. `aster access grant @bob --service TaskManager --role premium --billing monthly` — but this is post Day 0. The data model should be extensible enough to add a `billing` field later without schema migration.
