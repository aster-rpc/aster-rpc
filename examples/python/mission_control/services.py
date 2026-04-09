"""Mission Control services.

Two services with different lifetimes:

  MissionControl (shared)   -- fleet-wide: status, logs, metrics
  AgentSession   (session)  -- per-agent: register, heartbeat, commands

Services are defined without requires= so they work out of the box in
dev mode (open gate, no credentials). The --auth variant in server.py
shows how to layer on role-based access control (Chapter 5).
"""

import asyncio
from collections.abc import AsyncIterator

from aster import bidi_stream, client_stream, rpc, server_stream, service

from .types import (
    Assignment,
    Command,
    CommandResult,
    Heartbeat,
    IngestResult,
    LogEntry,
    MetricPoint,
    StatusRequest,
    StatusResponse,
    SubmitLogResult,
    TailRequest,
)

_LOG_LEVEL_RANK = {"debug": 0, "info": 1, "warn": 2, "error": 3}


@service(name="MissionControl", version=1)
class MissionControl:
    """Shared service -- one instance, all clients see the same state."""

    def __init__(self) -> None:
        self._log_queue: asyncio.Queue[LogEntry] = asyncio.Queue()
        self._metrics: list[MetricPoint] = []

    # -- Chapter 1: status -----------------------------------------------------

    @rpc()
    async def getStatus(self, req: StatusRequest) -> StatusResponse:
        return StatusResponse(
            agent_id=req.agent_id,
            status="running",
            uptime_secs=3600,
        )

    # -- Chapter 2: logging ----------------------------------------------------

    @rpc()
    async def submitLog(self, entry: LogEntry) -> SubmitLogResult:
        """Agents call this to push log entries."""
        await self._log_queue.put(entry)
        return SubmitLogResult(accepted=True)

    @server_stream()
    async def tailLogs(self, req: TailRequest) -> AsyncIterator[LogEntry]:
        """Stream log entries as they arrive, filtered by agent and level."""
        min_rank = _LOG_LEVEL_RANK.get(req.level.lower(), 0)
        while True:
            entry = await self._log_queue.get()
            if req.agent_id and entry.agent_id != req.agent_id:
                continue
            if _LOG_LEVEL_RANK.get(entry.level.lower(), 0) < min_rank:
                continue
            yield entry

    # -- Chapter 3: metrics ----------------------------------------------------

    @client_stream()
    async def ingestMetrics(
        self, stream: AsyncIterator[MetricPoint]
    ) -> IngestResult:
        """Receive a stream of metric points from an agent."""
        accepted = 0
        async for point in stream:
            self._metrics.append(point)
            accepted += 1
        return IngestResult(accepted=accepted)


@service(name="AgentSession", version=1, scoped="session")
class AgentSession:
    """Session-scoped -- one instance per connected agent."""

    def __init__(self, peer: str | None = None) -> None:
        self._peer = peer
        self._agent_id = ""
        self._capabilities: list[str] = []

    @rpc()
    async def register(self, hb: Heartbeat) -> Assignment:
        """Agent announces itself and gets an assignment."""
        self._agent_id = hb.agent_id
        self._capabilities = list(hb.capabilities)
        if "gpu" in hb.capabilities:
            return Assignment(task_id="train-42", command="python train.py")
        return Assignment(task_id="idle", command="sleep 60")

    @rpc()
    async def heartbeat(self, hb: Heartbeat) -> Assignment:
        """Periodic check-in -- update load, maybe get new work."""
        self._capabilities = list(hb.capabilities)
        return Assignment(task_id="continue", command="")

    @bidi_stream()
    async def runCommand(
        self, commands: AsyncIterator[Command]
    ) -> AsyncIterator[CommandResult]:
        """Execute commands on this agent. Admin-only."""
        async for cmd in commands:
            proc = await asyncio.create_subprocess_shell(
                cmd.command,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await proc.communicate()
            yield CommandResult(
                stdout=stdout.decode(),
                stderr=stderr.decode(),
                exit_code=proc.returncode or 0,
            )
