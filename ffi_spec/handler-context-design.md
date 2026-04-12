# Handler Context Access Design

**Date:** 2026-04-12
**Status:** Design note, pre-implementation
**Relates to:** Interceptor system (§S6), CallContext, @rpc dispatch

## Problem

Service handlers currently receive only the request object:

```typescript
async getStatus(req: StatusRequest): Promise<StatusResponse> { ... }
```

```python
@rpc
async def get_status(self, req: StatusRequest) -> StatusResponse: ...
```

There is no way for the handler to access the caller's identity, rcan
attributes, metadata, deadline, or any other per-call context. Developers
who want to make authorization decisions inside the handler (beyond what
interceptors enforce) have no mechanism to do so.

## Design

Two complementary access paths. Both use the same `CallContext` object
that interceptors already produce.

### Path 1: Explicit parameter (primary)

The `@rpc` decorator inspects the handler's signature at decoration time.
If the method accepts a second parameter (after `self` in Python, after
`req` in all languages), dispatch injects the `CallContext`. If the
method only accepts the request, dispatch calls it without context.

**Python:**
```python
@rpc
async def get_status(self, req: StatusRequest, ctx: CallContext) -> StatusResponse:
    if "ops" not in ctx.attributes.get("roles", ""):
        raise RpcError(StatusCode.PERMISSION_DENIED, "ops role required")
    return StatusResponse(agent_id=req.agent_id, status="running")
```

Detection: `inspect.signature(method).parameters` has 2 params (excl.
self) -> inject ctx. Has 1 param -> call without ctx.

**TypeScript:**
```typescript
@Rpc({ request: StatusRequest, response: StatusResponse })
async getStatus(req: StatusRequest, ctx: CallContext): Promise<StatusResponse> {
    if (!ctx.attributes["role"]?.includes("ops"))
        throw new RpcError(StatusCode.PERMISSION_DENIED, "ops role required");
    return new StatusResponse({ agent_id: req.agent_id, status: "running" });
}
```

Detection: `method.length > 1` -> inject ctx.

**Java:**
```java
@Rpc
public StatusResponse getStatus(StatusRequest req, CallContext ctx) {
    if (!ctx.attributes().getOrDefault("roles", "").contains("ops"))
        throw new RpcError(StatusCode.PERMISSION_DENIED, "ops role required");
    return new StatusResponse(req.agentId(), "running");
}
```

Detection: method parameter count via reflection. 2 params where the
second is `CallContext` -> inject ctx.

**Go:**
```go
func (s *MissionControl) GetStatus(ctx *aster.CallContext, req *StatusRequest) (*StatusResponse, error) {
    if !strings.Contains(ctx.Attributes["roles"], "ops") {
        return nil, &aster.RpcError{Code: aster.PermissionDenied, Message: "ops role required"}
    }
    return &StatusResponse{AgentID: req.AgentID, Status: "running"}, nil
}
```

Go convention: context is the FIRST parameter, not the second. This
matches `context.Context` idiom. Detection: first param is `*CallContext`.

**C#:**
```csharp
[Rpc]
public StatusResponse GetStatus(StatusRequest req, CallContext ctx) {
    if (!ctx.Attributes.GetValueOrDefault("roles", "").Contains("ops"))
        throw new RpcError(StatusCode.PermissionDenied, "ops role required");
    return new StatusResponse { AgentId = req.AgentId, Status = "running" };
}
```

Detection: method parameter count via reflection. Same as Java.

### Path 2: Implicit async-local (convenience fallback)

The dispatch sets the CallContext on an async-local variable before
invoking the handler. Handlers that don't take a context parameter can
still access it via a static lookup. Useful when context is needed deep
in a call chain without threading it through every function signature.

| Language | Mechanism | Access |
|----------|-----------|--------|
| Python | `contextvars.ContextVar` | `CallContext.current()` |
| TypeScript | `AsyncLocalStorage` | `CallContext.current()` |
| Java | `ScopedValue` (Java 25) | `CallContext.current()` |
| Go | `context.Context` with value | `aster.CallContextFrom(ctx)` |
| C# | `AsyncLocal<CallContext>` | `CallContext.Current` |

The async-local is always set, regardless of whether the handler takes
the explicit parameter. This means interceptors that run after dispatch
(e.g., metrics on_response) can also access it.

### Dispatch flow

```
1. Reactor delivers call -> (callId, header, request, peerId)
2. Parse header -> extract service, method, metadata, deadline
3. Admission check -> extract attributes from rcan credential
4. Build CallContext(service, method, peer, metadata, attributes, deadline, ...)
5. Run request interceptors: ctx = applyRequestInterceptors(interceptors, ctx, request)
6. Set async-local: CallContext._current.set(ctx)
7. Detect handler signature:
   - Has ctx param -> handler(request, ctx)    [or (ctx, request) in Go]
   - No ctx param -> handler(request)
8. Run response interceptors: response = applyResponseInterceptors(interceptors, ctx, response)
9. Clear async-local
```

Step 6 happens AFTER interceptors, so the handler sees the fully-
processed context (e.g., AuthInterceptor has already populated
attributes, DeadlineInterceptor has validated the deadline).

### CallContext fields available to handlers

All fields from the interceptor CallContext are available:

| Field | Type | Source |
|-------|------|--------|
| `service` | string | Parsed from header frame |
| `method` | string | Parsed from header frame |
| `callId` | string | Reactor call ID or generated UUID |
| `sessionId` | string? | Session-scoped service connection ID |
| `peer` | string | Reactor peer_id (endpoint ID hex) |
| `metadata` | map | Parsed from header frame metadata |
| `attributes` | map | From admission credential (rcan) |
| `deadline` | float? | From header frame deadline field |
| `isStreaming` | bool | True for streaming patterns |
| `pattern` | string | "unary", "server_stream", etc. |
| `idempotent` | bool | From method definition |
| `attempt` | int | Retry attempt number |

`attributes` is the key field for in-handler authorization. It contains
the key-value pairs from the caller's enrollment credential, which were
verified by Gate 2 (admission) before reaching the handler. Common
attributes:

- `aster.role` -- role string (e.g., "producer", "consumer", "admin")
- `aster.scope` -- scope restriction
- `aster.iid_*` -- cloud instance identity fields (AWS/GCP/Azure)
- Custom attributes set by the root key signer

### What this does NOT replace

Interceptors remain the right place for cross-cutting concerns that apply
to ALL methods (rate limiting, deadline enforcement, metrics, audit).
Handler-level context access is for **per-method authorization decisions**
that depend on the specific business logic of that method.

Example: an interceptor enforces "caller must have the ops role" at the
service level. The handler then further restricts "only the agent's own
owner can see its detailed status" by checking `ctx.attributes["owner"]
== req.agent_id`. The interceptor handles the gate; the handler handles
the business rule.

### Implementation scope per language

| Language | Explicit param detection | Async-local mechanism | Effort |
|----------|-------------------------|----------------------|--------|
| Python | `inspect.signature` | `contextvars.ContextVar` | Low -- both exist in stdlib |
| TypeScript | `method.length` | `AsyncLocalStorage` | Low -- Node.js 16+ |
| Java | `Method.getParameterTypes()` | `ScopedValue` (Java 25) | Low -- new in Java 25 |
| Go | First param type check | `context.WithValue` | Low -- idiomatic |
| C# | `MethodInfo.GetParameters()` | `AsyncLocal<T>` | Low -- .NET 6+ |

All five are small changes to the dispatch path in each language's
AsterServer. No FFI changes needed -- the context is constructed from
data already available at the dispatch layer.

## Inline Request Parameters (Zero-Boilerplate Handlers)

### Problem

Today, every RPC method requires an explicit `@wire_type` request class,
even for trivial single-field calls:

```typescript
@WireType("mission/StatusRequest")
class StatusRequest { agent_id: string = ""; }

@Rpc({ request: StatusRequest, response: StatusResponse })
async getStatus(req: StatusRequest): Promise<StatusResponse> {
    return new StatusResponse({ agent_id: req.agent_id, status: "running" });
}
```

`StatusRequest` is pure boilerplate. The dev wants to write:

```typescript
@Rpc({ response: StatusResponse })
async getStatus(agent_id: string): Promise<StatusResponse> {
    return new StatusResponse({ agent_id, status: "running" });
}
```

### Design

The `@rpc` decorator inspects the method signature and detects two modes:

**Mode 1 -- Explicit request type (existing behavior):**
Exactly one non-CallContext parameter that is a `@wire_type` class. Pass
it through as-is. No change.

**Mode 2 -- Inline parameters (new):**
Any other combination of parameters. The framework synthesizes a wire
type from the method signature at decoration time:

- Wire type name: `{MethodName}Request` (e.g., `getStatus` -> `GetStatusRequest`)
- Wire type package: same as the service's package
- Fields: one per parameter, names match parameter names, wire types
  per the language mapping table (section 11.3.2.3)
- Field IDs: NFC-name-sorted (same rule as explicit types)
- `CallContext` parameters are excluded -- they are framework injection,
  not wire fields

The synthesized type is registered internally with the contract identity
system so `contract_id` is computed identically to an explicit
`@wire_type` class with the same fields.

### Detection logic at decoration time

```
params = method signature parameters (excluding self/this)
ctx_param = any param typed as CallContext -> set aside for injection
remaining = params - ctx_param

if len(remaining) == 1 and remaining[0].type is @wire_type decorated:
    mode = EXPLICIT  (pass the single object through)
elif len(remaining) == 0:
    mode = NO_REQUEST  (method takes no input, e.g., list_agents)
else:
    mode = INLINE  (synthesize request type from remaining params)
```

### Examples across languages

**Python:**
```python
# Mode 1 -- explicit request type
@rpc
async def create_agent(self, req: CreateAgentRequest) -> Agent: ...

# Mode 2 -- inline params, framework synthesizes CreateAgentRequest
@rpc
async def create_agent(self, agent_name: str, config: AgentConfig) -> Agent: ...

# Mode 2 + context injection
@rpc
async def create_agent(self, agent_name: str, config: AgentConfig, ctx: CallContext) -> Agent: ...

# Mode 2 -- all primitives
@rpc
async def get_status(self, agent_id: str) -> StatusResponse: ...

# Mode 2 -- no request fields
@rpc
async def list_agents(self) -> AgentList: ...

# Mode 2 -- no request fields + context
@rpc
async def list_agents(self, ctx: CallContext) -> AgentList: ...
```

**TypeScript:**
```typescript
@Rpc({ response: StatusResponse })
async getStatus(agent_id: string): Promise<StatusResponse> { ... }

@Rpc({ response: Agent })
async createAgent(agent_name: string, config: AgentConfig, ctx: CallContext): Promise<Agent> { ... }
```

**Java:**
```java
@Rpc
public StatusResponse getStatus(String agentId) { ... }

@Rpc
public Agent createAgent(String agentName, AgentConfig config, CallContext ctx) { ... }
```

**Go:**
```go
func (s *Svc) GetStatus(ctx *aster.CallContext, agentId string) (*StatusResponse, error) { ... }
func (s *Svc) CreateAgent(ctx *aster.CallContext, agentName string, config *AgentConfig) (*Agent, error) { ... }
```

### Mixed parameter types

Mode 2 handles any mix of primitives and `@wire_type` classes:

```python
async def create_agent(self, agent_name: str, config: AgentConfig) -> Agent:
```

Synthesizes: `CreateAgentRequest { agent_name: string, config: AgentConfig }`
where `agent_name` is a primitive field and `config` is a nested message
reference (`TypeKind.REF` with the `AgentConfig` type hash in the
canonical encoding).

### Contract identity

The synthesized wire type produces the same `contract_id` as an explicit
`@wire_type` class with identical fields. This is guaranteed because:

1. The wire type name is deterministic (derived from method name)
2. The fields are deterministic (derived from parameter names + types)
3. Field IDs are NFC-name-sorted (independent of declaration order)
4. The language mapping table (section 11.3.2.3) resolves each parameter
   type to a wire type identically to how it resolves `@wire_type` fields

A producer using inline parameters and a consumer using a generated typed
client (which has the explicit request class from the contract manifest)
will agree on the contract because both resolve to the same canonical
bytes.

### Response types stay explicit

Responses are the public contract that consumers generate typed clients
from. A consumer reading the contract manifest sees
`StatusResponse { agent_id, status, uptime_secs }` and generates a class
from it. Auto-synthesizing response types would produce meaningless field
names (e.g., `value` for a single return) and hide the output schema.

Response types always require an explicit `@wire_type` class.

### Interaction with gen-client

`aster contract gen` and `aster contract preview` already introspect
method signatures. For Mode 2 methods:

- `gen` emits the synthesized request type in the manifest alongside
  the explicit response type, so consumers see a full schema
- `preview` shows the synthesized type with its fields, indistinguishable
  from an explicit type
- `gen-client` generates client methods that **mirror the producer's
  inline parameter signature**, not an explicit request class:

```typescript
// Producer wrote:
@Rpc({ response: StatusResponse })
async getStatus(agent_id: string): Promise<StatusResponse> { ... }

// gen-client produces for the consumer:
class MissionControlClient {
    async getStatus(agent_id: string): Promise<StatusResponse> {
        return this._invoke("getStatus", { agent_id }, StatusResponse);
    }
}
```

The synthesized request class (`GetStatusRequest`) exists internally on
the wire but is never exposed to either producer or consumer. Both sides
see `getStatus(agent_id: string)`. The method signature IS the contract.

For Mode 1 methods (explicit `@wire_type` request), gen-client still
generates the explicit request class -- the producer chose ceremony, so
the consumer gets the same shape.

The manifest distinguishes the two modes via a `request_style` field on
the method descriptor: `"inline"` (Mode 2) or `"explicit"` (Mode 1).
gen-client reads this to decide whether to emit a method with inline
params or a method taking a request object.
