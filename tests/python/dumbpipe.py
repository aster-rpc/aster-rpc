"""
Python implementation of dumbpipe — network-level compatible with the Rust version.

Protocol:
  - ALPN: b"DUMBPIPEV0"
  - The connecting side opens a bi-directional QUIC stream and sends a 5-byte
    handshake (b"hello") before any payload data.
  - The listening side accepts the bi-stream, consumes the 5-byte handshake,
    then pipes data bidirectionally.

Modes:
  - listen / connect: pipe stdin/stdout over a single bi-stream
  - listen_tcp / connect_tcp: forward TCP connections over bi-streams
  - listen_unix / connect_unix: forward Unix socket connections over bi-streams
"""

import asyncio
import sys
from typing import Optional

from aster_python import (
    NetClient,
    IrohConnection,
    IrohSendStream,
    IrohRecvStream,
    NodeAddr,
    create_endpoint,
)

# Must match dumbpipe Rust crate
ALPN = b"DUMBPIPEV0"
HANDSHAKE = b"hello"

COPY_BUF_SIZE = 64 * 1024


# ---------------------------------------------------------------------------
# Core helpers
# ---------------------------------------------------------------------------

async def send_handshake(send: IrohSendStream) -> None:
    """Send the dumbpipe handshake on a freshly opened bi-stream."""
    await send.write_all(HANDSHAKE)


async def recv_handshake(recv: IrohRecvStream) -> None:
    """Consume and validate the dumbpipe handshake on an accepted bi-stream.

    Raises ValueError if the handshake doesn't match.
    """
    data = await recv.read_exact(len(HANDSHAKE))
    if data != HANDSHAKE:
        raise ValueError(f"unexpected handshake: {data!r}")


async def pipe_streams(
    send: IrohSendStream,
    recv: IrohRecvStream,
    local_reader,
    local_writer,
) -> None:
    """Bidirectionally copy data between QUIC streams and local reader/writer.

    local_reader: an asyncio StreamReader (or anything with .read(n))
    local_writer: an asyncio StreamWriter (or anything with .write()/drain()/close())
    """

    async def quic_to_local():
        try:
            while True:
                chunk = await recv.read(COPY_BUF_SIZE)
                if chunk is None:
                    break
                local_writer.write(chunk)
                await local_writer.drain()
        except Exception:
            pass
        finally:
            try:
                local_writer.close()
            except Exception:
                pass

    async def local_to_quic():
        try:
            while True:
                chunk = await local_reader.read(COPY_BUF_SIZE)
                if not chunk:
                    break
                await send.write_all(chunk)
        except Exception:
            pass
        finally:
            try:
                await send.finish()
            except Exception:
                pass

    t_quic_to_local = asyncio.create_task(quic_to_local())
    t_local_to_quic = asyncio.create_task(local_to_quic())
    # Let both directions run to completion.
    # Using FIRST_COMPLETED here can cancel the reverse direction too early,
    # dropping response bytes in request/response forwarding scenarios.
    await asyncio.gather(t_quic_to_local, t_local_to_quic, return_exceptions=True)
    # Ensure finish / close are attempted
    try:
        await send.finish()
    except Exception:
        pass
    try:
        local_writer.close()
    except Exception:
        pass


# ---------------------------------------------------------------------------
# Listen / Connect  (stdin / stdout)
# ---------------------------------------------------------------------------

async def listen(secret_key: Optional[str] = None) -> None:
    """Listen for a single incoming connection and pipe stdin/stdout."""
    ep = await create_endpoint(ALPN)
    addr = ep.endpoint_addr_info()
    # Print the node address as a simple serialized ticket (base32 not needed for Python-only;
    # for interop we just print the NodeAddr bytes as hex so the Rust side can connect by ticket).
    # For true interop with the Rust CLI we'd need postcard-serialized EndpointTicket in base32.
    # Here we print the node_id for programmatic use.
    print(addr.endpoint_id, file=sys.stderr, flush=True)

    conn = await ep.accept()
    send, recv = await conn.accept_bi()
    await recv_handshake(recv)

    reader = asyncio.StreamReader()
    protocol = await asyncio.get_event_loop().connect_read_pipe(
        lambda: asyncio.StreamReaderProtocol(reader), sys.stdin.buffer
    )
    w_transport, w_protocol = await asyncio.get_event_loop().connect_write_pipe(
        asyncio.BaseProtocol, sys.stdout.buffer
    )

    class StdoutWriter:
        def write(self, data):
            w_transport.write(data)
        async def drain(self):
            pass
        def close(self):
            w_transport.close()

    await pipe_streams(send, recv, reader, StdoutWriter())
    await ep.close()


async def connect(node_addr: NodeAddr) -> None:
    """Connect to a listening dumbpipe endpoint and pipe stdin/stdout."""
    ep = await create_endpoint(ALPN)
    conn = await ep.connect_node_addr(node_addr, ALPN)
    send, recv = await conn.open_bi()
    await send_handshake(send)

    reader = asyncio.StreamReader()
    await asyncio.get_event_loop().connect_read_pipe(
        lambda: asyncio.StreamReaderProtocol(reader), sys.stdin.buffer
    )
    w_transport, _ = await asyncio.get_event_loop().connect_write_pipe(
        asyncio.BaseProtocol, sys.stdout.buffer
    )

    class StdoutWriter:
        def write(self, data):
            w_transport.write(data)
        async def drain(self):
            pass
        def close(self):
            w_transport.close()

    await pipe_streams(send, recv, reader, StdoutWriter())
    await ep.close()


# ---------------------------------------------------------------------------
# Listen-TCP / Connect-TCP
# ---------------------------------------------------------------------------

async def listen_tcp(host: str, port: int) -> NetClient:
    """Listen for incoming QUIC connections and forward each bi-stream to a TCP target.

    Returns the endpoint so the caller can manage its lifetime.
    """
    ep = await create_endpoint(ALPN)
    addr = ep.endpoint_addr_info()
    print(addr.endpoint_id, file=sys.stderr, flush=True)

    async def handle_connection(conn: IrohConnection):
        while True:
            try:
                send, recv = await conn.accept_bi()
            except Exception:
                break
            await recv_handshake(recv)
            asyncio.create_task(_forward_to_tcp(send, recv, host, port))

    async def accept_loop():
        while True:
            try:
                conn = await ep.accept()
                asyncio.create_task(handle_connection(conn))
            except Exception:
                break

    asyncio.create_task(accept_loop())
    return ep


async def _forward_to_tcp(
    send: IrohSendStream,
    recv: IrohRecvStream,
    host: str,
    port: int,
) -> None:
    try:
        tcp_reader, tcp_writer = await asyncio.open_connection(host, port)
        await pipe_streams(send, recv, tcp_reader, tcp_writer)
    except Exception:
        pass


async def connect_tcp(
    listen_addr: str,
    listen_port: int,
    remote_addr: NodeAddr,
) -> tuple:
    """Connect to a remote dumbpipe endpoint and listen on a local TCP port.

    Each incoming TCP connection opens a new bi-stream on the QUIC connection.
    Returns (endpoint, server) so the caller can manage their lifetime.
    """
    ep = await create_endpoint(ALPN)
    conn = await ep.connect_node_addr(remote_addr, ALPN)

    async def handle_tcp(tcp_reader, tcp_writer):
        try:
            send, recv = await conn.open_bi()
            await send_handshake(send)
            await pipe_streams(send, recv, tcp_reader, tcp_writer)
        except Exception:
            pass

    server = await asyncio.start_server(handle_tcp, listen_addr, listen_port)
    return ep, server


# ---------------------------------------------------------------------------
# Listen-Unix / Connect-Unix
# ---------------------------------------------------------------------------

async def listen_unix(socket_path: str) -> NetClient:
    """Listen for incoming QUIC connections and forward each bi-stream to a Unix socket.

    Returns the endpoint so the caller can manage its lifetime.
    """
    ep = await create_endpoint(ALPN)
    addr = ep.endpoint_addr_info()
    print(addr.endpoint_id, file=sys.stderr, flush=True)

    async def handle_connection(conn: IrohConnection):
        while True:
            try:
                send, recv = await conn.accept_bi()
            except Exception:
                break
            await recv_handshake(recv)
            asyncio.create_task(_forward_to_unix(send, recv, socket_path))

    async def accept_loop():
        while True:
            try:
                conn = await ep.accept()
                asyncio.create_task(handle_connection(conn))
            except Exception:
                break

    asyncio.create_task(accept_loop())
    return ep


async def _forward_to_unix(
    send: IrohSendStream,
    recv: IrohRecvStream,
    socket_path: str,
) -> None:
    try:
        unix_reader, unix_writer = await asyncio.open_unix_connection(socket_path)
        await pipe_streams(send, recv, unix_reader, unix_writer)
    except Exception:
        pass


async def connect_unix(
    socket_path: str,
    remote_addr: NodeAddr,
) -> tuple:
    """Connect to a remote dumbpipe endpoint and listen on a local Unix socket.

    Each incoming Unix connection opens a new bi-stream on the QUIC connection.
    Returns (endpoint, server) so the caller can manage their lifetime.
    """
    import os
    # Remove existing socket if present
    try:
        os.unlink(socket_path)
    except FileNotFoundError:
        pass

    ep = await create_endpoint(ALPN)
    conn = await ep.connect_node_addr(remote_addr, ALPN)

    async def handle_unix(unix_reader, unix_writer):
        try:
            send, recv = await conn.open_bi()
            await send_handshake(send)
            await pipe_streams(send, recv, unix_reader, unix_writer)
        except Exception:
            pass

    server = await asyncio.start_unix_server(handle_unix, path=socket_path)
    return ep, server


# ---------------------------------------------------------------------------
# Programmatic helpers for testing  (no stdin/stdout)
# ---------------------------------------------------------------------------

async def create_listener() -> tuple[NetClient, NodeAddr]:
    """Create a dumbpipe listener endpoint.

    Returns (endpoint, node_addr) — the caller can pass node_addr to a connector.
    """
    ep = await create_endpoint(ALPN)
    addr = ep.endpoint_addr_info()
    return ep, addr


async def accept_pipe(ep: NetClient) -> tuple[IrohSendStream, IrohRecvStream]:
    """Accept one connection + bi-stream and consume the handshake.

    Returns (send, recv) ready for data transfer.
    """
    conn = await ep.accept()
    send, recv = await conn.accept_bi()
    await recv_handshake(recv)
    return send, recv


async def connect_pipe(
    remote_addr: NodeAddr,
) -> tuple[NetClient, IrohSendStream, IrohRecvStream]:
    """Connect to a dumbpipe listener and send the handshake.

    Returns (endpoint, send, recv) ready for data transfer.
    """
    ep = await create_endpoint(ALPN)
    conn = await ep.connect_node_addr(remote_addr, ALPN)
    send, recv = await conn.open_bi()
    await send_handshake(send)
    return ep, send, recv