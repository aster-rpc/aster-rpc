"""
Concurrent multi-client tests.

These tests go beyond the single-client chaos tests by running multiple
stubs against a shared server simultaneously. They check:

1. No cross-talk: client A never receives client B's response.
2. Session state isolation: each session has its own counter.
3. Monotonicity under contention: sequential calls on one session see
   monotonically increasing counters even when other sessions are active.
4. No resource leaks: after N sessions open and close, the server still works.
5. Mixed patterns under load: unary + streaming calls interleaved.
"""

from __future__ import annotations

import pytest

pytest.skip(
    "aster.session retired -- Phase-8 CALL-frame mechanism removed; "
    "replaced by ClientSession + reactor-based session lifecycle",
    allow_module_level=True,
)

import asyncio
import uuid
from typing import AsyncIterator

from aster.codec import ForyCodec, wire_type
from aster.decorators import service, rpc, client_stream, server_stream, bidi_stream, _SERVICE_INFO_ATTR
from aster.framing import CALL, TRAILER, read_frame, write_frame
from aster.protocol import CallHeader, RpcStatus, StreamHeader
from aster.rpc_types import SerializationMode
from aster.session import (
    SessionServer, SessionStub,
    _ByteQueue, _FakeRecvStream, _FakeSendStream,
    _generate_session_stub_class,
)
from aster.status import StatusCode

from .workloads import (
    ChaosSession, EchoReq, EchoResp, CounterReq, CounterResp,
    SumItem, SumResp, ALL_TYPES,
)
from .harness import (
    History,
    check_request_response_pairing,
    check_no_silent_corruption,
)


def _codec():
    return ForyCodec(mode=SerializationMode.XLANG, types=list(ALL_TYPES))


def _create_session_pair():
    """Create a (stub, server_task) pair sharing a single in-process server."""
    info = getattr(ChaosSession, _SERVICE_INFO_ATTR)
    codec = _codec()

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
        server.run(header, s_send, s_recv, peer="test")
    )

    stub_cls = _generate_session_stub_class(info)
    stub = stub_cls(
        send=c_send, recv=c_recv, codec=codec,
        service_info=info, interceptors=None,
        session_id=session_id,
    )

    return stub, server_task


async def _cleanup(server_task):
    server_task.cancel()
    try:
        await server_task
    except (asyncio.CancelledError, Exception):
        pass


# =============================================================================
# Test 1: No cross-talk between concurrent sessions
#
# N sessions all call echo simultaneously with their own unique value.
# Each response must match the request that sent it.
# =============================================================================

@pytest.mark.timeout(30)
async def test_no_crosstalk_between_sessions():
    """N concurrent sessions must never receive each other's responses."""
    n_sessions = 10
    n_calls = 20

    sessions = []
    for _ in range(n_sessions):
        stub, task = _create_session_pair()
        sessions.append((stub, task))

    history = History()
    errors: list[str] = []

    async def client_work(client_id: int, stub):
        for i in range(n_calls):
            unique_val = client_id * 10000 + i
            op_id = history.invoke(
                workload="crosstalk", nemesis=None,
                service="ChaosSession", method="echo",
                request={"value": unique_val},
            )
            try:
                resp = await asyncio.wait_for(
                    stub.echo(EchoReq(value=unique_val)), timeout=5.0
                )
                val = resp.value if hasattr(resp, "value") else resp.get("value")
                history.complete(
                    op_id, "ok",
                    response={"value": val},
                    expected={"value": unique_val},
                )
                if val != unique_val:
                    errors.append(
                        f"CROSSTALK: client {client_id} sent {unique_val}, "
                        f"got {val}"
                    )
            except Exception as e:
                history.complete(op_id, "fail", error=str(e))

    try:
        tasks = [
            asyncio.create_task(client_work(i, stub))
            for i, (stub, _) in enumerate(sessions)
        ]
        await asyncio.gather(*tasks)

        history.timeout_pending()
        check_request_response_pairing(history)
        check_no_silent_corruption(history)

        assert not errors, "\n".join(errors)

        ok_count = sum(1 for op in history.completions if op.op_type == "ok")
        assert ok_count == n_sessions * n_calls, (
            f"Expected {n_sessions * n_calls} OK responses, got {ok_count}"
        )

    finally:
        for _, task in sessions:
            await _cleanup(task)


# =============================================================================
# Test 2: Session state isolation under concurrency
#
# Each session has its own counter. N sessions calling increment()
# concurrently must each see their own 1,2,3,... sequence.
# =============================================================================

@pytest.mark.timeout(30)
async def test_session_state_isolation():
    """Each session's counter must be independent of other sessions."""
    n_sessions = 8
    n_increments = 15

    sessions = []
    for _ in range(n_sessions):
        stub, task = _create_session_pair()
        sessions.append((stub, task))

    results: dict[int, list[int]] = {}
    errors: list[str] = []

    async def client_work(client_id: int, stub):
        counters = []
        for _ in range(n_increments):
            try:
                resp = await asyncio.wait_for(
                    stub.increment(CounterReq()), timeout=5.0
                )
                val = resp.counter if hasattr(resp, "counter") else resp.get("counter")
                counters.append(val)
            except Exception as e:
                errors.append(f"client {client_id}: {e}")
                break
        results[client_id] = counters

    try:
        tasks = [
            asyncio.create_task(client_work(i, stub))
            for i, (stub, _) in enumerate(sessions)
        ]
        await asyncio.gather(*tasks)

        assert not errors, "\n".join(errors)

        for client_id, counters in results.items():
            expected = list(range(1, n_increments + 1))
            assert counters == expected, (
                f"STATE LEAK: client {client_id} got counters {counters}, "
                f"expected {expected}. Another session's state bled through."
            )

    finally:
        for _, task in sessions:
            await _cleanup(task)


# =============================================================================
# Test 3: Monotonicity under contention
#
# One session calls increment() rapidly while other sessions call echo()
# to create contention on the event loop. The increment sequence must
# remain strictly monotonic.
# =============================================================================

@pytest.mark.timeout(30)
async def test_monotonicity_under_contention():
    """Counter must be strictly monotonic even under event loop contention."""
    n_noise_sessions = 5
    n_noise_calls = 50
    n_increments = 30

    counter_stub, counter_task = _create_session_pair()
    noise_sessions = []
    for _ in range(n_noise_sessions):
        stub, task = _create_session_pair()
        noise_sessions.append((stub, task))

    counters: list[int] = []
    errors: list[str] = []

    async def counter_work():
        for _ in range(n_increments):
            try:
                resp = await asyncio.wait_for(
                    counter_stub.increment(CounterReq()), timeout=5.0
                )
                val = resp.counter if hasattr(resp, "counter") else resp.get("counter")
                counters.append(val)
            except Exception as e:
                errors.append(f"counter: {e}")
                break

    async def noise_work(stub):
        for i in range(n_noise_calls):
            try:
                await asyncio.wait_for(
                    stub.echo(EchoReq(value=i)), timeout=5.0
                )
            except Exception:
                break

    try:
        all_tasks = [asyncio.create_task(counter_work())]
        for stub, _ in noise_sessions:
            all_tasks.append(asyncio.create_task(noise_work(stub)))

        await asyncio.gather(*all_tasks)

        assert not errors, "\n".join(errors)
        assert len(counters) == n_increments, (
            f"Expected {n_increments} counter values, got {len(counters)}"
        )

        for i in range(1, len(counters)):
            assert counters[i] > counters[i - 1], (
                f"MONOTONICITY VIOLATION at index {i}: "
                f"{counters[i-1]} -> {counters[i]}. "
                f"Full sequence: {counters}"
            )

    finally:
        await _cleanup(counter_task)
        for _, task in noise_sessions:
            await _cleanup(task)


# =============================================================================
# Test 4: Session churn -- open/close many sessions, server stays healthy
#
# Opens N sessions sequentially (each does a few calls then closes).
# After all are done, opens one more and verifies it works. Catches
# resource leaks (queues, tasks, instances not cleaned up).
# =============================================================================

@pytest.mark.timeout(30)
async def test_session_churn_no_leak():
    """Server handles many sequential sessions without degradation."""
    n_sessions = 50
    calls_per_session = 5

    for i in range(n_sessions):
        stub, task = _create_session_pair()
        try:
            for j in range(calls_per_session):
                resp = await asyncio.wait_for(
                    stub.echo(EchoReq(value=i * 100 + j)), timeout=3.0
                )
                val = resp.value if hasattr(resp, "value") else resp.get("value")
                assert val == i * 100 + j, (
                    f"session {i}, call {j}: expected {i * 100 + j}, got {val}"
                )
        finally:
            await _cleanup(task)

    # Final session -- must still work
    stub, task = _create_session_pair()
    try:
        resp = await asyncio.wait_for(
            stub.echo(EchoReq(value=9999)), timeout=3.0
        )
        val = resp.value if hasattr(resp, "value") else resp.get("value")
        assert val == 9999, f"final session: expected 9999, got {val}"
    finally:
        await _cleanup(task)


# =============================================================================
# Test 5: Mixed patterns under concurrent load
#
# Multiple sessions run different patterns simultaneously:
# - Some do unary echo
# - Some do client-stream sum
# - All against separate session instances
# Checks that pattern dispatch doesn't get confused under load.
# =============================================================================

@pytest.mark.timeout(30)
async def test_mixed_patterns_concurrent():
    """Different RPC patterns on concurrent sessions must not interfere."""
    n_echo = 5
    n_stream = 5
    echo_calls = 15
    stream_items = [1, 2, 3, 4, 5]
    expected_sum = sum(stream_items)

    echo_sessions = []
    stream_sessions = []
    for _ in range(n_echo):
        stub, task = _create_session_pair()
        echo_sessions.append((stub, task))
    for _ in range(n_stream):
        stub, task = _create_session_pair()
        stream_sessions.append((stub, task))

    errors: list[str] = []

    async def echo_work(client_id: int, stub):
        for i in range(echo_calls):
            val = client_id * 1000 + i
            try:
                resp = await asyncio.wait_for(
                    stub.echo(EchoReq(value=val)), timeout=5.0
                )
                got = resp.value if hasattr(resp, "value") else resp.get("value")
                if got != val:
                    errors.append(f"echo client {client_id}: sent {val}, got {got}")
            except Exception as e:
                errors.append(f"echo client {client_id}: {e}")
                break

    async def stream_work(client_id: int, stub):
        try:
            async def items():
                for v in stream_items:
                    yield SumItem(value=v)

            resp = await asyncio.wait_for(stub.sum_stream(items()), timeout=5.0)
            total = resp.total if hasattr(resp, "total") else resp.get("total")
            if total != expected_sum:
                errors.append(
                    f"stream client {client_id}: expected {expected_sum}, got {total}"
                )
        except Exception as e:
            errors.append(f"stream client {client_id}: {e}")

    try:
        all_tasks = []
        for i, (stub, _) in enumerate(echo_sessions):
            all_tasks.append(asyncio.create_task(echo_work(i, stub)))
        for i, (stub, _) in enumerate(stream_sessions):
            all_tasks.append(asyncio.create_task(stream_work(i + n_echo, stub)))

        await asyncio.gather(*all_tasks)
        assert not errors, "\n".join(errors)

    finally:
        for _, task in echo_sessions + stream_sessions:
            await _cleanup(task)


# =============================================================================
# Test 6: Concurrent calls on a single session (lock contention)
#
# The SessionStub has an asyncio.Lock that serializes calls. Multiple
# coroutines calling the same stub concurrently must all get correct
# responses -- the lock must serialize them, not corrupt them.
# =============================================================================

@pytest.mark.timeout(30)
async def test_concurrent_calls_single_session():
    """Multiple concurrent callers on one session must all get correct results."""
    stub, task = _create_session_pair()
    n_callers = 10
    calls_per_caller = 10

    results: dict[int, list[tuple[int, int]]] = {}
    errors: list[str] = []

    async def caller(caller_id: int):
        pairs = []
        for i in range(calls_per_caller):
            val = caller_id * 10000 + i
            try:
                resp = await asyncio.wait_for(
                    stub.echo(EchoReq(value=val)), timeout=10.0
                )
                got = resp.value if hasattr(resp, "value") else resp.get("value")
                pairs.append((val, got))
                if got != val:
                    errors.append(
                        f"CORRUPTION: caller {caller_id} sent {val}, got {got}"
                    )
            except Exception as e:
                errors.append(f"caller {caller_id} call {i}: {e}")
                break
        results[caller_id] = pairs

    try:
        callers = [
            asyncio.create_task(caller(i)) for i in range(n_callers)
        ]
        await asyncio.gather(*callers)

        assert not errors, "\n".join(errors)

        total_ok = sum(len(pairs) for pairs in results.values())
        assert total_ok == n_callers * calls_per_caller, (
            f"Expected {n_callers * calls_per_caller} OK, got {total_ok}"
        )

    finally:
        await _cleanup(task)


# =============================================================================
# Test 7: Concurrent increment on one session (linearizability)
#
# N coroutines all call increment() on the same session. The lock
# serializes them. The final counter must equal total calls, and every
# returned counter must be unique (no duplicates, no gaps).
# =============================================================================

@pytest.mark.timeout(30)
async def test_linearizable_increment():
    """Concurrent increments must produce a complete set of counter values."""
    stub, task = _create_session_pair()
    n_callers = 8
    calls_per_caller = 10
    total_calls = n_callers * calls_per_caller

    all_counters: list[int] = []
    lock = asyncio.Lock()
    errors: list[str] = []

    async def caller(caller_id: int):
        for _ in range(calls_per_caller):
            try:
                resp = await asyncio.wait_for(
                    stub.increment(CounterReq()), timeout=10.0
                )
                val = resp.counter if hasattr(resp, "counter") else resp.get("counter")
                async with lock:
                    all_counters.append(val)
            except Exception as e:
                errors.append(f"caller {caller_id}: {e}")
                break

    try:
        callers = [
            asyncio.create_task(caller(i)) for i in range(n_callers)
        ]
        await asyncio.gather(*callers)

        assert not errors, "\n".join(errors)
        assert len(all_counters) == total_calls, (
            f"Expected {total_calls} counter values, got {len(all_counters)}"
        )

        sorted_counters = sorted(all_counters)
        expected = list(range(1, total_calls + 1))
        assert sorted_counters == expected, (
            f"LINEARIZABILITY VIOLATION: counters {sorted_counters} != "
            f"expected {expected}. Duplicates or gaps indicate the session "
            f"lock failed to serialize concurrent calls."
        )

    finally:
        await _cleanup(task)
