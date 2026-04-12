"""
Soak tests -- sustained load over time.

These tests run longer than unit tests (30-60s) to expose:
- Memory leaks (queues, tasks, instances not cleaned up)
- State drift (counters, codec state, lock contention growing)
- Performance degradation under sustained load
- Rare race conditions that only manifest statistically

Mark: these are slow by design. Run with `pytest -m soak` or they
run as part of the normal suite with a 60s timeout.
"""

from __future__ import annotations

import asyncio
import time
import uuid
from typing import AsyncIterator

import pytest

from aster.codec import ForyCodec
from aster.decorators import _SERVICE_INFO_ATTR
from aster.rpc_types import SerializationMode
from aster.session import (
    SessionServer,
    _ByteQueue, _FakeRecvStream, _FakeSendStream,
    _generate_session_stub_class,
)

from .workloads import (
    ChaosSession, EchoReq, EchoResp, CounterReq, CounterResp,
    SumItem, SumResp, ALL_TYPES,
)
from .harness import History, check_request_response_pairing, check_no_silent_corruption


def _codec():
    return ForyCodec(mode=SerializationMode.XLANG, types=list(ALL_TYPES))


def _create_session():
    info = getattr(ChaosSession, _SERVICE_INFO_ATTR)
    codec = _codec()
    from aster.protocol import StreamHeader

    c2s = _ByteQueue()
    s2c = _ByteQueue()
    c_send = _FakeSendStream(c2s)
    s_recv = _FakeRecvStream(c2s)
    s_send = _FakeSendStream(s2c)
    c_recv = _FakeRecvStream(s2c)

    session_id = str(uuid.uuid4())
    header = StreamHeader(
        service=info.name, method="", version=info.version,
        callId=1,
        serializationMode=SerializationMode.XLANG.value,
    )

    server = SessionServer(
        service_class=ChaosSession, service_info=info, codec=codec,
    )
    server_task = asyncio.get_event_loop().create_task(
        server.run(header, s_send, s_recv, peer="soak")
    )

    stub_cls = _generate_session_stub_class(info)
    stub = stub_cls(
        send=c_send, recv=c_recv, codec=codec,
        service_info=info, interceptors=None,
        session_id=session_id,
    )

    return stub, server_task


async def _cleanup(task):
    task.cancel()
    try:
        await task
    except (asyncio.CancelledError, Exception):
        pass


# =============================================================================
# Soak 1: Sustained unary load on a single session
#
# Hundreds of echo calls on one session over time. Checks that latency
# doesn't degrade and no calls return wrong data.
# =============================================================================

@pytest.mark.timeout(60)
async def test_soak_sustained_unary():
    """Sustained unary load: latency must not degrade, no corruption."""
    stub, task = _create_session()
    n_calls = 500
    history = History()

    latencies: list[float] = []

    try:
        for i in range(n_calls):
            op_id = history.invoke(
                workload="soak_unary", nemesis=None,
                service="ChaosSession", method="echo",
                request={"value": i},
            )
            t0 = time.monotonic()
            try:
                resp = await asyncio.wait_for(
                    stub.echo(EchoReq(value=i)), timeout=5.0
                )
                elapsed = time.monotonic() - t0
                latencies.append(elapsed)
                val = resp.value if hasattr(resp, "value") else resp.get("value")
                history.complete(
                    op_id, "ok",
                    response={"value": val},
                    expected={"value": i},
                )
            except Exception as e:
                history.complete(op_id, "fail", error=str(e))

        history.timeout_pending()
        check_request_response_pairing(history)
        check_no_silent_corruption(history)

        ok_count = sum(1 for op in history.completions if op.op_type == "ok")
        assert ok_count == n_calls, f"Expected {n_calls} OK, got {ok_count}"

        # Check latency stability: last 10% should not be >5x the first 10%
        if len(latencies) > 20:
            first_10 = latencies[:len(latencies) // 10]
            last_10 = latencies[-len(latencies) // 10:]
            avg_first = sum(first_10) / len(first_10)
            avg_last = sum(last_10) / len(last_10)
            if avg_first > 0:
                ratio = avg_last / avg_first
                assert ratio < 5.0, (
                    f"LATENCY DEGRADATION: first 10% avg={avg_first*1000:.1f}ms, "
                    f"last 10% avg={avg_last*1000:.1f}ms, ratio={ratio:.1f}x"
                )

    finally:
        await _cleanup(task)


# =============================================================================
# Soak 2: Counter consistency over many increments
#
# Calls increment() many times on one session. Final counter must equal
# call count. No duplicates, no gaps.
# =============================================================================

@pytest.mark.timeout(60)
async def test_soak_counter_consistency():
    """Counter must be perfectly sequential over hundreds of increments."""
    stub, task = _create_session()
    n_calls = 500
    counters: list[int] = []

    try:
        for _ in range(n_calls):
            resp = await asyncio.wait_for(
                stub.increment(CounterReq()), timeout=5.0
            )
            val = resp.counter if hasattr(resp, "counter") else resp.get("counter")
            counters.append(val)

        expected = list(range(1, n_calls + 1))
        assert counters == expected, (
            f"COUNTER DRIFT after {n_calls} calls. "
            f"First divergence at index {next(i for i, (a, b) in enumerate(zip(counters, expected)) if a != b)}. "
            f"Last 5 values: {counters[-5:]}"
        )

    finally:
        await _cleanup(task)


# =============================================================================
# Soak 3: Session churn -- rapid open/close cycles
#
# Opens and closes sessions rapidly. Each session does a few calls.
# After many cycles, opens a final session and verifies it works.
# Catches resource leaks (tasks, queues, file descriptors).
# =============================================================================

@pytest.mark.timeout(60)
async def test_soak_session_churn():
    """Rapid session open/close must not leak resources."""
    n_sessions = 200
    calls_per = 3

    for i in range(n_sessions):
        stub, task = _create_session()
        try:
            for j in range(calls_per):
                resp = await asyncio.wait_for(
                    stub.echo(EchoReq(value=i * 100 + j)), timeout=3.0
                )
                val = resp.value if hasattr(resp, "value") else resp.get("value")
                assert val == i * 100 + j
        finally:
            await _cleanup(task)

    # Final session must still work cleanly
    stub, task = _create_session()
    try:
        resp = await asyncio.wait_for(
            stub.echo(EchoReq(value=99999)), timeout=3.0
        )
        val = resp.value if hasattr(resp, "value") else resp.get("value")
        assert val == 99999
    finally:
        await _cleanup(task)


# =============================================================================
# Soak 4: Mixed concurrent sessions over time
#
# Multiple sessions running concurrently for an extended period.
# Some do echo, some do increment, some do client-stream.
# All must remain correct throughout.
# =============================================================================

@pytest.mark.timeout(60)
async def test_soak_mixed_concurrent():
    """Multiple concurrent sessions with mixed patterns over sustained period."""
    duration_s = 10.0
    n_echo = 3
    n_counter = 3
    n_stream = 2

    sessions = []
    errors: list[str] = []
    counters: dict[int, list[int]] = {}
    echo_count = [0]
    stream_count = [0]

    for _ in range(n_echo + n_counter + n_stream):
        stub, task = _create_session()
        sessions.append((stub, task))

    end_time = time.monotonic() + duration_s

    async def echo_loop(client_id: int, stub):
        i = 0
        while time.monotonic() < end_time:
            val = client_id * 100000 + i
            try:
                resp = await asyncio.wait_for(
                    stub.echo(EchoReq(value=val)), timeout=5.0
                )
                got = resp.value if hasattr(resp, "value") else resp.get("value")
                if got != val:
                    errors.append(f"echo {client_id}: sent {val}, got {got}")
                    return
                echo_count[0] += 1
            except Exception as e:
                errors.append(f"echo {client_id}: {e}")
                return
            i += 1

    async def counter_loop(client_id: int, stub):
        vals = []
        while time.monotonic() < end_time:
            try:
                resp = await asyncio.wait_for(
                    stub.increment(CounterReq()), timeout=5.0
                )
                val = resp.counter if hasattr(resp, "counter") else resp.get("counter")
                vals.append(val)
            except Exception as e:
                errors.append(f"counter {client_id}: {e}")
                return
        counters[client_id] = vals

    async def stream_loop(client_id: int, stub):
        items = [1, 2, 3, 4, 5]
        expected = 15
        while time.monotonic() < end_time:
            try:
                async def gen():
                    for v in items:
                        yield SumItem(value=v)
                resp = await asyncio.wait_for(
                    stub.sum_stream(gen()), timeout=5.0
                )
                total = resp.total if hasattr(resp, "total") else resp.get("total")
                if total != expected:
                    errors.append(f"stream {client_id}: expected {expected}, got {total}")
                    return
                stream_count[0] += 1
            except Exception as e:
                errors.append(f"stream {client_id}: {e}")
                return

    try:
        tasks = []
        for i in range(n_echo):
            tasks.append(asyncio.create_task(
                echo_loop(i, sessions[i][0])
            ))
        for i in range(n_counter):
            idx = n_echo + i
            tasks.append(asyncio.create_task(
                counter_loop(idx, sessions[idx][0])
            ))
        for i in range(n_stream):
            idx = n_echo + n_counter + i
            tasks.append(asyncio.create_task(
                stream_loop(idx, sessions[idx][0])
            ))

        await asyncio.gather(*tasks)

        assert not errors, "\n".join(errors)

        # Counter sessions must be monotonic
        for client_id, vals in counters.items():
            for i in range(1, len(vals)):
                assert vals[i] > vals[i - 1], (
                    f"MONOTONICITY: counter client {client_id} at index {i}: "
                    f"{vals[i-1]} -> {vals[i]}"
                )

        # Sanity: we should have done meaningful work
        assert echo_count[0] > 10, f"Only {echo_count[0]} echo calls in {duration_s}s"
        assert stream_count[0] > 5, f"Only {stream_count[0]} stream calls in {duration_s}s"

    finally:
        for _, task in sessions:
            await _cleanup(task)


# =============================================================================
# Soak 5: Concurrent sessions with fault injection
#
# Multiple sessions where some have faults injected. The faulted sessions
# should fail cleanly. The healthy sessions must remain unaffected.
# =============================================================================

@pytest.mark.timeout(60)
async def test_soak_concurrent_with_faults():
    """Healthy sessions must be unaffected by faulted sessions on other streams."""
    n_healthy = 4
    n_faulted = 4
    calls_per_healthy = 50

    healthy_sessions = []
    faulted_sessions = []
    for _ in range(n_healthy):
        stub, task = _create_session()
        healthy_sessions.append((stub, task))
    for _ in range(n_faulted):
        stub, task = _create_session()
        faulted_sessions.append((stub, task))

    errors: list[str] = []
    healthy_ok = [0]

    async def healthy_work(client_id: int, stub):
        for i in range(calls_per_healthy):
            val = client_id * 10000 + i
            try:
                resp = await asyncio.wait_for(
                    stub.echo(EchoReq(value=val)), timeout=5.0
                )
                got = resp.value if hasattr(resp, "value") else resp.get("value")
                if got != val:
                    errors.append(f"HEALTHY {client_id}: sent {val}, got {got}")
                    return
                healthy_ok[0] += 1
            except Exception as e:
                errors.append(f"HEALTHY {client_id} call {i}: {e}")
                return

    async def faulted_work(client_id: int, stub, task):
        # Do a few calls then abruptly cancel the server task (simulating crash)
        try:
            for i in range(3):
                await asyncio.wait_for(
                    stub.echo(EchoReq(value=i)), timeout=3.0
                )
            # Kill the server side mid-session
            task.cancel()
            try:
                await task
            except (asyncio.CancelledError, Exception):
                pass
            # Try another call -- should fail
            try:
                await asyncio.wait_for(
                    stub.echo(EchoReq(value=999)), timeout=3.0
                )
            except Exception:
                pass  # Expected
        except Exception:
            pass  # Faulted sessions are expected to fail

    try:
        all_tasks = []
        for i, (stub, _) in enumerate(healthy_sessions):
            all_tasks.append(asyncio.create_task(healthy_work(i, stub)))
        for i, (stub, task) in enumerate(faulted_sessions):
            all_tasks.append(asyncio.create_task(faulted_work(i + n_healthy, stub, task)))

        await asyncio.gather(*all_tasks)

        # Healthy sessions must be completely unaffected
        assert not errors, "\n".join(errors)
        assert healthy_ok[0] == n_healthy * calls_per_healthy, (
            f"Expected {n_healthy * calls_per_healthy} healthy OK, "
            f"got {healthy_ok[0]}. Faulted sessions affected healthy ones."
        )

    finally:
        for _, task in healthy_sessions:
            await _cleanup(task)
        for _, task in faulted_sessions:
            await _cleanup(task)
