"""
aster_cli.shell.completer — Context-aware tab completion for the shell.

Provides prompt_toolkit Completer that queries the VFS and plugin
registry to offer context-sensitive suggestions.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Iterable

from prompt_toolkit.completion import CompleteEvent, Completion, Completer
from prompt_toolkit.document import Document

from aster_cli.shell.plugin import get_all_commands, get_commands_for_path
from aster_cli.shell.vfs import NodeKind, VfsNode, resolve_path

if TYPE_CHECKING:
    from aster_cli.shell.plugin import CommandContext


class ShellCompleter(Completer):
    """Context-aware completer for the Aster shell.

    Completion strategy:
    1. Empty input → show available commands + direct method invocations
    2. Command name partial → match registered commands
    3. After command name → delegate to command's get_completions()
    4. ./ prefix → method names (direct invocation)
    """

    def __init__(self, get_context: callable) -> None:
        """
        Args:
            get_context: Callable returning the current CommandContext.
        """
        self._get_context = get_context

    def get_completions(
        self, document: Document, complete_event: CompleteEvent
    ) -> Iterable[Completion]:
        text = document.text_before_cursor.lstrip()
        words = text.split()
        ctx = self._get_context()

        if not words or (len(words) == 1 and not text.endswith(" ")):
            # Completing command name
            partial = words[0] if words else ""
            yield from self._complete_command(ctx, partial)
        else:
            # Completing command arguments
            cmd_name = words[0]
            partial = words[-1] if not text.endswith(" ") else ""
            yield from self._complete_args(ctx, cmd_name, partial)

    def _complete_command(
        self, ctx: CommandContext, partial: str
    ) -> Iterable[Completion]:
        """Complete a command name."""
        partial_lower = partial.lower()

        # Registered commands valid at current path
        commands = get_commands_for_path(ctx.vfs_cwd)
        for cmd in sorted(commands, key=lambda c: c.name):
            if cmd.name.lower().startswith(partial_lower):
                yield Completion(
                    cmd.name,
                    start_position=-len(partial),
                    display=cmd.name,
                    display_meta=cmd.description,
                )

        # Direct method invocations (./) if in a service directory
        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if node and node.kind == NodeKind.SERVICE:
            for child in node.sorted_children():
                name = f"./{child.name}"
                if name.lower().startswith(partial_lower) or child.name.lower().startswith(partial_lower):
                    pattern = child.metadata.get("pattern", "unary")
                    sig = _short_sig(child.metadata)
                    yield Completion(
                        name,
                        start_position=-len(partial),
                        display=name,
                        display_meta=f"{pattern} {sig}",
                    )

    def _complete_args(
        self, ctx: CommandContext, cmd_name: str, partial: str
    ) -> Iterable[Completion]:
        """Complete arguments for a command."""
        # Handle ./ prefix as direct invocation args
        if cmd_name.startswith("./"):
            yield from self._complete_method_args(ctx, cmd_name[2:], partial)
            return

        commands = get_all_commands()
        cmd = commands.get(cmd_name)
        if cmd is None:
            return

        # Let the command provide its own completions
        suggestions = cmd.get_completions(ctx, partial)
        for s in suggestions:
            if s.lower().startswith(partial.lower()):
                yield Completion(s, start_position=-len(partial))

        # Also complete argument names from get_arguments()
        for arg in cmd.get_arguments():
            if not arg.positional:
                flag = f"--{arg.name}"
                if flag.startswith(partial):
                    yield Completion(
                        flag,
                        start_position=-len(partial),
                        display_meta=arg.description,
                    )

    def _complete_method_args(
        self, ctx: CommandContext, method_name: str, partial: str
    ) -> Iterable[Completion]:
        """Complete arguments for a direct method invocation."""
        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if not node or node.kind != NodeKind.SERVICE:
            return

        m_node = node.child(method_name)
        if m_node is None:
            return

        # Complete field names from metadata
        fields = m_node.metadata.get("fields", [])
        for f in fields:
            name = f.get("name", "")
            if not name:
                continue
            completion = f"{name}="
            if completion.lower().startswith(partial.lower()):
                ftype = f.get("type", "")
                yield Completion(
                    completion,
                    start_position=-len(partial),
                    display=completion,
                    display_meta=ftype,
                )


def _short_sig(metadata: dict) -> str:
    """Short method signature for completion display."""
    req = metadata.get("request_type", "")
    resp = metadata.get("response_type", "")
    if req and resp:
        return f"({req}) ->{resp}"
    return ""
