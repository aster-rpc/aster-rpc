"""
aster.trust.admission — Admission checks for enrollment credentials.

Spec reference: Aster-trust-spec.md §2.4, §3.2.

Two-phase admission:
  1. check_offline  — signature, expiry, endpoint_id binding, nonce (no network)
  2. check_runtime  — IID verification (one HTTP call to metadata endpoint)

``admit()`` orchestrates both phases. Refusal is logged with reason; the peer
sees only a QUIC-level connection close — no diagnostic is leaked.

Structural validation (also enforced here):
  - OTT nonce must be exactly 32 bytes; malformed → rejected
  - Policy credential MUST NOT carry a nonce; malformed → rejected
"""

from __future__ import annotations

import logging
import time
from typing import TYPE_CHECKING

from .credentials import AdmissionResult, ConsumerEnrollmentCredential, EnrollmentCredential
from .iid import IIDBackend, verify_iid
from .signing import verify_signature

if TYPE_CHECKING:
    from .nonces import NonceStoreProtocol

logger = logging.getLogger(__name__)


# ── Producer admission ────────────────────────────────────────────────────────


async def check_offline(
    cred: EnrollmentCredential | ConsumerEnrollmentCredential,
    peer_endpoint_id: str,
    nonce_store: "NonceStoreProtocol | None" = None,
) -> AdmissionResult:
    """Offline admission checks — no network calls.

    Checks (all required to pass):
      1. Structural validity (nonce length, policy-vs-OTT constraints)
      2. Signature valid against root_pubkey
      3. ``expires_at > now``
      4. Endpoint ID match (always for EnrollmentCredential; if set for Consumer)
      5. OTT nonce not already consumed

    Args:
        cred:              The enrollment credential to check.
        peer_endpoint_id:  The QUIC peer NodeId (from the handshake).
        nonce_store:       Required for OTT credentials; ignored otherwise.

    Returns:
        AdmissionResult with ``admitted=True`` on success.
    """
    # 1. Structural validation
    ok, reason = _validate_structure(cred)
    if not ok:
        logger.warning("Admission rejected (structural): %s", reason)
        return AdmissionResult(admitted=False, reason=reason)

    # 2. Signature verification
    if not verify_signature(cred):
        reason = "invalid signature"
        logger.warning("Admission rejected: %s for peer %s", reason, peer_endpoint_id)
        return AdmissionResult(admitted=False, reason=reason)

    # 3. Expiry check
    now = int(time.time())
    if cred.expires_at <= now:
        reason = f"credential expired (expires_at={cred.expires_at}, now={now})"
        logger.warning("Admission rejected: %s for peer %s", reason, peer_endpoint_id)
        return AdmissionResult(admitted=False, reason=reason)

    # 4. Endpoint ID binding
    if isinstance(cred, EnrollmentCredential):
        if cred.endpoint_id != peer_endpoint_id:
            reason = (
                f"endpoint_id mismatch: credential={cred.endpoint_id!r}, "
                f"peer={peer_endpoint_id!r}"
            )
            logger.warning("Admission rejected: %s", reason)
            return AdmissionResult(admitted=False, reason=reason)
    elif isinstance(cred, ConsumerEnrollmentCredential):
        if cred.endpoint_id is not None and cred.endpoint_id != peer_endpoint_id:
            reason = (
                f"OTT endpoint_id mismatch: credential={cred.endpoint_id!r}, "
                f"peer={peer_endpoint_id!r}"
            )
            logger.warning("Admission rejected: %s", reason)
            return AdmissionResult(admitted=False, reason=reason)

    # 5. OTT nonce consumption
    if isinstance(cred, ConsumerEnrollmentCredential) and cred.credential_type == "ott":
        assert cred.nonce is not None  # guaranteed by structural validation
        if nonce_store is None:
            reason = "OTT credential presented but no nonce_store configured"
            logger.warning("Admission rejected: %s", reason)
            return AdmissionResult(admitted=False, reason=reason)
        consumed = await nonce_store.consume(cred.nonce)
        if not consumed:
            reason = "OTT nonce already consumed"
            logger.warning("Admission rejected: %s for peer %s", reason, peer_endpoint_id)
            return AdmissionResult(admitted=False, reason=reason)

    return AdmissionResult(admitted=True, attributes=dict(cred.attributes))


async def check_runtime(
    cred: EnrollmentCredential | ConsumerEnrollmentCredential,
    iid_backend: IIDBackend | None = None,
    iid_token: str | None = None,
) -> AdmissionResult:
    """Runtime admission checks (IID verification).

    Only runs if ``aster.iid_provider`` is present in attributes.  For tests,
    pass a ``MockIIDBackend`` as ``iid_backend``.

    Returns:
        AdmissionResult with ``admitted=True`` if no IID attributes present
        (skip) or IID claims match.
    """
    ok, reason = await verify_iid(cred.attributes, iid_backend, iid_token)
    if not ok:
        logger.warning("Admission rejected (IID runtime): %s", reason)
        return AdmissionResult(admitted=False, reason=reason)
    return AdmissionResult(admitted=True, attributes=dict(cred.attributes))


async def admit(
    cred: EnrollmentCredential | ConsumerEnrollmentCredential,
    peer_endpoint_id: str,
    *,
    nonce_store: "NonceStoreProtocol | None" = None,
    iid_backend: IIDBackend | None = None,
    iid_token: str | None = None,
) -> AdmissionResult:
    """Orchestrate offline + runtime admission checks.

    Fails fast: if offline checks fail, runtime checks are skipped.
    Refusal reason is logged but never sent to peer.

    Args:
        cred:              Enrollment credential to validate.
        peer_endpoint_id:  QUIC peer NodeId from handshake.
        nonce_store:       Required when accepting OTT credentials.
        iid_backend:       Override IID backend (use MockIIDBackend in tests).
        iid_token:         Pre-fetched IID token from peer's handshake payload.

    Returns:
        AdmissionResult with ``admitted=True`` on full success.
    """
    offline = await check_offline(cred, peer_endpoint_id, nonce_store)
    if not offline.admitted:
        return offline

    runtime = await check_runtime(cred, iid_backend, iid_token)
    if not runtime.admitted:
        return runtime

    # Merge attributes (offline already set them)
    return AdmissionResult(admitted=True, attributes=offline.attributes)


# ── Structural validation ─────────────────────────────────────────────────────


def _validate_structure(
    cred: EnrollmentCredential | ConsumerEnrollmentCredential,
) -> tuple[bool, str | None]:
    """Return (ok, reason) for structural validity checks."""
    if isinstance(cred, ConsumerEnrollmentCredential):
        if cred.credential_type == "ott":
            if cred.nonce is None:
                return False, "OTT credential must carry a nonce"
            if len(cred.nonce) != 32:
                return (
                    False,
                    f"OTT nonce must be exactly 32 bytes; got {len(cred.nonce)}",
                )
        elif cred.credential_type == "policy":
            if cred.nonce is not None:
                return False, "Policy credential must not carry a nonce"
        else:
            return False, f"Unknown credential_type: {cred.credential_type!r}"
    return True, None
