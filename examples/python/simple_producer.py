"""
Simple Aster Hello World Producer.

Uses the declarative :class:`AsterServer` to stand up an RPC endpoint
serving HelloService. Configuration is automatic via ``ASTER_*`` env vars
or an ``aster.toml`` file.

--- Dev mode (no prior setup) ---

  # Terminal 1
  python simple_producer.py

  # Terminal 2 (copy the ASTER_ENDPOINT_ADDR from the output above)
  ASTER_ENDPOINT_ADDR=<printed> python simple_consumer.py

--- Production mode ---

  # 1. Generate root keypair (operator's machine):
  aster keygen root --out root.key

  # 2. Run producer with root public key:
  ASTER_ROOT_PUBKEY_FILE=root_pub.key python simple_producer.py

  # 3. Mint consumer credential and run consumer:
  aster trust sign --root-key root.key --type consumer --out consumer.token
  ASTER_ENDPOINT_ADDR=<printed> ASTER_ENROLLMENT_CREDENTIAL=consumer.token \\
    python simple_consumer.py
"""
from __future__ import annotations

import asyncio
import os
import sys

# Add examples/python to path so _hello_service is importable
sys.path.insert(0, os.path.dirname(__file__))
from _hello_service import HelloService  # noqa: E402

from aster import AsterServer  # noqa: E402


async def main() -> None:
    async with AsterServer(services=[HelloService()]) as srv:
        # The ASTER banner is printed automatically by AsterServer.start().
        # Just print the connect command for the consumer.
        print(f"\n  Connect with:")
        print(f"    ASTER_ENDPOINT_ADDR={srv.endpoint_addr_b64} python simple_consumer.py\n")
        print(f"  Or use the shell:")
        print(f"    aster shell {srv.endpoint_addr_b64}\n")
        print(f"  Waiting for connections... (Ctrl+C to stop)\n")
        try:
            await srv.serve()
        except (KeyboardInterrupt, asyncio.CancelledError):
            pass
    print("\nStopped.")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
