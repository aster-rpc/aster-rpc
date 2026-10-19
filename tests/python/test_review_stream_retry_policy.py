"""Python stream retry policy: one-shot request sources must not be replayed."""
from dataclasses import dataclass
from typing import AsyncIterator

import pytest

import aster.client as client_module
from aster.client import create_client
from aster.codec import wire_type
from aster.decorators import client_stream, service
from aster.interceptors import RetryInterceptor
from aster.rpc_types import ExponentialBackoff, RetryPolicy, SerializationMode
from aster.status import RpcError, StatusCode


@wire_type("assurance.stream-retry/Request")
@dataclass
class Request:
    value: str = ""


@service(name="StreamRetryPolicy", version=1, serialization=[SerializationMode.XLANG])
class Service:
    @client_stream(idempotent=True)
    async def ingest(self, requests: AsyncIterator[Request]) -> Request:
        raise NotImplementedError

    @client_stream(idempotent=False)
    async def non_idempotent(self, requests: AsyncIterator[Request]) -> Request:
        raise NotImplementedError


def client(transport):
    return create_client(
        Service,
        transport=transport,
        interceptors=[RetryInterceptor(RetryPolicy(max_attempts=3, backoff=ExponentialBackoff(initial_ms=400)))],
    )


@pytest.fixture(autouse=True)
def backoff_sleeps(monkeypatch):
    """Injected backoff sleep: records each requested delay, never sleeps.
    A one-shot request source is never retried, so none is requested."""
    sleeps = []

    async def fake_sleep(delay):
        sleeps.append(delay)

    monkeypatch.setattr(client_module, "time_sleep", fake_sleep)
    yield sleeps
    assert sleeps == []


async def requests():
    for value in ("a", "b", "c"):
        yield Request(value)


@pytest.mark.parametrize("partial", [False, True])
@pytest.mark.parametrize("method", ["ingest", "non_idempotent"])
async def test_one_shot_stream_is_not_retried_and_fresh_call_recovers(partial, method):
    bodies = []
    metadata = []

    class Transport:
        async def client_stream(self, **kwargs):
            body = []
            bodies.append(body)
            metadata.append(dict(kwargs["metadata"]))
            async for item in kwargs["requests"]:
                body.append(item.value)
                if len(bodies) == 1 and partial:
                    raise RpcError(StatusCode.UNAVAILABLE, "after one item")
            if len(bodies) == 1:
                raise RpcError(StatusCode.UNAVAILABLE, "after all items")
            return Request("".join(body))

    rpc = getattr(client(Transport()), method)
    with pytest.raises(RpcError) as failed:
        await rpc(requests(), metadata={"x-review": "one"})
    assert failed.value.code == StatusCode.UNAVAILABLE
    assert bodies == [["a"] if partial else ["a", "b", "c"]]
    assert metadata == [{"x-review": "one"}]

    reply = await rpc(requests(), metadata={"x-review": "two"})
    assert reply.value == "abc"
    assert len(bodies) == 2 and bodies[1] == ["a", "b", "c"]
    assert metadata[1] == {"x-review": "two"}


async def test_source_failure_is_not_retried():
    bodies = []

    class Transport:
        async def client_stream(self, **kwargs):
            body = []
            bodies.append(body)
            async for item in kwargs["requests"]:
                body.append(item.value)
            return Request("".join(body))

    async def broken():
        yield Request("first")
        raise RpcError(StatusCode.UNAVAILABLE, "source failed")

    rpc = client(Transport()).ingest
    with pytest.raises(RpcError) as failed:
        await rpc(broken())
    assert failed.value.code == StatusCode.UNAVAILABLE
    assert failed.value.message == "source failed"
    assert bodies == [["first"]]
    assert (await rpc(requests())).value == "abc"
