"""
Simple Aster Hello World Consumer.

Uses the declarative :class:`AsterClient` to:
  1. Mint a consumer policy credential from the root key.
  2. Run the consumer admission handshake against the producer.
  3. Call ``HelloService.say_hello``.

─── Prerequisites ────────────────────────────────────────────────────────────

  Start simple_producer.py in another terminal and copy the exported env vars:

    export ASTER_ROOT_KEY_FILE=~/.aster/root.key   # same root key as producer
    export ASTER_ADMISSION_ADDR=<printed by producer>

    python simple_consumer.py

Environment variables:
  ASTER_ROOT_KEY_FILE    Path to root key JSON (must match producer's root key).
  ASTER_ADMISSION_ADDR   Base64-encoded NodeAddr of the producer's admission endpoint.
  ASTER_HELLO_NAME       Name to greet (default: "World").
"""
from __future__ import annotations

import asyncio
import json
import os
import sys

# Add examples/python to path so _hello_service is importable
sys.path.insert(0, os.path.dirname(__file__))
from _hello_service import HelloService, HelloRequest  # noqa: E402

from aster import AsterClient  # noqa: E402


async def main() -> None:
    key_file = os.environ.get("ASTER_ROOT_KEY_FILE")
    admission_addr = os.environ.get("ASTER_ADMISSION_ADDR")
    if not key_file or not os.path.exists(key_file) or not admission_addr:
        print(
            "Error: ASTER_ROOT_KEY_FILE and ASTER_ADMISSION_ADDR must be set "
            "(see simple_producer.py output).",
            file=sys.stderr,
        )
        sys.exit(1)

    with open(key_file) as f:
        kd = json.load(f)
    priv = bytes.fromhex(kd["private_key"])
    pub = bytes.fromhex(kd["public_key"])

    name = os.environ.get("ASTER_HELLO_NAME", "World")

    async with AsterClient(
        root_pubkey=pub,
        root_privkey=priv,
        admission_addr=admission_addr,
    ) as c:
        print(f"[consumer] Admitted! Services: {[s.name for s in c.services]}")
        hello = await c.client(HelloService)
        print(f"[consumer] Calling say_hello(name={name!r})...")
        resp = await hello.say_hello(HelloRequest(name=name))
        print(f"\n  ★  {resp.message}\n")

    print("[consumer] Done.")


if __name__ == "__main__":
    asyncio.run(main())
