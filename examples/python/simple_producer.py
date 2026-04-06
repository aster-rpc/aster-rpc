"""
Simple Aster Hello World Producer.

Uses the declarative :class:`AsterServer` to stand up an RPC endpoint
serving HelloService. Configuration is automatic via ``ASTER_*`` env vars
or an ``aster.toml`` file.

─── Dev mode (no prior setup) ─────────────────────────────────────────────────

  # Terminal 1
  python simple_producer.py
  → Generates ephemeral root key, opens consumer gate, prints endpoint address.

  # Terminal 2
  ASTER_ENDPOINT_ADDR=<printed above> python simple_consumer.py

─── Production mode ───────────────────────────────────────────────────────────

  # 1. Generate root keypair (operator's machine):
  aster keygen root --out root.key
  aster keygen pubkey --in root.key --out root_pub.key

  # 2. Run producer with root public key:
  ASTER_ROOT_PUBKEY_FILE=root_pub.key python simple_producer.py

  # 3. Mint consumer credential and run consumer:
  aster authorize consumer --root-key root.key --type policy --out consumer.token
  ASTER_ENDPOINT_ADDR=<printed> ASTER_ENROLLMENT_CREDENTIAL=consumer.token \\
    python simple_consumer.py

Environment variables (all optional):
  ASTER_ROOT_PUBKEY_FILE    Path to root public key. Ephemeral if unset.
  ASTER_ALLOW_ALL_CONSUMERS If "true", skip consumer admission.
  ASTER_ALLOW_ALL_PRODUCERS If "true", skip producer admission (default).
  ASTER_SECRET_KEY          Base64 node identity key (for stable EndpointId).
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
        print(f"    export ASTER_ENDPOINT_ADDR={srv.endpoint_addr_b64}")
        if not srv._allow_all_consumers:
            print("    export ASTER_ENROLLMENT_CREDENTIAL=consumer.token")
        print("    python simple_consumer.py")
        if srv._allow_all_consumers:
            print("  (dev mode: consumer gate open — no credential needed)")
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
