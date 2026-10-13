"""
End-to-end Gate 0 (connection-level admission) through ``AsterServer``.

Enrollment owns Gate 0 when ``[enrollment]`` is configured (see
``test_enrollment_aster_server.py``). Without it a server is open, or an
application supplies its own Rust ``GatePolicy`` (``gate=``), the escape
hatch these tests cover:

  1. A peer the gate has not admitted cannot reach ``aster/1``; the client
     sees ``AdmissionDeniedError``.
  2. After ``gate.admit(peer, attributes)`` the peer connects, and the
     attributes reach Gate 3 (``requires=``).
  3. Without enrollment or a gate the server is open.
"""

from __future__ import annotations

from dataclasses import dataclass

import pytest

from aster import (
    AsterClient,
    AsterConfig,
    AsterServer,
    GatePolicy,
    SecretKey,
    rpc,
    service,
    wire_type,
)
from aster.runtime import AdmissionDeniedError
from aster.status import PermissionDeniedError


@wire_type("test.gate0/PingRequest")
@dataclass
class PingRequest:
    payload: str = ""


@wire_type("test.gate0/PingResponse")
@dataclass
class PingResponse:
    payload: str = ""


@service("Gate0PingService", version=1)
class Gate0PingService:
    @rpc
    async def ping(self, req: PingRequest) -> PingResponse:
        return PingResponse(payload=f"pong:{req.payload}")

    @rpc(requires="ops.admin")
    async def admin(self, req: PingRequest) -> PingResponse:
        return PingResponse(payload=f"admin:{req.payload}")


def _config(key: SecretKey | None = None) -> AsterConfig:
    return AsterConfig(
        relay_mode="disabled",
        bind_addr="127.0.0.1:0",
        secret_key=key.to_bytes() if key is not None else None,
    )


@pytest.mark.timeout(30)
async def test_unadmitted_peer_is_refused():
    gate = GatePolicy()
    async with AsterServer(services=[Gate0PingService()], config=_config(), gate=gate) as srv:
        assert srv.gate is gate
        client = AsterClient(config=_config(), address=srv.address)
        with pytest.raises(AdmissionDeniedError) as denied:
            await client.connect()
        assert not denied.value.had_credential
        assert "aster enroll node" in str(denied.value)
        await client.close()


@pytest.mark.timeout(30)
async def test_admitted_peer_reaches_rpc_with_gate_attributes():
    gate = GatePolicy()
    key = SecretKey.generate()
    async with AsterServer(services=[Gate0PingService()], config=_config(), gate=gate) as srv:
        gate.admit(key.public_key().to_hex(), attributes={"aster.role": "ops.admin"})
        async with AsterClient(config=_config(key), address=srv.address) as client:
            svc = await client.client(Gate0PingService)
            assert (await svc.ping(PingRequest(payload="x"))).payload == "pong:x"
            assert (await svc.admin(PingRequest(payload="y"))).payload == "admin:y"


@pytest.mark.timeout(30)
async def test_open_server_admits_anyone_but_requires_still_applies():
    async with AsterServer(services=[Gate0PingService()], config=_config()) as srv:
        assert srv.gate is None and srv.enrollment is None
        async with AsterClient(config=_config(), address=srv.address) as client:
            assert client.open_gate
            svc = await client.client(Gate0PingService)
            assert (await svc.ping(PingRequest(payload="z"))).payload == "pong:z"
            with pytest.raises(PermissionDeniedError):
                await svc.admin(PingRequest(payload="z"))
