"""
End-to-end tests for Gate 0 (connection-level admission hook).

Spec reference: Aster-trust-spec.md section 3.3.

Gate 0 is the QUIC connection-level admission gate.  When enabled on
AsterServer (allow_all_consumers=False), a MeshEndpointHook runs in a
background loop that checks every incoming connection:

  - Admission ALPNs (aster.consumer_admission, aster.producer_admission)
    are ALWAYS allowed through so new peers can present credentials.
  - Non-admitted peers trying to use the RPC ALPN (aster/1) are denied
    at the QUIC handshake layer.
  - After successful admission (valid credential), the peer's EndpointId
    is added to the admitted set and RPC calls succeed.

Tests:
  1. Unadmitted peer is denied on the RPC ALPN.
  2. Admission ALPN is always open (even for unadmitted peers).
  3. Admitted peer can make RPC calls after presenting a valid credential.
"""

from __future__ import annotations

import asyncio
import time
from dataclasses import dataclass

import pytest

from aster import (
    AsterServer,
    AsterClient,
    AsterConfig,
    create_endpoint_with_config,
    EndpointConfig,
    service,
    rpc,
    wire_type,
)
from aster.high_level import RPC_ALPN
from aster.trust.hooks import ALPN_CONSUMER_ADMISSION, MeshEndpointHook
from aster.trust.credentials import ConsumerEnrollmentCredential
from aster.trust.consumer import (
    ConsumerAdmissionRequest,
    ConsumerAdmissionResponse,
    consumer_cred_to_json,
)
from aster.trust.signing import generate_root_keypair, sign_credential


# ── Test service + wire types ────────────────────────────────────────────────


@wire_type("test.gate0/PingRequest")
@dataclass
class PingRequest:
    payload: str = ""


@wire_type("test.gate0/PingResponse")
@dataclass
class PingResponse:
    payload: str = ""


@service("Gate0PingService", version=1)
class Gate0PingService:
    @rpc
    async def ping(self, req: PingRequest) -> PingResponse:
        return PingResponse(payload=f"pong:{req.payload}")


# ── Helpers ──────────────────────────────────────────────────────────────────


def _make_policy_cred(
    priv_raw: bytes,
    pub_raw: bytes,
) -> ConsumerEnrollmentCredential:
    """Create a valid policy credential signed with the given root key."""
    cred = ConsumerEnrollmentCredential(
        credential_type="policy",
        root_pubkey=pub_raw,
        expires_at=int(time.time()) + 3600,
        attributes={},
    )
    cred.signature = sign_credential(cred, priv_raw)
    return cred


# ── Tests ────────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
@pytest.mark.timeout(30)
async def test_unadmitted_peer_denied_on_rpc_alpn():
    """An unadmitted peer connecting directly on aster/1 is rejected by Gate 0.

    The server has allow_all_consumers=False and Gate 0 is active.  A bare
    endpoint that has never been through admission tries to open a bidi
    stream on the RPC ALPN.  The connection should fail because the hook
    loop denies the peer.
    """
    priv_raw, pub_raw = generate_root_keypair()

    async with AsterServer(
        services=[Gate0PingService()],
        root_pubkey=pub_raw,
        allow_all_consumers=False,
    ) as srv:
        # Create a bare client endpoint that tries to connect directly on
        # the RPC ALPN, bypassing admission entirely.
        ep = await create_endpoint_with_config(
            EndpointConfig(alpns=[RPC_ALPN])
        )
        try:
            server_addr = srv.node.node_addr_info()
            conn = await ep.connect_node_addr(server_addr, RPC_ALPN)

            # The connection itself may succeed at the QUIC level, but the
            # hook loop should deny it.  The denial surfaces when we try to
            # open a stream or the connection is reset.
            with pytest.raises(Exception):
                send, recv = await asyncio.wait_for(
                    conn.open_bi(), timeout=5
                )
                # If open_bi succeeds, try to actually use the stream --
                # the server side will have rejected the connection so the
                # stream should fail.
                await send.write_all(b"hello")
                await send.finish()
                await asyncio.wait_for(
                    recv.read_to_end(64 * 1024), timeout=5
                )
        finally:
            await ep.close()


@pytest.mark.asyncio
@pytest.mark.timeout(30)
async def test_admission_alpn_always_open():
    """An unadmitted peer can connect on consumer_admission ALPN.

    Even with Gate 0 active, admission ALPNs are always allowed so that
    new peers can present their credentials.
    """
    priv_raw, pub_raw = generate_root_keypair()

    async with AsterServer(
        services=[Gate0PingService()],
        root_pubkey=pub_raw,
        allow_all_consumers=False,
    ) as srv:
        # Create a bare endpoint and connect on the consumer_admission ALPN.
        ep = await create_endpoint_with_config(
            EndpointConfig(alpns=[ALPN_CONSUMER_ADMISSION])
        )
        try:
            server_addr = srv.node.node_addr_info()
            conn = await ep.connect_node_addr(
                server_addr, ALPN_CONSUMER_ADMISSION
            )

            # Open a bidi stream and send an admission request.  Even with
            # an empty credential, the server will respond (denied, but the
            # connection itself is not blocked by Gate 0).
            send, recv = await conn.open_bi()
            req = ConsumerAdmissionRequest(credential_json="")
            await send.write_all(req.to_json().encode())
            await send.finish()

            raw_resp = await asyncio.wait_for(
                recv.read_to_end(64 * 1024), timeout=10
            )
            resp = ConsumerAdmissionResponse.from_json(raw_resp)

            # The response should be denied (empty credential + gate closed),
            # but the important thing is that we GOT a response at all --
            # proving Gate 0 let the admission ALPN through.
            assert resp.admitted is False
        finally:
            await ep.close()


@pytest.mark.asyncio
@pytest.mark.timeout(30)
async def test_admitted_peer_can_use_rpc():
    """After admission with a valid credential, the peer can make RPC calls.

    Full flow:
      1. Server starts with Gate 0 active (allow_all_consumers=False).
      2. Client connects via AsterClient with a valid policy credential.
         AsterClient automatically runs the admission handshake.
      3. After admission, the client can call the RPC service.
    """
    priv_raw, pub_raw = generate_root_keypair()

    # Build a credential file-like inline credential for AsterClient.
    cred = _make_policy_cred(priv_raw, pub_raw)

    async with AsterServer(
        services=[Gate0PingService()],
        root_pubkey=pub_raw,
        allow_all_consumers=False,
    ) as srv:
        addr_b64 = srv.endpoint_addr_b64

        # AsterClient with an inline credential.  We provide it via the
        # config enrollment_credential mechanism by writing a temp file.
        import json
        import tempfile
        import os

        cred_dict = {
            "credential_type": cred.credential_type,
            "root_pubkey": cred.root_pubkey.hex(),
            "expires_at": cred.expires_at,
            "attributes": cred.attributes,
            "endpoint_id": cred.endpoint_id,
            "nonce": cred.nonce.hex() if cred.nonce else None,
            "signature": cred.signature.hex(),
        }
        with tempfile.NamedTemporaryFile(
            mode="w", suffix=".json", delete=False
        ) as tf:
            json.dump(cred_dict, tf)
            cred_path = tf.name

        try:
            client = AsterClient(
                endpoint_addr=addr_b64,
                root_pubkey=pub_raw,
                enrollment_credential_file=cred_path,
            )
            await client.connect()

            # The client is now admitted.  Make an RPC call.
            svc = await client.client(Gate0PingService)
            resp = await svc.ping(PingRequest(payload="gate0"))
            assert isinstance(resp, PingResponse)
            assert resp.payload == "pong:gate0"

            await client.close()
        finally:
            os.unlink(cred_path)


@pytest.mark.asyncio
@pytest.mark.timeout(30)
async def test_admission_then_rpc_manual_flow():
    """Manual admission flow followed by RPC, proving Gate 0 transitions.

    This test uses low-level primitives (no AsterClient) to demonstrate
    the exact sequence:
      1. Unadmitted peer connects on consumer_admission ALPN (allowed).
      2. Peer presents a valid credential and is admitted.
      3. Same peer connects on RPC ALPN (now allowed by Gate 0).
    """
    priv_raw, pub_raw = generate_root_keypair()

    async with AsterServer(
        services=[Gate0PingService()],
        root_pubkey=pub_raw,
        allow_all_consumers=False,
    ) as srv:
        # Peer endpoint that registers both ALPNs.
        ep = await create_endpoint_with_config(
            EndpointConfig(alpns=[ALPN_CONSUMER_ADMISSION, RPC_ALPN])
        )
        try:
            server_addr = srv.node.node_addr_info()

            # Step 1: Connect on consumer_admission and present credential.
            cred = _make_policy_cred(priv_raw, pub_raw)
            conn_adm = await ep.connect_node_addr(
                server_addr, ALPN_CONSUMER_ADMISSION
            )
            send, recv = await conn_adm.open_bi()
            req = ConsumerAdmissionRequest(
                credential_json=consumer_cred_to_json(cred)
            )
            await send.write_all(req.to_json().encode())
            await send.finish()

            raw_resp = await asyncio.wait_for(
                recv.read_to_end(64 * 1024), timeout=10
            )
            resp = ConsumerAdmissionResponse.from_json(raw_resp)
            assert resp.admitted is True
            assert len(resp.services) == 1
            assert resp.services[0].name == "Gate0PingService"

            # Step 2: Now connect on the RPC ALPN.  Gate 0 should let us
            # through because we were admitted in step 1.
            from aster.client import create_client

            conn_rpc = await ep.connect_node_addr(server_addr, RPC_ALPN)
            client = create_client(Gate0PingService, connection=conn_rpc)
            resp = await client.ping(PingRequest(payload="manual"))
            assert isinstance(resp, PingResponse)
            assert resp.payload == "pong:manual"
            await client.close()
        finally:
            await ep.close()
