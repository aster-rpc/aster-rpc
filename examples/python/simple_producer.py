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

--- Production mode (Enrollment, files from the `aster` CLI) ---

  # 1. Operator: root key, fleet namespace, the producer's key + credential:
  aster key root generate
  ROOT=$(aster key root show --json | jq -r .root_pubkey)
  FLEET=$(aster enroll namespace create --out-secret fleet.secret --json | jq -r .namespace)
  SERVER=$(aster key node generate --out server.key --json | jq -r .node_id)
  aster enroll node --subject $SERVER --audience hello --out server.cred

  # 2. Run the producer with enrollment configured:
  ASTER_ROOT_PUBKEY=$ROOT ASTER_ENROLLMENT_AUDIENCE=hello \\
  ASTER_ENROLLMENT_FLEET_NAMESPACE=$FLEET ASTER_SECRET_KEY_FILE=server.key \\
  ASTER_ENROLLMENT_CREDENTIAL=server.cred \\
  ASTER_ENROLLMENT_STATE_PATH=./hello-enrollment.json python simple_producer.py

  # 3. Issue the consumer a credential and run it (see simple_consumer.py).
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
