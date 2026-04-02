"""Example: QUIC echo client using bare endpoints.

Usage: python echo_client.py <server_endpoint_id>
"""
import asyncio
import sys
from iroh_python import create_endpoint

ALPN = b"example/echo/1"


async def main(server_id: str):
    ep = await create_endpoint(ALPN)
    print(f"Client endpoint ID: {ep.endpoint_id()}")

    conn = await ep.connect(server_id, ALPN)
    print(f"Connected to {conn.remote_id()}")

    send, recv = await conn.open_bi()
    message = b"Hello from echo client!"
    await send.write_all(message)
    await send.finish()
    print(f"Sent: {message.decode()}")

    echo = await recv.read_to_end(65536)
    print(f"Echo: {echo.decode()}")
    assert echo == message


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <server_endpoint_id>")
        sys.exit(1)
    asyncio.run(main(sys.argv[1]))