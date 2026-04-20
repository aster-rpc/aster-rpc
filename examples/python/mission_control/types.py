"""Wire types for the Mission Control example.

All request/response types used across MissionControl and AgentSession
services. Each type has a stable wire identity via @wire_type so
TypeScript (or any other Aster binding) can interoperate.
"""

from dataclasses import dataclass, field

import pyfory

from aster import wire_type


# -- Chapter 1: Status --------------------------------------------------------

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


# -- Chapter 2: Logging -------------------------------------------------------

@wire_type("mission/LogEntry")
@dataclass
class LogEntry:
    timestamp: float = 0.0
    level: str = "info"
    message: str = ""
    agent_id: str = ""


@wire_type("mission/SubmitLogResult")
@dataclass
class SubmitLogResult:
    accepted: bool = True


@wire_type("mission/TailRequest")
@dataclass
class TailRequest:
    agent_id: str = ""
    level: str = "info"


# -- Chapter 3: Metrics -------------------------------------------------------

@wire_type("mission/MetricPoint")
@dataclass
class MetricPoint:
    name: str = ""
    value: float = 0.0
    timestamp: float = 0.0
    tags: dict[str, str] = field(default_factory=dict)


@wire_type("mission/IngestResult")
@dataclass
class IngestResult:
    # Java peers declare these as ``int`` (32-bit); pyfory's bare ``int``
    # defaults to ``int64`` and the fingerprint tuple diverges, so pin
    # the wire width explicitly.
    accepted: pyfory.int32 = 0
    dropped: pyfory.int32 = 0


# -- Chapter 4: Sessions & Commands -------------------------------------------

@wire_type("mission/Heartbeat")
@dataclass
class Heartbeat:
    agent_id: str = ""
    capabilities: list[str] = field(default_factory=list)
    load_avg: float = 0.0


@wire_type("mission/Assignment")
@dataclass
class Assignment:
    task_id: str = ""
    command: str = ""


@wire_type("mission/Command")
@dataclass
class Command:
    command: str = ""


@wire_type("mission/CommandResult")
@dataclass
class CommandResult:
    stdout: str = ""
    stderr: str = ""
    # Java peers declare ``int`` (32-bit); pin to match.
    exit_code: pyfory.int32 = -1
