"""
aster_cli.mcp.server -- MCP server that exposes Aster services as tools.

Implements the MCP (Model Context Protocol) JSON-RPC 2.0 stdio transport
directly -- no external MCP library needed. The protocol surface is small:
initialize, tools/list, tools/call.

Usage::

    aster mcp <peer-addr>                              # all tools
    aster mcp <peer-addr> --allow "Hello*:*"           # filtered
    aster mcp <peer-addr> --rcan ai-agent.token        # scoped credential
    aster mcp --demo                                   # offline demo
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import sys
from typing import Any, TYPE_CHECKING

if TYPE_CHECKING:
    from aster_cli.mcp.security import ToolFilter

logger = logging.getLogger(__name__)

PROTOCOL_VERSION = "2025-03-26"
SERVER_NAME = "aster-gateway"
SERVER_VERSION = "0.1.0"
SERVER_INSTRUCTIONS = (
    "This MCP server exposes Aster RPC services as tools. "
    "Each tool corresponds to a method on a remote service. "
    "Tool names are formatted as ServiceName.method_name. "
    "Call tools with the required parameters as described in their schemas."
)


class AsterMcpServer:
    """MCP server bridging Aster services to AI agents."""

    def __init__(
        self,
        connection: Any,
        tool_filter: ToolFilter | None = None,
    ) -> None:
        from aster_cli.mcp.security import ToolFilter as _ToolFilter

        self._connection = connection
        self._filter = tool_filter or _ToolFilter()
        self._tools: dict[str, dict[str, Any]] = {}

    async def setup(self) -> None:
        """Connect to peer, discover services, register tools."""
        await self._connection.connect()

        if hasattr(self._connection, "wait_for_manifests"):
            await self._connection.wait_for_manifests()

        from aster_cli.mcp.schema import service_to_tool_definitions

        services = await self._connection.list_services()
        tool_count = 0

        for svc in services:
            tool_defs = service_to_tool_definitions(svc)
            for tool_def in tool_defs:
                tool_name = tool_def["name"]
                tool_tags = list(tool_def.get("x-aster-tags", []) or [])

                if not self._filter.is_visible(tool_name, tool_tags):
                    logger.debug("Tool filtered out: %s", tool_name)
                    continue

                method_meta = None
                for m in svc.get("methods", []):
                    if f"{svc['name']}.{m['name']}" == tool_name:
                        method_meta = m
                        break

                self._tools[tool_name] = {
                    "service": svc["name"],
                    "method": method_meta or {},
                    "definition": tool_def,
                }
                tool_count += 1

        logger.info(
            "Registered %d tools from %d services", tool_count, len(services)
        )

    def _get_tool_list(self) -> list[dict[str, Any]]:
        return [
            {
                "name": name,
                "description": meta["definition"].get("description", ""),
                "inputSchema": meta["definition"].get("inputSchema", {}),
            }
            for name, meta in sorted(self._tools.items())
        ]

    async def _handle_call(
        self,
        tool_name: str,
        arguments: dict[str, Any],
    ) -> list[dict[str, Any]]:
        """Handle a tools/call request. Returns MCP content blocks."""
        from aster_cli.shell.invoker import _to_serializable

        meta = self._tools.get(tool_name)
        if meta is None:
            return [{"type": "text", "text": json.dumps({"error": f"Unknown tool: {tool_name}"})}]

        service_name = meta["service"]
        method_name = meta["method"].get("name", tool_name.split(".")[-1])
        pattern = meta["method"].get("pattern", "unary")

        tool_tags = list(meta["method"].get("tags", []) or [])
        if self._filter.needs_confirmation(tool_name, tool_tags):
            approved = await self._filter.confirm_call(tool_name, arguments)
            if not approved:
                return [{"type": "text", "text": json.dumps({"error": "Call denied by operator"})}]

        max_items = arguments.pop("aster_max_items", 100)
        timeout = arguments.pop("aster_timeout", 30.0)
        items = arguments.pop("aster_items", None)

        try:
            if pattern == "unary":
                result = await self._connection.invoke(
                    service_name, method_name, arguments
                )
                text = json.dumps(_to_serializable(result), indent=2, default=str)

            elif pattern == "server_stream":
                stream = await self._connection.server_stream(
                    service_name, method_name, arguments
                )
                collected = []
                try:
                    async for item in asyncio.wait_for(
                        _collect_stream(stream, max_items), timeout=timeout
                    ):
                        collected.append(_to_serializable(item))
                except asyncio.TimeoutError:
                    pass
                text = json.dumps(collected, indent=2, default=str)

            elif pattern == "client_stream":
                if items is None:
                    return [{"type": "text", "text": json.dumps({"error": "client_stream requires aster_items parameter"})}]
                result = await self._connection.client_stream(
                    service_name, method_name, items
                )
                text = json.dumps(_to_serializable(result), indent=2, default=str)

            else:
                text = json.dumps({"error": f"Unsupported pattern: {pattern}"})

        except Exception as e:
            logger.error("Tool call %s failed: %s", tool_name, e)
            text = json.dumps({"error": str(e)}, default=str)
            return [{"type": "text", "text": text, "isError": True}]

        return [{"type": "text", "text": text}]

    async def _dispatch(self, msg: dict[str, Any]) -> dict[str, Any] | None:
        """Route a JSON-RPC 2.0 request to the appropriate handler."""
        method = msg.get("method", "")
        msg_id = msg.get("id")
        params = msg.get("params", {})

        if method == "initialize":
            return _ok(msg_id, {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                "instructions": SERVER_INSTRUCTIONS,
            })

        if method == "notifications/initialized":
            return None

        if method == "tools/list":
            return _ok(msg_id, {"tools": self._get_tool_list()})

        if method == "tools/call":
            name = params.get("name", "")
            arguments = params.get("arguments", {})
            content = await self._handle_call(name, arguments)
            is_error = any(c.get("isError") for c in content)
            return _ok(msg_id, {"content": content, "isError": is_error})

        if method == "ping":
            return _ok(msg_id, {})

        if method.startswith("notifications/"):
            return None

        return _error(msg_id, -32601, f"Method not found: {method}")

    async def run_stdio(self) -> None:
        """Serve MCP over stdin/stdout using JSON-RPC 2.0."""
        reader = asyncio.StreamReader()
        protocol = asyncio.StreamReaderProtocol(reader)
        await asyncio.get_event_loop().connect_read_pipe(lambda: protocol, sys.stdin)

        loop = asyncio.get_event_loop()
        transport, _ = await loop.connect_write_pipe(asyncio.BaseProtocol, sys.stdout)

        while True:
            line = await reader.readline()
            if not line:
                break

            line = line.strip()
            if not line:
                continue

            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                resp = _error(None, -32700, "Parse error")
                transport.write((json.dumps(resp) + "\n").encode())
                continue

            resp = await self._dispatch(msg)
            if resp is not None:
                transport.write((json.dumps(resp) + "\n").encode())

    @property
    def tool_count(self) -> int:
        return len(self._tools)

    @property
    def tool_names(self) -> list[str]:
        return sorted(self._tools.keys())


def _ok(msg_id: Any, result: dict[str, Any]) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": msg_id, "result": result}


def _error(msg_id: Any, code: int, message: str) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": msg_id, "error": {"code": code, "message": message}}


async def _collect_stream(stream: Any, max_items: int) -> Any:
    count = 0
    async for item in stream:
        yield item
        count += 1
        if count >= max_items:
            break


# ── CLI entry points ──────────────────────────────────────────────────────────


async def launch_mcp_server(
    peer_addr: str,
    rcan_path: str | None = None,
    allow: list[str] | None = None,
    deny: list[str] | None = None,
    confirm: list[str] | None = None,
) -> None:
    from aster_cli.mcp.security import ToolFilter
    from aster_cli.shell.app import PeerConnection

    connection = PeerConnection(peer_addr=peer_addr, rcan_path=rcan_path)
    tool_filter = ToolFilter(allow=allow, deny=deny, confirm=confirm)

    server = AsterMcpServer(connection, tool_filter=tool_filter)
    await server.setup()

    sys.stderr.write(f"\nAster MCP Gateway -- {server.tool_count} tools\n")
    for name in server.tool_names:
        sys.stderr.write(f"  {name}\n")
    sys.stderr.write("\n")

    try:
        await server.run_stdio()
    finally:
        await connection.close()


def register_mcp_subparser(subparsers: argparse._SubParsersAction) -> None:
    mcp_parser = subparsers.add_parser(
        "mcp",
        help="Run MCP tool server for AI agent integration",
    )
    mcp_parser.add_argument(
        "peer",
        metavar="ADDRESS",
        help="Peer address (aster1... ticket)",
    )
    mcp_parser.add_argument(
        "--rcan",
        default=None,
        metavar="PATH",
        help="Path to enrollment credential (scoped AI agent credential)",
    )
    mcp_parser.add_argument(
        "--allow",
        action="append",
        default=[],
        metavar="PATTERN",
        help="Glob pattern for allowed tools (e.g., 'HelloService.*')",
    )
    mcp_parser.add_argument(
        "--deny",
        action="append",
        default=[],
        metavar="PATTERN",
        help="Glob pattern for denied tools (e.g., '*.delete_*')",
    )
    mcp_parser.add_argument(
        "--confirm",
        action="append",
        default=[],
        metavar="PATTERN",
        help="Glob pattern for tools requiring human approval",
    )


def run_mcp_command(args: argparse.Namespace) -> int:
    try:
        asyncio.run(launch_mcp_server(
            peer_addr=args.peer,
            rcan_path=args.rcan,
            allow=args.allow or None,
            deny=args.deny or None,
            confirm=args.confirm or None,
        ))
    except KeyboardInterrupt:
        pass

    return 0
