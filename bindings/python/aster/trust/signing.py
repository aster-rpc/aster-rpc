"""
aster.trust.signing — Canonical signing bytes and ed25519 helpers.

Spec reference: Aster-trust-spec.md §2.2, §2.4.

Canonical signing bytes format:

  EnrollmentCredential:
    endpoint_id.encode('utf-8')        # variable length
    || root_pubkey                     # 32 bytes
    || u64_be(expires_at)              # 8 bytes
    || canonical_json(attributes)      # UTF-8, sorted keys

  ConsumerEnrollmentCredential:
    u8(type_code)                      # 0x00 = policy, 0x01 = ott
    || u8(has_endpoint_id)             # 0x00 or 0x01
    || endpoint_id.encode()?           # present only if has_endpoint_id == 0x01
    || root_pubkey                     # 32 bytes
    || u64_be(expires_at)              # 8 bytes
    || canonical_json(attributes)      # UTF-8, sorted keys
    || u8(has_nonce)                   # 0x00 or 0x01
    || nonce?                          # present only if has_nonce == 0x01 (32 bytes)
"""

from __future__ import annotations

import json
import struct
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .credentials import ConsumerEnrollmentCredential, EnrollmentCredential


def _canonical_json(attributes: dict[str, str]) -> bytes:
    """Encode attributes as canonical JSON: UTF-8, sorted keys, no extra whitespace.

    NOTE: The authoritative implementation is in Rust core (core/src/signing.rs).
    This Python copy exists only as a private helper for sign_credential/verify_signature.
    """
    return json.dumps(attributes, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def _canonical_signing_bytes(cred: "EnrollmentCredential | ConsumerEnrollmentCredential") -> bytes:
    """Produce the canonical byte sequence that is signed/verified.

    NOTE: The authoritative implementation is in Rust core (core/src/signing.rs).
    This Python copy exists only as a private helper for sign_credential/verify_signature.
    """
    from .credentials import EnrollmentCredential, ConsumerEnrollmentCredential

    if isinstance(cred, EnrollmentCredential):
        return _producer_signing_bytes(cred)
    elif isinstance(cred, ConsumerEnrollmentCredential):
        return _consumer_signing_bytes(cred)
    else:
        raise TypeError(f"Unsupported credential type: {type(cred)}")


def _producer_signing_bytes(cred: "EnrollmentCredential") -> bytes:
    parts = [
        cred.endpoint_id.encode("utf-8"),
        cred.root_pubkey,
        struct.pack(">Q", cred.expires_at),
        _canonical_json(cred.attributes),
    ]
    return b"".join(parts)


def _consumer_signing_bytes(cred: "ConsumerEnrollmentCredential") -> bytes:
    # type_code: 0 = policy, 1 = ott
    type_code = b"\x01" if cred.credential_type == "ott" else b"\x00"

    # optional endpoint_id
    if cred.endpoint_id is not None:
        eid_bytes = cred.endpoint_id.encode("utf-8")
        eid_part = b"\x01" + eid_bytes
    else:
        eid_part = b"\x00"

    # optional nonce
    if cred.nonce is not None:
        nonce_part = b"\x01" + cred.nonce
    else:
        nonce_part = b"\x00"

    parts = [
        type_code,
        eid_part,
        cred.root_pubkey,
        struct.pack(">Q", cred.expires_at),
        _canonical_json(cred.attributes),
        nonce_part,
    ]
    return b"".join(parts)


# ── ed25519 helpers ───────────────────────────────────────────────────────────


def generate_root_keypair() -> tuple[bytes, bytes]:
    """Generate a new ed25519 root key pair.

    Returns:
        (private_key_bytes, public_key_bytes) each serialised as raw 32-byte
        scalars using the standard Raw encoding.

    Note: ``private_key_bytes`` is the 32-byte seed (not the 64-byte PKCS8
    blob).  Use ``load_private_key(private_key_bytes)`` to reload it.
    """
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

    privkey = Ed25519PrivateKey.generate()
    priv_raw = privkey.private_bytes_raw()   # 32-byte seed
    pub_raw = privkey.public_key().public_bytes_raw()  # 32-byte pubkey
    return priv_raw, pub_raw


def load_private_key(priv_raw: bytes):
    """Load an ed25519 private key from its 32-byte raw seed."""
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

    return Ed25519PrivateKey.from_private_bytes(priv_raw)


def load_public_key(pub_raw: bytes):
    """Load an ed25519 public key from its 32-byte raw encoding."""
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

    return Ed25519PublicKey.from_public_bytes(pub_raw)


def sign_credential(
    cred: "EnrollmentCredential | ConsumerEnrollmentCredential",
    root_privkey_raw: bytes,
) -> bytes:
    """Sign a credential with the root private key.

    Returns the 64-byte ed25519 signature.  The credential's ``signature``
    field is NOT mutated — the caller assigns the return value.
    """
    privkey = load_private_key(root_privkey_raw)
    msg = _canonical_signing_bytes(cred)
    return privkey.sign(msg)


def verify_signature(
    cred: "EnrollmentCredential | ConsumerEnrollmentCredential",
    root_pubkey_raw: bytes | None = None,
) -> bool:
    """Verify the credential signature.

    If ``root_pubkey_raw`` is provided it overrides ``cred.root_pubkey``
    (allows external pinned trust root).  Otherwise ``cred.root_pubkey``
    is used (self-contained credential).

    Returns True on success, False on any failure (invalid signature,
    wrong key, malformed bytes, etc.).
    """
    from cryptography.exceptions import InvalidSignature

    pubkey_bytes = root_pubkey_raw if root_pubkey_raw is not None else cred.root_pubkey
    try:
        pubkey = load_public_key(pubkey_bytes)
        msg = _canonical_signing_bytes(cred)
        pubkey.verify(cred.signature, msg)
        return True
    except (InvalidSignature, Exception):  # noqa: BLE001
        return False
