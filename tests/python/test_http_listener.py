"""Real native authenticated HTTPS/WT serving through Python dispatch.

Install bindings/python/requirements-test.txt; the WT tests deliberately fail
collection if the test client is missing instead of silently skipping coverage.
"""

from __future__ import annotations

import asyncio
import datetime
import socket
import ssl
from dataclasses import dataclass
from typing import AsyncIterator

import pytest
from aioquic.asyncio import QuicConnectionProtocol, connect
from aioquic.h3.connection import H3_ALPN, H3Connection
from aioquic.h3.events import HeadersReceived, WebTransportStreamDataReceived
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import StreamDataReceived
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import NameOID

from aster import AsterServer, RPCAuthRequest, RPCAuthResponse, bidi_stream, client_stream, rpc, server_stream, service, wire_type
from aster.capabilities import any_of
from aster.codec import ForyCodec
from aster.interceptors.base import CallContext
from aster.status import StatusCode
from aster.upcalls import UpcallPolicy, UpcallRuntime
from aster.wire import END_STREAM, HEADER, TRAILER, RpcStatus, StreamHeader, decode_frame, encode_frame

pytestmark = [pytest.mark.network, pytest.mark.timeout(30)]


@wire_type("test.http/Request")
@dataclass
class Request:
    text: str = ""


@wire_type("test.http/Response")
@dataclass
class Response:
    text: str = ""
    peer: str = ""
    role: str = ""
    iroh: bool = False


@service(name="HTTPTest", version=1)
class Echo:
    def __init__(self):
        self.calls = 0
        self.started = asyncio.Event()
        self.cancelled = asyncio.Event()
        self.retained_contexts = []

    @rpc()
    async def echo(self, request: Request, ctx: CallContext) -> Response:
        self.calls += 1
        return Response(request.text, ctx.peer or "", ctx.attributes.get("aster.role", ""), ctx.iroh_authenticated)

    @rpc(requires=any_of("operator"))
    async def secret(self, request: Request, ctx: CallContext) -> Response:
        return await self.echo(request, ctx)

    @rpc()
    async def retained(self, request: Request, ctx: CallContext) -> Response:
        self.retained_contexts.append(ctx)
        return await self.echo(request, ctx)

    @rpc()
    async def hold(self, request: Request) -> Response:
        self.started.set()
        try:
            await asyncio.Event().wait()
        finally:
            self.cancelled.set()
        return Response()

    @server_stream()
    async def count(self, request: Request) -> AsyncIterator[Response]:
        yield Response(request.text + "1")
        yield Response(request.text + "2")

    @client_stream()
    async def collect(self, requests: AsyncIterator[Request]) -> Response:
        return Response("".join([request.text async for request in requests]))

    @bidi_stream()
    async def chat(self, requests: AsyncIterator[Request]) -> AsyncIterator[Response]:
        async for request in requests:
            yield Response(request.text.upper())


@pytest.fixture(scope="module")
def pem():
    key = ec.generate_private_key(ec.SECP256R1())
    subject = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "localhost")])
    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (
        x509.CertificateBuilder().subject_name(subject).issuer_name(subject)
        .public_key(key.public_key()).serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(minutes=1))
        .not_valid_after(now + datetime.timedelta(days=7))
        .add_extension(x509.SubjectAlternativeName([x509.DNSName("localhost")]), False)
        .sign(key, hashes.SHA256())
    )
    return (
        cert.public_bytes(serialization.Encoding.PEM),
        key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()),
    )


@pytest.fixture
async def running_server():
    echo = Echo()
    async with AsterServer(services=[echo]) as server:
        yield server, echo


def auth(request: RPCAuthRequest) -> RPCAuthResponse:
    assert isinstance(request, RPCAuthRequest)
    assert request.service == "HTTPTest"
    token = request.metadata.get("authorization")
    if token == "Bearer alice":
        return RPCAuthResponse("alice", {"aster.role": "operator"})
    if token == "Bearer bob":
        return RPCAuthResponse("bob", {"aster.role": "reader"})
    return RPCAuthResponse()


async def start(server, pem, **options):
    cert, key = pem
    return await server.start_http(
        authenticator=options.pop("authenticator", auth),
        certificate_pem=cert, private_key_pem=key, **options,
    )


CODEC = ForyCodec(types=[Request, Response])


async def open_http(addr, method="echo", token=None, requests=None, *, service="HTTPTest", session_id=None):
    host, port = addr.rsplit(":", 1)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE  # Ephemeral test-only self-signed PEM.
    reader, writer = await asyncio.open_connection(host, int(port), ssl=context)
    requests = requests or [Request("hello")]
    body = b"".join(encode_frame(CODEC.encode(request), END_STREAM if i == len(requests) - 1 else 0)
                    for i, request in enumerate(requests))
    headers = (
        f"POST /aster/{service}/{method} HTTP/1.1\r\nHost: localhost\r\n"
        "Content-Type: application/aster-frames\r\nConnection: close\r\n"
        f"Content-Length: {len(body)}\r\n"
    )
    if token is not None:
        headers += f"Authorization: Bearer {token}\r\n"
    if session_id is not None:
        headers += f"Aster-Session-Id: {session_id}\r\n"
    writer.write(headers.encode() + b"\r\n" + body)
    await writer.drain()
    return reader, writer


def response(body):
    values = []
    status = None
    while body:
        payload, flags, used = decode_frame(body)
        body = body[used:]
        if flags & TRAILER:
            status = RpcStatus.decode(payload)
        else:
            values.append(CODEC.decode(payload, Response))
    assert status is not None, "native response omitted its terminal trailer"
    return values, status.code


async def read_http(reader):
    head = await reader.readuntil(b"\r\n\r\n")
    assert head.startswith(b"HTTP/1.1 200"), head
    fields = {}
    for line in head.split(b"\r\n")[1:]:
        if b":" in line:
            name, value = line.split(b":", 1)
            fields[name.lower()] = value.strip().lower()
    if fields.get(b"transfer-encoding") == b"chunked":
        body = bytearray()
        while True:
            length = int((await reader.readline()).split(b";", 1)[0], 16)
            if not length:
                await reader.readuntil(b"\r\n")
                break
            body += await reader.readexactly(length)
            assert await reader.readexactly(2) == b"\r\n"
        return response(bytes(body))
    if b"content-length" in fields:
        return response(await reader.readexactly(int(fields[b"content-length"])))
    return response(await reader.read())


async def request_http(addr, method="echo", token=None, requests=None, **kwargs):
    reader, writer = await open_http(addr, method, token, requests, **kwargs)
    try:
        return await asyncio.wait_for(read_http(reader), 5)
    finally:
        writer.close()
        await writer.wait_closed()


def assert_released(addr, quic=False):
    host, port = addr.rsplit(":", 1)
    with socket.socket() as tcp:
        tcp.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        tcp.bind((host, int(port)))
        tcp.listen()
    if quic:
        with socket.socket(type=socket.SOCK_DGRAM) as udp:
            udp.bind((host, int(port)))


async def test_https_auth_gate3_principals_and_bound_readiness(running_server, pem):
    server, echo = running_server
    async with await start(server, pem, webtransport=True) as listener:
        assert listener.tcp_addr == listener.quic_addr
        assert not listener.tcp_addr.endswith(":0")
        assert (await request_http(listener.tcp_addr))[1] == StatusCode.UNAUTHENTICATED
        assert echo.calls == 0
        values, code = await request_http(listener.tcp_addr, "secret", "alice")
        assert code == StatusCode.OK and values == [Response("hello", "alice", "operator", False)]
        assert (await request_http(listener.tcp_addr, "secret", "bob"))[1] == StatusCode.PERMISSION_DENIED
        values, code = await request_http(listener.tcp_addr, "echo", "bob")
        assert code == StatusCode.OK and values == [Response("hello", "bob", "reader", False)]
        # A new address/principal must not inherit attributes from connid=0.
        assert (await request_http(listener.tcp_addr, "secret", "bob"))[1] == StatusCode.PERMISSION_DENIED
        assert echo.calls == 2
        for method, requests, expected in (
            ("count", [Request("v")], [Response("v1"), Response("v2")]),
            ("collect", [Request("a"), Request("b")], [Response("ab")]),
            ("chat", [Request("a"), Request("b")], [Response("A"), Response("B")]),
        ):
            values, code = await request_http(listener.tcp_addr, method, "alice", requests)
            assert code == StatusCode.OK and values == expected
    assert_released(listener.tcp_addr, True)


async def test_auth_bounds_failure_and_timeout_are_fail_closed(running_server, pem):
    server, echo = running_server
    entered, cancelled = asyncio.Event(), asyncio.Event()

    async def slow(request):
        entered.set()
        try:
            await asyncio.Event().wait()
        finally:
            cancelled.set()

    runtime = UpcallRuntime(default_policy=UpcallPolicy(timeout=1.0, failure_mode="fail_open"))
    try:
        async with await start(server, pem, authenticator=slow, upcall_runtime=runtime, max_pending_auth=1) as listener:
            first = asyncio.create_task(request_http(listener.tcp_addr, token="alice"))
            await asyncio.wait_for(entered.wait(), 2)
            assert (await request_http(listener.tcp_addr, token="alice"))[1] == StatusCode.RESOURCE_EXHAUSTED
            assert (await first)[1] == StatusCode.UNAVAILABLE
            await asyncio.wait_for(cancelled.wait(), 2)
            assert echo.calls == 0
            await runtime.aclose()
            assert (await request_http(listener.tcp_addr, token="alice"))[1] == StatusCode.UNAVAILABLE
        async with await start(server, pem, max_auth_payload_bytes=32) as listener:
            assert (await request_http(listener.tcp_addr, token="alice"))[1] == StatusCode.RESOURCE_EXHAUSTED
        async with await start(server, pem, authenticator=lambda req: RPCAuthResponse("")) as listener:
            assert (await request_http(listener.tcp_addr, token="alice"))[1] == StatusCode.UNAVAILABLE
        async with await start(server, pem, authenticator=lambda req: RPCAuthResponse("alice", {"huge": "x" * 1024}), max_auth_payload_bytes=512) as listener:
            assert (await request_http(listener.tcp_addr, token="alice"))[1] == StatusCode.UNAUTHENTICATED
        assert echo.calls == 0
    finally:
        await runtime.aclose()


async def test_cancel_stop_then_retry_and_parent_close_join(running_server, pem):
    server, echo = running_server
    listener = await start(server, pem)
    assert listener.quic_addr is None
    reader, writer = await open_http(listener.tcp_addr, "hold", "alice")
    try:
        await asyncio.wait_for(echo.started.wait(), 2)
        stop = asyncio.create_task(listener.stop(10))
        await asyncio.sleep(0.02)
        stop.cancel()
        with pytest.raises(asyncio.CancelledError):
            await stop
        await asyncio.wait_for(listener.stop(), 3)
        await asyncio.wait_for(echo.cancelled.wait(), 2)
        assert_released(listener.tcp_addr)
        await listener.stop()
    finally:
        writer.close()
        await writer.wait_closed()
    other = await start(server, pem, webtransport=True)
    await server.close()
    assert_released(other.tcp_addr, True)
    await other.aclose()


async def test_invalid_config_and_partial_bind_release(running_server, pem):
    server, _ = running_server
    for config in ({"max_pending_auth": 0}, {"max_auth_payload_bytes": -1}, {"authenticator": None}, {"bind_addr": ""}):
        with pytest.raises((ValueError, TypeError)):
            await start(server, pem, **config)
    with pytest.raises(ValueError):
        await start(server, (b"invalid", b"invalid"))
    with socket.socket(type=socket.SOCK_DGRAM) as occupied:
        occupied.bind(("127.0.0.1", 0))
        addr = f"127.0.0.1:{occupied.getsockname()[1]}"
        with pytest.raises(ValueError):
            await start(server, pem, bind_addr=addr, webtransport=True)
        assert_released(addr)
    listener = await start(server, pem)
    for grace in (-1, float("inf"), float("nan"), 1e20):
        with pytest.raises(ValueError):
            await listener.stop(grace)
    await listener.aclose()


async def test_server_close_winning_python_start_continuation_cleans_auth(running_server, pem, monkeypatch):
    from aster import http
    from aster.server import ServerError

    server, _ = running_server
    ready, release = asyncio.Queue(), asyncio.Event()
    runtime = UpcallRuntime()
    original = http._start_http

    async def paused(*args, **kwargs):
        listener = await original(*args, **kwargs)
        ready.put_nowait(listener.tcp_addr)
        await release.wait()
        return listener

    monkeypatch.setattr(http, "_start_http", paused)
    startup = asyncio.create_task(start(server, pem, upcall_runtime=runtime))
    try:
        addr = await asyncio.wait_for(ready.get(), 3)
        await server.close()
        assert_released(addr)
        release.set()
        with pytest.raises(ServerError, match="closed during HTTP startup"):
            await startup
        assert runtime._native.channel_count() == 0
    finally:
        release.set()
        if not startup.done():
            startup.cancel()
        await asyncio.gather(startup, return_exceptions=True)
        await runtime.aclose()


async def test_terminal_responses_end_with_retained_python_context(running_server, pem):
    server, echo = running_server
    async with await start(server, pem, webtransport=True) as listener:
        values, code = await request_http(listener.tcp_addr, "retained", "alice")
        assert code == StatusCode.OK and values == [Response("hello", "alice", "operator", False)]
        assert len(echo.retained_contexts) == 1
        host, port = listener.quic_addr.rsplit(":", 1)
        config = QuicConfiguration(is_client=True, alpn_protocols=H3_ALPN, verify_mode=ssl.CERT_NONE)
        async with connect(host, int(port), configuration=config, create_protocol=WTClient) as client:
            session = await client.session(listener.quic_addr)
            values, code = await client.result(client.call(session, "retained", "alice"))
            assert code == StatusCode.OK and values == [Response("wt", "alice", "operator", False)]
        assert len(echo.retained_contexts) == 2
    assert_released(listener.tcp_addr, True)


class WTClient(QuicConnectionProtocol):
    """Test-only H3 client using aioquic's documented HTTP/3 event API."""

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.http = H3Connection(self._quic, enable_webtransport=True)
        self.headers = asyncio.Queue()
        self.streams = {}

    def quic_event_received(self, event):
        # A client-initiated WT bidi stream is already associated with its
        # session. Its reverse direction carries raw application bytes, without
        # another WT prefix; route it before aioquic's generic H3 frame parser.
        if isinstance(event, StreamDataReceived) and event.stream_id in self.streams:
            self.streams[event.stream_id].put_nowait(event)
            return
        for event in self.http.handle_event(event):
            if isinstance(event, HeadersReceived):
                self.headers.put_nowait(event)
            elif isinstance(event, WebTransportStreamDataReceived):
                self.streams[event.stream_id].put_nowait(event)

    async def session(self, authority):
        session = self._quic.get_next_available_stream_id()
        self.http.send_headers(session, [
            (b":method", b"CONNECT"), (b":scheme", b"https"),
            (b":authority", authority.encode()), (b":path", b"/aster/wt"),
            (b":protocol", b"webtransport"),
            (b"sec-webtransport-http3-draft", b"draft02"),
            # Session credentials must not authorize a stream missing metadata.
            (b"authorization", b"Bearer alice"),
        ])
        self.transmit()
        headers = await asyncio.wait_for(self.headers.get(), 3)
        assert dict(headers.headers)[b":status"] == b"200"
        return session

    def call(self, session, method="echo", token=None, requests=None, *, service="HTTPTest", session_id=0):
        stream = self.http.create_webtransport_stream(session)
        self.streams[stream] = asyncio.Queue()
        header = StreamHeader(service=service, method=method, version=1, session_id=session_id,
            metadata_keys=["authorization"] if token else [],
            metadata_values=[f"Bearer {token}"] if token else [])
        requests = requests or [Request("wt")]
        body = b"".join(encode_frame(CODEC.encode(request), END_STREAM if i == len(requests) - 1 else 0)
                        for i, request in enumerate(requests))
        self._quic.send_stream_data(stream,
            encode_frame(header.encode(), HEADER) + body,
            end_stream=True)
        self.transmit()
        return stream

    async def result(self, stream):
        data = bytearray()
        while True:
            event = await asyncio.wait_for(self.streams[stream].get(), 3)
            data += event.data
            if getattr(event, "end_stream", getattr(event, "stream_ended", False)):
                return response(bytes(data))


async def test_webtransport_per_rpc_auth_stop_and_listener_shutdown(running_server, pem):
    server, echo = running_server
    listener = await start(server, pem, webtransport=True)
    host, port = listener.quic_addr.rsplit(":", 1)
    config = QuicConfiguration(is_client=True, alpn_protocols=H3_ALPN, verify_mode=ssl.CERT_NONE)
    async with connect(host, int(port), configuration=config, create_protocol=WTClient) as client:
        session = await client.session(listener.quic_addr)
        assert (await client.result(client.call(session)))[1] == StatusCode.UNAUTHENTICATED
        values, code = await client.result(client.call(session, "secret", "alice"))
        assert code == StatusCode.OK and values == [Response("wt", "alice", "operator", False)]
        assert (await client.result(client.call(session, "secret", "bob")))[1] == StatusCode.PERMISSION_DENIED
        for method, requests, expected in (
            ("count", [Request("v")], [Response("v1"), Response("v2")]),
            ("collect", [Request("a"), Request("b")], [Response("ab")]),
            ("chat", [Request("a"), Request("b")], [Response("A"), Response("B")]),
        ):
            values, code = await client.result(client.call(session, method, "alice", requests))
            assert code == StatusCode.OK and values == expected
        hold = client.call(session, "hold", "alice")
        await asyncio.wait_for(echo.started.wait(), 2)
        client._quic.stop_stream(hold, 1)
        client.transmit()
        await asyncio.wait_for(echo.cancelled.wait(), 2)
        echo.started.clear()
        echo.cancelled.clear()
        client.call(session, "hold", "alice")
        await asyncio.wait_for(echo.started.wait(), 2)
        await asyncio.wait_for(listener.stop(), 5)
        await asyncio.wait_for(echo.cancelled.wait(), 2)
        assert_released(listener.tcp_addr, True)
    await listener.aclose()
