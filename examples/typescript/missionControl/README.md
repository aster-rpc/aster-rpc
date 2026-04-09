# Mission Control Example (TypeScript)

A control plane for managing remote agents, demonstrating all four Aster
RPC patterns, session-scoped services, and capability-based auth.

See the full walkthrough: [GUIDE_TypeScript.md](../../mission-control/GUIDE_TypeScript.md)

## Quick start

```bash
# Terminal 1: start the server
cd bindings/typescript
bun run ../../examples/typescript/missionControl/server.ts

# Terminal 2: connect an agent
bun run ../../examples/typescript/missionControl/agent.ts aster1...

# Terminal 3: tail logs
bun run ../../examples/typescript/missionControl/operator.ts aster1...
```

## Cross-language (Chapter 7)

The wire types use the same `mission/` tags as the Python example.
Start the TypeScript server, then connect with a Python client:

```bash
# TS server
bun run server.ts

# Python client
aster call aster1... MissionControl.getStatus '{"agent_id": "py-worker"}'
```

## With auth (Chapter 5)

```bash
aster trust keygen --out-key ~/.aster/root.key
ASTER_ROOT_PUBKEY_FILE=~/.aster/root.pub bun run server.ts --auth
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
