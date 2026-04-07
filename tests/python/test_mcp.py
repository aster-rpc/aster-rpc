"""Tests for the Aster MCP server: schema conversion, security filtering, and server setup."""

from __future__ import annotations

import json

import pytest

from aster_cli.mcp.schema import (
    aster_type_to_json_schema,
    field_to_json_schema,
    method_to_tool_definition,
    service_to_tool_definitions,
)
from aster_cli.mcp.security import ToolFilter


# ── Schema conversion tests ───────────────────────────────────────────────────


class TestAsterTypeToJsonSchema:
    def test_str(self):
        assert aster_type_to_json_schema("str") == {"type": "string"}

    def test_int(self):
        assert aster_type_to_json_schema("int") == {"type": "integer"}

    def test_float(self):
        assert aster_type_to_json_schema("float") == {"type": "number"}

    def test_bool(self):
        assert aster_type_to_json_schema("bool") == {"type": "boolean"}

    def test_bytes(self):
        result = aster_type_to_json_schema("bytes")
        assert result["type"] == "string"
        assert result["contentEncoding"] == "base64"

    def test_list_str(self):
        result = aster_type_to_json_schema("list[str]")
        assert result == {"type": "array", "items": {"type": "string"}}

    def test_list_int(self):
        result = aster_type_to_json_schema("list[int]")
        assert result == {"type": "array", "items": {"type": "integer"}}

    def test_dict_str_int(self):
        result = aster_type_to_json_schema("dict[str, int]")
        assert result == {"type": "object", "additionalProperties": {"type": "integer"}}

    def test_optional_str(self):
        result = aster_type_to_json_schema("Optional[str]")
        assert result["type"] == "string"
        assert result["nullable"] is True

    def test_nested_list(self):
        result = aster_type_to_json_schema("list[list[str]]")
        assert result["type"] == "array"
        assert result["items"]["type"] == "array"

    def test_unknown_type(self):
        result = aster_type_to_json_schema("MyCustomType")
        assert result["type"] == "object"
        assert "MyCustomType" in result.get("description", "")

    def test_empty_string(self):
        result = aster_type_to_json_schema("")
        assert result["type"] == "string"

    def test_case_insensitive_primitives(self):
        assert aster_type_to_json_schema("String")["type"] == "string"
        assert aster_type_to_json_schema("Int")["type"] == "integer"
        assert aster_type_to_json_schema("Bool")["type"] == "boolean"


class TestFieldToJsonSchema:
    def test_basic_field(self):
        result = field_to_json_schema({"name": "x", "type": "str"})
        assert result == {"type": "string"}

    def test_field_with_default(self):
        result = field_to_json_schema({"name": "x", "type": "str", "default": "hello"})
        assert result["default"] == "hello"

    def test_field_with_description(self):
        result = field_to_json_schema({"name": "x", "type": "int", "description": "The count"})
        assert result["description"] == "The count"


class TestMethodToToolDefinition:
    def test_unary_method(self):
        tool = method_to_tool_definition("Svc", {
            "name": "do_thing",
            "pattern": "unary",
            "request_type": "Req",
            "response_type": "Resp",
            "fields": [{"name": "id", "type": "int", "required": True}],
        })
        assert tool["name"] == "Svc:do_thing"
        assert "unary" in tool["description"]
        assert tool["inputSchema"]["properties"]["id"]["type"] == "integer"
        assert "id" in tool["inputSchema"]["required"]

    def test_server_stream_adds_meta_params(self):
        tool = method_to_tool_definition("Svc", {
            "name": "watch",
            "pattern": "server_stream",
            "fields": [],
        })
        assert "_max_items" in tool["inputSchema"]["properties"]
        assert "_timeout" in tool["inputSchema"]["properties"]

    def test_client_stream_adds_items_param(self):
        tool = method_to_tool_definition("Svc", {
            "name": "upload",
            "pattern": "client_stream",
            "fields": [],
        })
        assert "_items" in tool["inputSchema"]["properties"]
        assert "_items" in tool["inputSchema"]["required"]

    def test_optional_field_not_required(self):
        tool = method_to_tool_definition("Svc", {
            "name": "m",
            "pattern": "unary",
            "fields": [
                {"name": "required_field", "type": "str", "required": True},
                {"name": "optional_field", "type": "str", "required": False, "default": "x"},
            ],
        })
        assert "required_field" in tool["inputSchema"]["required"]
        assert "optional_field" not in tool["inputSchema"]["required"]

    def test_timeout_in_description(self):
        tool = method_to_tool_definition("Svc", {
            "name": "m",
            "pattern": "unary",
            "timeout": 15.0,
            "fields": [],
        })
        assert "15.0s" in tool["description"]


class TestServiceToToolDefinitions:
    def test_basic_service(self):
        tools = service_to_tool_definitions({
            "name": "Hello",
            "methods": [
                {"name": "greet", "pattern": "unary", "fields": []},
                {"name": "stream", "pattern": "server_stream", "fields": []},
            ],
        })
        names = [t["name"] for t in tools]
        assert "Hello:greet" in names
        assert "Hello:stream" in names

    def test_bidi_stream_excluded(self):
        tools = service_to_tool_definitions({
            "name": "Chat",
            "methods": [
                {"name": "unary_m", "pattern": "unary", "fields": []},
                {"name": "bidi_m", "pattern": "bidi_stream", "fields": []},
            ],
        })
        names = [t["name"] for t in tools]
        assert "Chat:unary_m" in names
        assert "Chat:bidi_m" not in names

    def test_empty_methods(self):
        tools = service_to_tool_definitions({"name": "Empty", "methods": []})
        assert tools == []

    def test_missing_methods_key(self):
        tools = service_to_tool_definitions({"name": "NoMethods"})
        assert tools == []


# ── Security filter tests ─────────────────────────────────────────────────────


class TestToolFilter:
    def test_no_filters_all_visible(self):
        filt = ToolFilter()
        assert filt.is_visible("Anything:any_method")

    def test_deny_hides(self):
        filt = ToolFilter(deny=["*:delete_*"])
        assert filt.is_visible("Svc:get_data")
        assert not filt.is_visible("Svc:delete_record")

    def test_allow_restricts(self):
        filt = ToolFilter(allow=["Hello*:*"])
        assert filt.is_visible("HelloService:say_hello")
        assert not filt.is_visible("OtherService:get_data")

    def test_deny_wins_over_allow(self):
        filt = ToolFilter(allow=["Svc:*"], deny=["Svc:delete_*"])
        assert filt.is_visible("Svc:get_data")
        assert not filt.is_visible("Svc:delete_record")

    def test_confirm_patterns(self):
        filt = ToolFilter(confirm=["*:write_*", "*:admin_*"])
        assert filt.needs_confirmation("Svc:write_record")
        assert filt.needs_confirmation("Svc:admin_reset")
        assert not filt.needs_confirmation("Svc:get_data")

    def test_no_confirm_patterns(self):
        filt = ToolFilter()
        assert not filt.needs_confirmation("Anything:any")

    def test_has_filters(self):
        assert not ToolFilter().has_filters
        assert ToolFilter(allow=["*"]).has_filters
        assert ToolFilter(deny=["*"]).has_filters
        assert ToolFilter(confirm=["*"]).has_filters

    def test_multiple_allow(self):
        filt = ToolFilter(allow=["Hello:*", "Status:*"])
        assert filt.is_visible("Hello:greet")
        assert filt.is_visible("Status:check")
        assert not filt.is_visible("Other:method")

    def test_wildcard_patterns(self):
        filt = ToolFilter(deny=["Admin*:*"])
        assert not filt.is_visible("AdminService:anything")
        assert filt.is_visible("UserService:anything")


# ── Server setup tests ────────────────────────────────────────────────────────


class TestAsterMcpServer:
    @pytest.mark.asyncio
    async def test_setup_with_demo(self):
        from aster_cli.mcp.server import AsterMcpServer
        from aster_cli.shell.app import DemoConnection

        conn = DemoConnection()
        server = AsterMcpServer(conn)
        await server.setup()

        assert server.tool_count > 0
        names = server.tool_names
        # DemoConnection has HelloWorld, FileStore, Analytics services
        assert any("HelloWorld" in n for n in names)
        assert any("FileStore" in n for n in names)

    @pytest.mark.asyncio
    async def test_setup_with_filter(self):
        from aster_cli.mcp.server import AsterMcpServer
        from aster_cli.shell.app import DemoConnection

        conn = DemoConnection()
        filt = ToolFilter(allow=["HelloWorld:*"])
        server = AsterMcpServer(conn, tool_filter=filt)
        await server.setup()

        names = server.tool_names
        assert all("HelloWorld" in n for n in names)
        assert not any("FileStore" in n for n in names)

    @pytest.mark.asyncio
    async def test_setup_with_deny(self):
        from aster_cli.mcp.server import AsterMcpServer
        from aster_cli.shell.app import DemoConnection

        conn = DemoConnection()
        filt = ToolFilter(deny=["*:sync"])
        server = AsterMcpServer(conn, tool_filter=filt)
        await server.setup()

        names = server.tool_names
        assert "FileStore:sync" not in names

    @pytest.mark.asyncio
    async def test_bidi_stream_excluded(self):
        from aster_cli.mcp.server import AsterMcpServer
        from aster_cli.shell.app import DemoConnection

        conn = DemoConnection()
        server = AsterMcpServer(conn)
        await server.setup()

        names = server.tool_names
        # FileStore has a bidi_stream "sync" method — should be excluded
        assert "FileStore:sync" not in names
