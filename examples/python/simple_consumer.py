"""
Simple Aster Hello World Consumer.

Demonstrates the Y.2 dynamic client flow:
  1. Mint a consumer policy credential from the root key.
  2. Connect to the producer's admission endpoint.
  3. Receive ConsumerAdmissionResponse (services + RPC address).
  4. Connect to the RPC endpoint and call HelloService.say_hello.

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
import base64
import json
import os
import sys
import time

# Add examples/python to path so _hello_service is importable
sys.path.insert(0, os.path.dirname(__file__))
from _hello_service import HelloService, HelloRequest  # noqa: E402

from aster import create_endpoint_with_config, EndpointConfig, NodeAddr
from aster.client import create_client
from aster.trust.consumer import (
    ConsumerAdmissionRequest,
    ConsumerAdmissionResponse,
    consumer_cred_to_json,
)
from aster.trust.credentials import ConsumerEnrollmentCredential
from aster.trust.signing import sign_credential

RPC_ALPN = b"aster/1"
ADMISSION_ALPN = b"aster.consumer_admission"


async def main() -> None:
    # ── 1. Load root key and mint a policy credential ─────────────────────────
    root_key_file = os.environ.get("ASTER_ROOT_KEY_FILE")
    if not root_key_file or not os.path.exists(root_key_file):
        print(
            "Error: set ASTER_ROOT_KEY_FILE to the root key used by the producer.\n"
            "  (same file as ASTER_ROOT_KEY_FILE on the producer side)",
            file=sys.stderr,
        )
        sys.exit(1)

    with open(root_key_file) as f:
        kd = json.load(f)
    priv_raw = bytes.fromhex(kd["private_key"])
    pub_raw = bytes.fromhex(kd["public_key"])

    cred = ConsumerEnrollmentCredential(
        credential_type="policy",
        root_pubkey=pub_raw,
        expires_at=int(time.time()) + 3600,
        attributes={"aster.role": "consumer"},
    )
    cred.signature = sign_credential(cred, priv_raw)
    print("[consumer] Minted policy credential")

    # ── 2. Load admission addr ────────────────────────────────────────────────
    admission_addr_b64 = os.environ.get("ASTER_ADMISSION_ADDR")
    if not admission_addr_b64:
        print("Error: set ASTER_ADMISSION_ADDR to the producer's admission address", file=sys.stderr)
        sys.exit(1)

    admission_addr = NodeAddr.from_bytes(base64.b64decode(admission_addr_b64))
    print(f"[consumer] Connecting to admission endpoint {admission_addr.endpoint_id[:16]}...")

    # ── 3. Consumer admission ─────────────────────────────────────────────────
    ep = await create_endpoint_with_config(EndpointConfig(alpns=[ADMISSION_ALPN, RPC_ALPN]))

    conn = await ep.connect_node_addr(admission_addr, ADMISSION_ALPN)
    send, recv = await conn.open_bi()

    req = ConsumerAdmissionRequest(credential_json=consumer_cred_to_json(cred))
    await send.write_all(req.to_json().encode())
    await send.finish()

    raw = await recv.read_to_end(64 * 1024)
    resp = ConsumerAdmissionResponse.from_json(raw)

    if not resp.admitted:
        print("[consumer] Admission denied — check that ASTER_ROOT_KEY_FILE matches the producer's key")
        sys.exit(1)

    print(f"[consumer] Admitted! Services: {[s.name for s in resp.services]}")

    # ── 4. Extract RPC address from service channels ──────────────────────────
    if not resp.services or "rpc" not in resp.services[0].channels:
        print("[consumer] No RPC address in admission response", file=sys.stderr)
        sys.exit(1)

    rpc_addr = NodeAddr.from_bytes(base64.b64decode(resp.services[0].channels["rpc"]))
    print(f"[consumer] RPC endpoint: {rpc_addr.endpoint_id[:16]}...")

    # ── 5. Call HelloService.say_hello ────────────────────────────────────────
    name = os.environ.get("ASTER_HELLO_NAME", "World")

    rpc_conn = await ep.connect_node_addr(rpc_addr, RPC_ALPN)
    client = create_client(HelloService, connection=rpc_conn)

    print(f"[consumer] Calling say_hello(name={name!r})...")
    response = await client.say_hello(HelloRequest(name=name))
    print(f"\n  ★  {response.message}\n")

    await client.close()
    await ep.close()
    print("[consumer] Done.")


if __name__ == "__main__":
    asyncio.run(main())
