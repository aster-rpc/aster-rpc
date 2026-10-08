"""RPC-07: real QUIC pool admission, retry budgets and recovery.

Only observation is instrumented: every transport attempt delegates to the
ordinary Iroh transport and native one-slot SHARED pool.
"""
from __future__ import annotations

import asyncio
import json
from dataclasses import dataclass

import pytest

from aster import AsterClient, AsterServer, rpc, service, wire_type
from aster.config import AsterConfig
from aster.interceptors import CircuitBreakerInterceptor, DeadlineInterceptor, RetryInterceptor
from aster.rpc_types import RetryPolicy
from aster.status import RpcError, StatusCode


@wire_type("test.review.retrydeadline/Request")
@dataclass
class Request:
    tag: str = ""


@wire_type("test.review.retrydeadline/Response")
@dataclass
class Response:
    tag: str = ""


@service(name="ReviewRetryDeadline", version=1)
class Probe:
    def __init__(self):
        self.hold_entered = asyncio.Event()
        self.release_hold = asyncio.Event()
        self.transient_calls = 0
        self.active_holds = 0
        self.received_tags = []

    @rpc(idempotent=True, timeout=5)
    async def call(self, request: Request) -> Response:
        self.received_tags.append(request.tag)
        if request.tag == "hold":
            self.active_holds += 1
            self.hold_entered.set()
            try:
                await self.release_hold.wait()
                return Response("released")
            finally:
                self.active_holds -= 1
        if request.tag == "transient":
            self.transient_calls += 1
            if self.transient_calls == 1:
                raise RpcError(StatusCode.UNAVAILABLE, "transient native response")
            return Response("retried")
        return Response(request.tag)


@pytest.mark.asyncio
@pytest.mark.timeout(30)
@pytest.mark.parametrize("phase", ["backoff", "pool", "without_interceptor"])
async def test_native_pool_retry_deadline_and_recovery(phase, monkeypatch):
    probe = Probe()
    config = AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0")
    tasks = []
    loop = asyncio.get_running_loop()
    observations = []
    attempt_entered = asyncio.Event()
    backoff_entered = asyncio.Event()

    class ObservedRetry(RetryInterceptor):
        def backoff_seconds(self, attempt):
            delay = super().backoff_seconds(attempt)
            observations.append(("backoff", attempt, loop.time(), delay))
            backoff_entered.set()
            return delay

    policy = RetryPolicy(max_attempts=3)
    policy.backoff.initial_ms = policy.backoff.max_ms = 400 if phase == "pool" else 800
    policy.backoff.jitter = 0
    interceptors = [ObservedRetry(policy)]
    if phase != "without_interceptor":
        interceptors.insert(0, DeadlineInterceptor())

    async with AsterServer(services=[probe], allow_all_consumers=True, config=config) as server:
        async with AsterClient(address=server.address, config=config) as owner:
            client = await owner.client(Probe, interceptors=interceptors)
            blocker = await owner.client(Probe)
            native = client._transport._client
            native.shared_pool_size = 1
            assert blocker._transport._client.shared_pool_size == 1
            original_unary = client._transport.unary

            async def observed_unary(*args, **kwargs):
                observations.append(("attempt", kwargs["request"].tag, loop.time(), kwargs["deadline_secs"]))
                attempt_entered.set()
                return await original_unary(*args, **kwargs)

            monkeypatch.setattr(client._transport, "unary", observed_unary)
            try:
                if phase != "pool":
                    hold = asyncio.ensure_future(blocker.call(Request("hold"), timeout=5))
                    tasks.append(hold)
                    await asyncio.wait_for(probe.hold_entered.wait(), timeout=3)
                    started = loop.time()
                    target = asyncio.ensure_future(client.call(Request("transient"), timeout=1))
                    tasks.append(target)
                    await asyncio.wait_for(attempt_entered.wait(), timeout=2)
                    # A real handler owns the only pooled stream. The target's
                    # first native attempt cannot reach its handler yet.
                    await asyncio.sleep(0.35)
                    assert not target.done() and probe.transient_calls == 0
                    probe.release_hold.set()
                    assert (await hold).tag == "released"
                else:
                    started = loop.time()
                    target = asyncio.ensure_future(client.call(Request("transient"), timeout=1))
                    tasks.append(target)
                    await asyncio.wait_for(backoff_entered.wait(), timeout=2)
                    assert probe.transient_calls == 1
                    hold = asyncio.ensure_future(blocker.call(Request("hold"), timeout=5))
                    tasks.append(hold)
                    await asyncio.wait_for(probe.hold_entered.wait(), timeout=3)

                if phase == "without_interceptor":
                    assert (await target).tag == "retried"
                    assert loop.time() - started > 1
                    assert probe.transient_calls == 2
                else:
                    with pytest.raises(RpcError) as failure:
                        await target
                    assert failure.value.code == StatusCode.DEADLINE_EXCEEDED
                    if phase == "backoff":
                        assert isinstance(failure.value.__cause__, TimeoutError)
                    assert 0.9 <= loop.time() - started < 2.5
                    assert probe.transient_calls == 1

                elapsed = loop.time() - started
                target_attempts = [x for x in observations if x[0] == "attempt" and x[1] == "transient"]
                assert len(target_attempts) == (1 if phase == "backoff" else 2)
                assert all(x[3] == 1 for x in target_attempts)
                if phase == "pool":
                    # Second attempt entered native while the sole pool slot
                    # stayed owned by the blocker, and never reached the server.
                    assert probe.active_holds == 1 and not hold.done()
                    assert target_attempts[1][2] > observations[1][2]
                    probe.release_hold.set()
                    assert (await hold).tag == "released"

                assert (await client.call(Request("healthy"), timeout=1)).tag == "healthy"
                assert probe.transient_calls == (2 if phase == "without_interceptor" else 1)
                dispatcher = server._server._dispatcher
                until = loop.time() + 2
                while dispatcher.in_flight or dispatcher.server.live_calls():
                    assert loop.time() < until, "native/handler calls did not drain"
                    await asyncio.sleep(0.01)
                assert probe.active_holds == 0
                print(json.dumps({"phase": phase, "elapsed_s": elapsed,
                                  "target_attempts": len(target_attempts),
                                  "server_transient_calls": probe.transient_calls,
                                  "native_live_calls": dispatcher.server.live_calls(),
                                  "handler_in_flight": dispatcher.in_flight,
                                  "healthy_follow_up": True, "observations": observations}))
            finally:
                probe.release_hold.set()
                for task in tasks:
                    if not task.done():
                        task.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)


@pytest.mark.asyncio
@pytest.mark.timeout(30)
async def test_native_saturated_pool_cancellation_and_recovery(monkeypatch):
    probe = Probe()
    config = AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0")
    tasks = []
    entered = asyncio.Event()
    attempted = []
    async with AsterServer(services=[probe], allow_all_consumers=True, config=config) as server:
        async with AsterClient(address=server.address, config=config) as owner:
            client = await owner.client(Probe)
            client._transport._client.shared_pool_size = 1
            original = client._transport.unary

            async def observe(*args, **kwargs):
                tag = kwargs["request"].tag
                if tag.startswith("queued-"):
                    attempted.append(tag)
                    if len(attempted) == 16:
                        entered.set()
                return await original(*args, **kwargs)

            monkeypatch.setattr(client._transport, "unary", observe)
            try:
                hold = asyncio.ensure_future(client.call(Request("hold"), timeout=5))
                tasks.append(hold)
                await asyncio.wait_for(probe.hold_entered.wait(), timeout=3)
                queued = [asyncio.ensure_future(client.call(Request(f"queued-{i}"), timeout=3)) for i in range(16)]
                tasks.extend(queued)
                await asyncio.wait_for(entered.wait(), timeout=3)
                # Each observed transport invocation delegates directly to the
                # native client; the entered blocker owns its only pool slot.
                await asyncio.sleep(0.05)
                assert all(not task.done() for task in queued)
                assert probe.received_tags == ["hold"]
                for task in queued:
                    task.cancel()
                cancelled = await asyncio.gather(*queued, return_exceptions=True)
                assert all(isinstance(result, asyncio.CancelledError) for result in cancelled)
                assert not hold.done() and probe.active_holds == 1
                probe.release_hold.set()
                assert (await hold).tag == "released"
                for i in range(16):
                    tag = f"healthy-{i}"
                    assert (await client.call(Request(tag), timeout=1)).tag == tag
                assert probe.received_tags == ["hold"] + [f"healthy-{i}" for i in range(16)]
                dispatcher = server._server._dispatcher
                until = asyncio.get_running_loop().time() + 2
                while dispatcher.in_flight or dispatcher.server.live_calls():
                    assert asyncio.get_running_loop().time() < until, "cancelled pool calls did not drain"
                    await asyncio.sleep(0.01)
                assert probe.active_holds == 0
                print(json.dumps({"queued_native_attempts": len(attempted), "cancelled": len(cancelled),
                                  "healthy_follow_ups": 16, "cancelled_server_dispatches": 0,
                                  "native_live_calls": dispatcher.server.live_calls(),
                                  "handler_in_flight": dispatcher.in_flight}))
            finally:
                probe.release_hold.set()
                for task in tasks:
                    if not task.done():
                        task.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)


@pytest.mark.asyncio
@pytest.mark.timeout(30)
@pytest.mark.parametrize("aftermath", ["later_refusal", "cancelled_probe"])
async def test_native_breaker_probe_recovery(aftermath):
    """An abandoned half-open admission must allow a later real peer call."""
    probe = Probe()
    config = AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0")
    first = CircuitBreakerInterceptor(failure_threshold=1, recovery_timeout=0.02)
    second = CircuitBreakerInterceptor(failure_threshold=1, recovery_timeout=60)
    breakers = [first, second] if aftermath == "later_refusal" else [first]
    pending = None
    async with AsterServer(services=[probe], allow_all_consumers=True, config=config) as server:
        async with AsterClient(address=server.address, config=config) as owner:
            client = await owner.client(Probe, interceptors=breakers)
            try:
                with pytest.raises(RpcError) as seed:
                    await client.call(Request("transient"), timeout=2)
                assert seed.value.code == StatusCode.UNAVAILABLE
                assert first.state == first.OPEN and first.failure_count == 1
                await asyncio.sleep(0.03)
                if aftermath == "later_refusal":
                    with pytest.raises(RpcError, match="open"):
                        await client.call(Request("refused"), timeout=2)
                    assert probe.received_tags == ["transient"]
                    assert second.state == second.OPEN
                    # Make the later breaker's cooldown eligible too. This only
                    # advances admission policy, never bypasses native transport.
                    second.opened_at -= second.recovery_timeout
                else:
                    pending = asyncio.ensure_future(client.call(Request("hold"), timeout=3))
                    await asyncio.wait_for(probe.hold_entered.wait(), timeout=2)
                    assert first.half_open_calls == 1
                    pending.cancel()
                    with pytest.raises(asyncio.CancelledError):
                        await pending
                    probe.release_hold.set()
                assert first.state == first.HALF_OPEN
                assert first.half_open_calls == 0 and first.failure_count == 1
                assert (await client.call(Request("healthy-probe"), timeout=2)).tag == "healthy-probe"
                assert all(b.state == b.CLOSED and b.half_open_calls == 0 and b.failure_count == 0 for b in breakers)
                dispatcher = server._server._dispatcher
                until = asyncio.get_running_loop().time() + 2
                while dispatcher.in_flight or dispatcher.server.live_calls():
                    assert asyncio.get_running_loop().time() < until, "breaker calls did not drain"
                    await asyncio.sleep(0.01)
                assert probe.active_holds == 0
                assert probe.received_tags == (["transient", "healthy-probe"] if aftermath == "later_refusal" else ["transient", "hold", "healthy-probe"])
                print(json.dumps({"breaker_aftermath": aftermath, "healthy_native_probe": True,
                                  "server_tags": probe.received_tags, "native_live_calls": 0,
                                  "handler_in_flight": 0, "states": [b.state for b in breakers]}))
            finally:
                probe.release_hold.set()
                if pending is not None:
                    if not pending.done():
                        pending.cancel()
                    await asyncio.gather(pending, return_exceptions=True)
