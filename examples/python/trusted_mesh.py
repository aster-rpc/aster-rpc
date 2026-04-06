"""
Full Trust Workflow — Aster RPC Example.

Demonstrates the complete Aster trust model: root keypair generation,
credential signing, Gate 0 admission, and authenticated RPC calls.

The trust model (from Aster-trust-spec.md):
  1. An **operator** generates an ed25519 root keypair offline.
  2. The operator signs an ``ConsumerEnrollmentCredential`` (policy type)
     and distributes it to the consumer.
  3. The **producer** starts with the root public key.  Gate 0 is active,
     so all incoming connections must present a valid credential.
  4. The **consumer** connects with its credential.  The producer verifies
     the signature and admits the consumer.
  5. Authenticated RPC calls proceed.

This example runs everything in a single process for demonstration:
  - Generates the root keypair
  - Signs a consumer credential
  - Starts a producer with Gate 0 enabled
  - Starts a consumer that presents the credential
  - Makes an RPC call

Usage:

  python trusted_mesh.py

  (No ASTER_* env vars needed — the example is self-contained.)
"""
from __future__ import annotations

import asyncio
import json
import os
import sys
import tempfile
import time
from dataclasses import dataclass

from aster import AsterServer, AsterClient
from aster.codec import wire_type
from aster.decorators import service, rpc
from aster.trust.signing import generate_root_keypair, sign_credential
from aster.trust.credentials import ConsumerEnrollmentCredential


# ── Message types ────────────────────────────────────────────────────────────


@wire_type("example.trust/WhoAmIRequest")
@dataclass
class WhoAmIRequest:
    pass


@wire_type("example.trust/WhoAmIResponse")
@dataclass
class WhoAmIResponse:
    message: str = ""


@wire_type("example.trust/SecureDataRequest")
@dataclass
class SecureDataRequest:
    query: str = ""


@wire_type("example.trust/SecureDataResponse")
@dataclass
class SecureDataResponse:
    data: str = ""
    access_level: str = ""


# ── Service definition ───────────────────────────────────────────────────────


@service("TrustedService")
class TrustedService:
    """A service that is only accessible to authenticated consumers."""

    @rpc
    async def who_am_i(self, req: WhoAmIRequest) -> WhoAmIResponse:
        """Return info about the authenticated caller."""
        return WhoAmIResponse(
            message="You are an authenticated consumer with a valid credential."
        )

    @rpc
    async def get_secure_data(self, req: SecureDataRequest) -> SecureDataResponse:
        """Return sensitive data (only reachable after admission)."""
        return SecureDataResponse(
            data=f"Sensitive result for query: {req.query!r}",
            access_level="full",
        )


# ── Main demo ────────────────────────────────────────────────────────────────


async def main() -> None:
    print("=" * 60)
    print("  Aster Trusted Mesh Example")
    print("=" * 60)

    # ── Step 1: Generate root keypair (operator's machine) ──────────────
    print("\n--- Step 1: Generate root keypair ---")
    root_privkey, root_pubkey = generate_root_keypair()
    print(f"  root_pubkey  : {root_pubkey.hex()[:32]}...")
    print(f"  root_privkey : (kept secret, {len(root_privkey)} bytes)")

    # ── Step 2: Sign a consumer enrollment credential ───────────────────
    print("\n--- Step 2: Sign consumer enrollment credential ---")
    expires_at = int(time.time()) + 3600  # valid for 1 hour

    cred = ConsumerEnrollmentCredential(
        credential_type="policy",
        root_pubkey=root_pubkey,
        expires_at=expires_at,
        attributes={
            "aster.role": "consumer",
            "aster.name": "example-consumer",
            "team": "engineering",
        },
    )

    # Sign the credential with the root private key.
    # In production this happens on the operator's machine and the signed
    # credential is distributed out-of-band.
    signature = sign_credential(cred, root_privkey)
    cred.signature = signature
    print(f"  credential_type : {cred.credential_type}")
    print(f"  expires_at      : {cred.expires_at} ({time.ctime(cred.expires_at)})")
    print(f"  attributes      : {cred.attributes}")
    print(f"  signature       : {signature.hex()[:32]}...")

    # Write the credential to a temporary JSON file (simulates distribution).
    cred_dict = {
        "credential_type": cred.credential_type,
        "root_pubkey": cred.root_pubkey.hex(),
        "expires_at": cred.expires_at,
        "attributes": cred.attributes,
        "signature": cred.signature.hex(),
    }
    cred_file = os.path.join(tempfile.gettempdir(), "aster_example_consumer.token")
    with open(cred_file, "w") as f:
        json.dump(cred_dict, f)
    print(f"  credential file : {cred_file}")

    # ── Step 3: Start producer with Gate 0 enabled ──────────────────────
    print("\n--- Step 3: Start producer (Gate 0 enabled) ---")

    # allow_all_consumers=False forces Gate 0 admission.
    # Only consumers presenting a valid credential signed by root_pubkey
    # will be admitted.
    srv = AsterServer(
        services=[TrustedService()],
        root_pubkey=root_pubkey,
        allow_all_consumers=False,
    )
    await srv.start()
    srv.serve()  # Start accept loop in background

    addr_b64 = srv.endpoint_addr_b64
    print(f"  endpoint_addr   : {addr_b64[:40]}...")
    print(f"  Gate 0 active   : True (credential required)")

    # ── Step 4: Consumer connects with credential ───────────────────────
    print("\n--- Step 4: Consumer connects with credential ---")

    client = AsterClient(
        endpoint_addr=addr_b64,
        root_pubkey=root_pubkey,
        enrollment_credential_file=cred_file,
    )
    await client.connect()
    print(f"  Admitted!  Services: {[s.name for s in client.services]}")

    # ── Step 5: Make authenticated RPC calls ────────────────────────────
    print("\n--- Step 5: Make authenticated RPC calls ---")
    trusted = await client.client(TrustedService)

    resp1 = await trusted.who_am_i(WhoAmIRequest())
    print(f"  who_am_i -> {resp1.message}")

    resp2 = await trusted.get_secure_data(SecureDataRequest(query="latest metrics"))
    print(f"  get_secure_data -> {resp2.data} (access_level={resp2.access_level})")

    # ── Step 6: Demonstrate rejection (consumer without credential) ─────
    print("\n--- Step 6: Unauthenticated consumer (rejected) ---")
    try:
        bad_client = AsterClient(
            endpoint_addr=addr_b64,
            # No enrollment_credential_file — will be rejected.
        )
        await bad_client.connect()
        print("  ERROR: should have been rejected!")
    except PermissionError as e:
        print(f"  Correctly rejected: {e}")
    except Exception as e:
        print(f"  Rejected (expected): {type(e).__name__}: {e}")

    # ── Cleanup ─────────────────────────────────────────────────────────
    print("\n--- Cleanup ---")
    await client.close()
    await srv.close()
    try:
        os.unlink(cred_file)
    except OSError:
        pass
    print("  Done. All resources released.")

    print("\n" + "=" * 60)
    print("  Trust workflow complete.")
    print("=" * 60)


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
