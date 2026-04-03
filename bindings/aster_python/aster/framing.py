"""
aster.framing — Wire-level frame read/write.

Spec reference: §6.1 (stream framing)

Frame layout (on a QUIC stream):

    ┌──────────┬───────┬──────────┐
    │ Length   │ Flags │ Payload  │
    │ 4 bytes  │1 byte │ variable │
    │ LE u32   │       │          │
    └──────────┴───────┴──────────┘

- **Length** is the total size of *Flags + Payload* (i.e. ``len(payload) + 1``).
  Maximum 16 MiB per frame.  A Length of 0 is invalid.
- **Flags** is a 1-byte bitfield (see constants below).
- **Payload** is the serialized bytes.
"""

from __future__ import annotations

import struct
from typing import Protocol, runtime_checkable

# ── Flag constants ───────────────────────────────────────────────────────────

COMPRESSED: int = 0x01   # Bit 0 — payload is zstd-compressed
TRAILER: int = 0x02      # Bit 1 — trailing status frame
HEADER: int = 0x04       # Bit 2 — stream header (first frame)
ROW_SCHEMA: int = 0x08   # Bit 3 — Fory row schema frame
CALL: int = 0x10         # Bit 4 — per-call header in a session stream
CANCEL: int = 0x20       # Bit 5 — cancel current call in a session stream

# ── Limits ───────────────────────────────────────────────────────────────────

MAX_FRAME_SIZE: int = 16 * 1024 * 1024  # 16 MiB

# ── Errors ───────────────────────────────────────────────────────────────────


class FramingError(Exception):
    """Raised when a framing violation is detected."""


# ── Stream protocol abstractions ─────────────────────────────────────────────
# These allow framing to work with both real Iroh streams and in-memory buffers.


@runtime_checkable
class SendStream(Protocol):
    """Minimal async send-stream interface (matches IrohSendStream)."""

    async def write_all(self, data: bytes) -> None: ...


@runtime_checkable
class RecvStream(Protocol):
    """Minimal async recv-stream interface (matches IrohRecvStream)."""

    async def read_exact(self, n: int) -> bytes: ...


# ── Length header encoding ───────────────────────────────────────────────────

_LENGTH_FMT = "<I"  # little-endian unsigned 32-bit
_LENGTH_SIZE = 4
_FLAGS_SIZE = 1


# ── write_frame ──────────────────────────────────────────────────────────────


async def write_frame(
    stream: SendStream,
    payload: bytes,
    flags: int = 0,
) -> None:
    """Write a single frame to *stream*.

    Args:
        stream: An async send stream with a ``write_all`` method.
        payload: The serialized payload bytes.
        flags: The 1-byte flag bitfield.

    Raises:
        FramingError: If the frame exceeds ``MAX_FRAME_SIZE`` or is empty.
    """
    frame_body_len = _FLAGS_SIZE + len(payload)

    if frame_body_len < _FLAGS_SIZE + 1 and flags & TRAILER == 0:
        # Zero-length payload is only valid for certain control scenarios;
        # the spec says Length==0 is invalid.  We allow an empty payload
        # only when it is a TRAILER (status-only frame with no extra data
        # is fine — the flags byte alone is the content).
        pass  # Allow trailers with empty payload

    if frame_body_len == _FLAGS_SIZE and not (flags & TRAILER):
        raise FramingError("zero-length payload is not permitted")

    if frame_body_len > MAX_FRAME_SIZE:
        raise FramingError(
            f"frame size {frame_body_len} exceeds maximum {MAX_FRAME_SIZE}"
        )

    header = struct.pack(_LENGTH_FMT, frame_body_len)
    await stream.write_all(header + bytes([flags]) + payload)


# ── read_frame ───────────────────────────────────────────────────────────────


async def read_frame(
    stream: RecvStream,
) -> tuple[bytes, int] | None:
    """Read a single frame from *stream*.

    Returns:
        A ``(payload, flags)`` tuple, or ``None`` if the stream has ended
        cleanly (i.e. the peer called ``finish()`` and there are no more
        bytes to read).

    Raises:
        FramingError: On wire-format violations (zero length, oversized frame).
    """
    # Read the 4-byte length prefix.
    try:
        length_bytes = await stream.read_exact(_LENGTH_SIZE)
    except Exception:
        # Stream ended or was reset — treat as clean EOF.
        return None

    if len(length_bytes) < _LENGTH_SIZE:
        return None

    (frame_body_len,) = struct.unpack(_LENGTH_FMT, length_bytes)

    if frame_body_len == 0:
        raise FramingError("received zero-length frame")

    if frame_body_len > MAX_FRAME_SIZE:
        raise FramingError(
            f"frame size {frame_body_len} exceeds maximum {MAX_FRAME_SIZE}"
        )

    body = await stream.read_exact(frame_body_len)
    if len(body) < frame_body_len:
        raise FramingError(
            f"incomplete frame: expected {frame_body_len} bytes, got {len(body)}"
        )

    flags = body[0]
    payload = body[1:]
    return payload, flags