"""Mission Control services with role-based access control (Chapter 5).

Same services as services.py but with requires= on each method.
Used by server.py --auth.
"""

import asyncio
from collections.abc import AsyncIterator

from aster import any_of, bidi_stream, client_stream, rpc, server_stream, service

from .roles import Role
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
    """Shared service with capability-based auth."""

    def __init__(self) -> None:
        self._log_queue: asyncio.Queue[LogEntry] = asyncio.Queue()
        self._metrics: list[MetricPoint] = []

    @rpc(requires=Role.STATUS)
    async def getStatus(self, req: StatusRequest) -> StatusResponse:
        return StatusResponse(
            agent_id=req.agent_id,
            status="running",
            uptime_secs=3600,
        )

    @rpc()
    async def submitLog(self, entry: LogEntry) -> SubmitLogResult:
        await self._log_queue.put(entry)
        return SubmitLogResult(accepted=True)

    @server_stream(requires=any_of(Role.LOGS, Role.ADMIN))
    async def tailLogs(self, req: TailRequest) -> AsyncIterator[LogEntry]:
        min_rank = _LOG_LEVEL_RANK.get(req.level.lower(), 0)
        while True:
            entry = await self._log_queue.get()
            if req.agent_id and entry.agent_id != req.agent_id:
                continue
            if _LOG_LEVEL_RANK.get(entry.level.lower(), 0) < min_rank:
                continue
            yield entry

    @client_stream(requires=Role.INGEST)
    async def ingestMetrics(
        self, stream: AsyncIterator[MetricPoint]
    ) -> IngestResult:
        accepted = 0
        async for point in stream:
            self._metrics.append(point)
            accepted += 1
        return IngestResult(accepted=accepted)


@service(name="AgentSession", version=1, scoped="session")
class AgentSession:
    """Session-scoped service with capability-based auth."""

    def __init__(self, peer: str | None = None) -> None:
        self._peer = peer
        self._agent_id = ""
        self._capabilities: list[str] = []

    @rpc(requires=Role.INGEST)
    async def register(self, hb: Heartbeat) -> Assignment:
        self._agent_id = hb.agent_id
        self._capabilities = list(hb.capabilities)
        if "gpu" in hb.capabilities:
            return Assignment(task_id="train-42", command="python train.py")
        return Assignment(task_id="idle", command="sleep 60")

    @rpc()
    async def heartbeat(self, hb: Heartbeat) -> Assignment:
        self._capabilities = list(hb.capabilities)
        return Assignment(task_id="continue", command="")

    @bidi_stream(requires=Role.ADMIN)
    async def runCommand(
        self, commands: AsyncIterator[Command]
    ) -> AsyncIterator[CommandResult]:
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
