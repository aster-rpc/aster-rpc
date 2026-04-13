"""
Jepsen-inspired chaos tests for Aster RPC.

Runs every workload against every nemesis (4x4 = 16 combinations).
After each run, property checkers scan the recorded history for
invariant violations.

Usage:
    uv run pytest tests/chaos/ -v --timeout=30
"""

import pytest

pytest.skip(
    "aster.session retired -- Phase-8 CALL-frame mechanism removed; "
    "replaced by ClientSession + reactor-based session lifecycle",
    allow_module_level=True,
)

import asyncio

from .harness import (
    History,
    check_request_response_pairing,
    check_no_silent_corruption,
    check_cancel_trailer_received,
    check_bounded_resources,
)
from .workloads import (
    unary_workload,
    session_sequential_workload,
    client_stream_workload,
    cancel_workload,
)
from .nemeses import NoFault, KillRecvStream, CorruptFrame, SlowHandler


WORKLOADS = [
    unary_workload,
    session_sequential_workload,
    client_stream_workload,
    cancel_workload,
]

NEMESES = [
    NoFault(),
    KillRecvStream(kill_after_reads=8),
    CorruptFrame(corrupt_after_reads=10),
    SlowHandler(delay_s=5.0),
]


@pytest.mark.chaos
@pytest.mark.parametrize("workload", WORKLOADS, ids=lambda w: w.__name__)
@pytest.mark.parametrize("nemesis", NEMESES, ids=lambda n: n.name)
async def test_chaos(workload, nemesis):
    history = History()

    try:
        await asyncio.wait_for(
            workload(nemesis, history),
            timeout=20.0,
        )
    except asyncio.TimeoutError:
        history.timeout_pending()

    # -- Property checks (run all, collect failures) --------------------------
    failures: list[str] = []

    try:
        check_request_response_pairing(history)
    except AssertionError as e:
        failures.append(f"PAIRING: {e}")

    try:
        check_no_silent_corruption(history)
    except AssertionError as e:
        failures.append(f"CORRUPTION: {e}")

    try:
        check_bounded_resources(history)
    except AssertionError as e:
        failures.append(f"RESOURCES: {e}")

    # Cancel-specific check: only meaningful for cancel workload with no_fault
    if workload is cancel_workload and isinstance(nemesis, NoFault):
        try:
            check_cancel_trailer_received(history)
        except AssertionError as e:
            failures.append(f"CANCEL: {e}")

    if failures:
        # Print the full history for debugging
        print(f"\n--- History ({len(history.ops)} ops) ---")
        for op in history.ops:
            if op.op_type == "invoke":
                print(f"  INVOKE {op.op_id} {op.service}.{op.method} req={op.request}")
            else:
                print(f"  {op.op_type.upper():7} {op.op_id} resp={op.response} err={op.error}")
        print("--- End History ---\n")

        pytest.fail(
            f"{len(failures)} invariant violation(s) in "
            f"{workload.__name__} x {nemesis.name}:\n"
            + "\n".join(f"  - {f}" for f in failures)
        )
