"""Tests for the aster shell components (VFS, commands, display, plugin system)."""

from __future__ import annotations

import asyncio
import json

import pytest

from aster_cli.shell.app import (
    DemoConnection,
    _build_identity_banner_lines,
    _populate_directory,
    _populate_from_connection,
)
from aster_cli.handle_validation import validate_handle
from aster_cli.shell.commands import _parse_call_args
from aster_cli.shell.display import Display, _format_size
from aster_cli.shell.plugin import (
    CommandContext,
    get_all_commands,
    get_command,
    get_commands_for_path,
)
from aster_cli.shell.vfs import (
    NodeKind,
    VfsNode,
    build_root,
    resolve_path,
)
from rich.console import Console


# ── Fixtures ──────────────────────────────────────────────────────────────────


@pytest.fixture
def root():
    return build_root()


@pytest.fixture
def demo_conn():
    conn = DemoConnection()
    asyncio.get_event_loop().run_until_complete(conn.connect())
    return conn


@pytest.fixture
def ctx(root, demo_conn):
    console = Console(file=open("/dev/null", "w"))
    display = Display(console=console)
    return CommandContext(
        vfs_cwd="/",
        vfs_root=root,
        connection=demo_conn,
        display=display,
        peer_name="test",
    )


@pytest.fixture
def populated_ctx(root, demo_conn):
    svc_count, blob_count = asyncio.get_event_loop().run_until_complete(
        _populate_from_connection(root, demo_conn)
    )
    console = Console(file=open("/dev/null", "w"))
    display = Display(console=console)
    return CommandContext(
        vfs_cwd="/",
        vfs_root=root,
        connection=demo_conn,
        display=display,
        peer_name="test",
    )


# ── VFS tests ─────────────────────────────────────────────────────────────────


class TestVfs:
    def test_build_root(self, root):
        assert root.kind == NodeKind.ROOT
        assert root.path == "/"
        assert "blobs" in root.children
        assert "services" in root.children
        assert "gossip" in root.children

    def test_resolve_absolute(self, root):
        node, path = resolve_path(root, "/", "/blobs")
        assert node is not None
        assert node.kind == NodeKind.BLOBS
        assert path == "/blobs"

    def test_resolve_relative(self, root):
        node, path = resolve_path(root, "/", "services")
        assert node is not None
        assert node.kind == NodeKind.SERVICES

    def test_resolve_dotdot(self, root):
        node, path = resolve_path(root, "/blobs", "..")
        assert node is not None
        assert node.kind == NodeKind.ROOT
        assert path == "/"

    def test_resolve_nonexistent(self, root):
        node, path = resolve_path(root, "/", "nonexistent")
        assert node is None

    def test_child_case_insensitive(self, root):
        assert root.child("Blobs") is not None
        assert root.child("BLOBS") is not None

    def test_sorted_children(self, root):
        names = [c.name for c in root.sorted_children()]
        assert names == sorted(names)


# ── Plugin system tests ───────────────────────────────────────────────────────


class TestPlugins:
    def test_commands_registered(self):
        commands = get_all_commands()
        assert "ls" in commands
        assert "cd" in commands
        assert "help" in commands
        assert "exit" in commands
        assert "describe" in commands
        assert "invoke" in commands
        assert "cat" in commands
        assert "save" in commands
        assert "discover" in commands
        assert "access" in commands
        assert "grant" in commands
        assert "revoke" in commands
        assert "visibility" in commands
        assert "update-service" in commands
        assert "delegation" in commands
        assert "public" in commands
        assert "private" in commands

    def test_get_command(self):
        assert get_command("ls") is not None
        assert get_command("nonexistent") is None

    def test_commands_for_root(self):
        cmds = get_commands_for_path("/")
        names = {c.name for c in cmds}
        assert "ls" in names
        assert "cd" in names
        assert "help" in names

    def test_commands_for_services(self):
        cmds = get_commands_for_path("/services/HelloWorld")
        names = {c.name for c in cmds}
        assert "describe" in names
        assert "invoke" in names
        assert "join" in names
        assert "whoami" in names
        assert "publish" in names
        assert "access" in names
        assert "grant" in names
        assert "revoke" in names
        assert "public" in names
        assert "private" in names

    def test_describe_not_valid_at_root(self):
        cmd = get_command("describe")
        assert not cmd.is_valid_at("/")
        assert cmd.is_valid_at("/services/Foo")


# ── Command tests ─────────────────────────────────────────────────────────────


class TestCommands:
    @pytest.mark.asyncio
    async def test_ls_root(self, ctx):
        cmd = get_command("ls")
        await cmd.execute([], ctx)

    @pytest.mark.asyncio
    async def test_cd_and_back(self, ctx):
        cd = get_command("cd")
        await cd.execute(["services"], ctx)
        assert ctx.vfs_cwd == "/services"
        await cd.execute([".."], ctx)
        assert ctx.vfs_cwd == "/"

    @pytest.mark.asyncio
    async def test_cd_nonexistent(self, populated_ctx):
        cd = get_command("cd")
        old_cwd = populated_ctx.vfs_cwd
        await cd.execute(["nonexistent"], populated_ctx)
        assert populated_ctx.vfs_cwd == old_cwd  # didn't change

    @pytest.mark.asyncio
    async def test_ls_services(self, populated_ctx):
        cd = get_command("cd")
        ls = get_command("ls")
        await cd.execute(["services"], populated_ctx)
        await ls.execute([], populated_ctx)

    @pytest.mark.asyncio
    async def test_ls_methods(self, populated_ctx):
        cd = get_command("cd")
        ls = get_command("ls")
        await cd.execute(["services"], populated_ctx)
        await cd.execute(["HelloWorld"], populated_ctx)
        await ls.execute([], populated_ctx)

    @pytest.mark.asyncio
    async def test_describe(self, populated_ctx):
        cd = get_command("cd")
        await cd.execute(["/services/HelloWorld"], populated_ctx)
        desc = get_command("describe")
        await desc.execute([], populated_ctx)

    @pytest.mark.asyncio
    async def test_invoke_unary(self, populated_ctx):
        cd = get_command("cd")
        await cd.execute(["/services/HelloWorld"], populated_ctx)
        invoke = get_command("invoke")
        await invoke.execute(["sayHello", "name=World"], populated_ctx)

    @pytest.mark.asyncio
    async def test_pwd(self, ctx):
        cmd = get_command("pwd")
        await cmd.execute([], ctx)

    @pytest.mark.asyncio
    async def test_help(self, ctx):
        cmd = get_command("help")
        await cmd.execute([], ctx)

    @pytest.mark.asyncio
    async def test_ls_blobs(self, populated_ctx):
        cd = get_command("cd")
        ls = get_command("ls")
        await cd.execute(["/blobs"], populated_ctx)
        await ls.execute([], populated_ctx)


class TestDirectoryMode:
    @pytest.mark.asyncio
    async def test_populate_directory_continues_after_handle_error(self):
        from aster_cli.shell.vfs import build_directory_root

        class FakeDirectoryConnection:
            async def list_handles(self):
                return [
                    {"handle": "@current", "registered": False},
                    {"handle": "@remote", "registered": True},
                ]

            async def get_handle_info(self, handle):
                if handle == "@current":
                    raise RuntimeError("not registered")
                return {
                    "readme": "",
                    "services": [
                        {
                            "name": "ShellTestService",
                            "published": True,
                            "version": 1,
                            "description": "remote service",
                            "endpoints": 0,
                            "methods": [{"name": "ping"}],
                        }
                    ],
                }

        root = build_directory_root()
        count = await _populate_directory(root, FakeDirectoryConnection())
        aster_node = root.child("aster")

        assert count == 2
        assert aster_node is not None
        assert "@current" in aster_node.children
        assert "@remote" in aster_node.children
        remote_node = aster_node.child("@remote")
        assert remote_node is not None
        assert "ShellTestService" in remote_node.children


# ── Argument parsing tests ────────────────────────────────────────────────────


class TestArgParsing:
    def test_key_value(self):
        result = _parse_call_args(["name=World", "count=5"])
        assert result["name"] == "World"
        assert result["count"] == 5

    def test_json_string(self):
        result = _parse_call_args(['{"name": "World"}'])
        assert result["name"] == "World"

    def test_empty(self):
        assert _parse_call_args([]) == {}

    def test_quoted_values(self):
        result = _parse_call_args(['name="Hello World"'])
        assert result["name"] == "Hello World"

    def test_positional(self):
        result = _parse_call_args(["World"])
        assert "_positional" in result
        assert result["_positional"] == "World"


class TestHandleValidation:
    def test_valid_handle(self):
        assert validate_handle("alice-dev") == (True, "available")

    def test_reserved_handle(self):
        ok, reason = validate_handle("admin")
        assert not ok
        assert "reserved" in reason

    def test_numeric_handle_rejected(self):
        ok, reason = validate_handle("12345")
        assert not ok
        assert "numeric" in reason


# ── Display tests ─────────────────────────────────────────────────────────────


class TestDisplay:
    def test_format_size(self):
        assert _format_size(500) == "500 B"
        assert "KB" in _format_size(2048)
        assert "MB" in _format_size(2 * 1024 * 1024)
        assert "GB" in _format_size(2 * 1024 * 1024 * 1024)

    def test_raw_mode(self):
        console = Console(file=open("/dev/null", "w"))
        display = Display(console=console, raw=True)
        # Should not crash
        display.info("test")
        display.json_value({"key": "value"})


class TestIdentityBanner:
    def test_verified_banner_with_remote(self):
        lines = _build_identity_banner_lines(
            {
                "root_pubkey": "ab" * 32,
                "display_handle": "@alice-test",
                "handle_status": "verified",
                "remote": {"status": "verified"},
            },
            remote_error=None,
            air_gapped=False,
        )
        assert any("verified" in line for line in lines)
        assert any("Remote connected" in line for line in lines)

    def test_air_gapped_banner(self):
        lines = _build_identity_banner_lines(
            {
                "root_pubkey": "ab" * 32,
                "display_handle": "@alice-test",
                "handle_status": "pending",
            },
            remote_error=None,
            air_gapped=True,
        )
        assert any("Air-gapped" in line for line in lines)
        assert any("Remote: disabled" in line for line in lines)


# ── Demo connection tests ────────────────────────────────────────────────────


class TestDemoConnection:
    @pytest.mark.asyncio
    async def test_list_services(self, demo_conn):
        services = await demo_conn.list_services()
        assert len(services) == 3
        names = {s["name"] for s in services}
        assert "HelloWorld" in names

    @pytest.mark.asyncio
    async def test_invoke(self, demo_conn):
        result = await demo_conn.invoke("HelloWorld", "sayHello", {"name": "Test"})
        assert "Hello, Test!" in result["message"]

    @pytest.mark.asyncio
    async def test_list_blobs(self, demo_conn):
        blobs = await demo_conn.list_blobs()
        assert len(blobs) == 1
        assert blobs[0]["is_collection"] is True

    @pytest.mark.asyncio
    async def test_read_blob(self, demo_conn):
        content = await demo_conn.read_blob("deadbeef0123")
        assert b"Hello from the mesh" in content

    @pytest.mark.asyncio
    async def test_get_contract(self, demo_conn):
        contract = await demo_conn.get_contract("HelloWorld")
        assert contract is not None
        assert contract["name"] == "HelloWorld"
        assert len(contract["methods"]) == 2
