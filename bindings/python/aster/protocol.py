"""
aster.protocol -- Wire-protocol dataclasses.

Spec reference: §6.2 (StreamHeader), §6.4 (RpcStatus/trailer)

These types are always serialized with Fory XLANG, regardless of the
service's negotiated serialization mode.
"""

from __future__ import annotations

from dataclasses import dataclass, field

import pyfory

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
    version: pyfory.int32 = 0
    callId: pyfory.int32 = 0
    deadline: pyfory.int16 = 0          # relative seconds, 0 = no deadline
    serializationMode: pyfory.int8 = 0  # XLANG=0, NATIVE=1, ROW=2, JSON=3
    metadataKeys: list[str] = field(default_factory=list)
    metadataValues: list[str] = field(default_factory=list)
    # Session identifier (multiplexed-streams, spec §6). 0 = stateless
    # SHARED pool stream; non-zero = stream belongs to the session with
    # this id on this (peer, connection). Monotonically allocated
    # client-side per connection. Treated as 4-byte little-endian on
    # the wire; signed int32 here is equivalent for any value fitting
    # the monotonic counter in practice.
    sessionId: pyfory.int32 = 0


@dataclass
@wire_type("_aster/CallHeader")
class CallHeader:
    """Per-call header within a session stream (CALL flag).

    Used for session-scoped services (Phase 8) where multiple RPCs
    share a single QUIC stream.
    """

    method: str = ""
    callId: pyfory.int32 = 0
    deadline: pyfory.int16 = 0          # relative seconds, 0 = no deadline
    metadataKeys: list[str] = field(default_factory=list)
    metadataValues: list[str] = field(default_factory=list)


@dataclass
@wire_type("_aster/RpcStatus")
class RpcStatus:
    """Trailing status frame (TRAILER flag).

    Sent as the last frame on a stream to communicate the outcome of
    the RPC to the peer.
    """

    code: int = 0
    message: str = ""
    detailKeys: list[str] = field(default_factory=list)
    detailValues: list[str] = field(default_factory=list)