"""
Simple Aster Hello World Producer.

Demonstrates the full producer setup:
  1. Generate (or load) a root key pair for consumer credential verification.
  2. Start a consumer admission endpoint (aster.consumer_admission ALPN).
  3. Start an Aster RPC server (aster/1 ALPN) serving HelloService.
  4. Print everything the consumer needs to connect.

─── Quick start (no prior setup) ───────────────────────────────────────────────

  python simple_producer.py

  → Prints ephemeral root_pubkey, admission_addr, and rpc_addr.
  → Copy the export lines into the consumer's terminal.

─── With a stable root key ──────────────────────────────────────────────────────

  # 1. Generate once:
  aster keygen root --out ~/.aster/root.key

  # 2. Run producer:
  ASTER_ROOT_KEY_FILE=~/.aster/root.key python simple_producer.py

  # 3. Mint consumer credentials from the same root key:
  aster trust sign --root-key ~/.aster/root.key --type policy --out consumer.json

  # 4. Run consumer (other terminal):
  ASTER_ROOT_KEY_FILE=~/.aster/root.key \\
  ASTER_ADMISSION_ADDR=<printed above> \\
  python simple_consumer.py

Environment variables:
  ASTER_ROOT_KEY_FILE   Path to root key JSON (from 'aster keygen root').
                        If not set, a fresh ephemeral key is generated each run.
"""
from __future__ import annotations

import asyncio
import base64
import json
import os
import sys

# Add examples/python to path so _hello_service is importable
sys.path.insert(0, os.path.dirname(__file__))
from _hello_service import HelloService, HelloRequest, HelloResponse  # noqa: E402

from aster_python import create_endpoint_with_config, EndpointConfig
from aster_python.aster.registry.models import ServiceSummary
from aster_python.aster.server import Server
from aster_python.aster.trust.consumer import serve_consumer_admission
from aster_python.aster.trust.hooks import MeshEndpointHook
from aster_python.aster.trust.nonces import InMemoryNonceStore
from aster_python.aster.trust.signing import generate_root_keypair

RPC_ALPN = b"aster/1"
ADMISSION_ALPN = b"aster.consumer_admission"

# Contract ID is deterministic from the service definition.
# In production, use `aster contract gen` to compute this from the actual schema.
_CONTRACT_BYTES = b"demo/HelloService:v1:say_hello(HelloRequest)->HelloResponse"
import blake3 as _blake3  # noqa: E402
CONTRACT_ID: str = _blake3.blake3(_CONTRACT_BYTES).hexdigest()


async def main() -> None:
    # ── 1. Load or generate root key ─────────────────────────────────────────
    root_key_file = os.environ.get("ASTER_ROOT_KEY_FILE")
    if root_key_file and os.path.exists(root_key_file):
        with open(root_key_file) as f:
            kd = json.load(f)
        priv_raw = bytes.fromhex(kd["private_key"])
        pub_raw = bytes.fromhex(kd["public_key"])
        print(f"[producer] Loaded root key from {root_key_file}")
    else:
        priv_raw, pub_raw = generate_root_keypair()
        print("[producer] Generated ephemeral root key (set ASTER_ROOT_KEY_FILE to persist)")

    # ── 2. Start endpoints ────────────────────────────────────────────────────
    rpc_ep = await create_endpoint_with_config(EndpointConfig(alpns=[RPC_ALPN]))
    admission_ep = await create_endpoint_with_config(EndpointConfig(alpns=[ADMISSION_ALPN]))

    rpc_addr_b64 = base64.b64encode(rpc_ep.endpoint_addr_info().to_bytes()).decode()
    admission_addr_b64 = base64.b64encode(admission_ep.endpoint_addr_info().to_bytes()).decode()

    # ── 3. Consumer admission ─────────────────────────────────────────────────
    hook = MeshEndpointHook()
    nonce_store = InMemoryNonceStore()
    services = [
        ServiceSummary(
            name="HelloService",
            version=1,
            contract_id=CONTRACT_ID,
            # Encode RPC endpoint address so the consumer can connect directly.
            channels={"rpc": rpc_addr_b64},
        )
    ]

    admission_task = asyncio.create_task(
        serve_consumer_admission(
            admission_ep,
            root_pubkey=pub_raw,
            hook=hook,
            nonce_store=nonce_store,
            services_getter=lambda: services,
            registry_ticket_getter=lambda: "",
        )
    )

    # ── 4. RPC server ─────────────────────────────────────────────────────────
    server = Server(rpc_ep, services=[HelloService()])
    serve_task = asyncio.create_task(server.serve())

    # ── 5. Print connection info ──────────────────────────────────────────────
    print()
    print("╔══════════════════════════════════════════════════════════════════╗")
    print("║  Aster Hello World Producer — ready                             ║")
    print("╠══════════════════════════════════════════════════════════════════╣")
    print(f"  root_pubkey    : {pub_raw.hex()}")
    print(f"  contract_id    : {CONTRACT_ID}")
    print(f"  admission_addr : {admission_addr_b64}")
    print(f"  rpc_addr       : {rpc_addr_b64}")
    print("╠══════════════════════════════════════════════════════════════════╣")
    print("  Run consumer with:")
    print(f"    export ASTER_ROOT_KEY_FILE={root_key_file or '<path-to-root.key>'}")
    print(f"    export ASTER_ADMISSION_ADDR={admission_addr_b64}")
    print("    python simple_consumer.py")
    print("╚══════════════════════════════════════════════════════════════════╝")
    print()
    print("  Waiting for connections... (Ctrl+C to stop)")

    # ── 6. Run until cancelled ────────────────────────────────────────────────
    try:
        await asyncio.Event().wait()
    except (KeyboardInterrupt, asyncio.CancelledError):
        pass
    finally:
        serve_task.cancel()
        admission_task.cancel()
        await asyncio.gather(serve_task, admission_task, return_exceptions=True)
        await server.close()
        await rpc_ep.close()
        await admission_ep.close()
        print("\n[producer] Stopped.")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
