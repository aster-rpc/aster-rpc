"""
aster.protocol -- Wire-protocol dataclasses.

Spec reference: §6.2 (StreamHeader), §6.4 (RpcStatus/trailer)

These types are always serialized with Fory XLANG, regardless of the
service's negotiated serialization mode. Every field uses
``pyfory.field(id=N)`` so the Fory struct-fingerprint is tag-ID based and
stable across bindings -- without explicit IDs Java's Fory would snake_case
the field name when computing its fingerprint while Python leaves it
verbatim, and the two sides hash-mismatch with
``Hash X is not consistent with Y for type T`` at decode time. IDs here
MUST stay in sync with the ``@ForyField(id = N)`` annotations on the
matching Java types under
``bindings/java/aster-runtime/src/main/java/site/aster/server/wire/``.
"""

from __future__ import annotations

from dataclasses import dataclass

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

    service: str = pyfory.field(0, default="")
    method: str = pyfory.field(1, default="")
    version: pyfory.int32 = pyfory.field(2, default=0)
    callId: pyfory.int32 = pyfory.field(3, default=0)
    # relative seconds, 0 = no deadline
    deadline: pyfory.int16 = pyfory.field(4, default=0)
    # XLANG=0, NATIVE=1, ROW=2, JSON=3
    serializationMode: pyfory.int8 = pyfory.field(5, default=0)
    metadataKeys: list[str] = pyfory.field(6, default_factory=list)
    metadataValues: list[str] = pyfory.field(7, default_factory=list)
    # Session identifier (multiplexed-streams, spec §6). 0 = stateless
    # SHARED pool stream; non-zero = stream belongs to the session with
    # this id on this (peer, connection). Monotonically allocated
    # client-side per connection.
    sessionId: pyfory.int32 = pyfory.field(8, default=0)


@dataclass
@wire_type("_aster/CallHeader")
class CallHeader:
    """Per-call header within a session stream (CALL flag).

    Used for session-scoped services (Phase 8) where multiple RPCs
    share a single QUIC stream.
    """

    method: str = pyfory.field(0, default="")
    callId: pyfory.int32 = pyfory.field(1, default=0)
    # relative seconds, 0 = no deadline
    deadline: pyfory.int16 = pyfory.field(2, default=0)
    metadataKeys: list[str] = pyfory.field(3, default_factory=list)
    metadataValues: list[str] = pyfory.field(4, default_factory=list)


@dataclass
@wire_type("_aster/RpcStatus")
class RpcStatus:
    """Trailing status frame (TRAILER flag).

    Sent as the last frame on a stream to communicate the outcome of
    the RPC to the peer.
    """

    code: pyfory.int32 = pyfory.field(0, default=0)
    message: str = pyfory.field(1, default="")
    detailKeys: list[str] = pyfory.field(2, default_factory=list)
    detailValues: list[str] = pyfory.field(3, default_factory=list)