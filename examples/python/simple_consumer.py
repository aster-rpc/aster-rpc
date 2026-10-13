"""
Simple Aster Hello World Consumer.

Uses the declarative :class:`AsterClient` to connect to a producer and
call ``HelloService.say_hello``.

All configuration comes from environment variables (or an ``aster.toml``
config file via :class:`AsterConfig`).

─── Dev mode (no credentials) ──────────────────────────────────────────────

  Start simple_producer.py without enrollment configured (an open server):

    # Terminal 1
    python simple_producer.py

    # Terminal 2
    ASTER_ENDPOINT_ADDR=<printed by producer> python simple_consumer.py

─── Production mode (Enrollment) ───────────────────────────────────────────

    # Operator (the `aster` CLI), once the producer runs with enrollment
    # (see simple_producer.py):
    CLIENT=$(aster key node generate --out client.key --json | jq -r .node_id)
    aster enroll node --subject $CLIENT --audience hello --out client.cred

    # Consumer:
    ASTER_ENDPOINT_ADDR=<producer addr> \\
    ASTER_ROOT_PUBKEY=$ROOT \\
    ASTER_ENROLLMENT_AUDIENCE=hello \\
    ASTER_ENROLLMENT_FLEET_NAMESPACE=$FLEET \\
    ASTER_SECRET_KEY_FILE=client.key \\
    ASTER_ENROLLMENT_CREDENTIAL=client.cred \\
    python simple_consumer.py

Environment variables:
  ASTER_ENDPOINT_ADDR               Producer's endpoint address (required).
  ASTER_ROOT_PUBKEY                 Root public key (enrollment).
  ASTER_ENROLLMENT_AUDIENCE         Fleet audience (enrollment).
  ASTER_ENROLLMENT_FLEET_NAMESPACE  Fleet policy namespace (enrollment).
  ASTER_ENROLLMENT_CREDENTIAL       This node's credential (aster enroll node --out).
  ASTER_SECRET_KEY_FILE             The node key the credential was issued for.
  ASTER_HELLO_NAME                  Name to greet (default: "World").
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
