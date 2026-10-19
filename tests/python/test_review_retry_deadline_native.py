"""RPC-07: the Python retry policy wired over the real native transport.

One thin wiring test: a native UNAVAILABLE reaches ``RetryInterceptor`` as an
``RpcError``, the retry goes back through the native pool, the caller's
deadline reaches every native attempt, and a healthy follow-up works. The
backoff sleep is injected, so nothing waits on the wall clock.

The rest is tested elsewhere, once:

- pool saturation with cancelled queued acquisitions, and a healthy peer
  progressing beside a stalled and disconnected peer: Rust
  ``aster/tests/it/rpc_client.rs``
  (``cancelled_queued_acquisitions_leave_a_saturated_pool_reusable``,
  ``a_healthy_peer_progresses_beside_a_stalled_and_disconnected_peer``);
- retry deadline budgets (expiry in backoff, attempts without a
  ``DeadlineInterceptor``) and the breaker's half-open probe slot (cancelled,
  refused by a later breaker): the fake-clock tests in
  ``test_aster_interceptors.py``; an abandoned native probe:
  ``an_abandoned_half_open_probe_leaves_the_breaker_recoverable`` in
  ``aster/tests/it/rpc_client.rs``.
"""
from __future__ import annotations

import asyncio
from dataclasses import dataclass

import pytest

import aster.client as client_module
from aster import AsterClient, AsterServer, rpc, service, wire_type
from aster.config import AsterConfig
from aster.interceptors import DeadlineInterceptor, RetryInterceptor
from aster.rpc_types import RetryPolicy
from aster.status import RpcError, StatusCode
from wait_budget import WAIT_BUDGET


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
        self.transient_calls = 0
        self.received_tags = []

    @rpc(idempotent=True, timeout=5)
    async def call(self, request: Request) -> Response:
        self.received_tags.append(request.tag)
        if request.tag == "transient":
            self.transient_calls += 1
            if self.transient_calls == 1:
                raise RpcError(StatusCode.UNAVAILABLE, "transient native response")
            return Response("retried")
        return Response(request.tag)


@pytest.mark.asyncio
@pytest.mark.timeout(30)
async def test_native_retry_and_deadline_wiring(monkeypatch):
    probe = Probe()
    config = AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0")
    sleeps = []
    attempts = []

    async def injected_sleep(delay):
        sleeps.append(delay)

    monkeypatch.setattr(client_module, "time_sleep", injected_sleep)
    policy = RetryPolicy(max_attempts=3)
    policy.backoff.initial_ms = policy.backoff.max_ms = 400
    policy.backoff.jitter = 0

    async with AsterServer(services=[probe], config=config) as server:
        async with AsterClient(address=server.address, config=config) as owner:
            client = await owner.client(
                Probe, interceptors=[DeadlineInterceptor(), RetryInterceptor(policy)]
            )
            native_unary = client._transport.unary

            async def observed_unary(*args, **kwargs):
                attempts.append((kwargs["request"].tag, kwargs["deadline_secs"]))
                return await native_unary(*args, **kwargs)

            monkeypatch.setattr(client._transport, "unary", observed_unary)

            reply = await asyncio.wait_for(client.call(Request("transient"), timeout=5), WAIT_BUDGET)
            assert reply.tag == "retried"
            assert probe.transient_calls == 2
            assert sleeps == [0.4]
            # The caller's deadline reached both native attempts.
            assert [tag for tag, _ in attempts] == ["transient", "transient"]
            assert all(deadline == 5 for _, deadline in attempts)

            assert (await client.call(Request("healthy"), timeout=1)).tag == "healthy"
            assert probe.received_tags == ["transient", "transient", "healthy"]
