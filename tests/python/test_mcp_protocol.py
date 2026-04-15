"""Tests for the lightweight MCP JSON-RPC 2.0 server (no mcp package dependency)."""

from __future__ import annotations

import json

import pytest

from aster_cli.mcp.server import AsterMcpServer, _ok, _error


# ── Helpers ──────────────────────────────────────────────────────────────────


def _req(method: str, msg_id: int = 1, params: dict | None = None) -> dict:
    msg = {"jsonrpc": "2.0", "id": msg_id, "method": method}
    if params is not None:
        msg["params"] = params
    return msg


class StubConnection:
    """Minimal connection that returns canned services."""

    def __init__(self, services: list[dict] | None = None):
        self._services = services or [
            {
                "name": "Greeter",
                "methods": [
                    {
                        "name": "hello",
                        "pattern": "unary",
                        "request_type": "Req",
                        "response_type": "Resp",
                        "fields": [
                            {"name": "name", "type": "str", "required": True},
                        ],
                    },
                    {
                        "name": "stream_greets",
                        "pattern": "server_stream",
                        "fields": [],
                    },
                    {
                        "name": "chat",
                        "pattern": "bidi_stream",
                        "fields": [],
                    },
                ],
            },
        ]

    async def connect(self):
        pass

    async def list_services(self):
        return self._services

    async def invoke(self, service, method, args):
        return {"echo": args}


async def _make_server(services=None, **filter_kwargs) -> AsterMcpServer:
    from aster_cli.mcp.security import ToolFilter

    conn = StubConnection(services)
    filt = ToolFilter(**filter_kwargs) if filter_kwargs else None
    server = AsterMcpServer(conn, tool_filter=filt)
    await server.setup()
    return server


# ── JSON-RPC helpers ─────────────────────────────────────────────────────────


class TestJsonRpcHelpers:
    def test_ok(self):
        resp = _ok(42, {"tools": []})
        assert resp == {"jsonrpc": "2.0", "id": 42, "result": {"tools": []}}

    def test_error(self):
        resp = _error(7, -32601, "Not found")
        assert resp == {
            "jsonrpc": "2.0",
            "id": 7,
            "error": {"code": -32601, "message": "Not found"},
        }


# ── Protocol dispatch tests ─────────────────────────────────────────────────


class TestInitialize:
    @pytest.mark.asyncio
    async def test_returns_capabilities(self):
        server = await _make_server()
        resp = await server._dispatch(_req("initialize"))

        assert resp["id"] == 1
        result = resp["result"]
        assert "protocolVersion" in result
        assert result["capabilities"] == {"tools": {}}
        assert result["serverInfo"]["name"] == "aster-gateway"


class TestToolsList:
    @pytest.mark.asyncio
    async def test_lists_registered_tools(self):
        server = await _make_server()
        resp = await server._dispatch(_req("tools/list"))

        tools = resp["result"]["tools"]
        names = [t["name"] for t in tools]
        assert "Greeter.hello" in names
        assert "Greeter.stream_greets" in names

    @pytest.mark.asyncio
    async def test_bidi_excluded(self):
        server = await _make_server()
        resp = await server._dispatch(_req("tools/list"))

        names = [t["name"] for t in resp["result"]["tools"]]
        assert "Greeter.chat" not in names

    @pytest.mark.asyncio
    async def test_tools_have_input_schema(self):
        server = await _make_server()
        resp = await server._dispatch(_req("tools/list"))

        hello = next(t for t in resp["result"]["tools"] if t["name"] == "Greeter.hello")
        schema = hello["inputSchema"]
        assert schema["type"] == "object"
        assert "name" in schema["properties"]

    @pytest.mark.asyncio
    async def test_filtered_tools(self):
        server = await _make_server(deny=["Greeter.stream_*"])
        resp = await server._dispatch(_req("tools/list"))

        names = [t["name"] for t in resp["result"]["tools"]]
        assert "Greeter.hello" in names
        assert "Greeter.stream_greets" not in names


class TestToolsCall:
    @pytest.mark.asyncio
    async def test_unary_call(self):
        server = await _make_server()
        resp = await server._dispatch(_req("tools/call", params={
            "name": "Greeter.hello",
            "arguments": {"name": "world"},
        }))

        result = resp["result"]
        assert not result.get("isError")
        content = result["content"]
        assert len(content) == 1
        assert content[0]["type"] == "text"
        parsed = json.loads(content[0]["text"])
        assert parsed["echo"]["name"] == "world"

    @pytest.mark.asyncio
    async def test_unknown_tool(self):
        server = await _make_server()
        resp = await server._dispatch(_req("tools/call", params={
            "name": "NoSuch.method",
            "arguments": {},
        }))

        content = resp["result"]["content"]
        parsed = json.loads(content[0]["text"])
        assert "error" in parsed

    @pytest.mark.asyncio
    async def test_meta_params_stripped(self):
        server = await _make_server()
        resp = await server._dispatch(_req("tools/call", params={
            "name": "Greeter.hello",
            "arguments": {"name": "test", "aster_max_items": 50, "aster_timeout": 10},
        }))

        content = resp["result"]["content"]
        parsed = json.loads(content[0]["text"])
        assert "aster_max_items" not in parsed.get("echo", {})
        assert "aster_timeout" not in parsed.get("echo", {})


class TestPing:
    @pytest.mark.asyncio
    async def test_ping(self):
        server = await _make_server()
        resp = await server._dispatch(_req("ping"))
        assert resp["result"] == {}


class TestNotifications:
    @pytest.mark.asyncio
    async def test_initialized_returns_none(self):
        server = await _make_server()
        resp = await server._dispatch(
            {"jsonrpc": "2.0", "method": "notifications/initialized"}
        )
        assert resp is None

    @pytest.mark.asyncio
    async def test_unknown_notification_returns_none(self):
        server = await _make_server()
        resp = await server._dispatch(
            {"jsonrpc": "2.0", "method": "notifications/whatever"}
        )
        assert resp is None


class TestUnknownMethod:
    @pytest.mark.asyncio
    async def test_returns_method_not_found(self):
        server = await _make_server()
        resp = await server._dispatch(_req("some/unknown"))
        assert resp["error"]["code"] == -32601
