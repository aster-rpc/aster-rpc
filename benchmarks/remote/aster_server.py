#!/usr/bin/env python3
"""Aster benchmark server -- runs on remote machine.

Usage:
    cd ~/bench && source .venv/bin/activate
    python aster_server.py
"""
import asyncio
from dataclasses import dataclass
from aster import service, rpc, wire_type, AsterServer, AsterConfig


@wire_type("mission/StatusRequest")
@dataclass
class StatusRequest:
    agent_id: str = ""


@wire_type("mission/StatusResponse")
@dataclass
class StatusResponse:
    agent_id: str = ""
    status: str = "idle"
    uptime_secs: int = 0


@wire_type("mission/Heartbeat")
@dataclass
class Heartbeat:
    agent_id: str = ""
    capabilities: list = None
    load_avg: float = 0.0

    def __post_init__(self):
        if self.capabilities is None:
            self.capabilities = []


@wire_type("mission/Assignment")
@dataclass
class Assignment:
    task_id: str = ""
    command: str = ""


@service(name="MissionControl", version=1)
class MissionControl:
    @rpc()
    async def getStatus(self, req: StatusRequest) -> StatusResponse:
        return StatusResponse(
            agent_id=req.agent_id,
            status="running",
            uptime_secs=3600,
        )


@service(name="AgentSession", version=1, scoped="session")
class AgentSession:
    def __init__(self, peer=None):
        self._peer = peer
        self._agent_id = ""

    @rpc()
    async def register(self, hb: Heartbeat) -> Assignment:
        self._agent_id = hb.agent_id
        return Assignment(task_id="idle", command="sleep 60")

    @rpc()
    async def heartbeat(self, hb: Heartbeat) -> Assignment:
        return Assignment(task_id="continue", command="")


async def main():
    config = AsterConfig.from_env()
    async with AsterServer(
        services=[MissionControl(), AgentSession()],
        config=config,
    ) as srv:
        print(srv.address, flush=True)
        await srv.serve()


if __name__ == "__main__":
    asyncio.run(main())
