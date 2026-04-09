# Day 0 Launch Plan

**Status:** Active  
**Date:** 2026-04-09  
**Scope:** Everything in `examples/mission-control/GUIDE.md` works flawlessly.  
**Non-scope:** Java FFI, MCP server, marketplace, aster.site, HA, anything not
in the guide.

## The objective

A developer reads the Mission Control guide, follows every command, and
nothing breaks. Seven chapters, four RPC patterns, two service scopes,
auth, client generation, and cross-language interop. That's Day 0.

## Current state

The foundations are built from specs. The bumps are at the seams —
where independently-correct subsystems meet real usage for the first
time. The work is not "build new things" but "verify the existing things
compose correctly."

## Strategy: automated verification, not manual testing

Stop burning LLM tokens on exploratory debugging. Instead:

1. **Write the test that proves each chapter works**
2. **Run the tests in CI**
3. **Fix what fails**
4. **Repeat until green**

Every hour spent on automated tests saves ten hours of manual debugging.

---

## Phase 1: Guide conformance test suite (highest priority)

Create `tests/python/test_guide_chapters.py` — one test per chapter of
the Mission Control guide. Each test is a self-contained integration test
that does exactly what the guide says a user would do.

### Chapter 1: First service + shell invocation

```
- Start AsterServer with MissionControl service
- Connect AsterClient
- Call getStatus via generated client
- Assert response fields match
```

### Chapter 2: Server streaming (tailLogs)

```
- Start server with log queue
- Submit a log entry via submitLog
- Open tailLogs stream, receive the entry
- Assert entry content matches
```

### Chapter 3: Client streaming (ingestMetrics)

```
- Start server
- Stream 100 MetricPoints via ingestMetrics
- Assert IngestResult.accepted == 100
```

### Chapter 4: Session-scoped service (AgentSession)

```
- Start server with AgentSession (scoped="session")
- Connect, call register with Heartbeat
- Assert Assignment returned
- Call heartbeat, assert response
- Open runCommand bidi stream, send a command, receive result
```

### Chapter 5: Auth + capabilities

```
- Generate root key
- Start server with allow_all_consumers=False
- Enroll consumer with ops.status capability
- Connect with credential, call getStatus (should succeed)
- Call tailLogs (should fail — missing ops.logs)
```

### Chapter 6: Client generation

```
- Start server
- Run aster contract gen-client against the server
- Import the generated client module
- Call a method via the generated client
- Assert response
```

### Chapter 7: Cross-language (TypeScript)

```
- Start Python server
- Connect TypeScript client (proxy mode)
- Call getStatus from TypeScript
- Assert response matches
```

**Framework:** pytest-asyncio, using the existing `conftest.py` fixtures
(`node`, `node_pair`). Each test starts its own `AsterServer` in-process
— no external processes to manage.

**Goal:** All 7 tests green in CI. When they are, Day 0 is done.

---

## Phase 2: Automated quality gates (run in CI)

### 2a. Type checking

```bash
pyright bindings/python/aster/ cli/aster_cli/
```

Already installed. Catches type mismatches like the `dict` vs `object`
bug, generic aliases, wrong argument counts. Run it.

### 2b. Dead code detection

```bash
vulture bindings/python/aster/ cli/aster_cli/ --min-confidence 80
```

Already installed. Catches unused functions that accumulate as the code
evolves. The shell command classes are false positives (decorator
registration) — add a whitelist file.

### 2c. Import verification

```bash
python -c "from aster import AsterServer, AsterClient, service, rpc, server_stream, client_stream, bidi_stream, wire_type"
```

One-liner that proves the public API imports. If the native module isn't
built, if a circular import exists, if a dependency is missing — this
catches it.

### 2d. Generated client compilation

```bash
# Part of the Chapter 6 test, but also a standalone check:
aster contract gen-client <test-server-ticket> --out /tmp/gen-test --package test_client
python -m py_compile /tmp/gen-test/test_client/**/*.py
```

### 2e. Test coverage

```bash
pytest tests/python/ --cov=aster --cov-report=term-missing --timeout=30
```

Already have coverage.py installed. Don't aim for 100% — aim for "every
code path in the guide is covered." The guide IS the coverage target.

### 2f. Validate script

Update `scripts/validate.sh` to run all of the above:

```bash
#!/bin/bash
set -euo pipefail

echo "=== Build ==="
./scripts/build.sh

echo "=== Import check ==="
python -c "from aster import AsterServer, AsterClient"

echo "=== Type check ==="
pyright bindings/python/aster/ cli/aster_cli/ || true  # warn, don't block yet

echo "=== Dead code ==="
vulture bindings/python/aster/ --min-confidence 80

echo "=== Tests ==="
pytest tests/python/ -v --timeout=30

echo "=== Done ==="
```

---

## Phase 2b: Type and structure conformance tests

The bugs we hit tonight all shared a pattern: a type that works in
isolation fails when it crosses a subsystem boundary (manifest
extraction, Fory serialization, codegen, dynamic invocation). These
tests catch that class of bug systematically.

Create `tests/python/test_type_conformance.py` — each test defines a
service with a specific type pattern, starts a server, connects a
client, and verifies the round-trip.

### Primitive field types

```
@wire_type @dataclass with fields: str, int, float, bool, bytes
→ server returns populated instance
→ client receives matching values
```

Catches: basic Fory registration, field ordering.

### Nested dataclass fields

```
@dataclass Outer:
    inner: Inner = Inner()
@dataclass Inner:
    value: str = ""
→ round-trip Outer with populated Inner
```

Catches: nested type registration, Fory recursive serialization.

### list[T] where T is a @wire_type dataclass

```
@dataclass Result:
    items: list[Item] = field(default_factory=list)
@dataclass Item:
    name: str = ""
→ round-trip Result with 3 Items
```

Catches: Fory type hash for parameterized lists, element type
registration, the `list` vs `list[Item]` bug.

### dict[str, T] fields

```
@dataclass Config:
    settings: dict[str, str] = field(default_factory=dict)
→ round-trip with populated dict
```

Catches: dict serialization, Fory map handling.

### Optional fields

```
@dataclass Partial:
    name: str = ""
    nickname: Optional[str] = None
→ round-trip with nickname=None, then with nickname="Bob"
```

Catches: optional field handling, None serialization.

**Important:** pyfory does NOT support PEP 604 union syntax (`str | None`)
in `@wire_type` dataclass field annotations. Always use `Optional[str]`
from `typing`. This applies to both server-side type definitions and
generated client types. The codegen handles this automatically, but
developers authoring `@wire_type` classes must use `Optional[X]`.
This should be documented prominently in the `@wire_type` API docs.

### Generic wrapper types (SignedRequest[T] pattern)

```
@wire_type("test/Wrapper")
@dataclass
class Wrapper(Generic[T]):
    payload: str = ""
    signature: str = ""

Method signature: request: Wrapper[Payload] -> Result
→ manifest extraction finds wire_tag on Wrapper
→ isinstance check doesn't fail on Wrapper[Payload]
→ round-trip works
```

Catches: the `Subscripted generics` isinstance bug, generic unwrapping
in manifest extraction, `_unwrap_generic` in codec decode.

### Forward reference response types

```
Service in module A imports types from module B.
Method returns a type defined in module B.
from __future__ import annotations is active.
→ manifest extraction resolves the string "ResultType" to the class
→ response_wire_tag is populated (not empty)
→ response_fields are populated
```

Catches: the `JoinResult` resolution bug where forward references
from `__future__` annotations weren't resolved across modules.

### Enum fields

```
class Status(str, Enum):
    ACTIVE = "active"
    INACTIVE = "inactive"

@dataclass Response:
    status: Status = Status.ACTIVE
→ round-trip with enum value
→ cross-language: TS sends "active" string, Python receives Status.ACTIVE
```

Catches: enum coercion in codec decode, cross-language enum interop.

### Empty dataclass (no fields)

```
@wire_type("test/Empty")
@dataclass
class Empty:
    pass
→ round-trip works
→ codegen produces valid class
```

Catches: edge case in Fory, codegen for stub types.

### Large field count (10+ fields)

```
@dataclass BigType:
    field1: str = ""
    field2: int = 0
    ... (12 fields)
→ round-trip preserves all fields
→ codegen produces correct dataclass
```

Catches: field ordering bugs, manifest truncation.

### Session-scoped service with state

```
@service(scoped="session")
class Stateful:
    def __init__(self):
        self._count = 0
    @rpc
    async def increment(self, req) -> CountResponse:
        self._count += 1
        return CountResponse(count=self._count)

→ Two clients connect, each increments 3 times
→ Client A sees 1, 2, 3
→ Client B sees 1, 2, 3 (independent state)
```

Catches: session isolation, per-connection instance lifecycle.

### Server streaming with typed elements

```
@server_stream
async def stream_items(self, req) -> AsyncIterator[Item]:
    for i in range(5):
        yield Item(name=f"item-{i}")
→ client receives 5 typed Item objects (not dicts)
```

Catches: streaming + Fory decode per frame, element type registration.

### Client streaming with aggregation

```
@client_stream
async def aggregate(self, stream) -> Summary:
    count = 0
    async for item in stream:
        count += 1
    return Summary(count=count)
→ client sends 10 items, receives Summary(count=10)
```

Catches: client stream framing, final response decode.

### Bidi streaming round-trip

```
@bidi_stream
async def echo(self, stream):
    async for item in stream:
        yield Response(echo=item.message)
→ client sends 3, receives 3 matching responses
```

Catches: bidi channel lifecycle, concurrent send/receive.

### Codegen round-trip (manifest → generate → call)

```
→ Start server with a service using all patterns
→ Run aster contract gen-client against it
→ Import the generated client
→ Call each method through the generated client
→ Assert responses match
```

Catches: the full codegen pipeline — manifest extraction, type
classification, code generation, codec registration, transport wiring.

### Cross-language codec compatibility

```
→ Python server encodes a complex type (nested, list, optional, enum)
→ Save the raw bytes
→ TypeScript client decodes the same bytes
→ Assert field values match
→ And the reverse direction
```

Catches: Fory xlang wire format mismatches between Python and
TypeScript. This is the ultimate interop test.

---

## Phase 3: Known bugs to fix (from tonight's session)

These are the specific issues we hit. Each should be a targeted fix
with a regression test.

### P1 — Blocks guide chapters

| Bug | Chapter | Status |
|-----|---------|--------|
| Generic type isinstance check in codec decode (`Subscripted generics`) | 5 (auth) | Fixed |
| Fory type hash mismatch for `list[T]` fields in generated types | 6 (gen) | Fixed |
| Generated client codec not shared with transport | 6 (gen) | Fixed |
| `SignedRequest[T]` wire_tag missing from manifest | 5, 6 | Fixed |
| Shell methods showing 0 (VFS loaded flag + dict vs object) | Shell | Fixed |
| Doc sync requires `share()` call on server to start engine | Shell | Fixed |

### P2 — Should fix before launch

| Bug | Impact |
|-----|--------|
| Types without wire_tags generate as empty stubs (`class Foo: pass`) | Generated client can't call signed methods |
| `ServiceSummary` wire format camelCase/snake_case mismatch (TS ↔ Python) | Cross-language interop |
| `list[T]` in dynamic types (shell invoke) still uses bare `list` when manifest lacks `element_wire_tag` | Shell invocation of methods returning lists |
| Blob download via ticket still broken (bypassed with `download_collection_hash`) | Dead code in ArtifactRef.ticket field |

### P3 — Nice to have

| Bug | Impact |
|-----|--------|
| Doc sync takes 15-20s before entries arrive | Slow shell startup |
| No endpoint TTL reaper in @aster service | Stale endpoints in discovery |
| `HealthServer` has zero callers | Dead code or missing test |

---

## Phase 4: What NOT to do

- **Do not add features.** The scope is the guide. Nothing else.
- **Do not refactor for elegance.** If it works, ship it.
- **Do not chase 100% coverage.** Cover the guide paths.
- **Do not debug manually.** Write a test, run the test, fix what fails.
- **Do not spend tokens on exploration.** Use automated tools (pyright,
  vulture, pytest) to find issues. Use LLMs only for targeted fixes
  once you know what's broken.

---

## Execution order

```
Week 1:
  Day 1: Write test_guide_chapters.py (chapters 1-4)
  Day 2: Fix whatever fails. Write chapters 5-6 tests.
  Day 3: Fix whatever fails. Write chapter 7 test (TS interop).
  Day 4: Wire validate.sh. Run full suite. Fix stragglers.
  Day 5: Run the guide end-to-end manually one time. Ship.
```

The guide IS the spec. The tests ARE the acceptance criteria. When the
tests pass, Day 0 is done.

---

## The city metaphor

The foundations are solid — built from specs, not vibes. The plumbing
works (QUIC, contract identity, trust model, Fory serialization). The
buildings are standing (framework, CLI, shell, @aster service). Day 0
is planting trees and painting crosswalks so residents don't trip on
their first walk through town. That's what automated tests do — they
walk every path before the residents arrive.
