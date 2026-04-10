"""
aster_cli.shell.plugin -- Plugin system for shell commands.

Every interactive shell command is a plugin that can also be invoked
as a CLI subcommand (e.g., ``ls`` at /blobs ↔ ``aster blob ls``).

Plugins self-register via the @register decorator, declaring:
  - name: the command name (e.g., "ls", "cat", "describe")
  - context: glob pattern for where the command is valid (e.g., "/blobs", "/services/*")
  - cli_noun_verb: optional (noun, verb) tuple for CLI mapping (e.g., ("blob", "ls"))
"""

from __future__ import annotations

import argparse
import fnmatch
from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from aster_cli.shell.vfs import VfsNode


@dataclass
class Argument:
    """Describes a command argument for both autocomplete and CLI argparse."""

    name: str
    description: str = ""
    required: bool = False
    positional: bool = False
    choices: list[str] | None = None
    default: Any = None
    type: type = str


class ShellCommand(ABC):
    """Base class for all shell commands (plugins).

    Subclass this and use @register to make it available in the shell
    and optionally as a CLI subcommand.
    """

    # Command metadata -- set by subclasses
    name: str = ""
    description: str = ""
    contexts: list[str] = []  # glob patterns for valid VFS paths
    cli_noun_verb: tuple[str, str] | None = None  # ("blob", "ls") → aster blob ls
    hidden: bool = False  # hide from help/autocomplete

    @abstractmethod
    async def execute(
        self,
        args: list[str],
        ctx: CommandContext,
    ) -> None:
        """Execute the command.

        Args:
            args: Parsed argument tokens after the command name.
            ctx: Execution context with VFS, display, connection.
        """

    def get_arguments(self) -> list[Argument]:
        """Return argument definitions for autocomplete and CLI registration."""
        return []

    def get_completions(self, ctx: CommandContext, partial: str) -> list[str]:
        """Return completion suggestions for the current partial input.

        Override for dynamic completions (e.g., blob hashes, method params).
        """
        return []

    def is_valid_at(self, path: str) -> bool:
        """Check if this command is valid at the given VFS path."""
        if not self.contexts:
            return True  # global command
        return any(fnmatch.fnmatch(path, pat) for pat in self.contexts)


@dataclass
class CommandContext:
    """Runtime context passed to every command execution."""

    vfs_cwd: str  # current VFS path
    vfs_root: VfsNode  # root of the VFS tree
    connection: Any  # AsterClient or peer connection
    display: Any  # Display instance for rich output
    peer_name: str = ""  # display name of connected peer
    interactive: bool = True  # False when called from CLI
    raw_output: bool = False  # True for pipe-friendly JSON output
    guide: Any = None  # GuideManager instance (optional)
    # Active session (set inside `session <ServiceName>` subshells). When
    # set, method invocations are routed through the persistent session
    # bidi stream instead of opening a new stream per call -- the only
    # way to call methods on session-scoped services from the shell.
    session: Any = None


# ── Plugin registry ───────────────────────────────────────────────────────────

_registry: dict[str, ShellCommand] = {}


def register(cmd_class: type[ShellCommand]) -> type[ShellCommand]:
    """Class decorator to register a shell command plugin.

    Usage::

        @register
        class LsCommand(ShellCommand):
            name = "ls"
            description = "List contents"
            contexts = ["/", "/blobs", "/services", "/services/*"]
    """
    instance = cmd_class()
    _registry[instance.name] = instance
    return cmd_class


def get_command(name: str) -> ShellCommand | None:
    """Look up a registered command by name."""
    return _registry.get(name)


def get_commands_for_path(path: str) -> list[ShellCommand]:
    """Get all commands valid at the given VFS path."""
    return [cmd for cmd in _registry.values() if cmd.is_valid_at(path) and not cmd.hidden]


def get_all_commands() -> dict[str, ShellCommand]:
    """Get all registered commands."""
    return dict(_registry)


def register_cli_subcommands(subparsers: argparse._SubParsersAction) -> None:
    """Register all plugin CLI noun-verb subcommands with argparse.

    This creates subcommands like ``aster blob ls``, ``aster service describe``, etc.
    """
    # Group commands by CLI noun
    noun_groups: dict[str, list[ShellCommand]] = {}
    for cmd in _registry.values():
        if cmd.cli_noun_verb:
            noun, _verb = cmd.cli_noun_verb
            noun_groups.setdefault(noun, []).append(cmd)

    for noun, commands in sorted(noun_groups.items()):
        noun_parser = subparsers.add_parser(noun, help=f"{noun.title()} commands")
        noun_subs = noun_parser.add_subparsers(dest=f"{noun}_command")

        for cmd in commands:
            _noun, verb = cmd.cli_noun_verb  # type: ignore[misc]
            verb_parser = noun_subs.add_parser(verb, help=cmd.description)

            # Add common args
            verb_parser.add_argument(
                "peer", help="Peer address to connect to"
            )
            verb_parser.add_argument(
                "--rcan", default=None, help="Path to RCAN credential"
            )
            verb_parser.add_argument(
                "--json", action="store_true", dest="raw_json",
                help="Output raw JSON (for scripting)",
            )

            # Add command-specific args
            for arg in cmd.get_arguments():
                if arg.positional:
                    verb_parser.add_argument(
                        arg.name,
                        nargs="?" if not arg.required else None,
                        default=arg.default,
                        help=arg.description,
                    )
                else:
                    verb_parser.add_argument(
                        f"--{arg.name}",
                        required=arg.required,
                        default=arg.default,
                        help=arg.description,
                    )
