"""Example: QUIC echo server using bare endpoints.

Run this in one terminal, then run echo_client.py in another
(passing this server's endpoint ID as an argument).
"""
import asyncio
from aster_python import create_endpoint

ALPN = b"example/echo/1"


async def main():
    ep = await create_endpoint(ALPN)
    print(f"Echo server listening. Endpoint ID: {ep.endpoint_id()}")

    conn = await ep.accept()
    print(f"Accepted connection from {conn.remote_id()}")

    send, recv = await conn.accept_bi()
    data = await recv.read_to_end(65536)
    print(f"Received: {data.decode()}")

    await send.write_all(data)
    await send.finish()
    print("Echoed back. Done.")


if __name__ == "__main__":
    asyncio.run(main())