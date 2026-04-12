# Session Instructions: Handler Context + Inline Params

Paste this as the opening message of a new Claude Code session.

---

I need you to implement two related features from the design doc at `ffi_spec/handler-context-design.md`. Read that doc fully first — it contains the complete design, detection logic, examples per language, and interaction with gen-client.

## What to implement

### 1. CallContext injection into handlers

The `@rpc` dispatch path in each language needs to:
- Build a `CallContext` from the reactor call data (peer_id, metadata, attributes, deadline) — the CallContext class already exists in each language's interceptors package (built tonight, see `bindings/java/src/main/java/com/aster/interceptors/CallContext.java`, `bindings/go/interceptors.go`, `bindings/dotnet/src/Aster/Interceptors/CallContext.cs`)
- Detect if the handler method accepts a `CallContext` parameter
- If yes, inject it; if no, call without it
- Set the async-local (Python `contextvars.ContextVar`, TS `AsyncLocalStorage`, Java `ScopedValue`, Go `context.WithValue`, .NET `AsyncLocal<T>`) so handlers can also access context via `CallContext.current()`

### 2. Inline request parameters (zero-boilerplate handlers)

The `@rpc` decorator needs to detect Mode 1 vs Mode 2:
- Mode 1: exactly one non-CallContext param that is a `@wire_type` class → pass through (existing behavior, don't break it)
- Mode 2: anything else → synthesize a wire type from the parameter names + types

The synthesized type must:
- Be named `{MethodName}Request` (e.g., `getStatus` → `GetStatusRequest`)
- Use the service's package
- Have fields matching parameter names with wire types per the language mapping table (§11.3.2.3 in `ffi_spec/Aster-ContractIdentity.md`)
- Register with the contract identity system so contract_id is computed correctly
- Be invisible to the developer — they write `def get_status(self, agent_id: str)`, not `def get_status(self, req: GetStatusRequest)`

### 3. Update gen-client to mirror inline signatures

The manifest's MethodDescriptor needs a `request_style` field: `"inline"` or `"explicit"`.

`aster contract gen` (in `cli/aster_cli/contract.py`) must:
- Detect which mode was used at decoration time
- Set `request_style` accordingly
- For inline mode, include the individual parameter names and types in the method descriptor

`gen-client` (in `cli/aster_cli/codegen.py` and `cli/aster_cli/codegen_typescript.py`) must:
- Read `request_style` from the manifest
- For `"inline"`: generate client methods with matching inline params, constructing the request object internally
- For `"explicit"`: generate client methods taking the request class (existing behavior)

`aster contract preview` (in `cli/aster_cli/contract.py` `_preview_command`) should show inline params naturally.

## Languages to update

All five, in this order:
1. **Python** — reference implementation. Update `bindings/python/aster/decorators.py` (@rpc), `bindings/python/aster/server.py` (dispatch), `bindings/python/aster/runtime.py` (AsterServer dispatch). Test with `tests/python/test_aster_interceptors.py` and add new tests.
2. **TypeScript** — `bindings/typescript/packages/aster/src/` decorators + server dispatch.
3. **Java** — `bindings/java/src/main/java/com/aster/server/AsterServer.java` dispatch path.
4. **Go** — `bindings/go/server.go` dispatch path.
5. **.NET** — `bindings/dotnet/src/Aster/AsterServer.cs` dispatch path.

## Key files to read first

- `ffi_spec/handler-context-design.md` — the full design doc
- `bindings/python/aster/interceptors/base.py` — CallContext definition (Python)
- `bindings/python/aster/decorators.py` — current @rpc decorator
- `bindings/python/aster/server.py` — current server dispatch
- `bindings/python/aster/runtime.py` — AsterServer._dispatch_reactor_call

## Testing

- Existing Python tests must not break (998 passing as of tonight)
- Add tests for: Mode 2 inline params, CallContext injection, async-local access, mixed params (primitives + @wire_type), no-params methods, contract_id stability between Mode 1 and Mode 2 for equivalent schemas
- Run `uv run pytest tests/python/ -v --timeout=30` after Python changes
- Build-verify Java (`mvn compile -P fast`), Go (`go build ./...`), .NET (`dotnet build src/Aster/`)
