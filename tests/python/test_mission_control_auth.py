"""The Mission Control auth chapter end to end (Python port of
``examples/rust/mission-control/tests/auth.rs``).

The server is configured from an ``aster.toml`` exactly as the README tells an
operator to write it; the credentials are the files ``aster enroll node
--attributes`` produces (written here by the native module's test-only
``_enrollment_test_credential``); the clients connect with ``AsterClient``.
Each role reaches exactly the methods whose ``requires=`` it satisfies, with
the role names of ``examples/python/mission_control/roles.py``.
"""

from __future__ import annotations

import asyncio
import os
import secrets
import sys
from pathlib import Path

import pytest

from aster import AsterClient, AsterConfig, AsterServer, EnrollmentConfig, SecretKey
from aster._aster import _enrollment_test_credential
from aster.runtime import AdmissionDeniedError
from aster.status import PermissionDeniedError

# The examples are not a package on the test path (CI runs .venv/bin/pytest,
# which does not add the working directory), so import them from the repo.
_REPO = str(Path(__file__).resolve().parents[2])
if _REPO not in sys.path:
    sys.path.insert(0, _REPO)

from examples.python.mission_control.roles import Role  # noqa: E402
from examples.python.mission_control.services_auth import AgentSession, MissionControl  # noqa: E402
from examples.python.mission_control.types import (  # noqa: E402
    Command,
    LogEntry,
    MetricPoint,
    StatusRequest,
    TailRequest,
)

AUDIENCE = "mission-control"

pytestmark = pytest.mark.timeout(120)


def enroll(root_seed: bytes, node: SecretKey, roles: list[Role], out: Path) -> None:
    """What ``aster enroll node --subject <key> --audience mission-control
    --attributes roles.json --out <file>`` writes, with roles.json
    ``{"aster.role": {"type": "string", "value": "<roles>"}}``."""
    chain = _enrollment_test_credential(
        root_seed,
        node.public_key().to_hex(),
        AUDIENCE,
        roles=",".join(r.value for r in roles) if roles else None,
    )
    out.write_text(chain + "\n")


def toml_path(path: Path) -> str:
    return str(path).replace("\\", "\\\\")


async def metrics():
    yield MetricPoint(name="cpu", value=1.0, timestamp=0.0)


async def denied(awaitable) -> bool:
    try:
        await awaitable
    except PermissionDeniedError:
        return True
    return False


async def stream_denied(stream) -> bool:
    try:
        async for _ in stream:
            return False
    except PermissionDeniedError:
        return True
    return False


async def run_command(session) -> str:
    channel = session.runCommand()
    try:
        await channel.send(Command(command="echo hello"))
        await channel.close()
        return (await asyncio.wait_for(channel.recv(), 10)).stdout
    finally:
        try:
            await channel.close()
        except Exception:  # noqa: BLE001
            pass  # swallow-ok: best-effort teardown; the resource may already be closed


async def test_roles_in_credentials_gate_each_method(tmp_path: Path):
    # `aster key root generate`, `aster enroll namespace create`,
    # `aster key node generate --out server.key`.
    root_seed = secrets.token_bytes(32)
    root_pub = SecretKey.from_bytes(root_seed).public_key().to_hex()
    fleet = SecretKey.generate().public_key().to_hex()
    server_key = SecretKey.generate()
    server_key_file = tmp_path / "server.key"
    server_key.write_file(str(server_key_file))
    server_cred = tmp_path / "server.cred"
    enroll(root_seed, server_key, [], server_cred)

    config_file = tmp_path / "aster.toml"
    config_file.write_text(
        f"""
[trust]
root_pubkey = "{root_pub}"
enrollment_credential = "{toml_path(server_cred)}"

[network]
secret_key_file = "{toml_path(server_key_file)}"
relay_mode = "disabled"
bind_addr = "127.0.0.1:0"

[enrollment]
audience = "{AUDIENCE}"
fleet_namespace = "{fleet}"
state_path = "{toml_path(tmp_path / 'server-enrollment.json')}"
"""
    )

    async def connect(name: str, roles: list[Role]) -> AsterClient:
        key = SecretKey.generate()
        key_file = tmp_path / f"{name}.key"
        key.write_file(str(key_file))
        cred = tmp_path / f"{name}.cred"
        enroll(root_seed, key, roles, cred)
        config = AsterConfig(
            relay_mode="disabled",
            bind_addr="127.0.0.1:0",
            secret_key_file=str(key_file),
            root_pubkey=root_pub,
            enrollment_credential_file=str(cred),
            enrollment=EnrollmentConfig(audience=AUDIENCE, fleet_namespace=fleet),
        )
        client = AsterClient(config=config, address=address)
        await client.connect()
        clients.append(client)
        return client

    server = AsterServer(
        services=[MissionControl(), AgentSession],
        config=AsterConfig.from_file(config_file),
    )
    clients: list[AsterClient] = []
    async with server:
        assert server.enrollment is not None
        for _ in range(100):
            if server.node.node_addr_info().direct_addresses:
                break
            await asyncio.sleep(0.02)
        address = server.address
        try:
            # An operator watches the fleet.
            operator = await connect("operator", [Role.STATUS, Role.LOGS])
            mc = await operator.client(MissionControl)
            assert (await mc.getStatus(StatusRequest(agent_id="a1"))).status == "running"
            assert (await mc.submitLog(LogEntry(agent_id="a1", message="up"))).accepted
            logs = mc.tailLogs(TailRequest(agent_id="a1", level="info"))
            first = await asyncio.wait_for(logs.__anext__(), 10)
            assert first.message == "up"
            assert await denied(mc.ingestMetrics(metrics()))
            ops_session = await operator.client(AgentSession)
            assert await denied(run_command(ops_session))

            # An agent reports in.
            agent = await connect("agent", [Role.STATUS, Role.INGEST])
            mc_agent = await agent.client(MissionControl)
            assert (await mc_agent.ingestMetrics(metrics())).accepted == 1
            assert await stream_denied(mc_agent.tailLogs(TailRequest(agent_id="a1")))

            # An administrator runs commands but has no status role.
            admin = await connect("admin", [Role.ADMIN])
            admin_session = await admin.client(AgentSession)
            assert (await run_command(admin_session)).strip() == "hello"
            mc_admin = await admin.client(MissionControl)
            assert await denied(mc_admin.getStatus(StatusRequest(agent_id="a1")))

            # Enrolled with no roles: only the open method.
            nobody = await connect("nobody", [])
            mc_nobody = await nobody.client(MissionControl)
            assert (await mc_nobody.submitLog(LogEntry(agent_id="a1"))).accepted
            assert await denied(mc_nobody.getStatus(StatusRequest(agent_id="a1")))

            # No credential: cannot connect at all.
            stranger = AsterClient(
                config=AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0"),
                address=address,
            )
            with pytest.raises(AdmissionDeniedError):
                await stranger.connect()
            await stranger.close()
        finally:
            for client in clients:
                await client.close()


def test_auth_mode_refuses_to_start_without_enrollment(monkeypatch):
    """``--auth`` fails closed when enrollment is not configured."""
    for var in list(os.environ):
        if var.startswith("ASTER_"):
            monkeypatch.delenv(var)
    from examples.python.mission_control import server

    with pytest.raises(SystemExit) as exit_info:
        asyncio.run(server.run(auth=True))
    assert "--auth needs enrollment configured" in str(exit_info.value)
