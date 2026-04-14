# Mission Control Example (TypeScript)

A control plane for managing remote agents, demonstrating all four Aster
RPC patterns, session-scoped services, and capability-based auth.

**Full walkthrough on the docs site:** [Mission Control quickstart](https://docs.aster.site/docs/quickstart/mission-control) (switch to the TypeScript tab) — seven chapters covering all four RPC patterns, session-scoped services, capability-based auth, contract gen-client, and cross-language interop. The code in this directory is the runnable TypeScript implementation that the walkthrough builds up to chapter by chapter.

## Quick start

```bash
# Install dependencies
npm install

# Generate type metadata (required before first run)
npx aster-gen

# Start the server
node server.ts

# In another terminal: connect an agent
node agent.ts aster1...

# In another terminal: tail logs
node operator.ts aster1...
```

The `npx aster-gen` step reads your `tsconfig.json`, discovers all
`@Service` / `@WireType` decorated classes, and emits `aster-rpc.generated.ts`
with wire type metadata, field shapes, and contract identity hashes.
Re-run it after adding or changing wire types or RPC methods.

See the [TypeScript Build Setup](https://docs.aster.site/docs/guides/typescript-build-setup)
guide for Vite/Webpack plugin integration and other options.

## Cross-language (Chapter 7)

The wire types use the same `mission/` tags as the Python example.
Start the TypeScript server, then connect with a Python client:

```bash
# TS server
npx aster-gen && node server.ts

# Python client
aster call aster1... MissionControl.getStatus '{"agent_id": "py-worker"}'
```

## With auth (Chapter 5)

```bash
aster trust keygen --out-key ~/.aster/root.key
ASTER_ROOT_PUBKEY_FILE=~/.aster/root.pub node server.ts --auth
```

## Files

| File | Purpose |
|------|---------|
| `types.ts` | Wire types (request/response classes) |
| `roles.ts` | Capability roles (Chapter 5) |
| `services.ts` | Services without auth (Chapters 1-4) |
| `services-auth.ts` | Services with requires= (Chapter 5) |
| `server.ts` | Producer entry point |
| `agent.ts` | Example agent (unary + client streaming) |
| `operator.ts` | Example operator (server streaming) |
| `aster-rpc.generated.ts` | Auto-generated metadata (run `npx aster-gen`) |
| `tsconfig.json` | TypeScript config (used by aster-gen scanner) |
