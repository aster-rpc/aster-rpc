"""Review regressions: exercise the real channel with a controlled native boundary."""

import asyncio
from types import SimpleNamespace
from unittest.mock import AsyncMock

from aster.transport.iroh import IrohBidiChannel


def channel(native):
    transport = SimpleNamespace(
        _client=native,
        _codec=SimpleNamespace(encode=lambda x: x, decode=lambda x: x),
        _version_of=lambda _: 1,
        _session_id=0,
    )
    return IrohBidiChannel(transport, "Echo", "chat", None, 0, 1)


async def test_review_concurrent_send_recv_open_one_bidi_call():
    entered = asyncio.Event()
    release = asyncio.Event()
    pairs = []

    async def open_call(*args, **kwargs):
        sink = SimpleNamespace(send=AsyncMock(), finish=AsyncMock())
        stream = SimpleNamespace(next_batch=AsyncMock(return_value=[b"reply"]))
        pairs.append((sink, stream))
        entered.set()
        await release.wait()
        return sink, stream

    ch = channel(SimpleNamespace(bidi_open=open_call))
    sending = asyncio.create_task(ch.send(b"request"))
    await entered.wait()
    receiving = asyncio.create_task(ch.recv())
    await asyncio.sleep(0)  # both directions run while the first open is pending
    release.set()
    await asyncio.gather(sending, receiving)
    await ch.close()
    assert len(pairs) == 1, "one logical channel opened multiple independent native RPCs"


async def test_review_close_before_first_send_finishes_empty_request_stream():
    sink = SimpleNamespace(send=AsyncMock(), finish=AsyncMock())
    stream = SimpleNamespace(next_batch=AsyncMock(return_value=[]))
    native = SimpleNamespace(bidi_open=AsyncMock(return_value=(sink, stream)))
    ch = channel(native)
    await ch.close()
    await ch.wait_for_trailer()
    sink.finish.assert_awaited_once()
