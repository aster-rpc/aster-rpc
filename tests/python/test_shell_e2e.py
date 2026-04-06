"""End-to-end tests: shell PeerConnection against a real AsterServer."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass

import pytest

from aster import (
    AsterClient,
    AsterServer,
    rpc,
    service,
    wire_type,
)


# ── Test service ──────────────────────────────────────────────────────────────


@wire_type("test.shell/PingRequest")
@dataclass
class PingRequest:
    message: str = "hello"


@wire_type("test.shell/PingResponse")
@dataclass
class PingResponse:
    reply: str = ""


@service(name="ShellTestService", version=1)
class ShellTestService:
    @rpc()
    async def ping(self, req: PingRequest) -> PingResponse:
        return PingResponse(reply=f"pong: {req.message}")


# ── Tests ─────────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_shell_connects_to_real_peer():
    """Shell PeerConnection can connect to a real AsterServer and list services."""
    async with AsterServer(
        services=[ShellTestService()],
        allow_all_consumers=True,
    ) as server:
        addr_b64 = server.endpoint_addr_b64

        # Use AsterClient directly (same as PeerConnection.connect does)
        async with AsterClient(endpoint_addr=addr_b64) as client:
            services = client.services
            assert len(services) >= 1
            names = [s.name for s in services]
            assert "ShellTestService" in names


@pytest.mark.asyncio
async def test_shell_peer_connection_list_services():
    """PeerConnection.list_services() returns service summaries from admission."""
    from aster_cli.shell.app import PeerConnection

    async with AsterServer(
        services=[ShellTestService()],
        allow_all_consumers=True,
    ) as server:
        addr_b64 = server.endpoint_addr_b64

        conn = PeerConnection(peer_addr=addr_b64)
        try:
            await conn.connect()
            services = await conn.list_services()
            assert len(services) >= 1
            names = [s["name"] for s in services]
            assert "ShellTestService" in names

            # Check structure
            svc = next(s for s in services if s["name"] == "ShellTestService")
            assert svc["version"] == 1
            assert "contract_id" in svc
            assert len(svc["contract_id"]) > 0
        finally:
            await conn.close()


@pytest.mark.asyncio
async def test_shell_peer_connection_get_contract():
    """PeerConnection.get_contract() returns contract metadata."""
    from aster_cli.shell.app import PeerConnection

    async with AsterServer(
        services=[ShellTestService()],
        allow_all_consumers=True,
    ) as server:
        addr_b64 = server.endpoint_addr_b64

        conn = PeerConnection(peer_addr=addr_b64)
        try:
            await conn.connect()
            contract = await conn.get_contract("ShellTestService")
            assert contract is not None
            assert contract["name"] == "ShellTestService"
            assert contract["version"] == 1

            # Non-existent service
            assert await conn.get_contract("NonExistent") is None
        finally:
            await conn.close()


@pytest.mark.asyncio
async def test_shell_vfs_populated_from_real_peer():
    """VFS gets populated with real service data from a live peer."""
    from aster_cli.shell.app import PeerConnection, _populate_from_connection
    from aster_cli.shell.vfs import build_root

    async with AsterServer(
        services=[ShellTestService()],
        allow_all_consumers=True,
    ) as server:
        addr_b64 = server.endpoint_addr_b64

        conn = PeerConnection(peer_addr=addr_b64)
        try:
            await conn.connect()

            root = build_root()
            svc_count, blob_count = await _populate_from_connection(root, conn)

            assert svc_count >= 1

            # Check VFS structure
            services_node = root.child("services")
            assert services_node is not None
            assert services_node.child("ShellTestService") is not None
        finally:
            await conn.close()
