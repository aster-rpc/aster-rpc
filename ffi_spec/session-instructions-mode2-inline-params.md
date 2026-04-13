# Session Instructions: Mode 2 Inline Request Parameters + gen-client

Paste this as the opening message of a new Claude Code session to resume the
work deferred from `session-instructions-handler-context.md`.

---

The handler-context session (see previous commit) shipped Path 1 + Path 2
of `ffi_spec/handler-context-design.md` (explicit `CallContext` parameter
injection and `CallContext.current()` async-local) for **Python** and
**TypeScript**. Java / Go / .NET were marked out of scope because those
bindings don't yet have a method-level `@Rpc` dispatch framework.

**This session picks up the second half of that design doc: inline request
parameters (Mode 2) and the matching `gen-client` / manifest updates.** Read
`ffi_spec/handler-context-design.md` §"Inline Request Parameters" fully
before starting — it contains the mode-detection rules, the wire-type
synthesis rules, examples per language, and the contract-identity
equivalence proof.

## What to implement

### 1. Python @rpc Mode 2 detection and wire-type synthesis

At `@service` decoration time, for each `@rpc` method:

- Inspect the signature (minus `self` and minus any `CallContext` params —
  the latter is already filtered by `_extract_types_from_signature` in
  `bindings/python/aster/decorators.py`).
- Classify the method into one of three modes:
  - **EXPLICIT (Mode 1)** — exactly one param that is a `@wire_type` class.
    Pass-through, existing behavior.
  - **NO_REQUEST** — zero params. Synthesize an empty `{MethodName}Request`.
  - **INLINE (Mode 2)** — any other combination. Synthesize a
    `{MethodName}Request` wire type from the params.
- For INLINE/NO_REQUEST, synthesize a dataclass at decoration time:
  - Name: `{MethodName in PascalCase}Request`
  - Package: same as the service's package (`aster.contract.identity`
    package helpers already handle this)
  - Fields: one per parameter, names match, types resolved via the language
    mapping table (§11.3.2.3 in `ffi_spec/Aster-ContractIdentity.md`)
  - Field IDs: NFC-name-sorted (same rule as explicit `@wire_type`)
  - Register it with the contract identity system so `contract_id` matches
    an equivalent explicit class.
- Wrap the handler so dispatch still receives a single `request` argument:
  the wrapper unpacks `request.field_name` into inline positional args
  before invoking the user's handler. Keep `CallContext` injection working
  orthogonally (Path 1 from the previous session).
- Add a `request_style: "inline" | "explicit"` field on
  `MethodInfo` (`bindings/python/aster/service.py`) so downstream code can
  distinguish.

**Contract-identity equivalence test** — a producer with

```python
@rpc
async def get_status(self, agent_id: str) -> StatusResponse: ...
```

must produce the same `contract_id` for `GetStatusRequest` as

```python
@wire_type("mission/GetStatusRequest")
@dataclass
class GetStatusRequest:
    agent_id: str = ""
```

### 2. Python dispatch layer unpacking

`server.py`, `session.py`, and `transport/local.py` currently call
`invoke_handler_with_ctx(handler_method, request, ctx, accepts_ctx)`. For
Mode 2 methods, the wire request (the synthesized class) needs to be
unpacked into positional arguments before calling the user's handler. The
cleanest place to do this is inside `invoke_handler_with_ctx` by consulting
`method_info.request_style` — but that requires passing `method_info`
through, not just `accepts_ctx`. Alternative: install a small adapter
wrapper around the user's handler at decoration time so the dispatch layer
doesn't need to know about modes.

Prefer the adapter-at-decoration-time approach — it keeps dispatch simple
and means session/local/reactor all benefit without further changes.

### 3. TypeScript mirror

Same work in `bindings/typescript/packages/aster/src/decorators.ts` +
`server.ts` + `session.ts` + `transport/local.ts`. Note the TS runtime
constraints:

- TS erases parameter types at runtime, so Mode 2 detection must rely on
  something else. Options:
  - Require an explicit `request?: new (...) => any` in `@Rpc({...})` — if
    absent, treat as Mode 2 and synthesize from parameter *names* via
    `Function.toString()` parsing (fragile) or force an explicit
    `params: { name: "agent_id", type: "string" }[]` option.
  - Accept that Mode 2 is not viable in TS without a schema hint and
    document that producers need to use `@Rpc({ request: InlineSchema })`
    where `InlineSchema` is a class with field declarations.
- Whichever path: reach alignment with the user before implementing — this
  is a design decision, not a mechanical port.

### 4. `aster contract gen` — add `request_style` to the manifest

In `cli/aster_cli/contract.py` (the `_build_manifest` / `_method_dict`
helpers), add a `request_style` field to each method descriptor with value
`"inline"` or `"explicit"`. For inline mode, also include the parameter
list (name + wire type) so generated clients can emit matching signatures.

### 5. `aster contract gen-client` — emit inline or explicit clients

`cli/aster_cli/codegen.py` (Python client codegen) and
`cli/aster_cli/codegen_typescript.py` (TS client codegen) must branch on
`request_style`:

- **`"explicit"`** — existing behavior: emit a client method taking the
  request class.
- **`"inline"`** — emit a client method with matching inline params that
  internally constructs the synthesized request object:

  ```python
  async def get_status(self, agent_id: str) -> StatusResponse:
      return await self._invoke("getStatus", GetStatusRequest(agent_id=agent_id), StatusResponse)
  ```

### 6. `aster contract preview` — show inline params naturally

`_preview_command` in `cli/aster_cli/contract.py` should render inline
methods as `get_status(agent_id: str) -> StatusResponse` rather than
`get_status(req: GetStatusRequest) -> StatusResponse`.

### 7. Shell inspection + call paths — render inline signatures everywhere

Anywhere the shell or CLI shows a user the shape of a method, the display
must match what the producer actually wrote. A Mode 2 producer should
appear as `get_status(agent_id: str)` in every surface, not as an opaque
`GetStatusRequest` blob. Audit and update:

- **`cli/aster_cli/shell/commands.py`** — the `describe` / `inspect` /
  `services` commands currently read `metadata.get("request_type", "")`
  as a string and display it verbatim (see lines ~1182, 1191, 1558).
  For `request_style == "inline"`, render the param list instead of the
  synthesized request class name.
- **`cli/aster_cli/shell/completer.py`** — tab-completion at line ~152
  uses `request_type` to hint at the next argument; for inline methods it
  should hint the first unset inline param name and its type.
- **`cli/aster_cli/shell/app.py`** — the service browser pane (~645,
  ~1083, ~1094) builds `{"name": request_type, "hash": ...}` entries for
  the UI. For inline methods this should reflect the synthesized
  `{Method}Request` for contract-identity purposes **but** the rendered
  signature must show inline params — these are two separate concerns;
  don't conflate them.
- **`cli/aster_cli/shell/invoker.py`** — the call path at line ~28/43
  builds a `request_fields` list from the manifest. For inline methods
  it must read the new inline param list (from §4 above) and accept
  inline positional/keyword arguments at the shell prompt, then construct
  the synthesized request object before sending the call. A shell user
  typing `call mission.get_status agent_id=foo` on a Mode 2 method must
  work identically to calling a Mode 1 method with a single
  `GetStatusRequest(agent_id="foo")` arg.
- **`cli/aster_cli/shell/hooks.py`** — `MethodSchema.request_fields` at
  line ~134/197/202 iterates expected fields to prompt/validate user
  input. For inline methods the "fields" are the inline params directly;
  the hook contract doesn't need to change, only the data that feeds it.
- **`cli/aster_cli/mcp/schema.py`** — the MCP tool schema exposed to
  external agents must describe inline-style methods with top-level
  parameters on the tool, not a nested `req` object, so agents generate
  sensible tool calls.

The test for this is simple: take the `MissionControl` service from
`tests/python/test_codegen_e2e.py`, convert one of its methods to Mode 2,
and verify that `aster services describe MissionControl` + `aster call
mission.get_status agent_id=foo` still work end-to-end and show the
inline signature in all display surfaces.

## Key files to read first

- `ffi_spec/handler-context-design.md` — full design (both halves)
- `ffi_spec/Aster-ContractIdentity.md` §11.3.2.3 — language mapping table
- `bindings/python/aster/decorators.py` — @rpc decorator; the previous
  session wired `CallContext` detection here and added `accepts_ctx`
- `bindings/python/aster/contract/identity.py` — wire-type registration +
  `contract_id` computation
- `cli/aster_cli/contract.py` — manifest generation + preview
- `cli/aster_cli/codegen.py` / `codegen_typescript.py` — current client
  generators

## Testing

- **Mode 2 dispatch** — Python handler with `(self, agent_id: str)`,
  Python client calling it, both via `LocalTransport` and `AsterServer`
- **Mixed params** — `(self, agent_name: str, config: AgentConfig)` where
  `AgentConfig` is a `@wire_type` class (tests the REF-type path)
- **No-request** — `(self)` and `(self, ctx: CallContext)` both synthesize
  an empty `{Method}Request`
- **Contract-identity equivalence** — compute `contract_id` of an
  explicit `GetStatusRequest` and of the synthesized one from a
  Mode 2 handler; assert equal
- **Wire interop** — a Mode 2 Python producer + an explicit-style Python
  consumer (using the generated client) must round-trip
- **Cross-language interop** — Python Mode 2 producer + generated TS
  client; verify via `tests/python/test_codegen_e2e.py` style
- Regression: existing tests must not break (1014 passing + 4
  handler-context tests as of the previous session)

## Commands

```bash
./scripts/build.sh
uv run pytest tests/python/ -v --timeout=30
cd bindings/typescript/packages/aster && bun run test
```

## What is NOT in scope

- Java / Go / .NET inline params — same reason as before, no method-level
  dispatch framework to hook into
- Any changes to `CallContext` itself — that layer is done
