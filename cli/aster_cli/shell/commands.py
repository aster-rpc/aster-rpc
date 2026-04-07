"""
aster_cli.shell.commands — Built-in shell commands.

Each command is a plugin: usable interactively and (where applicable)
as ``aster <noun> <verb>`` from the command line.
"""

from __future__ import annotations

import json
from typing import Any

from aster_cli.shell.plugin import (
    Argument,
    CommandContext,
    ShellCommand,
    register,
)
from aster_cli.shell.vfs import (
    NodeKind,
    VfsNode,
    ensure_loaded,
    resolve_path,
)


# ── Navigation ────────────────────────────────────────────────────────────────


@register
class CdCommand(ShellCommand):
    name = "cd"
    description = "Change directory"
    contexts = []  # global

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        target = args[0] if args else "/"
        node, path = resolve_path(ctx.vfs_root, ctx.vfs_cwd, target)

        if node is None:
            ctx.display.error(f"no such path: {path}")
            return

        if node.kind in (NodeKind.BLOB, NodeKind.METHOD, NodeKind.README):
            ctx.display.error(f"{path} is not a directory")
            return

        # Lazy-load children
        await ensure_loaded(node, ctx.connection)

        # Update cwd — the caller reads ctx.vfs_cwd after execute
        ctx.vfs_cwd = path  # type: ignore[misc]

    def get_completions(self, ctx: CommandContext, partial: str) -> list[str]:
        return _complete_path(ctx, partial, dirs_only=True)


@register
class LsCommand(ShellCommand):
    name = "ls"
    description = "List contents of the current or specified path"
    contexts = []  # global
    cli_noun_verb = None  # ls is context-dependent — mapped per noun below

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        target = args[0] if args else "."
        node, path = resolve_path(ctx.vfs_root, ctx.vfs_cwd, target)

        if node is None:
            ctx.display.error(f"no such path: {path}")
            return

        await ensure_loaded(node, ctx.connection)

        if node.kind == NodeKind.ROOT:
            entries = [
                {"name": c.name, "kind": "dir", "detail": _kind_detail(c)}
                for c in node.sorted_children()
            ]
            ctx.display.directory_listing(entries)

        elif node.kind == NodeKind.SERVICES:
            services = []
            for c in node.sorted_children():
                await ensure_loaded(c, ctx.connection)
                services.append({
                    "name": c.name,
                    "method_count": len(c.children),
                    "version": c.metadata.get("version", 1),
                    "scoped": c.metadata.get("scoped", "shared"),
                })
            ctx.display.service_table(services)

        elif node.kind == NodeKind.SERVICE:
            methods = []
            for c in node.sorted_children():
                methods.append({
                    "name": c.name,
                    "pattern": c.metadata.get("pattern", "unary"),
                    "signature": _method_signature(c.metadata),
                    "timeout": c.metadata.get("timeout"),
                })
            ctx.display.method_table(methods, node.name)

        elif node.kind == NodeKind.BLOBS:
            blobs = []
            for c in node.sorted_children():
                blobs.append({
                    "hash": c.metadata.get("hash", c.name),
                    "size": c.metadata.get("size", "?"),
                })
            ctx.display.blob_table(blobs)

        elif node.kind == NodeKind.ASTER:
            handles = []
            for c in node.sorted_children():
                await ensure_loaded(c, ctx.connection)
                # Count non-README children as services
                svc_count = sum(
                    1 for ch in c.children.values() if ch.kind == NodeKind.SERVICE
                )
                handles.append({
                    "name": c.name,
                    "registered": c.metadata.get("registered", True),
                    "service_count": svc_count,
                    "description": "",
                })
            ctx.display.handle_listing(handles)

        elif node.kind == NodeKind.HANDLE:
            services = []
            for c in node.sorted_children():
                if c.kind == NodeKind.README:
                    continue
                await ensure_loaded(c, ctx.connection)
                published = c.metadata.get("published", True)
                services.append({
                    "display_name": c.name,
                    "name": c.metadata.get("name", c.name),
                    "published": published,
                    "method_count": len([ch for ch in c.children.values() if ch.kind == NodeKind.METHOD]),
                    "version": c.metadata.get("version", 1),
                    "endpoints": c.metadata.get("endpoints", 0),
                    "description": c.metadata.get("description", ""),
                })
            # Show README hint if present
            readme = node.child("README.md")
            if readme:
                ctx.display.info("README.md available — use: cat README.md")
            ctx.display.handle_service_listing(services, node.name)

        elif node.kind == NodeKind.README:
            content = node.metadata.get("content", "")
            ctx.display.readme_content(content)

        elif node.kind in (NodeKind.BLOB, NodeKind.METHOD):
            ctx.display.json_value(node.metadata)

        else:
            entries = [
                {"name": c.name, "kind": "dir" if c.children or not c.loaded else "file"}
                for c in node.sorted_children()
            ]
            ctx.display.directory_listing(entries)

    def get_completions(self, ctx: CommandContext, partial: str) -> list[str]:
        return _complete_path(ctx, partial, dirs_only=False)


# ── Introspection ─────────────────────────────────────────────────────────────


@register
class DescribeCommand(ShellCommand):
    name = "describe"
    description = "Show detailed contract info for a service"
    contexts = ["/services", "/services/*", "/aster/*/*"]
    cli_noun_verb = ("service", "describe")

    def get_arguments(self) -> list[Argument]:
        return [Argument(name="service", description="Service name", positional=True)]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        # Determine which service to describe
        node, path = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if node and node.kind == NodeKind.SERVICE:
            service_name = node.name
        elif args:
            service_name = args[0]
        else:
            ctx.display.error("usage: describe <service>")
            return

        # Fetch contract details
        try:
            contract = await ctx.connection.get_contract(service_name)
            if contract is None:
                ctx.display.error(f"no contract found for {service_name}")
                return
            ctx.display.contract_tree(
                contract if isinstance(contract, dict) else _contract_to_dict(contract)
            )
        except Exception as e:
            ctx.display.error(f"failed to get contract: {e}")

    def get_completions(self, ctx: CommandContext, partial: str) -> list[str]:
        services_node = ctx.vfs_root.child("services")
        if services_node is None:
            return []
        return [c.name for c in services_node.sorted_children()
                if c.name.lower().startswith(partial.lower())]


@register
class CatCommand(ShellCommand):
    name = "cat"
    description = "Display file or blob content"
    contexts = []  # global — works in blobs and directory handle contexts
    cli_noun_verb = ("blob", "cat")

    def get_arguments(self) -> list[Argument]:
        return [Argument(name="target", description="File name or blob hash", positional=True)]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        # Try resolving as a path first (e.g., "cat README.md" in a handle dir)
        if args:
            node, path = resolve_path(ctx.vfs_root, ctx.vfs_cwd, args[0])
            if node and node.kind == NodeKind.README:
                content = node.metadata.get("content", "")
                ctx.display.readme_content(content)
                return

        if not args:
            # If we're at a blob node, use that
            node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
            if node and node.kind == NodeKind.BLOB:
                blob_hash = node.metadata.get("hash", node.name)
            else:
                ctx.display.error("usage: cat <target>")
                return
        else:
            blob_hash = args[0]

        try:
            content = await ctx.connection.read_blob(blob_hash)
            if isinstance(content, bytes):
                try:
                    text = content.decode("utf-8")
                    ctx.display.print(text)
                except UnicodeDecodeError:
                    ctx.display.info(f"(binary data, {len(content)} bytes)")
                    ctx.display.print(content.hex()[:200] + ("…" if len(content) > 100 else ""))
            else:
                ctx.display.print(str(content))
        except Exception as e:
            ctx.display.error(f"failed to read: {e}")

    def get_completions(self, ctx: CommandContext, partial: str) -> list[str]:
        # Complete file names in current dir
        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if node is None:
            return []
        return [c.name for c in node.sorted_children()
                if c.name.lower().startswith(partial.lower())]


@register
class SaveCommand(ShellCommand):
    name = "save"
    description = "Download a blob to a local file"
    contexts = ["/blobs", "/blobs/*"]
    cli_noun_verb = ("blob", "save")

    def get_arguments(self) -> list[Argument]:
        return [
            Argument(name="hash", description="Blob hash (or prefix)", positional=True, required=True),
            Argument(name="path", description="Local file path", positional=True, required=True),
        ]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        if len(args) < 2:
            # If at a blob node, only need the output path
            node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
            if node and node.kind == NodeKind.BLOB and len(args) == 1:
                blob_hash = node.metadata.get("hash", node.name)
                out_path = args[0]
            else:
                ctx.display.error("usage: save <hash> <path>")
                return
        else:
            blob_hash = args[0]
            out_path = args[1]

        try:
            from rich.progress import Progress

            with Progress(console=ctx.display.console) as progress:
                task = progress.add_task(f"Downloading {blob_hash[:12]}…", total=None)
                content = await ctx.connection.read_blob(blob_hash)
                progress.update(task, total=len(content), completed=len(content))

            with open(out_path, "wb") as f:
                f.write(content if isinstance(content, bytes) else content.encode())

            ctx.display.success(f"Saved to {out_path} ({len(content)} bytes)")
        except Exception as e:
            ctx.display.error(f"failed to save blob: {e}")


# ── Service invocation ────────────────────────────────────────────────────────


@register
class InvokeCommand(ShellCommand):
    name = "invoke"
    description = "Invoke an RPC method"
    contexts = ["/services/*", "/aster/*/*"]
    cli_noun_verb = ("service", "invoke")

    def get_arguments(self) -> list[Argument]:
        return [
            Argument(name="method", description="Method name", positional=True, required=True),
            Argument(name="args", description="JSON arguments"),
        ]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        if not args:
            ctx.display.error("usage: invoke <method> [key=value ...] or <method> '{json}'")
            return

        # If we're calling from /services/<name>, resolve service name
        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if node and node.kind == NodeKind.SERVICE:
            service_name = node.name
            method_name = args[0]
            call_args = args[1:]
        elif "/" in args[0] or "." in args[0]:
            # service.method or service/method syntax
            parts = args[0].replace("/", ".").split(".", 1)
            service_name = parts[0]
            method_name = parts[1] if len(parts) > 1 else ""
            call_args = args[1:]
        else:
            ctx.display.error("navigate to a service first, or use service.method syntax")
            return

        # Parse arguments
        payload = _parse_call_args(call_args)

        # Delegate to invoker
        from aster_cli.shell.invoker import invoke_method
        await invoke_method(ctx, service_name, method_name, payload)

    def get_completions(self, ctx: CommandContext, partial: str) -> list[str]:
        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if node and node.kind == NodeKind.SERVICE:
            return [c.name for c in node.sorted_children()
                    if c.name.lower().startswith(partial.lower())]
        return []


# ── Direct method invocation (./methodName syntax) ───────────────────────────


@register
class DirectInvokeCommand(ShellCommand):
    """Hidden command that handles ./methodName syntax."""

    name = "./"
    description = "Direct method invocation"
    contexts = ["/services/*", "/aster/*/*"]
    hidden = True

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        # args[0] is the full "./methodName" or "methodName"
        if not args:
            return
        method_name = args[0].lstrip("./")
        call_args = args[1:]

        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if not node or node.kind != NodeKind.SERVICE:
            ctx.display.error("direct invocation requires being in a service directory")
            return

        payload = _parse_call_args(call_args)

        from aster_cli.shell.invoker import invoke_method
        await invoke_method(ctx, node.name, method_name, payload)


# ── Shell utilities ───────────────────────────────────────────────────────────


@register
class PwdCommand(ShellCommand):
    name = "pwd"
    description = "Print current path"
    contexts = []

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        ctx.display.print(ctx.vfs_cwd)


@register
class HelpCommand(ShellCommand):
    name = "help"
    description = "Show available commands"
    contexts = []

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        from aster_cli.shell.plugin import get_commands_for_path

        commands = get_commands_for_path(ctx.vfs_cwd)
        ctx.display.print("[bold]Available commands:[/bold]")
        for cmd in sorted(commands, key=lambda c: c.name):
            ctx.display.print(f"  [green]{cmd.name:16s}[/green] {cmd.description}")

        # Show methods as direct invocations if in a service
        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if node and node.kind == NodeKind.SERVICE and node.children:
            ctx.display.print()
            ctx.display.print("[bold]Direct invocation:[/bold]")
            for child in node.sorted_children():
                sig = _method_signature(child.metadata)
                ctx.display.print(f"  [green]./{child.name:14s}[/green] {sig}")


@register
class ExitCommand(ShellCommand):
    name = "exit"
    description = "Exit the shell"
    contexts = []

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        raise SystemExit(0)


@register
class RefreshCommand(ShellCommand):
    name = "refresh"
    description = "Re-fetch data from the peer"
    contexts = []

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        # Reset loaded flags so next ls/cd will re-fetch
        _reset_loaded(ctx.vfs_root)
        ctx.display.success("Cache cleared — next listing will fetch fresh data")


@register
class GenerateClientCommand(ShellCommand):
    name = "generate-client"
    description = "Generate a typed client for a service"
    contexts = ["/services", "/services/*"]
    cli_noun_verb = ("service", "generate-client")

    def get_arguments(self) -> list[Argument]:
        return [
            Argument(name="lang", description="Target language (python, go, java, etc.)", required=True),
            Argument(name="out", description="Output directory", required=True),
            Argument(name="service", description="Service name", positional=True),
        ]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        # Determine service
        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if node and node.kind == NodeKind.SERVICE:
            service_name = node.name
        elif args:
            service_name = args[0]
        else:
            ctx.display.error("usage: generate-client [--lang python] [--out ./clients/] <service>")
            return

        # Parse --lang and --out from args
        lang = "python"
        out = "./clients/"
        i = 0
        while i < len(args):
            if args[i] == "--lang" and i + 1 < len(args):
                lang = args[i + 1]
                i += 2
            elif args[i] == "--out" and i + 1 < len(args):
                out = args[i + 1]
                i += 2
            else:
                if args[i] != service_name:
                    service_name = args[i]
                i += 1

        ctx.display.info(f"Generating {lang} client for {service_name} → {out}")
        ctx.display.warning("Client generation is not yet wired — this is a placeholder")
        ctx.display.info(
            f"Will generate: {out}/{service_name.lower()}_client.{_lang_ext(lang)}"
        )


# ── Session subshell ──────────────────────────────────────────────────────────


@register
class SessionCommand(ShellCommand):
    name = "session"
    description = "Open a session subshell for a session-scoped service"
    contexts = ["/services", "/services/*"]

    def get_arguments(self) -> list[Argument]:
        return [Argument(name="service", description="Session-scoped service name", positional=True)]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        # Determine service
        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if node and node.kind == NodeKind.SERVICE:
            service_name = node.name
        elif args:
            service_name = args[0]
        else:
            ctx.display.error("usage: session <service>")
            return

        # Check if session-scoped
        svc_node = ctx.vfs_root.child("services")
        if svc_node:
            target = svc_node.child(service_name)
            if target and target.metadata.get("scoped") != "session":
                ctx.display.warning(f"{service_name} is not session-scoped (scoped={target.metadata.get('scoped', 'shared')})")
                ctx.display.info("Session subshell is designed for session-scoped services.")
                ctx.display.info("Shared services don't maintain per-connection state.")
                return

        # Fire session hooks
        from aster_cli.shell.hooks import get_hook_registry
        hooks = get_hook_registry()
        for hook in hooks.session_hooks:
            await hook.on_session_start(service_name, ctx)

        ctx.display.print()
        ctx.display.print(f"[bold cyan]Session opened: {service_name}[/bold cyan]")
        ctx.display.info("This is a dedicated session — state persists across calls.")
        ctx.display.info("Type 'end' to close the session and return to the main shell.")
        ctx.display.print()

        # Save main shell state
        saved_cwd = ctx.vfs_cwd
        ctx.vfs_cwd = f"/services/{service_name}"

        # Subshell loop
        from prompt_toolkit import PromptSession as PS
        from prompt_toolkit.formatted_text import HTML

        sub_session = PS()

        while True:
            try:
                prompt = HTML(
                    f"<style fg='#E6C06B'>{service_name}</style>"
                    f"<style fg='#666666'>~</style> "
                )
                text = await sub_session.prompt_async(prompt)
                text = text.strip()

                if not text:
                    continue

                if text in ("end", "exit", "quit"):
                    break

                # Parse and execute within the service context
                import shlex
                try:
                    parts = shlex.split(text)
                except ValueError:
                    parts = text.split()

                method_name = parts[0].lstrip("./")
                call_args = parts[1:]

                # Check if it's a valid method
                target_node = ctx.vfs_root.child("services")
                if target_node:
                    svc = target_node.child(service_name)
                    if svc and svc.child(method_name):
                        payload = _parse_call_args(call_args)
                        from aster_cli.shell.invoker import invoke_method
                        await invoke_method(ctx, service_name, method_name, payload)
                        continue

                # Built-in subshell commands
                if method_name == "help":
                    ctx.display.print("[bold]Session commands:[/bold]")
                    ctx.display.print("  [green]end[/green]         Close this session")
                    ctx.display.print("  [green]help[/green]        Show this help")
                    ctx.display.print()
                    ctx.display.print("[bold]Available methods:[/bold]")
                    if svc_node:
                        svc = svc_node.child(service_name)
                        if svc:
                            for c in svc.sorted_children():
                                sig = c.metadata.get("request_type", "")
                                pattern = c.metadata.get("pattern", "unary")
                                ctx.display.print(f"  [green]{c.name:20s}[/green] {pattern:15s} {sig}")
                elif method_name == "ls":
                    if svc_node:
                        svc = svc_node.child(service_name)
                        if svc:
                            for c in svc.sorted_children():
                                pattern = c.metadata.get("pattern", "unary")
                                ctx.display.print(f"  [green]{c.name}[/green]  [dim]{pattern}[/dim]")
                else:
                    ctx.display.error(f"unknown method: {method_name} (try 'help')")

            except KeyboardInterrupt:
                ctx.display.print()
                continue
            except EOFError:
                break

        # Restore state
        ctx.vfs_cwd = saved_cwd

        # Fire session end hooks
        for hook in hooks.session_hooks:
            await hook.on_session_end(service_name, ctx)

        ctx.display.info(f"Session closed: {service_name}")

    def get_completions(self, ctx: CommandContext, partial: str) -> list[str]:
        services_node = ctx.vfs_root.child("services")
        if services_node is None:
            return []
        return [c.name for c in services_node.sorted_children()
                if c.metadata.get("scoped") == "session"
                and c.name.lower().startswith(partial.lower())]


# ── CLI-mapped blob commands ──────────────────────────────────────────────────


@register
class BlobLsCommand(ShellCommand):
    name = "blob-ls"
    description = "List blobs (CLI: aster blob ls)"
    contexts = ["/blobs"]
    cli_noun_verb = ("blob", "ls")
    hidden = True  # hidden in interactive mode — use ls instead

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        # Navigate to /blobs and list
        node = ctx.vfs_root.child("blobs")
        if node:
            await ensure_loaded(node, ctx.connection)
            blobs = [
                {"hash": c.metadata.get("hash", c.name), "size": c.metadata.get("size", "?")}
                for c in node.sorted_children()
            ]
            ctx.display.blob_table(blobs)


@register
class ServiceLsCommand(ShellCommand):
    name = "service-ls"
    description = "List services (CLI: aster service ls)"
    contexts = ["/services"]
    cli_noun_verb = ("service", "ls")
    hidden = True

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        node = ctx.vfs_root.child("services")
        if node:
            await ensure_loaded(node, ctx.connection)
            services = []
            for c in node.sorted_children():
                await ensure_loaded(c, ctx.connection)
                services.append({
                    "name": c.name,
                    "method_count": len(c.children),
                    "version": c.metadata.get("version", 1),
                    "scoped": c.metadata.get("scoped", "shared"),
                })
            ctx.display.service_table(services)


# ── Helpers ───────────────────────────────────────────────────────────────────


def _complete_path(ctx: CommandContext, partial: str, dirs_only: bool = False) -> list[str]:
    """Complete a VFS path from a partial string.

    Splits partial into parent directory + leaf prefix, resolves the parent,
    and filters children by the prefix.

    Examples:
        partial=""      → children of cwd
        partial="He"    → children of cwd starting with "He"
        partial="srv/H" → children of cwd/srv starting with "H"
    """
    # Split into parent path and leaf prefix
    if "/" in partial and not partial.endswith("/"):
        parent_path = partial.rsplit("/", 1)[0] or "/"
        prefix = partial.rsplit("/", 1)[1]
    elif partial.endswith("/"):
        parent_path = partial.rstrip("/") or "/"
        prefix = ""
    else:
        parent_path = "."
        prefix = partial

    node, _path = resolve_path(ctx.vfs_root, ctx.vfs_cwd, parent_path)
    if node is None:
        return []

    prefix_lower = prefix.lower()
    results = []
    for c in node.sorted_children():
        if dirs_only and c.kind in (NodeKind.BLOB, NodeKind.METHOD):
            continue
        if c.name.lower().startswith(prefix_lower):
            suffix = "/" if c.kind not in (NodeKind.BLOB, NodeKind.METHOD) else ""
            results.append(c.name + suffix)
    return results


def _parse_call_args(args: list[str]) -> dict[str, Any]:
    """Parse call arguments from shell tokens.

    Supports:
      - key=value pairs: name="World" count=5
      - Raw JSON string: '{"name": "World"}'
      - Positional value for single-arg methods: "World"
    """
    if not args:
        return {}

    # Try as a single JSON string
    joined = " ".join(args)
    if joined.startswith("{"):
        try:
            return json.loads(joined)
        except json.JSONDecodeError:
            pass

    # Try as key=value pairs
    result: dict[str, Any] = {}
    positional: list[str] = []

    for arg in args:
        if "=" in arg:
            key, value = arg.split("=", 1)
            # Strip quotes
            value = value.strip("'\"")
            # Try to parse as JSON value
            try:
                result[key] = json.loads(value)
            except (json.JSONDecodeError, ValueError):
                result[key] = value
        else:
            positional.append(arg.strip("'\""))

    # If only positional args, store under numeric keys
    if positional and not result:
        if len(positional) == 1:
            # Single positional → try as raw value
            try:
                return {"_positional": json.loads(positional[0])}
            except (json.JSONDecodeError, ValueError):
                return {"_positional": positional[0]}
        for i, v in enumerate(positional):
            result[f"_arg{i}"] = v

    return result


def _method_signature(metadata: dict[str, Any]) -> str:
    """Build a human-readable method signature from metadata."""
    req = metadata.get("request_type", "")
    resp = metadata.get("response_type", "")
    if req and resp:
        return f"({req}) → {resp}"
    elif req:
        return f"({req}) → …"
    elif resp:
        return f"() → {resp}"
    return ""


def _kind_detail(node: VfsNode) -> str:
    """Short detail string for a top-level directory."""
    details = {
        NodeKind.BLOBS: "content-addressed storage",
        NodeKind.SERVICES: "RPC services",
        NodeKind.GOSSIP: "pub/sub topics",
        NodeKind.ASTER: "service directory",
    }
    return details.get(node.kind, "")


def _reset_loaded(node: VfsNode) -> None:
    """Recursively clear loaded flags."""
    node.loaded = node.kind == NodeKind.ROOT  # keep root loaded
    node.children.clear() if node.kind != NodeKind.ROOT else None
    for child in node.children.values():
        _reset_loaded(child)


def _lang_ext(lang: str) -> str:
    """File extension for a language."""
    return {
        "python": "py",
        "go": "go",
        "java": "java",
        "typescript": "ts",
        "javascript": "js",
        "csharp": "cs",
        "rust": "rs",
    }.get(lang.lower(), lang)
