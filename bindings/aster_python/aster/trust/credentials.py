"""
aster.trust.credentials — Enrollment credential data classes.

Spec reference: Aster-trust-spec.md §2.2.

Two credential types:
  EnrollmentCredential           — producer (bound to endpoint_id)
  ConsumerEnrollmentCredential   — consumer (policy or OTT)

Reserved attribute keys (aster.*):
  aster.role, aster.name
  aster.iid_provider, aster.iid_account, aster.iid_region, aster.iid_role_arn
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Literal


@dataclass
class EnrollmentCredential:
    """Producer enrollment credential (§2.2).

    Binds a root public key to a specific endpoint ID with an expiry time and
    optional attribute assertions. The ``signature`` field is ed25519 over the
    canonical signing bytes (see ``signing.canonical_signing_bytes``).
    """

    endpoint_id: str                    # hex — the producer's NodeId
    root_pubkey: bytes                  # 32 bytes — ed25519 public key
    expires_at: int                     # epoch seconds
    attributes: dict[str, str] = field(default_factory=dict)
    signature: bytes = b""              # 64 bytes after signing; empty before


@dataclass
class ConsumerEnrollmentCredential:
    """Consumer enrollment credential (§2.2).

    Two subtypes:
    - ``policy``: reusable, not bound to a specific NodeID.
      * ``nonce`` MUST be ``None``.
    - ``ott`` (one-time token): single-use nonce, optionally bound to NodeID.
      * ``nonce`` MUST be exactly 32 bytes (``secrets.token_bytes(32)``).
      * Presentations with ``len(nonce) != 32`` are rejected as malformed.

    Both presence flags (has_nonce, has_endpoint_id) and payload length are
    included in the canonical signing bytes, so an attacker cannot flip
    policy↔OTT or add/remove nonce without invalidating the signature.
    """

    credential_type: Literal["policy", "ott"]
    root_pubkey: bytes                  # 32 bytes — ed25519 public key
    expires_at: int                     # epoch seconds
    attributes: dict[str, str] = field(default_factory=dict)
    endpoint_id: str | None = None      # None for Policy; optional for OTT
    nonce: bytes | None = None          # 32 bytes for OTT; None for Policy
    signature: bytes = b""             # 64 bytes after signing


@dataclass
class AdmissionResult:
    """Outcome of an admission check (§2.4).

    ``reason`` is for structured logging only; it MUST NOT be leaked to the
    peer (prevents oracle attacks against the nonce store / IID validation).
    The peer sees only a QUIC-level connection close on refusal.
    """

    admitted: bool
    attributes: dict[str, str] | None = None
    reason: str | None = None           # internal logging only — never sent to peer


# ── Reserved attribute key constants ─────────────────────────────────────────

ATTR_ROLE = "aster.role"
ATTR_NAME = "aster.name"
ATTR_IID_PROVIDER = "aster.iid_provider"
ATTR_IID_ACCOUNT = "aster.iid_account"
ATTR_IID_REGION = "aster.iid_region"
ATTR_IID_ROLE_ARN = "aster.iid_role_arn"
