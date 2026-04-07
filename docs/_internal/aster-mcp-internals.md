# Aster MCP Server — Internal Engineering Guide

How the MCP server works, how to extend it, and how it maps to the existing shell infrastructure.

## Architecture

The MCP server reuses the same infrastructure as the aster shell:

```
                          ┌──────────────┐
                          │ AI Agent     │
                          │ (Claude)     │
                          └──────┬───────┘
                                 │ stdio JSON-RPC
                          ┌──────┴───────┐
                          │ FastMCP      │ ← mcp Python SDK
                          │ (server.py)  │
                          └──────┬───────┘
                                 │
              ┌──────────────────┼──────────────────┐
              │                  │                  │
     ┌────────┴────────┐ ┌──────┴──────┐ ┌────────┴────────┐
     │ schema.py       │ │ security.py │ │ PeerConnection  │
     │ FieldSchema →   │ │ ToolFilter  │ │ (from shell)    │
     │ JSON Schema     │ │ allow/deny  │ │ connect/invoke  │
     └─────────────────┘ └─────────────┘ └─────────────────┘
```

## Module map

| Module | Purpose | Reuses |
|--------|---------|--------|
| `cli/aster_cli/mcp/server.py` | Core MCP server, tool registration, call handling | `PeerConnection`, `DemoConnection`, `_to_serializable` |
| `cli/aster_cli/mcp/schema.py` | Aster type → JSON Schema conversion | Manifest `fields` format |
| `cli/aster_cli/mcp/security.py` | Tool visibility, allow/deny globs, confirm prompts | Standalone |

## How tools are registered

On `AsterMcpServer.setup()`:

1. `PeerConnection.connect()` → admission → service list
2. `PeerConnection._fetch_manifests()` → registry doc → blob collections → manifest.json
3. For each service, `service_to_tool_definitions()` converts methods → MCP tools
4. `ToolFilter.is_visible()` filters each tool
5. `FastMCP.add_tool()` registers surviving tools

The tool handler is a closure that captures `service_name`, `method_name`, `pattern` and delegates to `_handle_call()`.

## Schema conversion (`schema.py`)

### Type mapping

```
Aster type      → JSON Schema
str             → {"type": "string"}
int             → {"type": "integer"}
float           → {"type": "number"}
bool            → {"type": "boolean"}
bytes           → {"type": "string", "contentEncoding": "base64"}
list[X]         → {"type": "array", "items": <recurse X>}
dict[str, X]    → {"type": "object", "additionalProperties": <recurse X>}
Optional[X]     → {<recurse X>, "nullable": true}
MyCustomType    → {"type": "object", "description": "Aster type: MyCustomType"}
```

Nested dataclass types are opaque in Phase 1 (just `"type": "object"`). Phase 2 with live `ServiceInfo` can generate full nested schemas.

### Streaming meta-parameters

| Pattern | Added parameters | Purpose |
|---------|-----------------|---------|
| `server_stream` | `_max_items` (int, default 100), `_timeout` (float, default 30) | Control collection |
| `client_stream` | `_items` (array, required) | Provide stream input |
| `bidi_stream` | N/A | Excluded from tools in Phase 1 |

## Security (`security.py`)

### Filter evaluation order

```
For each tool:
  1. Is tool_name matched by any deny pattern? → HIDDEN
  2. Are allow patterns configured?
     - Yes: does tool_name match at least one? → VISIBLE, else HIDDEN
     - No: → VISIBLE
```

Deny always wins. Allow is opt-in restriction.

### Confirm flow

```
Agent calls tool
  → server.py._handle_call()
    → ToolFilter.needs_confirmation(tool_name)
      → if True: ToolFilter.confirm_call()
        → print to stderr, read stdin
        → "y" → proceed
        → anything else → return error to agent
```

The confirm prompt uses stderr (not stdout — stdout is the MCP JSON-RPC channel).

## Call handling (`server.py`)

### Unary

```python
result = await connection.invoke(service, method, arguments)
return json.dumps(_to_serializable(result))
```

### Server stream

```python
stream = await connection.server_stream(service, method, arguments)
collected = []
async for item in stream:
    collected.append(item)
    if len(collected) >= max_items:
        break
return json.dumps(collected)
```

With `asyncio.wait_for(timeout)` wrapping the collection.

### Client stream

```python
result = await connection.client_stream(service, method, items)
return json.dumps(result)
```

## Testing

### Unit tests (`test_mcp.py`)

- `TestAsterTypeToJsonSchema` — all type mappings (13 cases)
- `TestFieldToJsonSchema` — fields with defaults, descriptions
- `TestMethodToToolDefinition` — unary, server_stream, client_stream meta-params
- `TestServiceToToolDefinitions` — service-level conversion, bidi exclusion
- `TestToolFilter` — allow/deny/confirm patterns, wildcards, deny-wins-over-allow
- `TestAsterMcpServer` — setup with DemoConnection, filtered setup, bidi exclusion

### Integration testing

```bash
# Test with MCP inspector
aster mcp --demo
# In another terminal:
mcp dev "aster mcp --demo"
# Opens browser UI at localhost:6274 to interact with tools
```

### E2E testing

```bash
# Start a real producer
python examples/python/simple_producer.py

# In another terminal, run MCP server
aster mcp <addr-from-producer>

# Use MCP inspector or Claude to call tools
```

## Extending for Phase 2 (producer sidecar)

Phase 2 adds `AsterServer(mcp=True)` which auto-exposes services. The key difference:

- Phase 1: `PeerConnection` → remote peer → manifest JSON → tool schemas
- Phase 2: `Server._registry` → local `ServiceInfo` → richer schemas + `LocalTransport`

The server module is designed for this: `AsterMcpServer` takes a generic connection. Replace `PeerConnection` with a `LocalConnection` adapter that wraps the in-process `Server` and `ServiceRegistry`.

## Extending for Phase 3 (resources + bidirectional)

### Resources

```python
@mcp.resource("aster://services/{name}/manifest")
async def get_manifest(name: str) -> str:
    contract = await connection.get_contract(name)
    return json.dumps(contract)

@mcp.resource("aster://blobs/{hash}")
async def get_blob(hash: str) -> bytes:
    return await connection.read_blob(hash)
```

### Bidirectional (Aster services calling MCP tools)

An Aster service receives an `McpToolbox` via dependency injection:

```python
@service
class SmartService:
    def __init__(self, toolbox: McpToolbox):
        self.toolbox = toolbox
    
    @rpc
    async def process(self, req: Request) -> Response:
        # Call an MCP tool (e.g., Claude) as part of handling
        summary = await self.toolbox.call_tool("summarize", {"text": req.data})
        return Response(result=summary)
```

This creates agent-to-agent workflows where Aster services orchestrate AI capabilities.

## Dynamic Invocation via DynamicTypeFactory

The MCP server benefits from the same `DynamicTypeFactory` mechanism described in the shell internals. Previously, the MCP server could discover tools from a remote peer's manifest and expose them to an AI agent, but actually invoking those tools required the service's Python package to be installed locally (so the Fory codec had registered wire types for serialization).

With `DynamicTypeFactory`, `PeerConnection._synthesize_types()` creates wire-compatible dataclasses from the manifest's `request_wire_tag`, `response_wire_tag`, and `response_fields` at connect time. When an agent calls an MCP tool, `_handle_call()` can now build a typed request from the agent's dict arguments using the synthesized dataclass, just as the shell invoker does.

This closes the gap where MCP could discover and advertise tools but could not actually invoke them without having the service's types installed locally. Any peer's services are now fully callable through the MCP server out of the box.
