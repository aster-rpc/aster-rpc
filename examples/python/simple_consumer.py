"""
Simple Aster Hello World Consumer.

Uses the declarative :class:`AsterClient` to connect to a producer and
call ``HelloService.say_hello``.

All configuration comes from environment variables (or an ``aster.toml``
config file via :class:`AsterConfig`).

─── Dev mode (no credentials) ──────────────────────────────────────────────

  Start simple_producer.py (which auto-opens the consumer gate in dev mode):

    # Terminal 1
    python simple_producer.py

    # Terminal 2
    ASTER_ENDPOINT_ADDR=<printed by producer> python simple_consumer.py

─── Production mode (with enrollment credential) ──────────────────────────

    # Operator mints a consumer credential offline:
    aster authorize consumer --root-key root.key --type policy --out consumer.token

    # Consumer:
    ASTER_ENDPOINT_ADDR=<producer addr> \\
    ASTER_ENROLLMENT_CREDENTIAL=consumer.token \\
    python simple_consumer.py

Environment variables:
  ASTER_ENDPOINT_ADDR             Producer's endpoint address (required).
  ASTER_ENROLLMENT_CREDENTIAL     Path to pre-signed enrollment token (production).
  ASTER_ENROLLMENT_CREDENTIAL_IID Cloud IID token (when credential requires IID).
  ASTER_SECRET_KEY                Base64 node identity key (for stable EndpointId).
  ASTER_HELLO_NAME                Name to greet (default: "World").
"""
from __future__ import annotations

import asyncio
import os
import sys

# Add examples/python to path so _hello_service is importable
sys.path.insert(0, os.path.dirname(__file__))
from _hello_service import HelloService, HelloRequest  # noqa: E402

from aster import AsterClient  # noqa: E402


async def main() -> None:
    name = os.environ.get("ASTER_HELLO_NAME", "World")

    async with AsterClient() as c:
        print(f"[consumer] Connected! Services: {[s.name for s in c.services]}")
        hello = await c.client(HelloService)
        print(f"[consumer] Calling say_hello(name={name!r})...")
        resp = await hello.say_hello(HelloRequest(name=name))
        print(f"\n  ★  {resp.message}\n")

    print("[consumer] Done.")


if __name__ == "__main__":
    asyncio.run(main())
