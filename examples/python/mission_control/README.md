# Mission Control Example

A control plane for managing remote agents, demonstrating all four Aster
RPC patterns, session-scoped services, and capability-based auth.

**Full walkthrough on the docs site:** [Mission Control quickstart](https://docs.aster.site/docs/quickstart/mission-control) — seven chapters covering all four RPC patterns, session-scoped services, capability-based auth, contract gen-client, and cross-language interop. The code in this directory is the runnable Python implementation that the walkthrough builds up to chapter by chapter.

## Quick start

```bash
# Terminal 1: start the server
cd examples/python
python -m mission_control

# Terminal 2: connect an agent
python -m mission_control.agent aster1...

# Terminal 3: tail logs
python -m mission_control.operator aster1...
```

## With auth (Chapter 5)

```bash
# Generate keys
aster trust keygen --out-key ~/.aster/root.key

# Start with auth
ASTER_ROOT_PUBKEY_FILE=~/.aster/root.pub python -m mission_control --auth
```

## Files

| File | Purpose |
|------|---------|
| `types.py` | Wire types (request/response dataclasses) |
| `roles.py` | Capability roles (Chapter 5) |
| `services.py` | Services without auth (Chapters 1-4) |
| `services_auth.py` | Services with requires= (Chapter 5) |
| `server.py` | Producer entry point |
| `agent.py` | Example agent (unary + client streaming) |
| `operator.py` | Example operator (server streaming) |
