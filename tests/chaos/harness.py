"""
Jepsen-inspired history recorder and property checkers.

Each RPC operation is recorded as an invoke/completion pair.  After the
workload finishes (or times out), property checkers scan the history for
invariant violations.
"""

from __future__ import annotations

import time
import uuid
from dataclasses import dataclass, field
from typing import Any


@dataclass
class Operation:
    op_id: str
    op_type: str  # "invoke" | "ok" | "fail" | "timeout" | "cancel"
    timestamp: float
    workload: str
    nemesis: str | None
    service: str
    method: str
    request: Any = None
    response: Any = None
    expected: Any = None
    error: str | None = None


class History:
    def __init__(self) -> None:
        self._ops: list[Operation] = []

    def invoke(
        self,
        *,
        workload: str,
        nemesis: str | None,
        service: str,
        method: str,
        request: Any = None,
    ) -> str:
        op_id = str(uuid.uuid4())[:8]
        self._ops.append(Operation(
            op_id=op_id,
            op_type="invoke",
            timestamp=time.monotonic(),
            workload=workload,
            nemesis=nemesis,
            service=service,
            method=method,
            request=request,
        ))
        return op_id

    def complete(
        self,
        op_id: str,
        op_type: str,
        *,
        response: Any = None,
        expected: Any = None,
        error: str | None = None,
    ) -> None:
        self._ops.append(Operation(
            op_id=op_id,
            op_type=op_type,
            timestamp=time.monotonic(),
            workload="",
            nemesis=None,
            service="",
            method="",
            response=response,
            expected=expected,
            error=error,
        ))

    def timeout_pending(self) -> None:
        completed = {op.op_id for op in self._ops if op.op_type != "invoke"}
        for op in self._ops:
            if op.op_type == "invoke" and op.op_id not in completed:
                self.complete(op.op_id, "timeout", error="global timeout")

    @property
    def ops(self) -> list[Operation]:
        return list(self._ops)

    @property
    def invocations(self) -> list[Operation]:
        return [op for op in self._ops if op.op_type == "invoke"]

    @property
    def completions(self) -> list[Operation]:
        return [op for op in self._ops if op.op_type != "invoke"]


# -- Property checkers --------------------------------------------------------


def check_request_response_pairing(history: History) -> None:
    """Every invoke must have exactly one completion. No orphans."""
    invoke_ids = {op.op_id for op in history.invocations}
    completion_ids: dict[str, list[str]] = {}
    for op in history.completions:
        completion_ids.setdefault(op.op_id, []).append(op.op_type)

    orphan_invokes = invoke_ids - set(completion_ids.keys())
    assert not orphan_invokes, (
        f"Invocations with no completion (lost operations): {orphan_invokes}"
    )

    orphan_completions = set(completion_ids.keys()) - invoke_ids
    assert not orphan_completions, (
        f"Completions with no prior invoke: {orphan_completions}"
    )

    for op_id, types in completion_ids.items():
        assert len(types) == 1, (
            f"op {op_id} has {len(types)} completions: {types}"
        )


def check_no_silent_corruption(history: History) -> None:
    """Every OK response must match the expected value."""
    for op in history.completions:
        if op.op_type == "ok" and op.expected is not None:
            assert op.response == op.expected, (
                f"op {op.op_id}: expected {op.expected}, got {op.response}"
            )


def check_cancel_trailer_received(history: History) -> None:
    """Cancel operations must receive a CANCELLED trailer, not timeout."""
    for inv in history.invocations:
        if inv.method == "__cancel__":
            completions = [
                c for c in history.completions if c.op_id == inv.op_id
            ]
            assert len(completions) == 1, (
                f"cancel op {inv.op_id} has {len(completions)} completions"
            )
            c = completions[0]
            assert c.op_type == "cancel", (
                f"cancel op {inv.op_id} got {c.op_type} instead of cancel "
                f"(error: {c.error})"
            )


def check_bounded_resources(history: History, max_ops: int = 10000) -> None:
    """Sanity check: history size is bounded."""
    assert len(history.ops) <= max_ops * 2, (
        f"history has {len(history.ops)} entries, expected <= {max_ops * 2}"
    )


def check_deadline_respected(
    history: History, epsilon_s: float = 1.0
) -> None:
    """If a deadline was set, completion arrives within deadline + epsilon."""
    by_id: dict[str, list[Operation]] = {}
    for op in history.ops:
        by_id.setdefault(op.op_id, []).append(op)

    for op_id, ops in by_id.items():
        invokes = [o for o in ops if o.op_type == "invoke"]
        completions = [o for o in ops if o.op_type != "invoke"]
        if not invokes or not completions:
            continue
        inv = invokes[0]
        comp = completions[0]
        req = inv.request
        if isinstance(req, dict) and "deadline_s" in req:
            deadline = req["deadline_s"]
            elapsed = comp.timestamp - inv.timestamp
            assert elapsed <= deadline + epsilon_s, (
                f"op {op_id}: deadline {deadline}s but took {elapsed:.2f}s"
            )
