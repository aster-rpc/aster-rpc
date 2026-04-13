# Session Instructions: TypeScript Producer Introspection via TS Compiler API

**Status of the Mode 2 work this doc started as.** Python Mode 2 inline
request parameters shipped in commit `7fdbabb` (CallContext injection +
Mode 2 across Python decorators, dispatch, contract identity, CLI,
codegen, shell, MCP schema, and tests). This doc has been retargeted
because the TypeScript story is different from Python's and warrants a
different approach.

---

## Why TypeScript doesn't need Mode 2

In Python, Mode 2 lets producers skip the `@dataclass` + `@wire_type` +
annotated-fields-with-defaults boilerplate by writing handler methods
with inline positional parameters. The framework synthesizes the
request class under the hood.

In TypeScript, that boilerplate doesn't exist. Writing
`class GetStatusParams { agentId = ""; nonce = 0 }` is already
zero-ceremony idiomatic TS — exactly as terse as the Mode 2 aspirational
syntax in the design doc. There is no ergonomic gap to close. A TS
producer writing

```ts
class GetStatusParams { agentId = ""; nonce = 0 }

@Rpc()
async getStatus(p: GetStatusParams): Promise<StatusResponse> { ... }
```

already enjoys everything Mode 2 would give them. Porting Mode 2 to TS
would add runtime magic (either `Function.toString()` parsing or
`reflect-metadata` + experimental decorators) to solve a problem TS
producers don't actually have. Don't do it.

## The real TS gap: the `request:` option on `@Rpc` is scaffolding

Today `@Rpc({ request: GetStatusParams })` is required any time a
producer wants codegen/manifest publication to work, because TS erases
parameter types at runtime. The decorator has no way to see that the
handler's first parameter is `GetStatusParams` without being told
explicitly. That's the one piece of real friction left in the TS
producer experience — not Mode 2.

```ts
// Today (TS):
@Rpc({ request: GetStatusParams, response: StatusResponse })
async getStatus(p: GetStatusParams): Promise<StatusResponse> { ... }

// What we want (TS):
@Rpc()
async getStatus(p: GetStatusParams): Promise<StatusResponse> { ... }
```

The runtime can't close this gap — the types are gone. But the codegen
pipeline doesn't run at runtime; it runs at *build time*, where the
TypeScript source is still available and the compiler has full type
information. That's where we should solve it.

## The fix: TS Compiler API AST extraction in `aster contract gen`

Switch `aster contract gen` (and `gen-client`) to use the TypeScript
compiler API (`typescript` package, `ts.createProgram` → `TypeChecker`)
to read producer service classes directly from their `.ts` source
instead of importing and running them at runtime. This is how
`nestjs-swagger`, `class-validator-jsonschema`, `ts-json-schema-generator`,
and `tsoa` build typed metadata without runtime reflection.

### How it plumbs together

1. **Input** — `aster contract gen --service ./src/services/mission_control.ts:MissionControlService`
   (or a `tsconfig.json` + class name). The CLI no longer needs to
   `require()` or `ts-node` the file.
2. **Compiler setup** — load `tsconfig.json`, build a `ts.Program`,
   grab the `TypeChecker`.
3. **Class lookup** — walk the `SourceFile` AST looking for the class
   decorated with `@Service({...})` whose name matches the requested
   service.
4. **Method extraction** — for each `@Rpc`/`@ServerStream`/etc.
   decorated method on the class:
   - Read the method's parameters via `TypeChecker.getSignatureFromDeclaration`
   - For each parameter, resolve its type to a `ts.Type`, walk it, and
     emit a FieldDef list (primitives → wire type names, refs →
     `@WireType`-decorated classes already discovered in the program)
   - Read the return type, unwrap `Promise<T>`/`AsyncGenerator<T>`, and
     resolve to the response `@WireType` class
5. **Contract identity** — build the same `ServiceContract` / `TypeDef`
   graph the Python path already builds in
   `aster.contract.identity`, compute `contract_id` via the same
   BLAKE3 hashing pipeline. The hashes **must** match across
   languages, so the canonical-bytes layer stays shared — only the
   *introspection front end* is language-specific.
6. **Manifest output** — write the same JSON manifest shape
   `cli/aster_cli/contract.py` emits for Python producers. Consumers,
   codegen, shell, and MCP schema see no difference; they read a
   manifest and don't care which producer introspection path built it.

### What this lets us drop

- **`request?: new (...) => any` on `@Rpc`** — no longer needed.
  Producers write `@Rpc()` with typed parameters and the CLI reads
  the types from source. Keep the option around for one release as
  a deprecation alias, then delete.
- **`response?: new (...) => any` on `@Rpc`** — same.
- **Any runtime fallback that scans `module.__dict__` for
  `@WireType` classes** — all type discovery moves to build time.

### What this does NOT change

- The runtime dispatch path. `server.ts` / `session.ts` still read
  `methodInfo.handler` and invoke it. `acceptsCtx` detection stays on
  `Function.length`. CallContext injection stays exactly as shipped.
- The wire format. `contract_id`, type hashes, canonical bytes are
  unchanged.
- The Python producer path. Python producers still use the in-process
  `@service` scan — that's fast and works fine for a dynamically
  typed language. Only the TS entry point moves to build-time AST.

### Files to touch

- **New**: `cli/aster_cli/ts_introspect/` — a Python module that
  shells out to a tiny Node.js/Bun script shipped alongside the CLI.
  The script imports `typescript`, does the AST walk, and prints a
  JSON blob describing the service (name, version, methods, types).
  The Python side parses that JSON and hands it to the existing
  manifest builder.
  - Alternative: rewrite `aster contract gen` in TS for the TS code
    path. More work; probably not worth it right now.
- **Update**: `cli/aster_cli/contract.py` — add a `--lang ts` / file
  extension switch that routes to the AST introspector instead of the
  Python `import-the-service` path.
- **Update**: `cli/aster_cli/codegen_typescript.py` — no changes
  needed for this refactor itself; it already reads the manifest
  JSON, not runtime objects.
- **Update**: `bindings/typescript/packages/aster/src/decorators.ts`
  — mark `request` / `response` options as `@deprecated` with a
  JSDoc note pointing at the new path. Leave them functional for
  one release so producers can migrate.
- **Tests**: new integration test under `tests/typescript/` that
  runs `aster contract gen` against a fixture TS service and
  asserts the emitted manifest has the right `contract_id`,
  methods, and fields. Add a cross-language test: the same logical
  service written in Python vs TS should produce byte-identical
  `contract_id`.

### Why a separate Node script instead of calling TS compiler from Python

Two reasons:

1. **Version alignment.** The TS compiler API matches the version of
   `typescript` the producer is already using. A Node script can use
   whatever the producer's `tsconfig.json` + `package.json` pin, so
   introspection sees the same types the producer's own `tsc` sees.
   Python vendoring `typescript` would drift.
2. **Trivial to ship.** The CLI already spawns `bunx tsc` for TS
   client type-checking in tests. A `bunx tsx ./introspect.ts` call
   is the same pattern. No new dependency surface for the Python
   side beyond "there's a `node` or `bun` on PATH for TS producers."

### Order of work

1. Write the `introspect.ts` script standalone, against a hand-crafted
   fixture TS service, and verify the emitted JSON matches what the
   Python introspection path emits for an equivalent Python service.
2. Wire it into `aster contract gen` behind a `--lang ts` flag.
3. Add cross-language `contract_id` equivalence test.
4. Deprecate `request` / `response` on `@Rpc`.
5. Update `bindings/typescript/packages/aster/examples/` (if any) and
   the README to show the new `@Rpc()`-only form.

### Out of scope

- Porting Mode 2 inline request params to TS. Not needed. TS
  producers write classes.
- Switching TS decorators from Stage 3 back to experimental so
  `reflect-metadata` could work. Stage 3 is the right long-term
  choice; we're solving this at build time instead.
- Any changes to the Python introspection path — it stays as-is.

## Spec / doc cross-references

- `ffi_spec/handler-context-design.md` — design for Mode 2 (Python)
  and CallContext injection (both langs). Mark the TS Mode 2 section
  as "not pursued — see session-instructions-mode2-inline-params.md"
  when you get to it.
- `ffi_spec/Aster-ContractIdentity.md` §11.3.2.3 — language mapping
  table. The TS AST introspector uses this same table to resolve TS
  `ts.Type` nodes to wire type names.
- Commit `7fdbabb` — reference for what Python already has.
