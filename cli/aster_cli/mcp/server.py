"""
aster_cli.mcp.server -- MCP server that exposes Aster services as tools.

Connects to an Aster peer, discovers services from contract manifests,
and exposes each method as an MCP tool over stdio. AI agents (Claude, etc.)
can then discover and call Aster services with full type information.

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
from typing import Any

from mcp.server.fastmcp import FastMCP
from mcp.types import TextContent

from aster_cli.mcp.schema import method_to_tool_definition, service_to_tool_definitions
from aster_cli.mcp.security import ToolFilter
from aster_cli.shell.app import PeerConnection
from aster_cli.shell.invoker import _to_serializable

logger = logging.getLogger(__name__)


class AsterMcpServer:
    """MCP server bridging Aster services to AI agents.

    Lifecycle:
    1. Connect to peer (or use DemoConnection)
    2. Discover services from manifests
    3. Register each method as an MCP tool
    4. Serve tool calls via stdio
    """

    def __init__(
        self,
        connection: Any,
        tool_filter: ToolFilter | None = None,
    ) -> None:
        self._connection = connection
        self._filter = tool_filter or ToolFilter()
        self._mcp = FastMCP(
            "aster-gateway",
            instructions=(
                "This MCP server exposes Aster RPC services as tools. "
                "Each tool corresponds to a method on a remote service. "
                "Tool names are formatted as ServiceName.method_name. "
                "Call tools with the required parameters as described in their schemas."
            ),
        )
        self._tools: dict[str, dict[str, Any]] = {}  # tool_name → method metadata

    async def setup(self) -> None:
        """Connect to peer, discover services, register tools."""
        await self._connection.connect()

        # Wait for manifest fetch so we get method-level detail
        if hasattr(self._connection, "wait_for_manifests"):
            await self._connection.wait_for_manifests()

        services = await self._connection.list_services()
        tool_count = 0

        for svc in services:
            tool_defs = service_to_tool_definitions(svc)
            for tool_def in tool_defs:
                tool_name = tool_def["name"]

                # Apply security filter
                if not self._filter.is_visible(tool_name):
                    logger.debug("Tool filtered out: %s", tool_name)
                    continue

                # Store method metadata for invocation
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

                # Register with FastMCP
                self._register_tool(tool_name, tool_def, method_meta or {})
                tool_count += 1

        logger.info(
            "Registered %d tools from %d services", tool_count, len(services)
        )

    def _register_tool(
        self,
        tool_name: str,
        tool_def: dict[str, Any],
        method_meta: dict[str, Any],
    ) -> None:
        """Register a single tool with the FastMCP server.

        Builds a handler function whose parameter names and type hints
        match the manifest fields, so FastMCP generates an accurate
        JSON Schema (instead of generic **kwargs).
        """
        import inspect

        service_name = tool_name.split(".")[0]
        method_name = tool_name.split(".", 1)[1] if "." in tool_name else tool_name
        pattern = method_meta.get("pattern", "unary")

        # Build typed handler so FastMCP infers the correct inputSchema
        handler = _build_typed_handler(
            self, service_name, method_name, pattern, tool_def,
        )

        self._mcp.add_tool(
            fn=handler,
            name=tool_name,
            description=tool_def.get("description", ""),
        )

    async def _handle_call(
        self,
        service_name: str,
        method_name: str,
        pattern: str,
        arguments: dict[str, Any],
    ) -> str:
        """Handle an MCP tool call by invoking the Aster service."""
        tool_name = f"{service_name}.{method_name}"

        # Human-in-the-loop confirmation
        if self._filter.needs_confirmation(tool_name):
            approved = await self._filter.confirm_call(tool_name, arguments)
            if not approved:
                return json.dumps({"error": "Call denied by operator"})

        # Strip meta-parameters
        max_items = arguments.pop("aster_max_items", 100)
        timeout = arguments.pop("aster_timeout", 30.0)
        items = arguments.pop("aster_items", None)

        try:
            if pattern == "unary":
                result = await self._connection.invoke(
                    service_name, method_name, arguments
                )
                return json.dumps(_to_serializable(result), indent=2, default=str)

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
                return json.dumps(collected, indent=2, default=str)

            elif pattern == "client_stream":
                if items is None:
                    return json.dumps({"error": "client_stream requires _items parameter"})
                result = await self._connection.client_stream(
                    service_name, method_name, items
                )
                return json.dumps(_to_serializable(result), indent=2, default=str)

            else:
                return json.dumps({"error": f"Unsupported pattern: {pattern}"})

        except Exception as e:
            logger.error("Tool call %s failed: %s", tool_name, e)
            return json.dumps({"error": str(e)}, default=str)

    async def run_stdio(self) -> None:
        """Run the MCP server over stdio."""
        await self._mcp.run_stdio_async()

    @property
    def mcp(self) -> FastMCP:
        """Access the underlying FastMCP instance."""
        return self._mcp

    @property
    def tool_count(self) -> int:
        return len(self._tools)

    @property
    def tool_names(self) -> list[str]:
        return sorted(self._tools.keys())


_FIELD_TYPE_MAP = {
    "str": str, "string": str,
    "int": int, "integer": int, "int32": int, "int64": int,
    "float": float, "double": float, "number": float,
    "bool": bool, "boolean": bool,
}


def _build_typed_handler(
    server: AsterMcpServer,
    service_name: str,
    method_name: str,
    pattern: str,
    tool_def: dict[str, Any],
) -> Any:
    """Build an async handler with typed parameters from the tool inputSchema.

    FastMCP inspects the handler's signature to generate JSON Schema.
    By creating a function with explicit parameter names and type hints,
    the MCP tools/list response includes field-level detail instead of
    a generic **kwargs blob.
    """
    import inspect

    # Extract field info from the inputSchema
    schema = tool_def.get("inputSchema", {})
    properties = schema.get("properties", {})
    required_set = set(schema.get("required", []))

    # Filter out internal meta-params that we add ourselves
    _META_PARAMS = {"aster_max_items", "aster_timeout", "aster_items"}
    field_params = {
        k: v for k, v in properties.items()
        if k not in _META_PARAMS
    }

    # Build inspect.Parameter list
    params = [
        inspect.Parameter("self_unused", inspect.Parameter.POSITIONAL_OR_KEYWORD),
    ]
    annotations: dict[str, Any] = {}

    for fname, fschema in field_params.items():
        json_type = fschema.get("type", "string")
        py_type = _FIELD_TYPE_MAP.get(json_type, str)
        default = fschema.get("default", inspect.Parameter.empty)
        if fname not in required_set and default is inspect.Parameter.empty:
            default = None

        params.append(inspect.Parameter(
            fname,
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
            default=default,
            annotation=py_type,
        ))
        annotations[fname] = py_type

    # Add streaming meta-params back
    for meta_name in ("aster_max_items", "aster_timeout", "aster_items"):
        if meta_name in properties:
            meta_schema = properties[meta_name]
            json_type = meta_schema.get("type", "string")
            py_type = _FIELD_TYPE_MAP.get(json_type, Any)
            default = meta_schema.get("default", inspect.Parameter.empty)
            params.append(inspect.Parameter(
                meta_name,
                inspect.Parameter.POSITIONAL_OR_KEYWORD,
                default=default,
                annotation=py_type,
            ))

    # If no fields were found, fall back to **kwargs
    if len(params) == 1:
        async def fallback_handler(**kwargs: Any) -> str:
            return await server._handle_call(service_name, method_name, pattern, kwargs)
        return fallback_handler

    # Remove the self_unused placeholder
    params = params[1:]

    # Build the handler dynamically
    async def typed_handler(**kwargs: Any) -> str:
        return await server._handle_call(service_name, method_name, pattern, kwargs)

    # Patch the signature so FastMCP sees the real fields
    typed_handler.__signature__ = inspect.Signature(params)
    typed_handler.__annotations__ = annotations

    return typed_handler


async def _collect_stream(stream: Any, max_items: int) -> Any:
    """Async generator wrapper that collects stream items up to max."""
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
    """Launch the MCP server.

    Args:
        peer_addr: Aster peer address (aster1... ticket or base64 NodeAddr).
        rcan_path: Path to enrollment credential.
        allow: Glob patterns for allowed tools.
        deny: Glob patterns for denied tools.
        confirm: Glob patterns requiring human approval.
    """
    connection = PeerConnection(peer_addr=peer_addr, rcan_path=rcan_path)

    tool_filter = ToolFilter(allow=allow, deny=deny, confirm=confirm)

    server = AsterMcpServer(connection, tool_filter=tool_filter)
    await server.setup()

    logger.info(
        "MCP server ready: %d tools. Listening on stdio.",
        server.tool_count,
    )

    # Print tool summary to stderr (not stdout -- that's for MCP protocol)
    sys.stderr.write(f"\nAster MCP Gateway -- {server.tool_count} tools\n")
    for name in server.tool_names:
        sys.stderr.write(f"  {name}\n")
    sys.stderr.write("\n")

    try:
        await server.run_stdio()
    finally:
        await connection.close()


def register_mcp_subparser(subparsers: argparse._SubParsersAction) -> None:
    """Register the ``aster mcp`` subcommand."""
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
    """Execute the ``aster mcp`` command."""
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
