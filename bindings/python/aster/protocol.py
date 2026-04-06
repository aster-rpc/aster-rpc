"""
aster.protocol — Wire-protocol dataclasses.

Spec reference: §6.2 (StreamHeader), §6.4 (RpcStatus/trailer)

These types are always serialized with Fory XLANG, regardless of the
service's negotiated serialization mode.
"""

from __future__ import annotations

from dataclasses import dataclass, field

# Import wire_type from codec.py to avoid duplication
from aster.codec import wire_type


# ── Framework-internal protocol types ────────────────────────────────────────


@dataclass
@wire_type("_aster/StreamHeader")
class StreamHeader:
    """First frame on every QUIC stream (HEADER flag).

    Carries service routing, contract identity, call metadata, and
    the negotiated serialization mode.
    """

    service: str = ""
    method: str = ""
    version: int = 0
    contract_id: str = ""
    call_id: str = ""
    deadline_epoch_ms: int = 0
    serialization_mode: int = 0
    metadata_keys: list[str] = field(default_factory=list)
    metadata_values: list[str] = field(default_factory=list)


@dataclass
@wire_type("_aster/CallHeader")
class CallHeader:
    """Per-call header within a session stream (CALL flag).

    Used for session-scoped services (Phase 8) where multiple RPCs
    share a single QUIC stream.
    """

    method: str = ""
    call_id: str = ""
    deadline_epoch_ms: int = 0
    metadata_keys: list[str] = field(default_factory=list)
    metadata_values: list[str] = field(default_factory=list)


@dataclass
@wire_type("_aster/RpcStatus")
class RpcStatus:
    """Trailing status frame (TRAILER flag).

    Sent as the last frame on a stream to communicate the outcome of
    the RPC to the peer.
    """

    code: int = 0
    message: str = ""
    detail_keys: list[str] = field(default_factory=list)
    detail_values: list[str] = field(default_factory=list)