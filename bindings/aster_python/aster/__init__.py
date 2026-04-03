"""
aster — Aster RPC framework for Python.

This package implements the Aster RPC protocol (spec v0.7.1) on top of
the existing iroh transport bindings in ``aster_python``.

Phase 1 exports: wire protocol types and framing utilities.
Phase 2 exports: Fory serialization codec.
"""

from aster_python.aster.status import StatusCode, RpcError
from aster_python.aster.types import SerializationMode, RetryPolicy, ExponentialBackoff
from aster_python.aster.framing import (
    COMPRESSED,
    TRAILER,
    HEADER,
    ROW_SCHEMA,
    CALL,
    CANCEL,
    MAX_FRAME_SIZE,
    FramingError,
    write_frame,
    read_frame,
)
from aster_python.aster.protocol import StreamHeader, CallHeader, RpcStatus
from aster_python.aster.codec import (
    fory_tag,
    ForyCodec,
    DEFAULT_COMPRESSION_THRESHOLD,
)

__all__ = [
    # status.py
    "StatusCode",
    "RpcError",
    # types.py
    "SerializationMode",
    "RetryPolicy",
    "ExponentialBackoff",
    # framing.py
    "COMPRESSED",
    "TRAILER",
    "HEADER",
    "ROW_SCHEMA",
    "CALL",
    "CANCEL",
    "MAX_FRAME_SIZE",
    "FramingError",
    "write_frame",
    "read_frame",
    # protocol.py
    "StreamHeader",
    "CallHeader",
    "RpcStatus",
    # codec.py
    "fory_tag",
    "ForyCodec",
    "DEFAULT_COMPRESSION_THRESHOLD",
]
