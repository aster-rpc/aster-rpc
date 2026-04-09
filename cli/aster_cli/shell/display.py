"""
aster_cli.shell.display — Rich terminal output for the shell.

Handles all presentation: tables, JSON syntax highlighting, trees,
progress bars, streaming output, and status messages.
"""

from __future__ import annotations

import json
from typing import Any

from rich.console import Console
from rich.json import JSON as RichJSON
from rich.markup import escape
from rich.panel import Panel
from rich.table import Table
from rich.text import Text
from rich.tree import Tree


class Display:
    """Manages all terminal output for the shell."""

    def __init__(self, console: Console | None = None, raw: bool = False) -> None:
        self.console = console or Console()
        self.raw = raw  # raw JSON mode for piping

    # ── Primitives ────────────────────────────────────────────────────────────

    def print(self, *args: Any, **kwargs: Any) -> None:
        """Print with rich markup."""
        if self.raw:
            return
        self.console.print(*args, **kwargs)

    def info(self, msg: str) -> None:
        """Informational message (dimmed)."""
        self.print(f"[dim]{escape(msg)}[/dim]")

    def success(self, msg: str) -> None:
        """Success message (green)."""
        self.print(f"[green]{escape(msg)}[/green]")

    def error(self, msg: str) -> None:
        """Error message (red)."""
        self.console.print(f"[red bold]error:[/red bold] {escape(msg)}")

    def warning(self, msg: str) -> None:
        """Warning message (yellow)."""
        self.console.print(f"[yellow]warning:[/yellow] {escape(msg)}")

    # ── Structured output ─────────────────────────────────────────────────────

    def json_value(self, data: Any) -> None:
        """Pretty-print a JSON value with syntax highlighting."""
        text = json.dumps(data, indent=2, default=str)
        if self.raw:
            self.console.print(text, highlight=False)
        else:
            self.console.print(RichJSON(text))

    def directory_listing(self, entries: list[dict[str, str]]) -> None:
        """Display a directory listing (like ls).

        Args:
            entries: List of dicts with "name", "kind", and optional "detail".
        """
        if self.raw:
            self.console.print(json.dumps(entries, default=str), highlight=False)
            return

        for entry in entries:
            kind = entry.get("kind", "")
            name = entry.get("name", "")
            detail = entry.get("detail", "")

            if kind == "dir":
                line = f"[bold cyan]{escape(name)}/[/bold cyan]"
            elif kind == "method":
                line = f"[green]{escape(name)}[/green]"
            else:
                line = escape(name)

            if detail:
                line += f"    [dim]{escape(detail)}[/dim]"

            self.console.print(f"  {line}")

    def service_table(self, services: list[dict[str, Any]]) -> None:
        """Display a table of services."""
        if self.raw:
            self.console.print(json.dumps(services, default=str), highlight=False)
            return

        table = Table(show_header=True, header_style="bold", box=None, padding=(0, 2))
        table.add_column("Service", style="cyan bold")
        table.add_column("Methods", justify="right")
        table.add_column("Version", justify="right", style="dim")
        table.add_column("Pattern", style="dim")

        for svc in services:
            table.add_row(
                svc.get("name", "?"),
                str(svc.get("method_count", "?")),
                f"v{svc.get('version', '?')}",
                svc.get("scoped", "shared"),
            )

        self.console.print(table)

    def method_table(self, methods: list[dict[str, Any]], service_name: str) -> None:
        """Display a table of methods for a service."""
        if self.raw:
            self.console.print(json.dumps(methods, default=str), highlight=False)
            return

        table = Table(
            title=f"[bold]{escape(service_name)}[/bold]",
            show_header=True,
            header_style="bold",
            box=None,
            padding=(0, 2),
        )
        table.add_column("Method", style="green")
        table.add_column("Pattern", style="dim")
        table.add_column("Signature")
        table.add_column("Timeout", justify="right", style="dim")

        for m in methods:
            sig = m.get("signature", "")
            timeout = f"{m['timeout']}s" if m.get("timeout") else ""
            table.add_row(
                m.get("name", "?"),
                m.get("pattern", "unary"),
                sig,
                timeout,
            )

        self.console.print(table)

    def blob_table(self, blobs: list[dict[str, Any]]) -> None:
        """Display a table of blobs."""
        if self.raw:
            self.console.print(json.dumps(blobs, default=str), highlight=False)
            return

        table = Table(show_header=True, header_style="bold", box=None, padding=(0, 2))
        table.add_column("Name")
        table.add_column("Size", justify="right")
        table.add_column("Hash", style="dim")

        for b in blobs:
            hash_str = b.get("hash", "?")
            short_hash = hash_str[:16] + "…" if len(hash_str) > 16 else hash_str
            size = b.get("size", 0)
            tag = b.get("tag", "")
            is_coll = b.get("is_collection", False)
            if isinstance(size, int) and size > 0:
                size_str = _format_size(size)
            else:
                size_str = ""
            # Show collections like directories (cyan + trailing /)
            name = f"[bold cyan]{escape(tag)}/[/bold cyan]" if is_coll else (tag or short_hash)
            table.add_row(name, size_str, short_hash)

        self.console.print(table)

    def collection_entry_table(self, entries: list[dict[str, Any]]) -> None:
        """Display entries inside a HashSeq collection."""
        if self.raw:
            self.console.print(json.dumps(entries, default=str), highlight=False)
            return

        table = Table(show_header=True, header_style="bold", box=None, padding=(0, 2))
        table.add_column("Name", style="green")
        table.add_column("Size", justify="right")
        table.add_column("Hash", style="dim")

        for e in entries:
            size = e.get("size", 0)
            size_str = _format_size(size) if isinstance(size, int) and size > 0 else ""
            table.add_row(e.get("name", "?"), size_str, e.get("hash", "?"))

        self.console.print(table)

    def doc_entry_table(self, entries: list[dict[str, Any]]) -> None:
        """Display a table of doc entries."""
        if self.raw:
            self.console.print(json.dumps(entries, default=str), highlight=False)
            return

        table = Table(show_header=True, header_style="bold", box=None, padding=(0, 2))
        table.add_column("Key", style="cyan")
        table.add_column("Size", justify="right")
        table.add_column("Author", style="dim")
        table.add_column("Hash", style="dim")

        for e in entries:
            key = e.get("key", "?")
            size = e.get("size", 0)
            size_str = _format_size(size) if isinstance(size, int) and size > 0 else ""
            author = e.get("author", "?")
            hash_val = e.get("hash", "?")
            table.add_row(key, size_str, author, hash_val)

        self.console.print(table)

    def contract_tree(self, contract: dict[str, Any]) -> None:
        """Display a contract as a rich tree."""
        if self.raw:
            self.console.print(json.dumps(contract, default=str), highlight=False)
            return

        name = contract.get("name", "Contract")
        version = contract.get("version", "?")
        tree = Tree(f"[bold cyan]{escape(name)}[/bold cyan] v{version}")

        # Contract ID
        cid = contract.get("contract_id")
        if cid:
            tree.add(f"[dim]contract_id:[/dim] {cid[:16]}…")

        # Methods
        methods = contract.get("methods", [])
        if methods:
            methods_branch = tree.add("[bold]methods[/bold]")
            for m in methods:
                m_name = m.get("name", "?")
                pattern = m.get("pattern", "unary")
                sig = m.get("signature", "")
                label = f"[green]{escape(m_name)}[/green] ({pattern})"
                if sig:
                    label += f"  [dim]{escape(sig)}[/dim]"
                method_node = methods_branch.add(label)

                # Capabilities
                requires = m.get("requires")
                if requires:
                    method_node.add(f"[yellow]requires:[/yellow] {escape(str(requires))}")

        # Types
        types = contract.get("types", [])
        if types:
            types_branch = tree.add("[bold]types[/bold]")
            for t in types:
                t_name = t.get("name", "?")
                t_hash = t.get("hash", "")
                label = f"[magenta]{escape(t_name)}[/magenta]"
                if t_hash:
                    label += f"  [dim]{t_hash[:12]}…[/dim]"
                types_branch.add(label)

        self.console.print(tree)

    def streaming_value(self, index: int, value: Any) -> None:
        """Display a single value from a streaming response."""
        if self.raw:
            self.console.print(json.dumps(value, default=str), highlight=False)
        else:
            text = json.dumps(value, indent=2, default=str)
            self.console.print(
                f"[dim]#{index}[/dim] ", end=""
            )
            self.console.print(RichJSON(text))

    def rpc_result(self, result: Any, elapsed_ms: float | None = None) -> None:
        """Display an RPC invocation result."""
        if elapsed_ms is not None and not self.raw:
            self.info(f"({elapsed_ms:.0f}ms)")
        self.json_value(result)

    def welcome(self, peer_name: str, service_count: int, blob_count: int) -> None:
        """Display the welcome banner on connection."""
        self.console.print()
        self.console.print(
            Panel(
                f"[bold]Connected to {escape(peer_name)}[/bold]\n"
                f"[dim]{service_count} services, {blob_count} blobs[/dim]",
                border_style="cyan",
                padding=(0, 2),
            )
        )
        self.console.print()

    def directory_welcome(self, handle: str, handle_count: int) -> None:
        """Display the welcome banner for directory mode."""
        self.console.print()
        self.console.print(
            Panel(
                f"[bold]aster.site[/bold] [dim]— service directory[/dim]\n"
                f"[dim]Logged in as[/dim] [bold cyan]{escape(handle)}[/bold cyan]"
                f" [dim]({handle_count} handles in directory)[/dim]",
                border_style="#61D6C2",
                padding=(0, 2),
            )
        )
        self.console.print()

    def handle_listing(self, handles: list[dict[str, Any]]) -> None:
        """Display a listing of handles in the directory."""
        if self.raw:
            self.console.print(json.dumps(handles, default=str), highlight=False)
            return

        for h in handles:
            name = h.get("name", "?")
            registered = h.get("registered", True)
            svc_count = h.get("service_count", 0)
            description = h.get("description", "")

            if registered:
                line = f"[bold cyan]{escape(name)}/[/bold cyan]"
            else:
                line = f"[dim]{escape(name)}/[/dim]"

            detail_parts = []
            if svc_count:
                detail_parts.append(f"{svc_count} services")
            if description:
                detail_parts.append(description)
            if detail_parts:
                line += f"    [dim]{escape(' — '.join(detail_parts))}[/dim]"

            self.console.print(f"  {line}")

    def handle_service_listing(self, services: list[dict[str, Any]], handle: str) -> None:
        """Display services within a handle's directory."""
        if self.raw:
            self.console.print(json.dumps(services, default=str), highlight=False)
            return

        table = Table(
            title=f"[bold]{escape(handle)}[/bold]",
            show_header=True,
            header_style="bold",
            box=None,
            padding=(0, 2),
        )
        table.add_column("Service", style="cyan bold")
        table.add_column("Methods", justify="right")
        table.add_column("Version", justify="right", style="dim")
        table.add_column("Endpoints", justify="right", style="dim")
        table.add_column("Description", style="dim")

        for svc in services:
            name = svc.get("display_name", svc.get("name", "?"))
            published = svc.get("published", True)
            style = "" if published else "dim"
            marker = "●" if published else "⬡"

            table.add_row(
                Text(f"{marker} {name}", style=f"cyan bold {style}".strip()),
                str(svc.get("method_count", "?")),
                f"v{svc.get('version', '?')}",
                str(svc.get("endpoints", 0)) if published else "-",
                svc.get("description", ""),
            )

        self.console.print(table)

    def readme_content(self, content: str) -> None:
        """Display README.md content with basic markdown rendering."""
        if self.raw:
            self.console.print(content, highlight=False)
            return

        from rich.markdown import Markdown
        self.console.print(Markdown(content))


def _format_size(size_bytes: int) -> str:
    """Human-readable file size."""
    if size_bytes < 1024:
        return f"{size_bytes} B"
    elif size_bytes < 1024 * 1024:
        return f"{size_bytes / 1024:.1f} KB"
    elif size_bytes < 1024 * 1024 * 1024:
        return f"{size_bytes / (1024 * 1024):.1f} MB"
    else:
        return f"{size_bytes / (1024 * 1024 * 1024):.1f} GB"
