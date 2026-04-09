"""
aster_cli.shell -- Interactive shell for exploring Aster peers.

Usage::

    aster shell <peer-addr> [--rcan <path>]

Provides a filesystem-like interface to navigate blobs, services,
and gossip topics on a connected peer, with dynamic RPC invocation
and smart autocomplete.
"""

from __future__ import annotations

from aster_cli.shell.app import launch_shell, register_shell_subparser, run_shell_command

__all__ = ["launch_shell", "register_shell_subparser", "run_shell_command"]
