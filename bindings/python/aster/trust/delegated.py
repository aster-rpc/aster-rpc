# STATUS: TBD - possibly retired.
# Aster authentication/authorization will be designed once in Rust and
# exposed to every binding (docs/_internal/bindings/BINDINGS_CATCHUP_PLAN.md,
# D3). Until then this module is frozen: fix bugs, but do not extend it
# or port it to other languages.
"""
aster.trust.delegated -- verification primitives for @aster-issued
enrollment tokens (Aster-trust-spec §3a).

**Not wired into the Python runtime.** The ``aster.admission`` ALPN is not
registered and no connection handler exists in this binding: the earlier
handler never received connections (its accept loop called
``accept_aster`` with an argument the native method does not take and
busy-spun) and its single-stream ``read_to_end`` protocol could not carry
the challenge/proof round-trip. Delegated admission will move into the
Rust core with credential admission (BINDINGS_CATCHUP_PLAN.md P5).

What remains here are pure, side-effect-free checks:
  - ``verify_attestation``: signing-key attestation vs @aster root key
  - ``verify_token``: enrollment token vs attestation + service binding
  - ``verify_proof_of_possession`` / ``build_challenge_bytes``

All crypto is ed25519; canonical JSON matches the Rust core form.
"""

from __future__ import annotations

import time
from dataclasses import dataclass

from aster.status import RpcError, StatusCode
from aster.trust.signing import canonical_json_value_bytes

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
    aster_root_pubkey: str  # hex -- the trust anchor


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
    """Deterministic JSON encoding for signature verification.

    Same canonical form as the Rust core / TS binding (sorted keys, compact,
    raw UTF-8 -- no ``\\uXXXX`` escaping of non-ASCII).
    """
    return canonical_json_value_bytes(obj)


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
        from aster._aster import ed25519_verify
        if not ed25519_verify(
            bytes.fromhex(aster_root_pubkey_hex),
            _canonical_json_bytes(payload),
            bytes.fromhex(attestation.root_signature),
        ):
            raise ValueError("signature invalid")
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
        from aster._aster import ed25519_verify
        if not ed25519_verify(
            bytes.fromhex(attestation.signing_pubkey),
            _canonical_json_bytes(token_payload),
            bytes.fromhex(token.signature),
        ):
            raise ValueError("signature invalid")
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

    # Service binding -- all three must match
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
        from aster._aster import ed25519_verify
        if not ed25519_verify(
            bytes.fromhex(consumer_pubkey_hex),
            challenge_bytes,
            bytes.fromhex(signature_hex),
        ):
            raise ValueError("signature invalid")
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
