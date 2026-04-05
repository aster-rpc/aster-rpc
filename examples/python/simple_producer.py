"""
Simple Aster Hello World Producer.

Uses the declarative :class:`AsterServer` to stand up:
  1. An RPC endpoint (``aster/1``) serving HelloService.
  2. A consumer admission endpoint (``aster.consumer_admission``) gating
     consumers with the root key.

─── Quick start (no prior setup) ───────────────────────────────────────────────

  python simple_producer.py

  → Prints ephemeral root_pubkey, admission_addr, and rpc_addr.
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

Environment variables:
  ASTER_ROOT_KEY_FILE   Path to root key JSON (from 'aster keygen root').
                        If not set, a fresh ephemeral key is generated each run.
"""
from __future__ import annotations

import asyncio
import json
import os
import sys

# Add examples/python to path so _hello_service is importable
sys.path.insert(0, os.path.dirname(__file__))
from _hello_service import HelloService  # noqa: E402

from aster import AsterServer  # noqa: E402
from aster.trust.signing import generate_root_keypair  # noqa: E402


def _load_or_generate_root_key() -> tuple[bytes, bytes, str | None]:
    path = os.environ.get("ASTER_ROOT_KEY_FILE")
    if path and os.path.exists(path):
        with open(path) as f:
            kd = json.load(f)
        priv = bytes.fromhex(kd["private_key"])
        pub = bytes.fromhex(kd["public_key"])
        print(f"[producer] Loaded root key from {path}")
        return priv, pub, path
    priv, pub = generate_root_keypair()
    print("[producer] Generated ephemeral root key (set ASTER_ROOT_KEY_FILE to persist)")
    return priv, pub, None


def _print_connection_info(pub: bytes, srv: AsterServer, key_path: str | None) -> None:
    print()
    print("╔══════════════════════════════════════════════════════════════════╗")
    print("║  Aster Hello World Producer — ready                             ║")
    print("╠══════════════════════════════════════════════════════════════════╣")
    print(f"  root_pubkey    : {pub.hex()}")
    for s in srv.services:
        print(f"  {s.name}.v{s.version} contract_id : {s.contract_id}")
    print(f"  admission_addr : {srv.admission_addr_b64}")
    print(f"  rpc_addr       : {srv.rpc_addr_b64}")
    print("╠══════════════════════════════════════════════════════════════════╣")
    print("  Run consumer with:")
    print(f"    export ASTER_ROOT_KEY_FILE={key_path or '<path-to-root.key>'}")
    print(f"    export ASTER_ADMISSION_ADDR={srv.admission_addr_b64}")
    print("    python simple_consumer.py")
    print("╚══════════════════════════════════════════════════════════════════╝")
    print()
    print("  Waiting for connections... (Ctrl+C to stop)")


async def main() -> None:
    _priv, pub, key_path = _load_or_generate_root_key()
    async with AsterServer(services=[HelloService()], root_pubkey=pub) as srv:
        _print_connection_info(pub, srv, key_path)
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
