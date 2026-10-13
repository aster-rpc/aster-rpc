"""
aster.trust.signing -- canonical JSON and ed25519 key helpers.

The legacy consumer/producer enrollment credentials (and their canonical
signing bytes) were removed with consumer admission: Enrollment credentials
are delegation chains issued by the ``aster`` CLI (``aster enroll node``) and
verified in Rust.
"""

from __future__ import annotations

import json
from typing import Any


def canonical_json_value_bytes(obj: Any) -> bytes:
    """Canonical JSON for arbitrary JSON values: UTF-8, sorted keys, compact.

    Byte-identical to ``serde_json::to_string`` over a ``serde_json::Value``
    (sorted map keys, no whitespace, non-ASCII emitted as raw UTF-8 rather
    than ``\\uXXXX`` escapes) and to ``JSON.stringify`` over key-sorted
    objects in the TypeScript binding. Use this for signed JSON payloads.
    """
    return json.dumps(
        obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")


def generate_root_keypair() -> tuple[bytes, bytes]:
    """Generate a new ed25519 key pair.

    Returns:
        (private_key_bytes, public_key_bytes) each serialised as raw 32-byte
        scalars using the standard Raw encoding.

    Note: ``private_key_bytes`` is the 32-byte seed (not the 64-byte PKCS8
    blob). A deployment's Enrollment root key is managed by the ``aster``
    CLI (``aster key root generate``), never by a running node.
    """
    from aster._aster import ed25519_generate_keypair

    return ed25519_generate_keypair()
