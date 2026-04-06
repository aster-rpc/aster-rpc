"""
Simple Aster Hello World Producer.

Uses the declarative :class:`AsterServer` to stand up an RPC endpoint
serving HelloService, with consumer admission gated by a root key.

Configuration is automatic — ``AsterServer`` reads ``ASTER_*`` environment
variables (or an ``aster.toml`` file via :class:`AsterConfig`) so application
code doesn't need to handle key loading, endpoint wiring, or admission setup.

─── Quick start (no prior setup) ───────────────────────────────────────────────

  python simple_producer.py

  → Generates an ephemeral root key, prints the endpoint address.
  → Copy the export lines into the consumer's terminal.

─── With a stable root key ──────────────────────────────────────────────────────

  # 1. Generate once:
  aster keygen root --out ~/.aster/root.key

  # 2. Run producer:
  ASTER_ROOT_KEY_FILE=~/.aster/root.key python simple_producer.py

  # 3. Run consumer (other terminal):
  ASTER_ROOT_KEY_FILE=~/.aster/root.key \\
  ASTER_ADMISSION_ADDR=<printed above> \\
  python simple_consumer.py

Environment variables (all optional):
  ASTER_ROOT_KEY_FILE       Path to root key JSON. Ephemeral if unset.
  ASTER_ALLOW_ALL_CONSUMERS If "true", skip consumer admission.
  ASTER_ALLOW_ALL_PRODUCERS If "true", skip producer admission (default).
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
        print()
        print("╔══════════════════════════════════════════════════════════════════╗")
        print("║  Aster Hello World Producer — ready                             ║")
        print("╠══════════════════════════════════════════════════════════════════╣")
        if srv.root_pubkey:
            print(f"  root_pubkey    : {srv.root_pubkey.hex()}")
        for s in srv.services:
            print(f"  {s.name}.v{s.version} contract_id : {s.contract_id}")
        print(f"  endpoint_addr  : {srv.endpoint_addr_b64}")
        print("╠══════════════════════════════════════════════════════════════════╣")
        print("  Run consumer with:")
        key_file = os.environ.get("ASTER_ROOT_KEY_FILE", "<path-to-root.key>")
        print(f"    export ASTER_ROOT_KEY_FILE={key_file}")
        print(f"    export ASTER_ADMISSION_ADDR={srv.endpoint_addr_b64}")
        print("    python simple_consumer.py")
        print("╚══════════════════════════════════════════════════════════════════╝")
        print()
        print("  Waiting for connections... (Ctrl+C to stop)")
        try:
            await srv.serve()
        except (KeyboardInterrupt, asyncio.CancelledError):
            pass
    print("\n[producer] Stopped.")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
