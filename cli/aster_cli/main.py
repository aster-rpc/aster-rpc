"""
aster_cli.main -- Entry point for the ``aster`` CLI.

Delegates to subcommand modules:
  aster contract gen ...   → aster_cli.contract
  aster trust keygen ...   → aster_cli.trust
  aster trust sign ...     → aster_cli.trust
"""

from __future__ import annotations

from aster_cli.contract import main


__all__ = ["main"]
