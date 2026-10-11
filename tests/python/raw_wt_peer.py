"""Real test-only HTTP/3 WebTransport peer, reusable by foreign binding tests.

CLI: python raw_wt_peer.py --url https://127.0.0.1:PORT/raw --scenario echo
The test peer accepts ephemeral certificates; never use its TLS policy in an app.
"""
from __future__ import annotations
import argparse
import asyncio
import json
import ssl
from contextlib import asynccontextmanager
from urllib.parse import urlsplit

from aioquic.asyncio import QuicConnectionProtocol, connect
from aioquic.h3.connection import H3_ALPN, H3Connection
from aioquic.h3.events import HeadersReceived, WebTransportStreamDataReceived, DatagramReceived
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import StreamDataReceived, StreamReset, ConnectionTerminated

class RawPeer(QuicConnectionProtocol):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.http = H3Connection(self._quic, enable_webtransport=True)
        self.headers = asyncio.Queue()
        self.streams = {}
        self.incoming = asyncio.Queue()
        self.datagrams = asyncio.Queue()
        self.client_bidi = set()
        self.session_id = None

    def quic_event_received(self, event):
        if isinstance(event, StreamDataReceived) and event.stream_id in self.client_bidi:
            self.streams[event.stream_id].put_nowait((event.data, event.end_stream))
            return
        if isinstance(event, StreamReset):
            self.streams.setdefault(event.stream_id, asyncio.Queue()).put_nowait(RuntimeError(f"stream reset: {event.error_code}"))
        if isinstance(event, ConnectionTerminated):
            error = RuntimeError(f"connection terminated: {event.error_code}")
            for queue in [self.headers, self.incoming, self.datagrams, *self.streams.values()]:
                queue.put_nowait(error)
            return
        for item in self.http.handle_event(event):
            if isinstance(item, HeadersReceived): self.headers.put_nowait(dict(item.headers))
            elif isinstance(item, DatagramReceived): self.datagrams.put_nowait(item.data)
            elif isinstance(item, WebTransportStreamDataReceived):
                if item.stream_id not in self.streams:
                    self.streams[item.stream_id] = asyncio.Queue()
                    self.incoming.put_nowait(item.stream_id)
                self.streams[item.stream_id].put_nowait((item.data, item.stream_ended))

    async def receive(self, queue):
        value = await asyncio.wait_for(queue.get(), 10)
        if isinstance(value, Exception): raise value
        return value

    async def session(self, url, token=None):
        parsed = urlsplit(url)
        self.session_id = self._quic.get_next_available_stream_id()
        headers = [(b":method", b"CONNECT"), (b":scheme", b"https"),
                   (b":authority", parsed.netloc.encode()),
                   (b":path", (parsed.path + ("?" + parsed.query if parsed.query else "")).encode()),
                   (b":protocol", b"webtransport"), (b"sec-webtransport-http3-draft", b"draft02")]
        if token is not None: headers.append((b"authorization", token.encode()))
        self.http.send_headers(self.session_id, headers)
        self.transmit()
        return int((await self.receive(self.headers))[b":status"])

    def open_stream(self, data=b"", *, uni=False, finish=False):
        stream = self.http.create_webtransport_stream(self.session_id, is_unidirectional=uni)
        self.streams[stream] = asyncio.Queue()
        if not uni: self.client_bidi.add(stream)
        self.write(stream, data, finish=finish)
        return stream

    def write(self, stream, data, *, finish=False):
        self._quic.send_stream_data(stream, data, end_stream=finish)
        self.transmit()

    async def read_all(self, stream):
        data = bytearray()
        while True:
            chunk, eof = await self.receive(self.streams[stream])
            data += chunk
            if eof: return bytes(data)

    def send_datagram(self, data):
        self.http.send_datagram(self.session_id, data)
        self.transmit()

@asynccontextmanager
async def peer(url):
    parsed = urlsplit(url)
    config = QuicConfiguration(is_client=True, alpn_protocols=H3_ALPN, max_datagram_frame_size=65536)
    config.verify_mode = ssl.CERT_NONE
    async with connect(parsed.hostname, parsed.port, configuration=config, create_protocol=RawPeer) as client:
        yield client

async def scenario(url, token=None, expected_status=200, mode="echo"):
    async with peer(url) as client:
        status = await client.session(url, token)
        assert status == expected_status, f"CONNECT status {status}, expected {expected_status}"
        if status != 200 or mode == "connect": return {"status": status, "result": "PASS"}
        if mode == "lifecycle":
            pending = client.open_stream(b"pending")
            reset = client.open_stream(b"reset-me")
            client.send_datagram(b"")
            client.send_datagram(b"lifecycle")
            assert await client.receive(client.datagrams) == b"continue"
            client.write(pending, b"-tail", finish=True)
            try:
                await client.read_all(reset)
                raise AssertionError("server did not RESET the second bidi")
            except RuntimeError as error:
                assert "stream reset: 7" in str(error), str(error)
            incoming = await client.receive(client.incoming)
            assert not incoming & 2, "expected server bidirectional stream"
            assert await client.read_all(incoming) == b"server-bi"
            client.write(incoming, b"SERVER-BI", finish=True)
            assert await client.receive(client.datagrams) == b"done"
            client.send_datagram(b"ready-close")
            await asyncio.wait_for(client.wait_closed(), 10)
            return {"status": status, "result": "PASS", "reset": True,
                    "server_bidi": True, "cancel_read_tail": True,
                    "empty_datagram": True, "closed": True}
        bidi = client.open_stream(b"control", finish=True)
        client.open_stream(b"input", uni=True, finish=True)
        client.send_datagram(b"feedback")
        assert await client.read_all(bidi) == b"CONTROL"
        frame = await client.receive(client.incoming)
        assert frame & 2, "expected server unidirectional stream"
        assert await client.read_all(frame) == b"frame"
        assert await client.receive(client.datagrams) == b"reply"
        return {"status": status, "result": "PASS", "bidi": True, "uni": True, "datagram": True}

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", required=True)
    parser.add_argument("--scenario", choices=["echo", "connect", "lifecycle"], default="echo")
    parser.add_argument("--token")
    parser.add_argument("--expected-status", type=int, default=200)
    args = parser.parse_args()
    print(json.dumps(asyncio.run(scenario(args.url, args.token, args.expected_status, args.scenario))), flush=True)
