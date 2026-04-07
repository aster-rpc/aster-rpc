"""
aster_cli.mcp — MCP (Model Context Protocol) server for Aster services.

Exposes Aster RPC services as MCP tools, enabling AI agents to discover
and call services dynamically with full type information and capability-based
security.

Usage::

    aster mcp <peer-addr> [--rcan PATH] [--allow PATTERN] [--deny PATTERN]
"""

from __future__ import annotations
