"""
aster_cli.main -- Entry point for the ``aster`` CLI.

Delegates to subcommand modules:
  aster contract gen ...   → aster_cli.contract

Root keys, enrollment credentials and trust profiles are managed by the Rust
``aster`` CLI (``aster-cli``): ``aster key root generate``, ``aster trust
init``, ``aster enroll node``, ``aster enroll delegate``, ``aster enroll
revoke``.
"""

from __future__ import annotations

from aster_cli.contract import main


__all__ = ["main"]
