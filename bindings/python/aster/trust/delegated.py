"""
aster.trust.delegated — Delegated admission via @aster-issued enrollment tokens.

Implements the ``aster.admission`` ALPN handler (Aster-trust-spec §3a).

Protocol:
  1. Consumer sends: EnrollmentToken (JSON) + SigningKeyAttestation (JSON)
  2. Service verifies: attestation → token → service binding
  3. Service sends: AdmissionChallenge (32-byte nonce + service identity)
  4. Consumer sends: AdmissionProof (signature over challenge with root key)
  5. Service verifies: proof of possession
  6. Service admits with: {handle, roles}

All crypto is ed25519. No network calls — verification uses cached
attestations and the @aster root pubkey received at publish time.
"""

from __future__ import annotations

import asyncio
import json
import logging
import secrets
import time
from dataclasses import dataclass
from typing import Any

from aster.status import RpcError, StatusCode
from aster.trust.signing import load_public_key

logger = logging.getLogger(__name__)

# ── Wire types for the admission protocol ────────────────────────────────────


@dataclass
class SigningKeyAttestation:
    """Attestation binding a signing key to the @aster root key."""
    signing_pubkey: str = ""
    key_id: str = ""
    valid_from: int = 0
    valid_until: int = 0
    root_signature: str = ""


@dataclass
class EnrollmentToken:
    """@aster-issued token granting a consumer access to a service."""
    consumer_handle: str = ""
    consumer_pubkey: str = ""
    target_handle: str = ""
    target_service: str = ""
    target_contract_id: str = ""
    roles: list[str] = None  # type: ignore[assignment]
    issued_at: int = 0
    expires_at: int = 0
    signing_key_id: str = ""
    signature: str = ""

    def __post_init__(self) -> None:
        if self.roles is None:
            self.roles = []


@dataclass
class DelegatedAdmissionPolicy:
    """Service-side policy for verifying delegated tokens."""
    target_handle: str
    target_service: str
    target_contract_id: str
    aster_root_pubkey: str  # hex — the trust anchor


@dataclass
class DelegatedAdmissionResult:
    """Result of a successful delegated admission."""
    admitted: bool = False
    handle: str = ""
    roles: list[str] = None  # type: ignore[assignment]
    consumer_pubkey: str = ""

    def __post_init__(self) -> None:
        if self.roles is None:
            self.roles = []


# ── Canonical JSON for signature verification ────────────────────────────────


def _canonical_json_bytes(obj: dict) -> bytes:
    """Deterministic JSON encoding for signature verification."""
    return json.dumps(obj, sort_keys=True, separators=(",", ":")).encode("utf-8")


# ── Verification functions ───────────────────────────────────────────────────


def verify_attestation(
    attestation: SigningKeyAttestation,
    *,
    aster_root_pubkey_hex: str,
    now: int | None = None,
) -> None:
    """Verify a signing-key attestation against the @aster root pubkey.

    Raises RpcError on failure.
    """
    if now is None:
        now = int(time.time())

    payload = {
        "signing_pubkey": attestation.signing_pubkey,
        "key_id": attestation.key_id,
        "valid_from": attestation.valid_from,
        "valid_until": attestation.valid_until,
    }

    try:
        from cryptography.exceptions import InvalidSignature
        root_pubkey = load_public_key(bytes.fromhex(aster_root_pubkey_hex))
        root_pubkey.verify(
            bytes.fromhex(attestation.root_signature),
            _canonical_json_bytes(payload),
        )
    except Exception as exc:
        raise RpcError(
            StatusCode.UNAUTHENTICATED,
            "signing-key attestation verification failed",
        ) from exc

    if not (attestation.valid_from <= now <= attestation.valid_until):
        raise RpcError(
            StatusCode.UNAUTHENTICATED,
            "signing-key attestation is not currently valid",
        )


def verify_token(
    token: EnrollmentToken,
    attestation: SigningKeyAttestation,
    *,
    policy: DelegatedAdmissionPolicy,
    now: int | None = None,
) -> None:
    """Verify an enrollment token against the attestation and service policy.

    Raises RpcError on failure.
    """
    if now is None:
        now = int(time.time())

    # Token must reference the correct signing key
    if token.signing_key_id != attestation.key_id:
        raise RpcError(
            StatusCode.UNAUTHENTICATED,
            "token signing key does not match attestation",
        )

    # Verify token signature against the attested signing key
    token_payload = {
        "consumer_handle": token.consumer_handle,
        "consumer_pubkey": token.consumer_pubkey,
        "target_handle": token.target_handle,
        "target_service": token.target_service,
        "target_contract_id": token.target_contract_id,
        "roles": token.roles,
        "issued_at": token.issued_at,
        "expires_at": token.expires_at,
        "signing_key_id": token.signing_key_id,
    }

    try:
        from cryptography.exceptions import InvalidSignature
        signing_pubkey = load_public_key(bytes.fromhex(attestation.signing_pubkey))
        signing_pubkey.verify(
            bytes.fromhex(token.signature),
            _canonical_json_bytes(token_payload),
        )
    except Exception as exc:
        raise RpcError(
            StatusCode.UNAUTHENTICATED,
            "enrollment token signature verification failed",
        ) from exc

    # Token expiry
    if not (token.issued_at <= now <= token.expires_at):
        raise RpcError(
            StatusCode.UNAUTHENTICATED,
            "enrollment token has expired",
        )

    # Service binding — all three must match
    if token.target_handle != policy.target_handle:
        raise RpcError(StatusCode.PERMISSION_DENIED, "token targets a different handle")
    if token.target_service != policy.target_service:
        raise RpcError(StatusCode.PERMISSION_DENIED, "token targets a different service")
    if token.target_contract_id != policy.target_contract_id:
        raise RpcError(StatusCode.PERMISSION_DENIED, "token targets a different contract")


def verify_proof_of_possession(
    *,
    consumer_pubkey_hex: str,
    challenge_bytes: bytes,
    signature_hex: str,
) -> None:
    """Verify the consumer's proof of possession of their root key.

    Raises RpcError on failure.
    """
    try:
        from cryptography.exceptions import InvalidSignature
        consumer_pubkey = load_public_key(bytes.fromhex(consumer_pubkey_hex))
        consumer_pubkey.verify(
            bytes.fromhex(signature_hex),
            challenge_bytes,
        )
    except Exception as exc:
        raise RpcError(
            StatusCode.UNAUTHENTICATED,
            "admission proof verification failed",
        ) from exc


def build_challenge_bytes(
    nonce: bytes, target_handle: str, target_service: str, alpn: str = "aster.admission"
) -> bytes:
    """Build the challenge payload that the consumer must sign."""
    return (
        nonce
        + target_handle.encode("utf-8")
        + target_service.encode("utf-8")
        + alpn.encode("utf-8")
    )


# ── Connection handler ───────────────────────────────────────────────────────


async def handle_delegated_admission_connection(
    conn: Any,
    *,
    policy: DelegatedAdmissionPolicy,
    hook: Any,
    peer_store: Any | None = None,
) -> None:
    """Handle one connection on the aster.admission ALPN.

    Runs the 6-step verification protocol:
    1. Read token + attestation from consumer
    2. Verify attestation against @aster root key
    3. Verify token against attestation + service binding
    4. Send challenge nonce
    5. Read proof of possession
    6. Verify proof, admit consumer

    Args:
        conn: QUIC connection with accept_bi() and remote_id().
        policy: Service-side admission policy (handle, service, contract, root key).
        hook: MeshEndpointHook for peer allowlist.
        peer_store: Optional PeerAttributeStore for attribute bridging.
    """
    peer_id = conn.remote_id()
    now = int(time.time())

    try:
        send, recv = await conn.accept_bi()

        # Step 1: Read token + attestation
        raw = await recv.read_to_end(64 * 1024)
        if not raw:
            logger.warning("delegated admission: empty request from %s", peer_id)
            return

        try:
            request = json.loads(raw)
        except json.JSONDecodeError:
            logger.warning("delegated admission: malformed JSON from %s", peer_id)
            await _send_reject(send, "malformed request")
            return

        # Parse token and attestation
        try:
            token_data = request.get("token", {})
            att_data = request.get("attestation", {})
            token = EnrollmentToken(**{k: token_data.get(k, v)
                                       for k, v in EnrollmentToken.__dataclass_fields__.items()
                                       for v in [EnrollmentToken.__dataclass_fields__[k].default]})
            attestation = SigningKeyAttestation(**{k: att_data.get(k, "")
                                                   for k in SigningKeyAttestation.__dataclass_fields__})
        except Exception:
            # Simpler parsing
            token = EnrollmentToken(
                consumer_handle=token_data.get("consumer_handle", ""),
                consumer_pubkey=token_data.get("consumer_pubkey", ""),
                target_handle=token_data.get("target_handle", ""),
                target_service=token_data.get("target_service", ""),
                target_contract_id=token_data.get("target_contract_id", ""),
                roles=token_data.get("roles", []),
                issued_at=token_data.get("issued_at", 0),
                expires_at=token_data.get("expires_at", 0),
                signing_key_id=token_data.get("signing_key_id", ""),
                signature=token_data.get("signature", ""),
            )
            attestation = SigningKeyAttestation(
                signing_pubkey=att_data.get("signing_pubkey", ""),
                key_id=att_data.get("key_id", ""),
                valid_from=att_data.get("valid_from", 0),
                valid_until=att_data.get("valid_until", 0),
                root_signature=att_data.get("root_signature", ""),
            )

        # Steps 2-3: Verify attestation and token
        try:
            verify_attestation(
                attestation,
                aster_root_pubkey_hex=policy.aster_root_pubkey,
                now=now,
            )
            verify_token(token, attestation, policy=policy, now=now)
        except RpcError as e:
            logger.info("delegated admission: denied %s: %s", peer_id, e.message)
            await _send_reject(send, e.message)
            return

        # Step 4: Send challenge
        nonce = secrets.token_bytes(32)
        challenge = {
            "nonce": nonce.hex(),
            "target_handle": policy.target_handle,
            "target_service": policy.target_service,
        }
        await send.write_all(json.dumps(challenge).encode())

        # Step 5: Read proof
        proof_raw = await recv.read_to_end(4096)
        if not proof_raw:
            logger.warning("delegated admission: no proof from %s", peer_id)
            return

        try:
            proof = json.loads(proof_raw)
        except json.JSONDecodeError:
            await _send_reject(send, "malformed proof")
            return

        # Step 6: Verify proof of possession
        challenge_bytes = build_challenge_bytes(
            nonce, policy.target_handle, policy.target_service
        )
        try:
            verify_proof_of_possession(
                consumer_pubkey_hex=token.consumer_pubkey,
                challenge_bytes=challenge_bytes,
                signature_hex=proof.get("signature", ""),
            )
        except RpcError as e:
            logger.info("delegated admission: proof failed %s: %s", peer_id, e.message)
            await _send_reject(send, e.message)
            return

        # Admitted!
        logger.info(
            "delegated admission: admitted %s (handle=%s, roles=%s)",
            peer_id, token.consumer_handle, token.roles,
        )

        # Store admission attributes
        if peer_store is not None:
            from aster.peer_store import PeerAdmission
            from aster.trust.credentials import ATTR_ROLE
            peer_store.admit(PeerAdmission(
                endpoint_id=peer_id,
                handle=token.consumer_handle,
                attributes={ATTR_ROLE: ",".join(token.roles)},
                admission_path="aster.admission",
            ))

        # Add to Gate 0 allowlist
        if hook is not None:
            hook.addPeer(peer_id)

        # Send success response
        result = {
            "admitted": True,
            "handle": token.consumer_handle,
            "roles": token.roles,
        }
        await send.write_all(json.dumps(result).encode())
        await send.finish()

    except asyncio.CancelledError:
        return
    except Exception as exc:
        logger.error("delegated admission: error for %s: %s", peer_id, exc)


async def _send_reject(send: Any, reason: str) -> None:
    """Send a rejection response and finish the stream."""
    try:
        result = json.dumps({"admitted": False, "reason": reason})
        await send.write_all(result.encode())
        await send.finish()
    except Exception:
        pass
