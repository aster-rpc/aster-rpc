"""
Tests for Phase 11: Trust Foundations.

Covers:
- EnrollmentCredential + ConsumerEnrollmentCredential signing / verification
- Valid / tampered / expired / wrong endpoint_id cases
- OTT nonce: consumed once, second call → False
- OTT nonce malformed length (16, 31, 33, 64 bytes) → rejected
- Policy credential with nonce → rejected as malformed
- Policy credential: reusable within expiry
- IID verification (mocked): matching claims → True; mismatched → False
- MeshEndpointHook: admission ALPN always allowed; unenrolled denied; admitted allowed
- CLI: keygen produces valid ed25519 pair; sign output verifies
- LocalTransport bypass: CallContext.peer is None, attributes == {}
- Auth interceptor: peer is None → allow (in-process trust)
"""

from __future__ import annotations

import json
import os
import secrets
import tempfile
import time
from pathlib import Path

import pytest

from aster.trust import (
    ALPN_CONSUMER_ADMISSION,
    ALPN_PRODUCER_ADMISSION,
    ConsumerEnrollmentCredential,
    EnrollmentCredential,
    InMemoryNonceStore,
    MeshEndpointHook,
    MockIIDBackend,
    admit,
    canonical_json,
    canonical_signing_bytes,
    check_offline,
    check_runtime,
    generate_root_keypair,
    sign_credential,
    verify_signature,
)
from aster.trust.hooks import _ADMISSION_ALPNS


# ── Key generation helpers ─────────────────────────────────────────────────────


def _make_signed_producer_cred(
    *,
    endpoint_id: str = "abc123",
    expires_at: int | None = None,
    attributes: dict | None = None,
    priv_raw: bytes | None = None,
    pub_raw: bytes | None = None,
) -> tuple[EnrollmentCredential, bytes, bytes]:
    """Return (signed_cred, priv_raw, pub_raw)."""
    if priv_raw is None or pub_raw is None:
        priv_raw, pub_raw = generate_root_keypair()
    if expires_at is None:
        expires_at = int(time.time()) + 3600  # 1 hour
    cred = EnrollmentCredential(
        endpoint_id=endpoint_id,
        root_pubkey=pub_raw,
        expires_at=expires_at,
        attributes=attributes or {},
    )
    cred.signature = sign_credential(cred, priv_raw)
    return cred, priv_raw, pub_raw


def _make_signed_consumer_cred(
    credential_type: str = "policy",
    *,
    endpoint_id: str | None = None,
    expires_at: int | None = None,
    attributes: dict | None = None,
    nonce: bytes | None = None,
    priv_raw: bytes | None = None,
    pub_raw: bytes | None = None,
) -> tuple[ConsumerEnrollmentCredential, bytes, bytes]:
    """Return (signed_cred, priv_raw, pub_raw)."""
    if priv_raw is None or pub_raw is None:
        priv_raw, pub_raw = generate_root_keypair()
    if expires_at is None:
        expires_at = int(time.time()) + 3600
    if credential_type == "ott" and nonce is None:
        nonce = secrets.token_bytes(32)
    cred = ConsumerEnrollmentCredential(
        credential_type=credential_type,
        root_pubkey=pub_raw,
        expires_at=expires_at,
        attributes=attributes or {},
        endpoint_id=endpoint_id,
        nonce=nonce,
    )
    cred.signature = sign_credential(cred, priv_raw)
    return cred, priv_raw, pub_raw


# ── canonical_json ─────────────────────────────────────────────────────────────


def test_canonical_json_sorted_keys():
    result = canonical_json({"z": "last", "a": "first", "m": "mid"})
    parsed = json.loads(result)
    assert list(parsed.keys()) == ["a", "m", "z"]


def test_canonical_json_empty():
    assert canonical_json({}) == b"{}"


def test_canonical_json_unicode():
    result = canonical_json({"k": "café"})
    assert "café".encode("utf-8") in result


# ── Producer credential: sign + verify ────────────────────────────────────────


def test_producer_sign_and_verify():
    cred, priv_raw, pub_raw = _make_signed_producer_cred()
    assert verify_signature(cred) is True


def test_producer_tampered_signature_rejected():
    cred, _, _ = _make_signed_producer_cred()
    tampered = bytearray(cred.signature)
    tampered[0] ^= 0xFF
    cred.signature = bytes(tampered)
    assert verify_signature(cred) is False


def test_producer_tampered_endpoint_id_rejected():
    cred, _, _ = _make_signed_producer_cred(endpoint_id="legit_node")
    cred.endpoint_id = "attacker_node"  # mutation after signing
    assert verify_signature(cred) is False


def test_producer_tampered_attributes_rejected():
    cred, _, _ = _make_signed_producer_cred(attributes={"aster.role": "producer"})
    cred.attributes["aster.role"] = "admin"  # mutation after signing
    assert verify_signature(cred) is False


# ── Consumer credential: sign + verify ────────────────────────────────────────


def test_consumer_policy_sign_and_verify():
    cred, _, _ = _make_signed_consumer_cred("policy")
    assert verify_signature(cred) is True


def test_consumer_ott_sign_and_verify():
    nonce = secrets.token_bytes(32)
    cred, _, _ = _make_signed_consumer_cred("ott", nonce=nonce)
    assert verify_signature(cred) is True


def test_consumer_flip_policy_to_ott_rejected():
    """Flipping credential_type invalidates signature."""
    cred, _, _ = _make_signed_consumer_cred("policy")
    cred.credential_type = "ott"
    cred.nonce = secrets.token_bytes(32)  # add nonce after signing
    assert verify_signature(cred) is False


def test_consumer_inject_nonce_into_policy_rejected():
    """Adding a nonce to a policy credential after signing invalidates it."""
    cred, _, _ = _make_signed_consumer_cred("policy")
    cred.nonce = secrets.token_bytes(32)  # inject nonce
    assert verify_signature(cred) is False


def test_consumer_remove_ott_endpoint_id_rejected():
    """Removing endpoint_id from OTT credential after signing invalidates it."""
    cred, _, _ = _make_signed_consumer_cred("ott", endpoint_id="node1")
    cred.endpoint_id = None
    assert verify_signature(cred) is False


# ── check_offline: producer ───────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_producer_offline_valid():
    cred, _, _ = _make_signed_producer_cred(endpoint_id="node1")
    result = await check_offline(cred, "node1")
    assert result.admitted is True
    assert result.attributes == {}


@pytest.mark.asyncio
async def test_producer_offline_expired():
    cred, _, _ = _make_signed_producer_cred(
        endpoint_id="node1", expires_at=int(time.time()) - 1
    )
    result = await check_offline(cred, "node1")
    assert result.admitted is False
    assert "expired" in (result.reason or "")


@pytest.mark.asyncio
async def test_producer_offline_wrong_endpoint_id():
    cred, _, _ = _make_signed_producer_cred(endpoint_id="legit_node")
    result = await check_offline(cred, "attacker_node")
    assert result.admitted is False
    assert "endpoint_id" in (result.reason or "")


@pytest.mark.asyncio
async def test_producer_offline_invalid_signature():
    cred, _, _ = _make_signed_producer_cred()
    cred.signature = b"\x00" * 64
    result = await check_offline(cred, cred.endpoint_id)
    assert result.admitted is False
    assert "signature" in (result.reason or "")


# ── check_offline: OTT consumer ───────────────────────────────────────────────


@pytest.mark.asyncio
async def test_ott_nonce_consumed_once():
    nonce = secrets.token_bytes(32)
    cred, _, _ = _make_signed_consumer_cred("ott", nonce=nonce)
    store = InMemoryNonceStore()
    result1 = await check_offline(cred, "any_peer", store)
    assert result1.admitted is True
    # Re-sign with same nonce (simulate reuse attempt)
    result2 = await check_offline(cred, "any_peer", store)
    assert result2.admitted is False
    assert "nonce" in (result2.reason or "")


@pytest.mark.asyncio
async def test_ott_no_nonce_store_rejected():
    nonce = secrets.token_bytes(32)
    cred, _, _ = _make_signed_consumer_cred("ott", nonce=nonce)
    result = await check_offline(cred, "any_peer", nonce_store=None)
    assert result.admitted is False
    assert "nonce_store" in (result.reason or "")


@pytest.mark.asyncio
async def test_ott_nonce_wrong_length_rejected():
    """OTT credentials with nonce != 32 bytes must be rejected as malformed."""
    priv_raw, pub_raw = generate_root_keypair()
    for bad_len in [16, 31, 33, 64]:
        bad_nonce = secrets.token_bytes(bad_len)
        cred = ConsumerEnrollmentCredential(
            credential_type="ott",
            root_pubkey=pub_raw,
            expires_at=int(time.time()) + 3600,
            nonce=bad_nonce,
        )
        # Sign the malformed cred to get past signature check
        cred.signature = sign_credential(cred, priv_raw)
        store = InMemoryNonceStore()
        # Validation happens at structural level via nonce_store.consume
        with pytest.raises(ValueError, match="32 bytes"):
            await store.consume(bad_nonce)
        # check_offline also calls nonce_store.consume — must propagate error
        result = await check_offline(cred, "peer", store)
        assert result.admitted is False, f"Should reject nonce of length {bad_len}"


@pytest.mark.asyncio
async def test_policy_with_nonce_rejected():
    """Policy credential that carries a nonce is structurally invalid."""
    priv_raw, pub_raw = generate_root_keypair()
    bad_nonce = secrets.token_bytes(32)
    cred = ConsumerEnrollmentCredential(
        credential_type="policy",
        root_pubkey=pub_raw,
        expires_at=int(time.time()) + 3600,
        nonce=bad_nonce,  # MUST NOT carry nonce
    )
    cred.signature = sign_credential(cred, priv_raw)
    store = InMemoryNonceStore()
    result = await check_offline(cred, "peer", store)
    assert result.admitted is False
    assert "nonce" in (result.reason or "")


@pytest.mark.asyncio
async def test_policy_credential_reusable():
    """Policy credentials are reusable within expiry."""
    cred, _, _ = _make_signed_consumer_cred("policy")
    # Call check_offline multiple times — no nonce_store needed
    for _ in range(5):
        result = await check_offline(cred, "any_peer")
        assert result.admitted is True


@pytest.mark.asyncio
async def test_consumer_ott_endpoint_id_mismatch():
    nonce = secrets.token_bytes(32)
    cred, _, _ = _make_signed_consumer_cred("ott", endpoint_id="bound_node", nonce=nonce)
    store = InMemoryNonceStore()
    result = await check_offline(cred, "different_node", store)
    assert result.admitted is False
    assert "endpoint_id" in (result.reason or "")


@pytest.mark.asyncio
async def test_consumer_ott_no_endpoint_id_any_peer():
    """OTT without endpoint_id binding is usable by any peer (once)."""
    nonce = secrets.token_bytes(32)
    cred, _, _ = _make_signed_consumer_cred("ott", endpoint_id=None, nonce=nonce)
    store = InMemoryNonceStore()
    result = await check_offline(cred, "any_random_peer", store)
    assert result.admitted is True


# ── check_runtime: IID ────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_iid_mock_pass():
    cred, _, _ = _make_signed_producer_cred(
        attributes={"aster.iid_provider": "aws", "aster.iid_account": "123456789012"}
    )
    mock = MockIIDBackend(should_pass=True)
    result = await check_runtime(cred, iid_backend=mock)
    assert result.admitted is True
    assert mock.call_count == 1


@pytest.mark.asyncio
async def test_iid_mock_fail():
    cred, _, _ = _make_signed_producer_cred(
        attributes={"aster.iid_provider": "aws", "aster.iid_account": "wrong_account"}
    )
    mock = MockIIDBackend(should_pass=False, reason="account mismatch")
    result = await check_runtime(cred, iid_backend=mock)
    assert result.admitted is False
    assert "account mismatch" in (result.reason or "")


@pytest.mark.asyncio
async def test_iid_skipped_when_no_iid_attribute():
    """check_runtime skips IID check when aster.iid_provider not present."""
    cred, _, _ = _make_signed_producer_cred(attributes={"aster.role": "producer"})
    mock = MockIIDBackend(should_pass=True)
    result = await check_runtime(cred, iid_backend=mock)
    assert result.admitted is True
    assert mock.call_count == 0  # never called


@pytest.mark.asyncio
async def test_iid_attribute_mismatch():
    cred, _, _ = _make_signed_producer_cred(
        attributes={"aster.iid_provider": "aws", "aster.iid_account": "expected_account"}
    )
    mock = MockIIDBackend(
        should_pass=True,
        expected_attributes={"aster.iid_account": "different_account"},
    )
    result = await check_runtime(cred, iid_backend=mock)
    assert result.admitted is False


# ── admit() ───────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_admit_full_success():
    cred, _, _ = _make_signed_producer_cred(
        endpoint_id="ep1", attributes={"aster.role": "producer"}
    )
    result = await admit(cred, "ep1")
    assert result.admitted is True
    assert result.attributes == {"aster.role": "producer"}


@pytest.mark.asyncio
async def test_admit_offline_fail_skips_runtime():
    cred, _, _ = _make_signed_producer_cred(endpoint_id="ep1")
    cred.signature = b"\x00" * 64  # bad signature
    mock = MockIIDBackend(should_pass=True)
    result = await admit(cred, "ep1", iid_backend=mock)
    assert result.admitted is False
    assert mock.call_count == 0  # runtime not called


# ── NonceStore (InMemory) ─────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_in_memory_nonce_store_consume_once():
    store = InMemoryNonceStore()
    nonce = secrets.token_bytes(32)
    assert await store.consume(nonce) is True
    assert await store.consume(nonce) is False


@pytest.mark.asyncio
async def test_in_memory_nonce_store_independent_nonces():
    store = InMemoryNonceStore()
    n1 = secrets.token_bytes(32)
    n2 = secrets.token_bytes(32)
    assert await store.consume(n1) is True
    assert await store.consume(n2) is True


@pytest.mark.asyncio
async def test_in_memory_nonce_store_wrong_length_raises():
    store = InMemoryNonceStore()
    for bad_len in [0, 16, 31, 33, 64]:
        with pytest.raises(ValueError, match="32 bytes"):
            await store.consume(secrets.token_bytes(bad_len))


@pytest.mark.asyncio
async def test_in_memory_nonce_store_is_consumed():
    store = InMemoryNonceStore()
    nonce = secrets.token_bytes(32)
    assert await store.is_consumed(nonce) is False
    await store.consume(nonce)
    assert await store.is_consumed(nonce) is True


# ── NonceStore (File backend) ─────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_file_nonce_store_consume_once():
    from aster.trust.nonces import NonceStore

    with tempfile.TemporaryDirectory() as d:
        store = NonceStore(Path(d) / "nonces.json")
        nonce = secrets.token_bytes(32)
        assert await store.consume(nonce) is True
        assert await store.consume(nonce) is False


@pytest.mark.asyncio
async def test_file_nonce_store_persists_across_instances():
    from aster.trust.nonces import NonceStore

    with tempfile.TemporaryDirectory() as d:
        path = Path(d) / "nonces.json"
        nonce = secrets.token_bytes(32)

        store1 = NonceStore(path)
        assert await store1.consume(nonce) is True

        # New instance loads from file
        store2 = NonceStore(path)
        store2._cache = None  # clear in-memory cache to force file reload
        assert await store2.consume(nonce) is False


@pytest.mark.asyncio
async def test_file_nonce_store_missing_file_is_fresh():
    from aster.trust.nonces import NonceStore

    with tempfile.TemporaryDirectory() as d:
        store = NonceStore(Path(d) / "subdir" / "nonces.json")
        nonce = secrets.token_bytes(32)
        assert await store.consume(nonce) is True


# ── MeshEndpointHook ──────────────────────────────────────────────────────────


def test_mesh_hook_admission_alpn_always_allowed():
    hook = MeshEndpointHook(allow_unenrolled=False)
    for alpn in [ALPN_PRODUCER_ADMISSION, ALPN_CONSUMER_ADMISSION]:
        assert hook.should_allow("unknown_peer", alpn) is True


def test_mesh_hook_rejects_unenrolled_on_normal_alpn():
    hook = MeshEndpointHook(allow_unenrolled=False)
    assert hook.should_allow("unknown_peer", b"aster/1") is False


def test_mesh_hook_allows_admitted_peer():
    hook = MeshEndpointHook(allow_unenrolled=False)
    hook.add_peer("trusted_ep")
    assert hook.should_allow("trusted_ep", b"aster/1") is True


def test_mesh_hook_removes_peer():
    hook = MeshEndpointHook(allow_unenrolled=False)
    hook.add_peer("ep1")
    hook.remove_peer("ep1")
    assert hook.should_allow("ep1", b"aster/1") is False


def test_mesh_hook_allow_unenrolled_mode():
    hook = MeshEndpointHook(allow_unenrolled=True)
    assert hook.should_allow("any_peer", b"aster/1") is True


def test_mesh_hook_admission_alpns_in_frozenset():
    assert ALPN_PRODUCER_ADMISSION in _ADMISSION_ALPNS
    assert ALPN_CONSUMER_ADMISSION in _ADMISSION_ALPNS


# ── CLI: keygen ───────────────────────────────────────────────────────────────


def test_cli_keygen_produces_valid_key_pair():
    from aster_cli.trust import _keygen_command
    from aster.trust.signing import load_private_key, load_public_key

    with tempfile.TemporaryDirectory() as d:
        key_path = Path(d) / "root.key"

        class FakeArgs:
            out_key = str(key_path)

        ret = _keygen_command(FakeArgs())
        assert ret == 0
        assert key_path.exists()

        with open(key_path) as f:
            key_data = json.load(f)
        priv_raw = bytes.fromhex(key_data["private_key"])
        pub_raw = bytes.fromhex(key_data["public_key"])
        assert len(priv_raw) == 32
        assert len(pub_raw) == 32

        # Verify key pair is consistent: sign something and verify
        privkey = load_private_key(priv_raw)
        pubkey = load_public_key(pub_raw)
        msg = b"test message"
        sig = privkey.sign(msg)
        pubkey.verify(sig, msg)  # raises on failure


def test_cli_keygen_refuses_existing_file():
    from aster_cli.trust import _keygen_command

    with tempfile.NamedTemporaryFile(delete=False) as tf:
        tf.write(b"existing content")
        existing_path = tf.name

    try:
        class FakeArgs:
            out_key = existing_path

        ret = _keygen_command(FakeArgs())
        assert ret == 1  # must refuse
    finally:
        os.unlink(existing_path)


# ── CLI: sign ─────────────────────────────────────────────────────────────────


def test_cli_sign_producer_credential():
    from aster_cli.trust import _keygen_command, _sign_command

    with tempfile.TemporaryDirectory() as d:
        key_path = Path(d) / "root.key"
        cred_path = Path(d) / "cred.json"

        class Keygen:
            out_key = str(key_path)

        _keygen_command(Keygen())

        class Sign:
            root_key = str(key_path)
            endpoint_id = "deadbeef123"
            attributes = json.dumps({"aster.role": "producer"})
            expires = "2030-01-01T00:00:00Z"
            type = "producer"
            out = str(cred_path)

        ret = _sign_command(Sign())
        assert ret == 0
        assert cred_path.exists()

        with open(cred_path) as f:
            doc = json.load(f)
        assert doc["type"] == "producer"
        assert doc["endpoint_id"] == "deadbeef123"
        assert doc["attributes"]["aster.role"] == "producer"
        sig = bytes.fromhex(doc["signature"])
        assert len(sig) == 64

        # Reconstruct credential and verify signature
        cred = EnrollmentCredential(
            endpoint_id=doc["endpoint_id"],
            root_pubkey=bytes.fromhex(doc["root_pubkey"]),
            expires_at=doc["expires_at"],
            attributes=doc["attributes"],
            signature=sig,
        )
        assert verify_signature(cred) is True


def test_cli_sign_ott_credential():
    from aster_cli.trust import _keygen_command, _sign_command

    with tempfile.TemporaryDirectory() as d:
        key_path = Path(d) / "root.key"
        cred_path = Path(d) / "ott.json"

        class Keygen:
            out_key = str(key_path)

        _keygen_command(Keygen())

        class Sign:
            root_key = str(key_path)
            endpoint_id = None
            attributes = None
            expires = None
            type = "ott"
            out = str(cred_path)

        ret = _sign_command(Sign())
        assert ret == 0

        with open(cred_path) as f:
            doc = json.load(f)
        assert doc["type"] == "ott"
        assert doc["nonce"] is not None
        nonce = bytes.fromhex(doc["nonce"])
        assert len(nonce) == 32

        cred = ConsumerEnrollmentCredential(
            credential_type="ott",
            root_pubkey=bytes.fromhex(doc["root_pubkey"]),
            expires_at=doc["expires_at"],
            attributes=doc.get("attributes") or {},
            endpoint_id=doc.get("endpoint_id"),
            nonce=nonce,
            signature=bytes.fromhex(doc["signature"]),
        )
        assert verify_signature(cred) is True


# ── LocalTransport bypass ─────────────────────────────────────────────────────


def test_local_transport_call_context_peer_is_none():
    """LocalTransport CallContext.peer is None — Gates 0 and 1 are bypassed."""
    from aster.interceptors import CallContext

    ctx = CallContext(service="TestService", method="DoSomething")
    assert ctx.peer is None
    assert ctx.attributes == {}


def test_auth_interceptor_allows_when_peer_is_none():
    """Auth interceptor canonical behaviour: peer is None → allow (in-process trust)."""
    from aster.interceptors import AuthInterceptor, CallContext

    interceptor = AuthInterceptor()
    ctx = CallContext(service="TestService", method="DoSomething")
    assert ctx.peer is None

    # The auth interceptor should not raise when peer is None
    # (in-process LocalTransport calls are trusted by construction)
    import asyncio

    async def run():
        # on_request is the hook used by server dispatch; peer=None means in-process
        result = await interceptor.on_request(ctx, b"request")
        return result

    # Should complete without raising
    asyncio.get_event_loop().run_until_complete(run())


# ── Canonical signing bytes determinism ───────────────────────────────────────


def test_signing_bytes_deterministic():
    priv_raw, pub_raw = generate_root_keypair()
    cred = EnrollmentCredential(
        endpoint_id="ep1",
        root_pubkey=pub_raw,
        expires_at=9999999999,
        attributes={"a": "1", "b": "2"},
    )
    b1 = canonical_signing_bytes(cred)
    b2 = canonical_signing_bytes(cred)
    assert b1 == b2


def test_consumer_signing_bytes_type_code_distinct():
    """Policy and OTT produce different canonical bytes (type code differs)."""
    priv_raw, pub_raw = generate_root_keypair()
    policy = ConsumerEnrollmentCredential(
        credential_type="policy",
        root_pubkey=pub_raw,
        expires_at=9999999999,
    )
    ott = ConsumerEnrollmentCredential(
        credential_type="ott",
        root_pubkey=pub_raw,
        expires_at=9999999999,
        nonce=secrets.token_bytes(32),
    )
    assert canonical_signing_bytes(policy) != canonical_signing_bytes(ott)
