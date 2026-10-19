"""Cross-binding interop smoke: the Python side of ``tests/interop/``.

Shared contract (must stay identical to ``tests/interop/rust/contract.rs``,
which documents every method's semantics, and to the other languages)::

    service InteropEcho v1
      echo(interop/EchoRequest) -> interop/EchoReply                (unary)
      generate(interop/EchoRequest) -> stream interop/EchoReply     (server stream)
      ingest(stream interop/EchoRequest) -> interop/EchoReply       (client stream)
      chat(stream interop/EchoRequest) -> stream interop/EchoReply  (bidi)
      hold(interop/EchoRequest) -> interop/EchoReply                (unary, blocks)
      cancelled(interop/EchoRequest) -> interop/EchoReply           (unary)
      traceEcho(interop/EchoRequest) -> interop/EchoReply           (unary)
    service InteropSession v1 (scope = session)
      add(interop/EchoRequest) -> interop/EchoReply                 (unary)
    interop/EchoRequest { message: string, count: varint32, data: binary }
    interop/EchoReply   { message: string, count: varint32, data: binary, server_lang: string }

Expose mini-matrix: ``expose-server`` serves HTTP service ``interop-http`` on
ALPN ``aster-expose/interop/1`` (``GET /hello``, ``POST /echo``, header
``x-server-lang``); ``expose-client <ticket>`` is a streaming-relay consumer.

Usage (from the repo root, after ``scripts/build.sh``)::

    uv run --no-sync python tests/interop/python/interop_echo.py contract
    uv run --no-sync python tests/interop/python/interop_echo.py server
    uv run --no-sync python tests/interop/python/interop_echo.py client <aster1-ticket>
    uv run --no-sync python tests/interop/python/interop_echo.py expose-server
    uv run --no-sync python tests/interop/python/interop_echo.py expose-client <aster1-ticket>

Output protocol (parsed by ``tests/interop/run.py``): ``CONTRACT_ID <hex>``,
``SESSION_CONTRACT_ID <hex>``, ``ADDRESS <aster1...>`` (server), ``CASE <name> OK|FAIL ...`` (client).
Open gate, relay disabled, loopback bind: works offline.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import sys
import time
from collections.abc import AsyncIterator
from dataclasses import dataclass

import pyfory

from aster import tracing
from aster import (
    AsterConfig,
    AsterServer,
    AsterTicket,
    HttpHandler,
    HttpResponse,
    IrohTransport,
    Node,
    NodeAddr,
    RPC_ALPN,
    RpcError,
    StatusCode,
    bidi_stream,
    client_stream,
    create_client,
    net_client,
    relay_http_stream,
    rpc,
    server_stream,
    service,
    wire_type,
)
from aster.contract.identity import contract_id_from_service
from aster.transport.iroh import next_session_id

EXPOSE_ALPN = b"aster-expose/interop/1"
EXPOSE_SERVICE = "interop-http"


@wire_type("interop/EchoRequest")
@dataclass
class EchoRequest:
    message: str = ""
    count: pyfory.Int32 = 0  # xlang varint32 == Rust i32
    data: bytes = b""


@wire_type("interop/EchoReply")
@dataclass
class EchoReply:
    message: str = ""
    count: pyfory.Int32 = 0
    data: bytes = b""
    server_lang: str = ""


@service(name="InteropEcho", version=1)
class InteropEcho:
    @rpc()
    async def echo(self, req: EchoRequest) -> EchoReply:
        return EchoReply(
            message=req.message,
            count=_wrap_i32(req.count + 1),
            data=req.data,
            server_lang="python",
        )

    @server_stream()
    async def generate(self, req: EchoRequest) -> AsyncIterator[EchoReply]:
        for i in range(max(req.count, 0)):
            yield EchoReply(message=req.message, count=i, data=req.data, server_lang="python")

    @client_stream()
    async def ingest(self, reqs: AsyncIterator[EchoRequest]) -> EchoReply:
        n = 0
        last = ""
        async for r in reqs:
            n += 1
            last = r.message
        return EchoReply(message=last, count=_wrap_i32(n), data=b"", server_lang="python")

    @bidi_stream()
    async def chat(self, reqs: AsyncIterator[EchoRequest]) -> AsyncIterator[EchoReply]:
        i = 0
        async for r in reqs:
            yield EchoReply(message=r.message, count=_wrap_i32(i), data=r.data, server_lang="python")
            i += 1

    @rpc()
    async def hold(self, req: EchoRequest) -> EchoReply:
        global _HOLD_CANCELLED
        try:
            await asyncio.sleep(60)
        except asyncio.CancelledError:
            _HOLD_CANCELLED += 1
            raise
        return EchoReply(message="timeout", count=-1, data=b"", server_lang="python")

    @rpc()
    async def cancelled(self, req: EchoRequest) -> EchoReply:
        return EchoReply(message="cancelled", count=_HOLD_CANCELLED, data=b"", server_lang="python")

    @rpc()
    async def traceEcho(self, req: EchoRequest) -> EchoReply:  # noqa: N802 (wire name)
        # The handler's trace carrier is filled from the dispatch trace context.
        return EchoReply(message=tracing.current_trace_id() or "", count=0, data=b"", server_lang="python")


# `hold` calls that observed a cancellation (process-wide).
_HOLD_CANCELLED = 0


@service(name="InteropSession", version=1, scoped="session")
class InteropSession:
    """One instance per (connection, session id)."""

    def __init__(self, peer=None) -> None:
        self.total = 0
        self.calls = 0

    @rpc()
    async def add(self, req: EchoRequest) -> EchoReply:
        self.total = _wrap_i32(self.total + req.count)
        self.calls += 1
        return EchoReply(message=f"calls={self.calls}", count=self.total, data=b"", server_lang="python")


def _wrap_i32(v: int) -> int:
    return (v + 2**31) % 2**32 - 2**31


def cases(client_lang: str) -> list[tuple[str, EchoRequest]]:
    """The shared cases. Must match ``cases()`` in the Rust and TS drivers."""
    return [
        ("small", EchoRequest(message=f"hello from {client_lang}", count=41, data=bytes([0, 1, 2, 255]))),
        # > 4096 bytes on the wire and highly compressible: exercises zstd.
        ("zstd", EchoRequest(
            message="héllo ✓ " * 200,
            count=-123_456,
            data=bytes(i % 251 for i in range(20_000)),
        )),
        # A single string field longer than 4095 bytes (Fory string-length edge).
        ("long_string", EchoRequest(message="x" * 5000, count=2**31 - 2, data=b"")),
    ]


def check(req: EchoRequest, reply: EchoReply) -> str:
    if reply.message != req.message:
        raise AssertionError(f"message mismatch (len {len(reply.message)} vs {len(req.message)})")
    if reply.count != _wrap_i32(req.count + 1):
        raise AssertionError(f"count {reply.count} != {_wrap_i32(req.count + 1)}")
    if bytes(reply.data) != req.data:
        raise AssertionError(f"data mismatch (len {len(reply.data)} vs {len(req.data)})")
    if not reply.server_lang:
        raise AssertionError("empty server_lang")
    return reply.server_lang


async def _stream_case(client) -> str:
    """``generate`` with count 3. Must match the Rust and TS drivers."""
    req = EchoRequest(message="stream", count=3, data=bytes([7, 8, 9]))
    replies = [r async for r in client.generate(req)]
    if len(replies) != 3:
        raise AssertionError(f"{len(replies)} replies != 3")
    for i, r in enumerate(replies):
        if r.count != i or r.message != req.message or bytes(r.data) != req.data:
            raise AssertionError(f"reply {i} mismatch: {r!r}")
    return replies[0].server_lang


async def _ingest_case(client) -> str:
    """``ingest`` with three requests: one reply, count 3, last message echoed."""

    async def reqs():
        for i in range(3):
            yield EchoRequest(message=f"m{i}", count=i, data=bytes([i]))

    reply = await client.ingest(reqs())
    if reply.count != 3 or reply.message != "m2":
        raise AssertionError(f"reply mismatch: {reply!r}")
    if not reply.server_lang:
        raise AssertionError("empty server_lang")
    return reply.server_lang


async def _chat_case(client) -> str:
    """``chat``, interleaved: send ``c<i>``, read its reply, three times; then
    end the request stream and expect the response stream to end."""
    lang = ""
    async with client.chat() as ch:
        for i in range(3):
            req = EchoRequest(message=f"c{i}", count=100 + i, data=bytes([i, 9]))
            await ch.send(req)
            r = await ch.recv()
            if r is None:
                raise AssertionError(f"stream ended before reply {i}")
            if r.count != i or r.message != req.message or bytes(r.data) != req.data:
                raise AssertionError(f"reply {i} mismatch: {r!r}")
            lang = r.server_lang
        await ch.close()
        extra = await ch.recv()
        if extra is not None:
            raise AssertionError(f"unexpected extra reply {extra!r}")
    if not lang:
        raise AssertionError("empty server_lang")
    return lang


async def _cancelled_count(client) -> int:
    return (await asyncio.wait_for(client.cancelled(EchoRequest(message="probe")), 10)).count


async def _expect_server_cancel(client, want: int) -> None:
    seen = 0
    for _ in range(50):
        seen = await _cancelled_count(client)
        if seen >= want:
            return
        await asyncio.sleep(0.1)
    raise AssertionError(f"server never observed the cancellation (count {seen}, want {want})")


async def _deadline_case(client) -> str:
    """``hold`` with a 1 s deadline (the smallest every client can express):
    DEADLINE_EXCEEDED, and the server observes a cancellation."""
    before = await _cancelled_count(client)
    try:
        r = await asyncio.wait_for(client.hold(EchoRequest(message="hold"), timeout=1), 15)
    except RpcError as exc:
        if exc.code != StatusCode.DEADLINE_EXCEEDED:
            raise AssertionError(f"expected DEADLINE_EXCEEDED, got {exc!r}") from exc
    else:
        raise AssertionError(f"hold returned {r!r} instead of an error")
    await _expect_server_cancel(client, before + 1)
    return "python"


async def _cancel_case(client) -> str:
    """``hold`` cancelled by the client after 300 ms: the server observes it."""
    before = await _cancelled_count(client)
    try:
        # short-wait-ok: the client cancels hold after 300 ms; expiry is the case under test
        r = await asyncio.wait_for(client.hold(EchoRequest(message="hold")), 0.3)
    except asyncio.TimeoutError:
        pass
    else:
        raise AssertionError(f"hold returned {r!r} before the client cancelled it")
    await _expect_server_cancel(client, before + 1)
    return "python"


_SESSION_CODEC = None


def _session_codec():
    """One codec for both session clients (types registered once)."""
    global _SESSION_CODEC
    if _SESSION_CODEC is None:
        from aster.codec import ForyCodec
        from aster.rpc_types import SerializationMode

        _SESSION_CODEC = ForyCodec(mode=SerializationMode.XLANG, types=[EchoRequest, EchoReply])
    return _SESSION_CODEC


async def _session_case(conn) -> str:
    """Two sessions on one connection: state persists per session and is
    isolated between them."""
    codec = _session_codec()
    sessions = []
    for _ in range(2):
        transport = IrohTransport(connection=conn, codec=codec, session_id=next_session_id(conn))
        sessions.append(create_client(InteropSession, transport=transport, codec=codec))
    a, b = sessions
    steps = [(a, 5, 5, "calls=1"), (a, 7, 12, "calls=2"), (b, 1, 1, "calls=1"),
             (a, 0, 12, "calls=3"), (b, 10, 11, "calls=2")]
    lang = ""
    for i, (s, add, total, calls) in enumerate(steps):
        r = await asyncio.wait_for(s.add(EchoRequest(message="add", count=add)), 15)
        if r.count != total or r.message != calls:
            raise AssertionError(f"step {i}: got total {r.count} {r.message!r}, want total {total} {calls!r}")
        lang = r.server_lang
    if not lang:
        raise AssertionError("empty server_lang")
    return lang


_TRACE_ID = "4bf92f3577b34da6a3ce929d0e0e4736"


async def _trace_case(client) -> str:
    """``traceEcho`` called under a known, sampled ``traceparent``: the client
    sends it, the server must report the same trace id from its dispatch
    context."""
    traceparent = f"00-{_TRACE_ID}-{os.urandom(8).hex()}-01"
    with tracing.with_trace_context(traceparent):
        reply = await client.traceEcho(EchoRequest(message="probe"))
    if reply.message != _TRACE_ID:
        raise AssertionError(f"server saw trace id {reply.message!r}, want {_TRACE_ID}")
    return reply.server_lang


async def _chain_trace_baggage_case(client) -> str:
    request_id = os.urandom(8).hex()
    traceparent = f"00-{_TRACE_ID}-{os.urandom(8).hex()}-01"
    with tracing.with_trace_context(traceparent, {"tenant": "interop-chain", "request": request_id}):
        if tracing.current_trace_id() != _TRACE_ID:
            raise AssertionError("Python source context lost the supplied trace id")
        source_baggage = tracing.current_baggage()
        if source_baggage.get("tenant") != "interop-chain" or source_baggage.get("request") != request_id:
            raise AssertionError(f"Python source baggage mismatch: {source_baggage!r}")
        reply = await client.traceEcho(EchoRequest(message=f"chain-trace:{request_id}"), timeout=12)
    rows = reply.message.splitlines()
    if len(rows) != 2:
        raise AssertionError(f"chain returned {len(rows)} handler receipt rows, expected 2: {reply.message!r}")
    parsed = []
    for row, lang in zip(rows, ("ts", "rust"), strict=True):
        fields = row.split("|")
        if len(fields) != 5:
            raise AssertionError(f"malformed {lang} receipt: {row!r}")
        got_lang, trace_id, tenant, got_request, remaining_ms = fields
        if got_lang != lang or trace_id != _TRACE_ID or tenant != "interop-chain" or got_request != request_id:
            raise AssertionError(f"{lang} receipt mismatch: {row!r}")
        parsed.append((lang, int(remaining_ms)))
    # TS exposes its inbound deadline. Rust's handler API does not; -1 is an
    # explicit unsupported sentinel. Native Rust deadline enforcement is
    # verified by the leaf cancellation case below.
    if parsed[0][1] <= 0 or parsed[0][1] > 12_000 or parsed[1][1] != -1:
        raise AssertionError(f"deadline observations unsupported, expired, or extended: {parsed!r}")
    return "python-client->ts->rust"


async def _chain_expect_leaf_cancel(client, want: int) -> None:
    seen = 0
    until = time.monotonic() + 4
    while time.monotonic() < until:
        seen = await asyncio.wait_for(_cancelled_count(client), until - time.monotonic())
        if seen == want:
            return
        if seen > want:
            raise AssertionError(f"Rust leaf cancellation count advanced unexpectedly: {seen}, expected {want}")
        await asyncio.sleep(0.1)
    raise AssertionError(f"Rust leaf never observed cancellation (count {seen}, want {want})")


async def _chain_deadline_case(client) -> str:
    before = await _cancelled_count(client)
    started = time.monotonic()
    try:
        # short-wait-ok: guard only; the elapsed check below already fails the case at 4 s
        await asyncio.wait_for(client.hold(EchoRequest(message="chain-deadline"), timeout=2), 8)
    except RpcError as exc:
        if exc.code != StatusCode.DEADLINE_EXCEEDED:
            raise AssertionError(f"expected DEADLINE_EXCEEDED, got {exc!r}") from exc
    else:
        raise AssertionError("held chain call unexpectedly returned")
    elapsed = time.monotonic() - started
    if elapsed < 1 or elapsed >= 4:
        raise AssertionError(f"chain deadline did not expire near the requested 2s budget: {elapsed:.3f}s")
    await _chain_expect_leaf_cancel(client, before + 1)
    return "python-client->ts->rust"


async def _chain_cancel_case(client) -> str:
    before = await _cancelled_count(client)
    request_id = os.urandom(8).hex()
    call = asyncio.create_task(client.hold(EchoRequest(message=f"chain-cancel:{request_id}"), timeout=10))
    leaf_started = False
    try:
        until = time.monotonic() + 4
        while time.monotonic() < until:
            remaining = until - time.monotonic()
            receipt = await asyncio.wait_for(
                client.cancelled(EchoRequest(message=f"chain-cancel:{request_id}")), min(1, remaining)
            )
            if receipt.message == "chain-hold-started":
                leaf_started = True
                break
        else:
            raise AssertionError("Rust leaf did not report the matching hold handler started within 4s")
        call.cancel()
        # short-wait-ok: promptness bound: caller cancellation must land well before the call's 10 s deadline
        await asyncio.wait_for(call, 2)
    except asyncio.TimeoutError:
        if leaf_started:
            raise AssertionError("caller cancellation was not observed promptly")
        raise AssertionError("Rust leaf did not report the matching hold handler started within 4s")
    except asyncio.CancelledError:
        pass
    except RpcError as exc:
        if exc.code != StatusCode.CANCELLED:
            raise
    else:
        raise AssertionError("held chain call returned before caller cancellation")
    finally:
        if not call.done():
            call.cancel()
            try:
                await call
            except asyncio.CancelledError:
                pass
    await _chain_expect_leaf_cancel(client, before + 1)
    return "python-client->ts->rust"


async def run_chain_client(ticket_str: str) -> bool:
    cfg = AsterConfig(alpns=(RPC_ALPN,), relay_mode="disabled", bind_addr="127.0.0.1:0")
    node = await Node.start(cfg)
    try:
        t = AsterTicket.from_string(ticket_str)
        addr = NodeAddr(endpoint_id=t.endpoint_id, relay_url=None, direct_addresses=t.direct_addrs)
        conn = await asyncio.wait_for(net_client(node).connect_node_addr(addr, RPC_ALPN), 15)
    except Exception as exc:  # noqa: BLE001
        print(f"CASE connect FAIL {type(exc).__name__}: {exc}", flush=True)
        await node.close()
        return False
    client = create_client(InteropEcho, connection=conn)
    all_ok = True
    try:
        for name, run in (
            ("chain_trace_baggage", _chain_trace_baggage_case),
            ("chain_remaining_deadline", _chain_deadline_case),
            ("chain_cancel", _chain_cancel_case),
        ):
            try:
                result = await asyncio.wait_for(run(client), 25)
                print(f"CASE {name} OK server_lang={result}", flush=True)
            except Exception as exc:  # noqa: BLE001
                all_ok = False
                print(f"CASE {name} FAIL {type(exc).__name__}: {exc}", flush=True)
    finally:
        conn.close(0, b"done")
        await node.close()
    return all_ok


async def _wait_stdin_eof() -> None:
    loop = asyncio.get_running_loop()
    while await loop.run_in_executor(None, sys.stdin.readline):
        pass


async def serve() -> None:
    config = AsterConfig.from_env().replace(relay_mode="disabled", bind_addr="127.0.0.1:0")
    srv = AsterServer(services=[InteropEcho(), InteropSession], config=config)
    try:
        await srv.start()
        for _ in range(100):
            if srv.node.node_addr_info().direct_addresses:
                break
            await asyncio.sleep(0.05)
        print(f"ADDRESS {srv.address}", flush=True)
        serve_task = srv.serve()
        await _wait_stdin_eof()
    finally:
        if "serve_task" in locals():
            serve_task.cancel()
        await srv.close()


async def run_client(ticket_str: str) -> bool:
    cfg = AsterConfig(alpns=(RPC_ALPN,), relay_mode="disabled", bind_addr="127.0.0.1:0")
    node = await Node.start(cfg)
    try:
        t = AsterTicket.from_string(ticket_str)
        addr = NodeAddr(endpoint_id=t.endpoint_id, relay_url=None, direct_addresses=t.direct_addrs)
        conn = await asyncio.wait_for(net_client(node).connect_node_addr(addr, RPC_ALPN), 15)
    except Exception as exc:  # noqa: BLE001
        print(f"CASE connect FAIL {type(exc).__name__}: {exc}", flush=True)
        await node.close()
        return False

    client = create_client(InteropEcho, connection=conn)
    all_ok = True
    for name, req in cases("python"):
        try:
            reply = await asyncio.wait_for(client.echo(req), 15)
            server_lang = check(req, reply)
            print(f"CASE {name} OK server_lang={server_lang}", flush=True)
        except Exception as exc:  # noqa: BLE001
            all_ok = False
            print(f"CASE {name} FAIL {type(exc).__name__}: {exc}", flush=True)
    for name, coro in (
        ("server_stream", _stream_case(client)),
        ("client_stream", _ingest_case(client)),
        ("bidi", _chat_case(client)),
        ("deadline", _deadline_case(client)),
        ("cancel", _cancel_case(client)),
        ("session", _session_case(conn)),
        ("trace", _trace_case(client)),
    ):
        try:
            server_lang = await asyncio.wait_for(coro, 30)
            print(f"CASE {name} OK server_lang={server_lang}", flush=True)
        except Exception as exc:  # noqa: BLE001
            all_ok = False
            print(f"CASE {name} FAIL {type(exc).__name__}: {exc}", flush=True)
    conn.close(0, b"done")
    await node.close()
    return all_ok


class _ExposeApp(HttpHandler):
    async def handle(self, request):
        headers = [("x-server-lang", "python")]
        if request.method == "GET" and request.uri == "/hello":
            return HttpResponse(200, headers, "hello from python")
        if request.method == "POST" and request.uri == "/echo":

            async def echo():
                if isinstance(request.body, (bytes, bytearray)):
                    yield bytes(request.body)
                else:
                    async for chunk in request.body:
                        yield bytes(chunk)

            return HttpResponse(200, headers, echo())
        return HttpResponse(404, headers, b"")


async def expose_serve() -> None:
    config = AsterConfig(alpns=(EXPOSE_ALPN,), relay_mode="disabled", bind_addr="127.0.0.1:0")
    node = await Node.start(config)
    expose = node.expose()
    expose.expose_http(EXPOSE_SERVICE, _ExposeApp())  # no authorizer: admits every peer
    serving = asyncio.ensure_future(expose.serve([EXPOSE_ALPN]))
    info = node.node_addr_info()
    for _ in range(100):
        if info.direct_addresses:
            break
        await asyncio.sleep(0.05)
        info = node.node_addr_info()
    ticket = AsterTicket(endpoint_id=info.endpoint_id, direct_addrs=list(info.direct_addresses))
    print(f"ADDRESS {ticket.to_string()}", flush=True)
    try:
        await _wait_stdin_eof()
    finally:
        serving.cancel()
        await node.shutdown()


async def _expose_hello(conn) -> str:
    resp = await relay_http_stream(conn, EXPOSE_SERVICE, "GET", "/hello")
    lang = (resp.header("x-server-lang") or b"").decode()
    body = await resp.read_body()
    if resp.status != 200:
        raise AssertionError(f"status {resp.status}")
    if not lang or body != f"hello from {lang}".encode():
        raise AssertionError(f"lang {lang!r} body {body!r}")
    return lang


async def _expose_echo(conn) -> str:
    """Three-chunk streamed upload, read back interleaved with the upload."""
    chunks = [bytes([ord("a") + i]) * (4000 + i) for i in range(3)]
    gate = [asyncio.Event() for _ in chunks]

    async def upload():
        for i, c in enumerate(chunks):
            yield c
            # Hold the next chunk until the response has produced this one.
            await asyncio.wait_for(gate[i].wait(), 10)

    resp = await relay_http_stream(conn, EXPOSE_SERVICE, "POST", "/echo", body=upload())
    if resp.status != 200:
        raise AssertionError(f"status {resp.status}")
    lang = (resp.header("x-server-lang") or b"").decode()
    if not lang:
        raise AssertionError("missing x-server-lang header")
    got = bytearray()
    sent = 0
    while (chunk := await resp.body.read()) is not None:
        got += chunk
        while sent < len(chunks) and len(got) >= sum(len(c) for c in chunks[: sent + 1]):
            gate[sent].set()
            sent += 1
    if bytes(got) != b"".join(chunks):
        raise AssertionError(f"echo mismatch (len {len(got)} vs {sum(len(c) for c in chunks)})")
    return lang


async def expose_client(ticket_str: str) -> bool:
    cfg = AsterConfig(alpns=(EXPOSE_ALPN,), relay_mode="disabled", bind_addr="127.0.0.1:0")
    node = await Node.start(cfg)
    try:
        t = AsterTicket.from_string(ticket_str)
        addr = NodeAddr(endpoint_id=t.endpoint_id, relay_url=None, direct_addresses=t.direct_addrs)
        conn = await asyncio.wait_for(net_client(node).connect_node_addr(addr, EXPOSE_ALPN), 15)
    except Exception as exc:  # noqa: BLE001
        print(f"CASE connect FAIL {type(exc).__name__}: {exc}", flush=True)
        await node.close()
        return False
    all_ok = True
    for name, coro in (("expose_hello", _expose_hello(conn)), ("expose_echo_stream", _expose_echo(conn))):
        try:
            print(f"CASE {name} OK server_lang={await asyncio.wait_for(coro, 20)}", flush=True)
        except Exception as exc:  # noqa: BLE001
            all_ok = False
            print(f"CASE {name} FAIL {type(exc).__name__}: {exc}", flush=True)
    conn.close(0, b"done")
    await node.close()
    return all_ok



async def campaign_client(ticket_str: str) -> None:
    """Persistent adapter load; the measured 128-slot queue is not an SDK queue."""
    from aster import observability
    from aster.logging import AsterLogHandler

    endpoint = os.environ["ASTER_CAMPAIGN_COLLECTOR"]
    observability.init(from_env=False, service_name="python-campaign", traces=False,
                       metrics=False, otlp_logs=True, otlp_endpoint=endpoint,
                       log_bridge=False, config_toml="otlp_timeout_ms = 1000")
    logger = logging.getLogger("app.campaign.python")
    logger.handlers = [AsterLogHandler()]
    logger.setLevel(logging.INFO)
    logger.propagate = False
    node = None
    conn = None
    client = None
    generation = 0
    settings = {}
    def reply(value):
        print(json.dumps(value, separators=(",", ":")), flush=True)

    async def replace(ticket):
        nonlocal conn, client
        if conn is not None:
            conn.close(0, b"replace peer")
        t = AsterTicket.from_string(ticket)
        addr = NodeAddr(endpoint_id=t.endpoint_id, relay_url=None, direct_addresses=t.direct_addrs)
        # short-wait-ok: campaign mode: connect expiry is a classified outcome shared by every language leg
        conn = await asyncio.wait_for(net_client(node).connect_node_addr(addr, RPC_ALPN), 2)
        client = create_client(InteropEcho, connection=conn)

    async def flush(command, result):
        try:
            await asyncio.to_thread(observability.flush, 1)
            result["flush_status"] = "ok"
        except RuntimeError as exc:
            message = str(exc)
            category = "timeout" if "telemetry flush timed out" in message else "sdk" if "telemetry SDK error:" in message else "other"
            result["flush_status"] = category
            if command.get("phase", "healthy") != "fault" or category not in command.get("expected_flush_errors", []):
                result["unexpected_errors"] += 1

    async def window(command):
        started = time.monotonic()
        duration = float(command.get("duration_s", 5))
        if not 0 < duration <= 300:
            raise ValueError("duration_s must be in (0,300]")
        phase = command.get("phase", "healthy")
        if phase not in ("healthy", "fault"):
            raise ValueError("phase must be healthy or fault")
        expected = command.get("expected_errors", []) if phase == "fault" else []
        if not isinstance(expected, list) or not all(isinstance(x, str) for x in expected):
            raise ValueError("expected_errors must be status-name strings")
        slow = float(command.get("slow_ms", 0)) / 1000
        if not 0 <= slow <= 1:
            raise ValueError("slow_ms must be in [0,1000]")
        window_id = command.get("window_id", "window")
        if not isinstance(window_id, str) or len(window_id.encode()) > 128:
            raise ValueError("window_id must be a string of at most 128 bytes")
        marker = f"campaign:python:{window_id};"
        result = dict(phase="campaign_window", window_phase=phase, generation=generation, log_marker=marker,
                      completed=0, attempted=0, unary=0, stream=0, cancel=0, unexpected_errors=0,
                      integrity_failures=0, active_calls=0, queue_capacity=128,
                      queue_scope="adapter_admission", queue_depth=0, queue_high_water=0,
                      latency_histogram=[0] * 27, first_success_elapsed_ms=None,
                      last_success_elapsed_ms=None, error_classes={}, app_log_emitted=0)
        queue = asyncio.Queue(maxsize=128)
        payload = bytes((i * 131 + i // 8) % 251 for i in range(16384))
        async def request(sequence):
            kind = ("unary", "stream", "unary", "cancel")[sequence % 4]
            req = EchoRequest(message="campaign", count=5 if kind == "stream" else 41, data=payload)
            if kind == "cancel":
                task = asyncio.create_task(client.hold(req, timeout=10))
                await asyncio.sleep(.05)
                if task.done():
                    await task
                    raise AssertionError("hold returned before caller cancellation")
                task.cancel()
                try:
                    await task
                except asyncio.CancelledError:
                    return kind
                raise AssertionError("hold accepted caller cancellation as success")
            if kind == "unary":
                check(req, await client.echo(req, timeout=2))
            else:
                count = 0
                async for r in client.generate(req, timeout=2):
                    if r.count != count or r.message != req.message or bytes(r.data) != payload or not r.server_lang:
                        raise AssertionError("stream integrity mismatch")
                    count += 1
                    if slow:
                        await asyncio.sleep(slow)
                if count != 5:
                    raise AssertionError("stream length mismatch")
            return kind
        async def worker():
            while True:
                sequence = await queue.get()
                began = time.monotonic()
                result["active_calls"] += 1
                status = "ok"
                success = False
                try:
                    # short-wait-ok: campaign mode: expiry is a classified outcome (local_timeout) shared by every language leg
                    kind = await asyncio.wait_for(request(sequence), 3)
                    result[kind] += 1
                    success = True
                    elapsed = (time.monotonic() - started) * 1000
                    # Recovery timing requires receipt from the peer.
                    if kind != "cancel":
                        if result["first_success_elapsed_ms"] is None:
                            result["first_success_elapsed_ms"] = elapsed
                        result["last_success_elapsed_ms"] = elapsed
                except AssertionError:
                    status = "integrity"
                    result["integrity_failures"] += 1
                except Exception as exc:
                    status = exc.code.name.lower() if isinstance(exc, RpcError) else "local_timeout" if isinstance(exc, TimeoutError) else "local_error"
                    result["error_classes"][status] = result["error_classes"].get(status, 0) + 1
                    if status not in expected or status not in ("unavailable", "deadline_exceeded", "cancelled"):
                        result["unexpected_errors"] += 1
                finally:
                    result["active_calls"] -= 1
                    result["attempted"] += 1
                    if success:
                        result["completed"] += 1
                        micros = max(1, int((time.monotonic() - began) * 1_000_000))
                        result["latency_histogram"][min(26, (micros - 1).bit_length())] += 1
                        logger.info("%s request %s %s", marker, sequence, status)
                        result["app_log_emitted"] += 1
                    queue.task_done()
        workers = [asyncio.create_task(worker()) for _ in range(4)]
        sequence = 0
        try:
            while time.monotonic() - started < duration:
                try:
                    queue.put_nowait(sequence)
                    sequence += 1
                    result["queue_high_water"] = max(result["queue_high_water"], queue.qsize())
                except asyncio.QueueFull:
                    pass
                await asyncio.sleep(.001)
        finally:
            while not queue.empty():
                queue.get_nowait()
                queue.task_done()
            try:
                # short-wait-ok: campaign mode: drain expiry is counted as an unexpected error, shared by every language leg
                await asyncio.wait_for(queue.join(), 4)
            except TimeoutError:
                result["unexpected_errors"] += 1
            for task in workers:
                task.cancel()
            await asyncio.gather(*workers, return_exceptions=True)
        result["queue_depth"] = queue.qsize()
        result["latency_histogram"] = dict(unit="us", upper_bounds_powers_of_two=True, counts=result["latency_histogram"])
        await flush(command, result)
        return result

    try:
        node = await Node.start(AsterConfig(alpns=(RPC_ALPN,), relay_mode="disabled", bind_addr="127.0.0.1:0"))
        await replace(ticket_str)
        identity = str(node.node_addr_info().endpoint_id)
        reply(dict(phase="campaign_ready", owner_identity=identity, pid=os.getpid(), generation=generation))
        loop = asyncio.get_running_loop()
        while line := await loop.run_in_executor(None, sys.stdin.readline):
            try:
                command = json.loads(line)
                op = command.get("command", command.get("cmd", command.get("op")))
                if op == "configure":
                    settings.update({k: v for k, v in command.items() if k not in ("command", "cmd", "op")})
                    result = dict(phase="campaign_configured")
                elif op in ("run", "baseline"):
                    result = await window(settings | command)
                elif op == "replace_peer":
                    await replace(command["ticket"])
                    generation = command["generation"]
                    result = dict(phase="campaign_peer_replaced", generation=generation)
                elif op in ("pause", "stop"):
                    result = dict(phase="campaign_" + op, active_calls=0, unexpected_errors=0)
                    await flush(settings | command, result)
                else:
                    raise ValueError("unknown campaign command")
                reply(result | dict(owner_identity=identity, pid=os.getpid()))
                if op == "stop":
                    break
            except Exception as exc:
                reply(dict(phase="campaign_error", error=type(exc).__name__, detail=str(exc)[:256]))
    finally:
        if conn is not None:
            conn.close(0, b"done")
        if node is not None:
            await node.close()
        await asyncio.to_thread(observability.shutdown, 1)


def main() -> int:
    if len(sys.argv) == 3 and sys.argv[1] == "campaign-client":
        asyncio.run(campaign_client(sys.argv[2]))
        return 0
    print(f"CONTRACT_ID {contract_id_from_service(InteropEcho)}", flush=True)
    print(f"SESSION_CONTRACT_ID {contract_id_from_service(InteropSession)}", flush=True)
    args = sys.argv[1:]
    if args == ["expose-server"]:
        asyncio.run(expose_serve())
        return 0
    if len(args) == 2 and args[0] == "expose-client":
        return 0 if asyncio.run(expose_client(args[1])) else 1
    if args == ["contract"]:
        return 0
    if args == ["server"]:
        asyncio.run(serve())
        return 0
    if len(args) == 2 and args[0] == "chain-client":
        return 0 if asyncio.run(run_chain_client(args[1])) else 1
    if len(args) == 2 and args[0] == "client":
        return 0 if asyncio.run(run_client(args[1])) else 1
    print("usage: interop_echo.py contract | server | chain-client <ts-ticket> | client <ticket> | expose-server | expose-client <ticket>", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
