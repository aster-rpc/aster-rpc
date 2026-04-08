# How to Regenerate the `AGENTS_aster.md` CLI Template

This document explains how to regenerate or update the LLM instruction template
that `aster init --ai` writes into user projects.

## What is it?

`aster init --ai` writes an `AGENTS_aster.md` file into the user's project
directory. This file teaches AI coding assistants (Claude Code, Cursor, Copilot,
etc.) how to build Aster services correctly.

The source template lives at:
```
cli/aster_cli/templates/llm/python.md
```

One template per language. Currently only Python exists. TypeScript, Go, etc.
will follow as those bindings ship.

## When to regenerate

Regenerate the template whenever:

- The Aster Python API changes (new decorators, new parameters, renamed types)
- New Iroh primitives are exposed via the Python bindings
- The AsterServer/AsterClient high-level API changes
- The CLI gains new commands relevant to service development
- Common mistakes change (e.g. a footgun is fixed, or a new one emerges)
- A new serialization mode or wire_type feature is added

## What the template must cover

The template is the single source of truth an LLM has for writing Aster code.
It must cover:

1. **Data types**: `@wire_type` tag format, dataclass rules, supported field types
   (int, float, str, bool, bytes, Enum/IntEnum, list, dict, Optional, set, nested
   dataclasses), default value requirements, mutable defaults pitfall

2. **Service definition**: `@service` decorator and its parameters (name, version,
   serialization, scoped), all four RPC patterns (@rpc, @server_stream,
   @client_stream, @bidi_stream) with correct type hints, method options
   (timeout, idempotent), session-scoped services

3. **Error handling**: RpcError, StatusCode, when to raise what

4. **Server (producer)**: AsterServer with persistent storage, identity files,
   ASTER_STORAGE_PATH, data directory configuration

5. **Client (consumer)**: AsterClient, typed stubs, streaming call patterns

6. **Serialization modes**: XLANG vs NATIVE vs ROW, when tags are required

7. **Iroh primitives** (with persistent examples):
   - Blobs: add, read, tags, tickets, GC rules
   - Docs: create, write, read, query, share, sync, live events, download policy
   - Gossip: subscribe, broadcast, recv, lagged handling
   - QUIC networking: endpoints, connections, streams, datagrams

8. **Common mistakes table**: every known footgun

9. **CLI quick reference**: key commands for development workflow

## How to regenerate

### Approach 1: Edit the template directly

The template is plain markdown with one placeholder: `{{aster_version}}` (replaced
at runtime by the installed package version).

Edit `cli/aster_cli/templates/llm/python.md` directly. Keep the structure and
ensure all sections above are covered.

### Approach 2: Ask Claude Code to regenerate

Use this prompt (or similar) in a Claude Code session in this repo:

```
Regenerate the Aster LLM template at cli/aster_cli/templates/llm/python.md.

Read these sources to build an accurate template:
- bindings/python/python/aster/__init__.py (public API exports)
- bindings/python/python/aster/decorators.py (service/rpc decorators)
- bindings/python/python/aster/codec.py (wire_type, serialization)
- bindings/python/python/aster/high_level.py (AsterServer, AsterClient)
- bindings/python/python/aster/config.py (AsterConfig, env vars)
- bindings/python/rust/src/*.rs (PyO3 bindings for blobs, docs, gossip, net)
- bindings/python/python/aster/_aster.pyi (type stubs)
- tests/python/ (usage examples)
- aster-docs/docs/ (published documentation)
- docs/_internal/Iroh-API-Docs.md (Iroh API reference)

Follow the structure and coverage requirements in this file
(docs/_internal/HOW_TO_REGEN_CLI_TEMPLATE.md).

The template must use persistent nodes (not in-memory) in all main examples,
reference ASTER_STORAGE_PATH for data directory configuration, and include
Enum/IntEnum in the supported types list.
```

### Approach 3: Add a new language

1. Create `cli/aster_cli/templates/llm/<language>.md`
2. Follow the same structure as `python.md` but with language-appropriate syntax
3. The CLI already supports `aster init --ai --language <name>` — it picks up
   new templates automatically from the templates directory

## Testing

After editing, verify:

```bash
# Template loads and renders
cd /tmp && rm -f AGENTS.md AGENTS_aster.md
aster init --ai
cat AGENTS_aster.md | head -5   # check version stamp

# Idempotent
aster init --ai
wc -l AGENTS.md                 # should still be 1 line

# Check all sections present
grep -c "^## " AGENTS_aster.md  # should match expected section count
```

## File locations

| File | Purpose |
|------|---------|
| `cli/aster_cli/templates/llm/python.md` | Python template source |
| `cli/aster_cli/init.py` | `aster init --ai` command implementation |
| `cli/aster_cli/contract.py` | CLI entrypoint (wires up `init` subparser) |
| `cli/pyproject.toml` | `package-data` ensures templates are bundled |
| `docs/_internal/HOW_TO_REGEN_CLI_TEMPLATE.md` | This file |
