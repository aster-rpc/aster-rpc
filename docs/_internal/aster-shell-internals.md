# Aster Shell — Internal Developer Guide

This document explains how the interactive `aster shell` works internally, how to extend it with new commands, hooks, and tours, and how the pieces fit together.

**Audience**: Developers building on the shell — adding commands, wiring the LLM plugin, building language-specific client generators, or integrating session-scoped services.

## Quick Start

```bash
# Install deps
uv pip install -e cli/

# Launch demo mode (no live peer needed)
aster shell --demo

# Run shell tests
uv run pytest tests/python/test_shell.py -v
```

## Architecture Overview

```
aster shell <peer> [--rcan creds.json]
       │
       ▼
   app.py::launch_shell()
       │
       ├── Connect to peer (PeerConnection) or use DemoConnection
       ├── Build VFS tree (vfs.py::build_root)
       ├── Pre-populate from connection (_populate_from_connection)
       ├── Init GuideManager if first_time (guide.py)
       ├── Create PromptSession (prompt_toolkit REPL)
       │
       └── REPL loop:
           ├── Read input (prompt_async with ShellCompleter)
           ├── Tokenize (shlex)
           ├── Resolve command (plugin registry or direct method invocation)
           ├── Execute command (ShellCommand.execute)
           ├── Fire guide events
           └── Handle Ctrl+C (double-tap to exit)
```

### Module Responsibilities

| Module | Lines | Role |
|--------|-------|------|
| `plugin.py` | ~180 | Plugin base class, registry, CLI noun-verb mapping |
| `vfs.py` | ~225 | Virtual filesystem tree, path resolution, lazy population |
| `display.py` | ~250 | Rich terminal output (tables, JSON, trees, progress) |
| `commands.py` | ~760 | All built-in commands (13 registered) |
| `invoker.py` | ~370 | Dynamic RPC invocation with hook integration |
| `completer.py` | ~155 | Context-aware tab completion |
| `guide.py` | ~230 | First-time guided tour system |
| `hooks.py` | ~255 | Extension hook protocols (InputBuilder, OutputRenderer, SessionHook) |
| `app.py` | ~610 | REPL loop, connection adapters, CLI wiring |

## Plugin System

### How Commands Work

Every shell command is a subclass of `ShellCommand`:

```python
from aster_cli.shell.plugin import ShellCommand, register, CommandContext, Argument

@register
class MyCommand(ShellCommand):
    name = "mycommand"
    description = "Does something useful"
    contexts = ["/services/*"]  # only valid inside a service dir
    cli_noun_verb = ("service", "mycommand")  # → aster service mycommand

    def get_arguments(self) -> list[Argument]:
        return [Argument(name="target", description="What to act on", positional=True)]

    def get_completions(self, ctx: CommandContext, partial: str) -> list[str]:
        # Return suggestions for tab completion
        return ["option1", "option2"]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        ctx.display.info(f"Running mycommand with {args}")
```

The `@register` decorator instantiates the class and adds it to the global registry. Commands are discovered at import time — `app.py` imports `commands.py` which triggers all `@register` decorators.

### CommandContext

Every command receives a `CommandContext` with:

| Field | Type | Description |
|-------|------|-------------|
| `vfs_cwd` | `str` | Current VFS path (mutable — `cd` updates this) |
| `vfs_root` | `VfsNode` | Root of the VFS tree |
| `connection` | `Any` | Connection adapter (PeerConnection or DemoConnection) |
| `display` | `Display` | Rich output instance |
| `peer_name` | `str` | Display name of connected peer |
| `interactive` | `bool` | `True` in shell, `False` from CLI |
| `raw_output` | `bool` | `True` when `--json` flag used |
| `guide` | `GuideManager` | Guided tour manager (may be disabled) |

### Adding a New Command

1. Add a class in `commands.py` (or create a new file and import it in `app.py`)
2. Decorate with `@register`
3. Set `cli_noun_verb` if it should be callable as `aster <noun> <verb>`
4. Add to `COMMANDS.md`

### Context Matching

`contexts` is a list of glob patterns matched against the current VFS path:

```python
contexts = []                    # global — valid everywhere
contexts = ["/blobs"]            # only at /blobs
contexts = ["/services/*"]       # at /services/<anything>
contexts = ["/services", "/services/*"]  # at /services or any child
```

### CLI Mapping

Setting `cli_noun_verb = ("blob", "cat")` means:
- Interactive: `cat deadbeef...` (at `/blobs`)
- CLI: `aster blob cat <peer> <hash>`

The mapping is wired automatically via `plugin.py::register_cli_subcommands()`.

## Virtual Filesystem (VFS)

The VFS presents a peer's resources as a navigable tree:

```
/ (ROOT)
├── blobs/ (BLOBS)
│   └── abc123def4… (BLOB) — leaf, has metadata: {hash, size}
├── services/ (SERVICES)
│   └── HelloWorld/ (SERVICE) — has metadata from service summary
│       ├── sayHello (METHOD) — leaf, has metadata: {pattern, fields, ...}
│       └── streamGreetings (METHOD)
└── gossip/ (GOSSIP) — future
```

### VfsNode

```python
@dataclass
class VfsNode:
    name: str                           # "HelloWorld"
    kind: NodeKind                      # NodeKind.SERVICE
    path: str                           # "/services/HelloWorld"
    children: dict[str, VfsNode]        # child nodes
    metadata: dict[str, Any]            # service/method/blob metadata
    loaded: bool                        # whether children fetched from peer
```

### Lazy Loading

Children are loaded from the connection on first access:
- `cd services` → triggers `ensure_loaded()` → calls `connection.list_services()`
- `cd HelloWorld` → triggers `ensure_loaded()` → calls `connection.get_contract()`
- `cd /blobs` → triggers `ensure_loaded()` → calls `connection.list_blobs()`

The `refresh` command clears all `loaded` flags so the next navigation re-fetches.

### Path Resolution

`resolve_path(root, cwd, target)` handles:
- Absolute: `/services/HelloWorld`
- Relative: `HelloWorld` (from `/services`)
- Parent: `..`
- Current: `.`

Returns `(node, resolved_path)` or `(None, target)` if not found.

## Hook System

### Why Hooks Exist

The invoker needs two extension points:
1. **Before invocation**: construct the RPC payload from user input + schema
2. **After invocation**: format the response for display

The defaults (field-by-field prompting, JSON dump) work for simple cases. For complex types, an LLM plugin replaces these with intelligent handling.

### InputBuilder

```python
class InputBuilder(Protocol):
    async def build_payload(
        self,
        method_schema: MethodSchema,   # fields, types, nested structure
        user_input: dict[str, Any],    # partial input from user
        ask: AskFn,                    # prompt the user interactively
    ) -> dict[str, Any] | None:
```

**Invoked when**: user calls a method with no args (or partial args) and the method has a schema.

**What the LLM plugin should do**:
1. Read `method_schema.request_fields` to understand what's needed
2. Check `user_input` for anything already provided
3. For missing fields, use `ask()` to prompt — but phrase questions naturally ("What file do you want to get?" not "▸ filename (str):")
4. For nested types (e.g., a field that is itself a struct), build sub-objects conversationally
5. Validate before returning
6. Return `None` to cancel

### OutputRenderer

```python
class OutputRenderer(Protocol):
    async def render_response(
        self,
        method_schema: MethodSchema,   # response type info
        result: Any,                   # deserialized response
        display: Display,              # rich output object
    ) -> bool:
```

**Invoked after**: a successful unary or client-stream call.

**What the LLM plugin should do**:
1. Look at `result` structure and `method_schema.response_fields`
2. Decide rendering: table (list of records), tree (nested), inline (scalar), summary (large)
3. Use `display.console.print()` with rich markup
4. Return `True` to suppress default JSON dump

### MethodSchema

The schema passed to hooks contains everything needed:

```python
@dataclass
class MethodSchema:
    service_name: str
    method_name: str
    pattern: str                              # "unary", "server_stream", etc.
    request_type: str                         # "HelloRequest"
    response_type: str                        # "HelloResponse"
    request_fields: list[FieldSchema] | None  # field definitions
    response_fields: list[FieldSchema] | None
    timeout: float | None

@dataclass
class FieldSchema:
    name: str
    type_name: str          # "str", "int", "list[str]", "MyNestedType"
    required: bool
    default: Any
    description: str
    nested_fields: list[FieldSchema] | None  # for complex types
```

### Registering Hooks

```python
from aster_cli.shell.hooks import get_hook_registry

registry = get_hook_registry()
registry.register_input_builder(MyLlmInputBuilder())
registry.register_output_renderer(MyLlmOutputRenderer())
```

Last registered wins. This should be called before the shell REPL starts — either in a plugin loader or in `app.py` before `_run_shell()`.

### Hook Flow in the Invoker

```
User: ./sayHello
       │
       ▼
  Has payload? ── Yes ──→ skip input phase
       │
      No
       │
       ▼
  hooks.input_builder exists?
       │         │
      Yes       No
       │         │
       ▼         ▼
  builder.build_payload()    _prompt_for_args() (field-by-field)
       │
       ▼
  connection.invoke()
       │
       ▼
  hooks.output_renderer exists?
       │         │
      Yes       No
       │         │
       ▼         ▼
  renderer.render_response()  display.rpc_result() (JSON dump)
       │
  returned True? ── No ──→ display.rpc_result() (fallback)
       │
      Yes → done
```

## Guided Tour

### How It Works

`GuideManager` holds a `Tour` (sequence of `TourStep`). The REPL fires events:
- `"connected"` — on startup
- `"command"` with value `"ls"` — after user runs ls
- `"cd"` with value `"/services"` — after user navigates

Each step has a trigger (event name) and optional trigger_value (with glob support). When a step's trigger matches, the hint is displayed and the tour advances.

### Custom Tours

```python
from aster_cli.shell.guide import Tour, TourStep, GuideManager

my_tour = Tour(
    name="trust-workflow",
    steps=[
        TourStep(id="start", trigger="connected", message="Let's set up trust..."),
        TourStep(id="keygen", trigger="command", trigger_value="keygen", message="Key generated! Now..."),
    ],
)

guide = GuideManager(display, tour=my_tour)
```

### Persistence

Tour completion is stored in `~/.aster/config.toml`:

```toml
[shell]
first_time = false
```

The `is_first_time()` function checks this flag. `mark_tour_complete()` writes it.

## Session Subshell

### When to Use

Session-scoped services (marked `scoped="session"` in the `@service` decorator) maintain per-connection state. The `session` command opens a dedicated subshell:

```
demo:/services$ session Analytics
Analytics~ getMetrics query="cpu"
Analytics~ watchMetrics interval=5
Analytics~ end
demo:/services$
```

### How It Works

1. `SessionCommand.execute()` saves the main shell CWD
2. Opens a new `PromptSession` with `ServiceName~` prompt
3. Runs a mini REPL that only accepts method names (or `help`/`ls`/`end`)
4. Method invocations go through the same `invoker.invoke_method()` pipeline
5. On exit, restores the main shell CWD
6. `SessionHook.on_session_start/end()` fire for lifecycle management

### Future: Real Session Persistence

Currently the subshell is a UX pattern — it's the same connection. When session-scoped transport is wired, the subshell will:
1. Open a dedicated QUIC stream or session
2. Send a session-init handshake
3. Route all calls through the session stream
4. Close the session stream on `end`

The `SessionHook` protocol is where this logic will be plugged in.

## Connection Adapters

### PeerConnection

Wraps a real Iroh node connection. Methods delegate to the transport:
- `list_services()` → consumer admission ALPN → service summaries
- `get_contract()` → contract metadata request
- `list_blobs()` → blobs client listing
- `invoke()` → transport unary call

### DemoConnection

Offline demo with sample data. No network required. Returns:
- 3 services (HelloWorld, FileStore, Analytics)
- 3 blobs (with fake hashes)
- Simulated RPC responses with 50ms latency

Used for:
- UX testing (`aster shell --demo`)
- CI tests (`tests/python/test_shell.py`)
- Demos and documentation

### Adding a New Connection Adapter

Implement these async methods:

```python
class MyConnection:
    async def connect(self) -> None: ...
    async def list_services(self) -> list[dict]: ...
    async def get_contract(self, name: str) -> dict | None: ...
    async def list_blobs(self) -> list[dict]: ...
    async def read_blob(self, hash: str) -> bytes: ...
    async def invoke(self, service: str, method: str, payload: dict) -> Any: ...
    async def server_stream(self, service: str, method: str, payload: dict) -> AsyncIterator: ...
    async def client_stream(self, service: str, method: str, values: list) -> Any: ...
    def bidi_stream(self, service: str, method: str, values: AsyncIterator) -> AsyncIterator: ...
    async def close(self) -> None: ...
```

## Display Layer

`Display` wraps a `rich.Console` and provides typed output methods:

| Method | Output |
|--------|--------|
| `info(msg)` | Dimmed text |
| `error(msg)` | Red bold prefix |
| `warning(msg)` | Yellow prefix |
| `json_value(data)` | Syntax-highlighted JSON |
| `directory_listing(entries)` | Colored names with kind indicators |
| `service_table(services)` | Table: name, methods, version, pattern |
| `method_table(methods, svc)` | Table: name, pattern, signature, timeout |
| `blob_table(blobs)` | Table: hash, size |
| `contract_tree(contract)` | Rich tree: methods, types, capabilities |
| `streaming_value(idx, val)` | Numbered JSON values for streaming |
| `rpc_result(result, ms)` | JSON + timing |
| `welcome(peer, svcs, blobs)` | Bordered panel |

All methods check `self.raw` — in raw mode, output is plain JSON for piping.

## Tab Completion

`ShellCompleter` (prompt_toolkit `Completer`) queries context to offer suggestions:

1. **Empty input / partial command**: registered command names + direct method invocations (`./*`)
2. **After command name**: delegates to `command.get_completions(ctx, partial)`
3. **Path arguments**: `_complete_path()` splits partial into parent + leaf prefix, resolves parent, filters children
4. **Method args**: field names from method metadata (e.g., `name=`, `count=`)
5. **Flags**: `--lang`, `--out` etc. from `get_arguments()`

### How Path Completion Works

```
Input: "cd He"
  1. Split: parent="." prefix="He"
  2. Resolve "." → current node (/services)
  3. Filter children starting with "He" → ["HelloWorld/"]
  4. Return ["HelloWorld/"]

Input: "cd services/He"
  1. Split: parent="services" prefix="He"
  2. Resolve "services" → /services node
  3. Filter → ["HelloWorld/"]
```

## Testing

Tests are in `tests/python/test_shell.py` (34 tests):

| Suite | What it covers |
|-------|---------------|
| `TestVfs` | Path resolution, child lookup, case-insensitive match |
| `TestPlugins` | Command registration, context filtering, lookup |
| `TestCommands` | ls, cd, describe, invoke, pwd, help at various paths |
| `TestArgParsing` | key=value, JSON, positional, empty args |
| `TestDisplay` | Size formatting, raw mode |
| `TestDemoConnection` | list_services, invoke, blobs, contract |

To test a new command:

```python
@pytest.mark.asyncio
async def test_my_command(populated_ctx):
    cmd = get_command("mycommand")
    await cmd.execute(["arg1"], populated_ctx)
    # Assert on side effects or display output
```

## File Map

```
cli/aster_cli/shell/
├── __init__.py       # package entry, re-exports
├── app.py            # REPL loop, connection adapters, CLI wiring
├── commands.py       # all built-in commands (@register decorated)
├── completer.py      # prompt_toolkit completer
├── display.py        # rich output formatters
├── guide.py          # guided tour system
├── hooks.py          # InputBuilder, OutputRenderer, SessionHook protocols
├── invoker.py        # RPC invocation with hook integration
├── plugin.py         # ShellCommand base, registry, CLI mapping
├── vfs.py            # virtual filesystem tree
└── COMMANDS.md       # command hierarchy and extension docs
```

## Dynamic Invocation (No Local Types)

When connecting to a remote peer, the shell can invoke any RPC method without having the service's Python package installed locally. This is powered by runtime type synthesis from manifest metadata.

### How it works

1. **Manifest wire tags.** Service manifests now include `request_wire_tag`, `response_wire_tag`, and `response_fields` for each method. These describe the Fory XLANG wire types needed to serialize requests and deserialize responses.

2. **Type synthesis.** `PeerConnection._synthesize_types()` iterates over the fetched manifests and calls `DynamicTypeFactory` (in `aster/dynamic.py`) to create wire-compatible Python dataclasses at runtime. `DynamicTypeFactory` uses `dataclasses.make_dataclass()` to build a class and applies the `@wire_type` decorator with the manifest-provided tag, so the codec can serialize/deserialize it on the wire.

3. **Invocation.** The shell's `invoke` command detects when synthesized types are available for a method. When the user provides `key=value` arguments (e.g., `./say_hello name=World`), the invoker constructs a typed request instance from the dict args using the synthesized dataclass, rather than sending a raw dict. This means the Fory codec produces correctly tagged wire frames that the remote producer can deserialize.

### User-facing effect

```
demo:/services/HelloWorld$ ./say_hello name=World
```

This works against any peer, even if the `HelloWorld` service package is not installed locally. The shell discovers the service via admission, fetches its manifest, synthesizes the request/response types, and performs a fully typed RPC call.
