"""Mission Control -- example Aster RPC application.

A control plane for managing remote agents, demonstrating all four
RPC patterns, session-scoped services, and capability-based auth.

See https://docs.aster.site/docs/quickstart/mission-control for the full walkthrough.
"""

from .roles import Role
from .services import AgentSession, MissionControl
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

__all__ = [
    "MissionControl",
    "AgentSession",
    "Role",
    "StatusRequest",
    "StatusResponse",
    "LogEntry",
    "SubmitLogResult",
    "TailRequest",
    "MetricPoint",
    "IngestResult",
    "Heartbeat",
    "Assignment",
    "Command",
    "CommandResult",
]
