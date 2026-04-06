# Aster Shell — Command Hierarchy

Every interactive shell command maps to a CLI subcommand for automation.
Commands are plugins registered via `@register` in `commands.py`.

## Command Map

| Interactive (shell)         | CLI equivalent               | Context        | Description                              |
|-----------------------------|------------------------------|----------------|------------------------------------------|
| `ls`                        | (context-dependent)          | `/` (global)   | List contents at current path            |
| `cd <path>`                 | —                            | `/` (global)   | Change directory                         |
| `pwd`                       | —                            | `/` (global)   | Print current path                       |
| `help`                      | —                            | `/` (global)   | Show available commands                  |
| `refresh`                   | —                            | `/` (global)   | Clear cache, re-fetch from peer          |
| `exit` / `quit` / `q`      | —                            | `/` (global)   | Exit the shell                           |
| `ls` (at /blobs)            | `aster blob ls <peer>`       | `/blobs`       | List blobs                               |
| `cat <hash>`                | `aster blob cat <peer> <hash>`| `/blobs`      | Display blob content                     |
| `save <hash> <path>`        | `aster blob save <peer> ...` | `/blobs`       | Download blob to local file              |
| `ls` (at /services)         | `aster service ls <peer>`    | `/services`    | List services                            |
| `describe [service]`        | `aster service describe <peer> <svc>` | `/services/*` | Show contract tree          |
| `invoke <method> [args]`    | `aster service invoke <peer> ...` | `/services/*` | Invoke an RPC method              |
| `./<method> [args]`         | —                            | `/services/*`  | Direct method invocation (shorthand)     |
| `<method> [args]`           | —                            | `/services/*`  | Bare method name (auto-detected)         |
| `session [service]`         | —                            | `/services/*`  | Open session subshell (session-scoped)   |
| `generate-client [opts]`    | `aster service generate-client ...` | `/services/*` | Generate typed client (stub)     |

## Argument Syntax

```
# Key-value pairs
./sayHello name="World" greeting="Hi"

# Raw JSON
invoke sayHello '{"name": "World"}'

# Positional (single-arg methods)
./sayHello "World"

# Interactive prompting (no args → prompt each field)
./sayHello
  ▸ name (str): _
```

## Session Subshell

Session-scoped services maintain per-connection state. The `session` command
opens a dedicated subshell with its own prompt:

```
demo:/services$ session Analytics
Session opened: Analytics
This is a dedicated session — state persists across calls.
Type 'end' to close the session and return to the main shell.

Analytics~ getMetrics query="cpu"
→ Analytics.getMetrics(query='cpu')
(42ms)
{ "cpu": 0.73, "timestamp": "..." }

Analytics~ end
Session closed: Analytics
demo:/services$
```

The subshell:
- Opens a persistent connection/session to the service
- All calls go through the same session (state persists)
- Lifecycle hooks fire on open/close (`SessionHook` protocol)
- Tab completion shows only the service's methods
- `end` / `exit` / Ctrl+D closes the session cleanly

## Plugin Architecture

```
ShellCommand (ABC)
├── name          — command name
├── description   — one-line help text
├── contexts[]    — glob patterns for valid VFS paths
├── cli_noun_verb — (noun, verb) for CLI mapping, or None
├── execute()     — run the command
├── get_arguments()    — argument definitions (for CLI + autocomplete)
└── get_completions()  — dynamic completion suggestions

@register          — class decorator to add to plugin registry
```

### Adding a New Command

1. Create a class extending `ShellCommand` in `commands.py` (or a new file)
2. Decorate with `@register`
3. Set `cli_noun_verb` if it should have a CLI equivalent
4. Add to this table

### Future Plugin Sources

- **LLM plugin** — See "Hook System" below
- **Custom user plugins** — Load from `~/.aster/plugins/`
- **Peer-provided commands** — Services can advertise shell extensions

## Hook System (`hooks.py`)

The shell provides two hook protocols for extensibility:

### InputBuilder — Construct RPC payloads

```python
class InputBuilder(Protocol):
    async def build_payload(
        self,
        method_schema: MethodSchema,   # full type info (fields, types, nested)
        user_input: dict[str, Any],    # what user already provided (may be empty)
        ask: AskFn,                    # callable to prompt user interactively
    ) -> dict[str, Any] | None:       # complete payload, or None if cancelled
        ...
```

The default `DefaultInputBuilder` prompts field-by-field. An LLM plugin replaces
this to:
- Understand the schema and explain fields in context
- Accept natural language ("get the CPU metrics for the last hour")
- Build nested objects conversationally
- Suggest defaults and validate constraints

### OutputRenderer — Format RPC responses

```python
class OutputRenderer(Protocol):
    async def render_response(
        self,
        method_schema: MethodSchema,   # response type info
        result: Any,                   # deserialized response
        display: Display,             # rich output (tables, trees, panels)
    ) -> bool:                        # True = handled, False = use default JSON
        ...
```

The default falls through to JSON. An LLM plugin replaces this to:
- Choose the best representation (table for lists, tree for nested, inline for scalars)
- Summarize large payloads
- Highlight important fields
- Explain error responses

### SessionHook — Session lifecycle

```python
class SessionHook(Protocol):
    async def on_session_start(self, service_name: str, ctx: Any) -> None: ...
    async def on_session_end(self, service_name: str, ctx: Any) -> None: ...
```

### Registering hooks

```python
from aster_cli.shell.hooks import get_hook_registry

registry = get_hook_registry()
registry.register_input_builder(MyLlmInputBuilder())
registry.register_output_renderer(MyLlmOutputRenderer())
registry.register_session_hook(MySessionHook())
```

### Hook invocation points in the invoker

```
User types: ./sayHello
                │
                ▼
        ┌─ Has payload? ─── Yes ──→ skip input building
        │       │
        │      No
        │       │
        │       ▼
        │  InputBuilder.build_payload()    ← LLM constructs payload
        │       │
        │       ▼
        │  connection.invoke()
        │       │
        │       ▼
        │  OutputRenderer.render_response() ← LLM formats result
        │       │
        │  rendered? ── No ──→ default JSON dump
        │       │
        │      Yes
        │       │
        └───────┴──→ done
```

## Guided Tour (`guide.py`)

First-time users get a step-by-step tour. The tour is a sequence of
`TourStep` objects, each triggered by an event:

| Step | Trigger | Hint |
|------|---------|------|
| 1 | `connected` | "Try `ls` to see what's here" |
| 2 | `command:ls` | "Try `cd services` to browse services" |
| 3 | `cd:/services` | "Try `ls` to see them, then `cd <Name>`" |
| 4 | `cd:/services/*` | "Try `./methodName` or `describe`" |
| 5 | `invoke` | "You're all set! Type `help` anytime." |

Tour completion is persisted in `~/.aster/config.toml` under `[shell].first_time`.

Custom tours can be created by building a `Tour` with custom steps and
passing it to `GuideManager`.

## VFS Structure

```
/
├── blobs/              → content-addressed storage
│   └── <hash>          → individual blob (leaf)
├── services/           → RPC services
│   └── <ServiceName>/  → single service
│       └── <method>    → method (leaf, invocable)
└── gossip/             → pub/sub topics (future)
    └── <topic>         → topic (future)
```

## Streaming Patterns

| Pattern          | Interactive behavior                                     |
|------------------|----------------------------------------------------------|
| `unary`          | Send request, display response                           |
| `server_stream`  | Send request, display values as they arrive              |
| `client_stream`  | Prompt for values line-by-line, send on empty line       |
| `bidi_stream`    | Split view: type to send, responses appear above         |

## Ctrl+C Behavior

- Single Ctrl+C: prints "Press Ctrl+C again to exit" and returns to prompt
- Double Ctrl+C (within 1.5s): clean disconnect and exit
- Ctrl+C during an RPC call: cancels the call, returns to prompt
- Ctrl+D: clean disconnect and exit
