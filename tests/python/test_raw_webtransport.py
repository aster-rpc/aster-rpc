"""PC13 real raw media sessions: no RPC framing and no implicit auth bypass."""
import asyncio
import socket
import pytest
from aster.http import generate_webtransport_certificate, RPCAuthResponse
from aster.webtransport import WebTransportListener, WebTransportOptions
from raw_wt_peer import peer, scenario

pytestmark = [pytest.mark.network, pytest.mark.timeout(30)]

@pytest.fixture
def options():
    cert = generate_webtransport_certificate("raw-test", ["localhost", "127.0.0.1"])
    return WebTransportOptions(cert.certificate_pem, cert.private_key_pem, path="/wt")

async def all_bytes(stream):
    data = bytearray()
    while chunk := await stream.read(3): data += chunk
    return bytes(data)

def released(addr):
    host, port = addr.rsplit(":", 1)
    for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
        with socket.socket(socket.AF_INET, kind) as sock: sock.bind((host, int(port)))

async def test_raw_media_controls_datagrams_ack_and_shutdown(options):
    listener = await WebTransportListener.start_public(options)
    task = asyncio.create_task(scenario(f"https://{listener.quic_addr}/wt"))
    session = None
    try:
        session = await listener.accept()
        assert not session.info()["closed"]
        assert session.max_datagram_size > 0
        control = await session.accept_bi()
        # Consuming a PoP/control prefix must leave the same receive half positioned.
        assert await control.read(3) == b"con"
        assert await all_bytes(control) == b"trol"
        await control.set_priority(-20)
        assert await control.priority() == -20
        with pytest.raises(RuntimeError, match="byte limit"):
            await control.write(b"x" * (options.max_read_bytes + 1))
        await control.write(b"CONTROL")
        await control.finish()
        assert await control.transport_acked()
        incoming = await session.accept_uni()
        assert not incoming.info()["has_send"]
        assert await all_bytes(incoming) == b"input"
        frame = await session.open_uni()
        await frame.set_priority(20)
        assert await frame.priority() == 20
        await frame.write(b"frame")
        await frame.finish()
        assert await frame.transport_acked()
        assert await session.receive_datagram() == b"feedback"
        session.send_datagram(b"reply")
        assert (await task)["result"] == "PASS"
        stats = session.stats()
        assert stats["udp_tx_bytes"] > 0 and stats["udp_rx_packets"] > 0
    finally:
        if session: session.close()
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)
        await listener.stop()
    released(listener.tcp_addr)

async def test_cancelled_accept_preserves_next_session_and_close_wakes_pending(options):
    listener = await WebTransportListener.start_public(options)
    try:
        with pytest.raises(asyncio.TimeoutError):
            await asyncio.wait_for(listener.accept(), 0.03)
        async with peer(f"https://{listener.quic_addr}/wt") as client:
            assert await client.session(f"https://{listener.quic_addr}/wt") == 200
            session = await asyncio.wait_for(listener.accept(), 3)
            with pytest.raises(asyncio.TimeoutError):
                await asyncio.wait_for(session.receive_datagram(), 0.03)
            client.send_datagram(b"next")
            assert await session.receive_datagram() == b"next"
            pending = asyncio.create_task(session.accept_uni())
            await asyncio.sleep(0)
            session.close(42, b"done")
            with pytest.raises(RuntimeError): await asyncio.wait_for(pending, 1)
            await asyncio.wait_for(session.closed(), 1)
    finally: await listener.stop()
    released(listener.tcp_addr)

async def test_preupgrade_auth_raw_query_and_wrong_route(options):
    seen = []
    def auth(request):
        seen.append(request)
        if request.metadata.get("authorization") == "Bearer allowed": return RPCAuthResponse("desktop-user")
        return RPCAuthResponse()
    listener = await WebTransportListener.start(options, auth)
    try:
        url = f"https://{listener.quic_addr}/wt"
        assert (await scenario(url, expected_status=401, mode="connect"))["result"] == "PASS"
        with pytest.raises(asyncio.TimeoutError): await asyncio.wait_for(listener.accept(), 0.03)
        assert (await scenario(url+"-wrong", expected_status=404, mode="connect"))["result"] == "PASS"
        async with peer(url) as client:
            assert await client.session(url+"?cap=a%2Fb&cap=c", "Bearer allowed") == 200
            session = await listener.accept()
            assert session.info()["principal"] == "desktop-user"
            assert seen[-1].service == "webtransport" and seen[-1].method == "CONNECT"
            assert seen[-1].metadata["aster.webtransport.query"] == "cap=a%2Fb&cap=c"
            assert seen[-1].metadata["aster.webtransport.path"] == "/wt"
            session.close()
    finally: await listener.stop()

async def test_limits_and_listener_stop_cancel_pending(options):
    listener = await WebTransportListener.start_public(options)
    pending = asyncio.create_task(listener.accept())
    await asyncio.sleep(0)
    await listener.stop()
    with pytest.raises(RuntimeError): await asyncio.wait_for(pending, 1)
    with pytest.raises(ValueError):
        await WebTransportListener.start_public(WebTransportOptions(options.certificate_pem, options.private_key_pem, max_sessions=0))
    with pytest.raises(RuntimeError):
        await WebTransportListener.start_public(WebTransportOptions(options.certificate_pem, options.private_key_pem, path="/aster/wt"))

async def test_reset_cancelled_read_position_server_bidi_and_empty_datagram(options):
    listener = await WebTransportListener.start_public(options)
    remote = asyncio.create_task(scenario(f"https://{listener.quic_addr}/wt", mode="lifecycle"))
    session = None
    try:
        session = await listener.accept()
        pending = await session.accept_bi()
        reset = await session.accept_bi()
        assert await pending.read() == b"pending"
        assert await reset.read() == b"reset-me"
        with pytest.raises(asyncio.TimeoutError): await asyncio.wait_for(pending.read(), 0.03)
        blocked_read = asyncio.create_task(reset.read())
        await asyncio.sleep(0.01)
        await reset.reset(7)
        await reset.stop(9)
        with pytest.raises(RuntimeError): await asyncio.wait_for(blocked_read, 1)
        assert not await reset.transport_acked()
        assert await session.receive_datagram() == b""
        assert await session.receive_datagram() == b"lifecycle"
        with pytest.raises(RuntimeError): session.send_datagram(b"x" * (session.max_datagram_size + 1))
        session.send_datagram(b"continue")
        assert await all_bytes(pending) == b"-tail"
        outgoing = await session.open_bi()
        await outgoing.write(b"server-bi")
        await outgoing.finish()
        assert await all_bytes(outgoing) == b"SERVER-BI"
        session.send_datagram(b"done")
        assert await session.receive_datagram() == b"ready-close"
        blocked = asyncio.create_task(session.accept_uni())
        await asyncio.sleep(0)
        session.close(19, b"done")
        with pytest.raises(RuntimeError): await asyncio.wait_for(blocked, 1)
        assert (await remote)["result"] == "PASS"
    finally:
        if session: session.close()
        remote.cancel()
        await asyncio.gather(remote, return_exceptions=True)
        await listener.stop()
    released(listener.tcp_addr)
