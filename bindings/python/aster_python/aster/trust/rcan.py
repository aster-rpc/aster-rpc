"""
aster.trust.rcan — rcan grant serialization (stub).

Phase 12 open design question (§14.12): the rcan grant format has not been
specified upstream.  This module ships as an opaque ``bytes`` pass-through.

When the upstream spec pins down the rcan serialization, this module will be
updated to provide encode/decode helpers.
"""

from __future__ import annotations


def encode_rcan(rcan_data: bytes) -> bytes:
    """Encode rcan grant data for inclusion in IntroducePayload.

    Currently a pass-through (opaque bytes).
    """
    return rcan_data


def decode_rcan(rcan_bytes: bytes) -> bytes:
    """Decode rcan grant bytes from IntroducePayload.

    Currently a pass-through (opaque bytes).
    """
    return rcan_bytes


def validate_rcan(rcan_bytes: bytes) -> tuple[bool, str | None]:
    """Validate an rcan grant.

    Phase 12: all non-empty grants are accepted (opaque).  Empty bytes → invalid.
    """
    if not rcan_bytes:
        return False, "rcan grant must not be empty"
    # TODO: implement full rcan validation once upstream format is specified (§14.12)
    return True, None
