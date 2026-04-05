"""
End-to-end tests for the consumer admission flow (§3.2, §3.2.2).

Proves the Y.2 Dynamic Client Flow from Aster-E2E-flow.md:
  1. Node A publishes a contract and starts a consumer admission listener.
  2. Node B presents a consumer credential over aster.consumer_admission.
  3. Node B receives ConsumerAdmissionResponse with services[] and registry_ticket.
  4. Node B joins the registry doc and resolves the service.

Also tests unit-level behaviour:
  - ConsumerAdmissionRequest / ConsumerAdmissionResponse serialisation round-trips
  - handle_consumer_admission_rpc with valid and invalid credentials
  - Denial produces no reason leak
"""

from __future__ import annotations

import asyncio
import json
import secrets
import time

import pytest

from aster.registry.models import ServiceSummary
from aster.trust.consumer import (
    ConsumerAdmissionRequest,
    ConsumerAdmissionResponse,
    consumer_cred_to_json,
    handle_consumer_admission_rpc,
)
from aster.trust.credentials import ConsumerEnrollmentCredential
from aster.trust.hooks import MeshEndpointHook
from aster.trust.nonces import InMemoryNonceStore
from aster.trust.signing import generate_root_keypair, sign_credential


# ── Helpers ────────────────────────────────────────────────────────────────────


def _make_policy_cred(
    priv_raw: bytes | None = None,
    pub_raw: bytes | None = None,
    attributes: dict | None = None,
) -> tuple[ConsumerEnrollmentCredential, bytes, bytes]:
    if priv_raw is None or pub_raw is None:
        priv_raw, pub_raw = generate_root_keypair()
    cred = ConsumerEnrollmentCredential(
        credential_type="policy",
        root_pubkey=pub_raw,
        expires_at=int(time.time()) + 3600,
        attributes=attributes or {},
    )
    cred.signature = sign_credential(cred, priv_raw)
    return cred, priv_raw, pub_raw


def _make_ott_cred(
    priv_raw: bytes | None = None,
    pub_raw: bytes | None = None,
) -> tuple[ConsumerEnrollmentCredential, bytes, bytes]:
    if priv_raw is None or pub_raw is None:
        priv_raw, pub_raw = generate_root_keypair()
    cred = ConsumerEnrollmentCredential(
        credential_type="ott",
        root_pubkey=pub_raw,
        expires_at=int(time.time()) + 3600,
        nonce=secrets.token_bytes(32),
    )
    cred.signature = sign_credential(cred, priv_raw)
    return cred, priv_raw, pub_raw


# ── Unit: ServiceSummary ───────────────────────────────────────────────────────


def test_service_summary_roundtrip():
    s = ServiceSummary(
        name="MyService",
        version=1,
        contract_id="a" * 64,
        channels={"stable": "b" * 64},
    )
    d = s.to_json_dict()
    s2 = ServiceSummary.from_json_dict(d)
    assert s2.name == "MyService"
    assert s2.version == 1
    assert s2.contract_id == "a" * 64
    assert s2.channels == {"stable": "b" * 64}


def test_service_summary_no_channels():
    s = ServiceSummary(name="Svc", version=2, contract_id="c" * 64, channels={})
    assert ServiceSummary.from_json_dict(s.to_json_dict()).channels == {}


# ── Unit: ConsumerAdmissionResponse serialisation ─────────────────────────────


def test_response_roundtrip_admitted():
    svc = ServiceSummary("X", 1, "d" * 64, {})
    resp = ConsumerAdmissionResponse(
        admitted=True,
        attributes={"aster.role": "reader"},
        services=[svc],
        registry_ticket="ticket_abc",
        root_pubkey="e" * 64,
    )
    resp2 = ConsumerAdmissionResponse.from_json(resp.to_json())
    assert resp2.admitted is True
    assert resp2.attributes == {"aster.role": "reader"}
    assert len(resp2.services) == 1
    assert resp2.services[0].name == "X"
    assert resp2.registry_ticket == "ticket_abc"
    assert resp2.root_pubkey == "e" * 64


def test_response_roundtrip_denied():
    resp = ConsumerAdmissionResponse(admitted=False, root_pubkey="f" * 64)
    resp2 = ConsumerAdmissionResponse.from_json(resp.to_json())
    assert resp2.admitted is False
    assert resp2.services == []
    assert resp2.registry_ticket == ""


def test_response_reason_not_in_wire():
    """Reason must never appear in the serialised response."""
    resp = ConsumerAdmissionResponse(
        admitted=False, reason="nonce already consumed", root_pubkey="0" * 64
    )
    wire = json.loads(resp.to_json())
    assert wire.get("reason") == ""   # always empty on wire


# ── Unit: ConsumerAdmissionRequest serialisation ──────────────────────────────


def test_request_roundtrip():
    req = ConsumerAdmissionRequest(credential_json='{"x":1}', iid_token="tok")
    req2 = ConsumerAdmissionRequest.from_json(req.to_json())
    assert req2.credential_json == '{"x":1}'
    assert req2.iid_token == "tok"


# ── Unit: handle_consumer_admission_rpc ───────────────────────────────────────


@pytest.mark.asyncio
async def test_handle_rpc_policy_admitted():
    cred, priv_raw, pub_raw = _make_policy_cred()
    hook = MeshEndpointHook()
    svc = ServiceSummary("Svc", 1, "a" * 64, {})

    req = ConsumerAdmissionRequest(
        credential_json=consumer_cred_to_json(cred)
    )
    resp = await handle_consumer_admission_rpc(
        req.to_json(),
        root_pubkey=pub_raw,
        hook=hook,
        peer_node_id="peer_abc",
        services=[svc],
        registry_ticket="ticket_xyz",
    )
    assert resp.admitted is True
    assert "peer_abc" in hook.admitted
    assert len(resp.services) == 1
    assert resp.services[0].name == "Svc"
    assert resp.registry_ticket == "ticket_xyz"
    assert resp.root_pubkey == pub_raw.hex()


@pytest.mark.asyncio
async def test_handle_rpc_ott_admitted_and_nonce_consumed():
    cred, _, pub_raw = _make_ott_cred()
    hook = MeshEndpointHook()
    nonce_store = InMemoryNonceStore()

    req = ConsumerAdmissionRequest(credential_json=consumer_cred_to_json(cred))
    resp = await handle_consumer_admission_rpc(
        req.to_json(),
        root_pubkey=pub_raw,
        hook=hook,
        peer_node_id="peer_ott",
        nonce_store=nonce_store,
    )
    assert resp.admitted is True
    assert "peer_ott" in hook.admitted

    # Replay must be denied
    resp2 = await handle_consumer_admission_rpc(
        req.to_json(),
        root_pubkey=pub_raw,
        hook=hook,
        peer_node_id="peer_ott",
        nonce_store=nonce_store,
    )
    assert resp2.admitted is False
    assert resp2.reason == ""   # no oracle leak


@pytest.mark.asyncio
async def test_handle_rpc_bad_signature_denied():
    cred, _, pub_raw = _make_policy_cred()
    cred.signature = b"\x00" * 64   # corrupt signature
    hook = MeshEndpointHook()

    req = ConsumerAdmissionRequest(credential_json=consumer_cred_to_json(cred))
    resp = await handle_consumer_admission_rpc(
        req.to_json(),
        root_pubkey=pub_raw,
        hook=hook,
        peer_node_id="bad_peer",
    )
    assert resp.admitted is False
    assert "bad_peer" not in hook.admitted
    assert resp.reason == ""


@pytest.mark.asyncio
async def test_handle_rpc_malformed_request_denied():
    hook = MeshEndpointHook()
    resp = await handle_consumer_admission_rpc(
        "not_valid_json",
        root_pubkey=b"\x00" * 32,
        hook=hook,
        peer_node_id="bad_peer",
    )
    assert resp.admitted is False


@pytest.mark.asyncio
async def test_handle_rpc_expired_credential_denied():
    cred, priv_raw, pub_raw = _make_policy_cred()
    cred.expires_at = int(time.time()) - 1   # expired
    cred.signature = sign_credential(cred, priv_raw)
    hook = MeshEndpointHook()

    req = ConsumerAdmissionRequest(credential_json=consumer_cred_to_json(cred))
    resp = await handle_consumer_admission_rpc(
        req.to_json(),
        root_pubkey=pub_raw,
        hook=hook,
        peer_node_id="expired_peer",
    )
    assert resp.admitted is False
    assert resp.reason == ""


# ── Integration: E2E flow (requires iroh) ─────────────────────────────────────


CONSUMER_ALPN = b"aster.consumer_admission"


@pytest.mark.asyncio
async def test_e2e_admission_returns_services_and_ticket():
    """
    Y.2 Dynamic Client Flow end-to-end (§3.2 + §3.2.2):

    Node A: starts a consumer admission endpoint; publishes a fake service.
    Node B: presents a policy credential; receives services[] + registry_ticket.
    """
    from aster import IrohNode, docs_client, create_endpoint_with_config, EndpointConfig

    # ── Node A setup ──────────────────────────────────────────────────────────
    node_a = await IrohNode.memory()
    dc_a = docs_client(node_a)
    doc_a = await dc_a.create()
    author_a = await dc_a.create_author()

    priv_raw, pub_raw = generate_root_keypair()
    hook_a = MeshEndpointHook()

    services_a = [ServiceSummary("GreetService", 1, "a" * 64, {})]

    # Admission endpoint for node A (separate bare endpoint on consumer_admission ALPN)
    ep_a = await create_endpoint_with_config(EndpointConfig(alpns=[CONSUMER_ALPN]))
    ep_a_addr = ep_a.endpoint_addr_info()
    ticket_a = await doc_a.share("read")

    admitted_event = asyncio.Event()

    async def _serve():
        from aster.trust.consumer import handle_consumer_admission_rpc

        conn = await ep_a.accept()
        peer_id = conn.remote_id()
        send, recv = await conn.accept_bi()
        raw = await recv.read_to_end(64 * 1024)
        resp = await handle_consumer_admission_rpc(
            raw.decode(),
            root_pubkey=pub_raw,
            hook=hook_a,
            peer_node_id=peer_id,
            services=services_a,
            registry_ticket=ticket_a,
        )
        await send.write_all(resp.to_json().encode())
        await send.finish()
        admitted_event.set()

    serve_task = asyncio.create_task(_serve())

    # ── Node B setup ──────────────────────────────────────────────────────────
    cred_b, _, _ = _make_policy_cred(priv_raw=priv_raw, pub_raw=pub_raw)
    ep_b = await create_endpoint_with_config(EndpointConfig(alpns=[CONSUMER_ALPN]))

    conn_b = await ep_b.connect_node_addr(ep_a_addr, CONSUMER_ALPN)
    send_b, recv_b = await conn_b.open_bi()

    req_b = ConsumerAdmissionRequest(credential_json=consumer_cred_to_json(cred_b))
    await send_b.write_all(req_b.to_json().encode())
    await send_b.finish()

    raw_resp = await recv_b.read_to_end(64 * 1024)
    resp_b = ConsumerAdmissionResponse.from_json(raw_resp)

    await asyncio.wait_for(admitted_event.wait(), timeout=10)
    await serve_task

    # ── Assertions ────────────────────────────────────────────────────────────
    assert resp_b.admitted is True
    assert len(resp_b.services) == 1
    assert resp_b.services[0].name == "GreetService"
    assert resp_b.registry_ticket == ticket_a
    assert resp_b.root_pubkey == pub_raw.hex()
    assert resp_b.reason == ""

    # Node B joins registry doc using the ticket
    node_b = await IrohNode.memory()
    node_b.add_node_addr(node_a)
    node_a.add_node_addr(node_b)
    dc_b = docs_client(node_b)
    doc_b, _ = await dc_b.join_and_subscribe(resp_b.registry_ticket)
    assert doc_b is not None

    await ep_a.close()
    await ep_b.close()


@pytest.mark.asyncio
async def test_e2e_admission_denied_with_wrong_key():
    """Credential signed with a different key is denied end-to-end."""
    from aster import create_endpoint_with_config, EndpointConfig

    _, server_pub = generate_root_keypair()   # server trusts this key
    priv_b, pub_b = generate_root_keypair()   # client uses a different key
    cred_b, _, _ = _make_policy_cred(priv_raw=priv_b, pub_raw=pub_b)

    hook_a = MeshEndpointHook()
    ep_a = await create_endpoint_with_config(EndpointConfig(alpns=[CONSUMER_ALPN]))
    ep_a_addr = ep_a.endpoint_addr_info()

    denied_event = asyncio.Event()

    async def _serve():
        conn = await ep_a.accept()
        peer_id = conn.remote_id()
        send, recv = await conn.accept_bi()
        raw = await recv.read_to_end(64 * 1024)
        resp = await handle_consumer_admission_rpc(
            raw.decode(),
            root_pubkey=server_pub,   # different from client's pub_b
            hook=hook_a,
            peer_node_id=peer_id,
        )
        await send.write_all(resp.to_json().encode())
        await send.finish()
        denied_event.set()

    serve_task = asyncio.create_task(_serve())

    ep_b = await create_endpoint_with_config(EndpointConfig(alpns=[CONSUMER_ALPN]))
    conn_b = await ep_b.connect_node_addr(ep_a_addr, CONSUMER_ALPN)
    send_b, recv_b = await conn_b.open_bi()

    req_b = ConsumerAdmissionRequest(credential_json=consumer_cred_to_json(cred_b))
    await send_b.write_all(req_b.to_json().encode())
    await send_b.finish()

    raw_resp = await recv_b.read_to_end(64 * 1024)
    resp_b = ConsumerAdmissionResponse.from_json(raw_resp)

    await asyncio.wait_for(denied_event.wait(), timeout=10)
    await serve_task

    assert resp_b.admitted is False
    assert resp_b.reason == ""
    assert hook_a.admitted == set()

    await ep_a.close()
    await ep_b.close()
