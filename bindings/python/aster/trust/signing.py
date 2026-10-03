"""
aster.trust.signing -- Canonical signing bytes and ed25519 helpers.

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

The authoritative implementation lives in Rust core (``core/src/signing.rs``);
this module serializes credentials to JSON and asks the native extension for
the bytes, so Python never produces its own variant.
"""

from __future__ import annotations

import json
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from .credentials import ConsumerEnrollmentCredential, EnrollmentCredential


def canonical_json_value_bytes(obj: Any) -> bytes:
    """Canonical JSON for arbitrary JSON values: UTF-8, sorted keys, compact.

    Byte-identical to ``serde_json::to_string`` over a ``serde_json::Value``
    (sorted map keys, no whitespace, non-ASCII emitted as raw UTF-8 rather
    than ``\\uXXXX`` escapes) and to ``JSON.stringify`` over key-sorted
    objects in the TypeScript binding. Use this for any signed JSON payload
    that is not an enrollment-credential attribute map (those go through
    the Rust core via :func:`_canonical_json`).
    """
    return json.dumps(
        obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")


def _canonical_json(attributes: dict[str, str]) -> bytes:
    """Canonical JSON for a credential attribute map, computed by Rust core
    (``core/src/signing.rs::canonical_json``)."""
    from aster._aster import contract as _native

    return bytes(_native.canonical_json(json.dumps(attributes)))


def _credential_json(cred: "EnrollmentCredential | ConsumerEnrollmentCredential") -> str:
    """Serialize a credential into the JSON shape the Rust core's
    ``CredentialData`` (``core/src/signing.rs``) deserializes."""
    from .credentials import ConsumerEnrollmentCredential, EnrollmentCredential

    if isinstance(cred, EnrollmentCredential):
        data: dict[str, Any] = {
            "kind": "producer",
            "endpoint_id": cred.endpoint_id,
            "root_pubkey": bytes(cred.root_pubkey).hex(),
            "expires_at": cred.expires_at,
            "attributes": cred.attributes,
        }
    elif isinstance(cred, ConsumerEnrollmentCredential):
        data = {
            "kind": "consumer",
            "credential_type": cred.credential_type,
            "root_pubkey": bytes(cred.root_pubkey).hex(),
            "expires_at": cred.expires_at,
            "attributes": cred.attributes,
            "endpoint_id": cred.endpoint_id,
            "nonce": bytes(cred.nonce).hex() if cred.nonce is not None else None,
        }
    else:
        raise TypeError(f"Unsupported credential type: {type(cred)}")
    return json.dumps(data)


def _canonical_signing_bytes(cred: "EnrollmentCredential | ConsumerEnrollmentCredential") -> bytes:
    """Produce the canonical byte sequence that is signed/verified.

    Computed by Rust core (``canonical_signing_bytes_from_json``) so every
    binding signs exactly the same bytes. Raises ``ValueError`` on malformed
    credentials (wrong pubkey/nonce length, unknown credential_type,
    non-string attribute values) and ``TypeError`` on unsupported types.
    """
    from aster._aster import contract as _native

    return bytes(_native.canonical_signing_bytes_from_json(_credential_json(cred)))


# ── ed25519 helpers ───────────────────────────────────────────────────────────


def generate_root_keypair() -> tuple[bytes, bytes]:
    """Generate a new ed25519 root key pair.

    Returns:
        (private_key_bytes, public_key_bytes) each serialised as raw 32-byte
        scalars using the standard Raw encoding.

    Note: ``private_key_bytes`` is the 32-byte seed (not the 64-byte PKCS8
    blob).
    """
    from aster._aster import ed25519_generate_keypair
    return ed25519_generate_keypair()


def sign_credential(
    cred: "EnrollmentCredential | ConsumerEnrollmentCredential",
    root_privkey_raw: bytes,
) -> bytes:
    """Sign a credential with the root private key.

    Returns the 64-byte ed25519 signature.  The credential's ``signature``
    field is NOT mutated -- the caller assigns the return value.
    """
    from aster._aster import ed25519_sign
    msg = _canonical_signing_bytes(cred)
    return ed25519_sign(root_privkey_raw, msg)


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
    from aster._aster import ed25519_verify

    pubkey_bytes = root_pubkey_raw if root_pubkey_raw is not None else cred.root_pubkey
    try:
        msg = _canonical_signing_bytes(cred)
    except (ValueError, TypeError):
        return False
    return ed25519_verify(pubkey_bytes, msg, cred.signature)
