"""
aster.wire -- the RPC wire layer: frames, envelope, payload compression.

Spec reference: Aster-SPEC §6.1 (framing), §6.9 (AEv1 envelope), §5.6 (zstd).

Every byte layout here is implemented once, in Rust core, and reached
through ``aster._aster.wire``:

- frames: ``[u32 LE length][flags][payload]`` (``core::framing``);
- the envelope types :class:`StreamHeader`, :class:`CallHeader` and
  :class:`RpcStatus` use the Fory-independent **AEv1** encoding
  (``core::envelope``) -- they are *not* Fory structs;
- zstd: payloads strictly larger than 4096 bytes are compressed at level 3
  and sent compressed only if that is smaller; decompression is streaming
  and refuses more than 16 MiB of output.

Only the async stream I/O (``read_frame`` / ``write_frame``) lives in Python,
because it drives Python-side stream objects.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from typing import Protocol, runtime_checkable

from aster._aster import wire as _native

# ── Flags (Aster-SPEC §6.1) ──────────────────────────────────────────────────

COMPRESSED: int = _native.FLAG_COMPRESSED  # payload is zstd-compressed
TRAILER: int = _native.FLAG_TRAILER        # trailing RpcStatus frame
HEADER: int = _native.FLAG_HEADER          # StreamHeader (first frame)
ROW_SCHEMA: int = _native.FLAG_ROW_SCHEMA  # Fory row schema frame
CALL: int = _native.FLAG_CALL              # CallHeader in a session stream
CANCEL: int = _native.FLAG_CANCEL          # cancel current call (session stream)
END_STREAM: int = _native.FLAG_END_STREAM  # last request frame of a call
TUNNEL: int = _native.FLAG_TUNNEL          # tunnel-redeem stream marker

# ── Limits (Aster-SPEC §5.6, §6.1) ───────────────────────────────────────────

MAX_FRAME_SIZE: int = _native.MAX_FRAME_SIZE
COMPRESSION_THRESHOLD: int = _native.COMPRESSION_THRESHOLD
ZSTD_LEVEL: int = _native.ZSTD_LEVEL
MAX_DECOMPRESSED_SIZE: int = _native.MAX_DECOMPRESSED_SIZE

# Pinned Fory release for payloads (shared with Rust core).
FORY_VERSION: str = _native.FORY_VERSION

_LENGTH_SIZE = 4


class FramingError(Exception):
    """A frame violates the wire format (zero / oversized length, truncation)."""


class EnvelopeError(ValueError):
    """An envelope (StreamHeader / CallHeader / RpcStatus) failed to decode or
    encode (wrong tag/version, truncation, trailing bytes, range, ...)."""


# ── Envelope types (AEv1) ────────────────────────────────────────────────────


@dataclass(slots=True)
class StreamHeader:
    """First frame on every stream (``HEADER`` flag)."""

    service: str = ""
    method: str = ""
    version: int = 0
    call_id: int = 0
    deadline: int = 0  # relative seconds, 0 = no deadline
    serialization_mode: int = 0  # XLANG=0 (NATIVE=1, ROW=2 reserved)
    metadata_keys: list[str] = field(default_factory=list)
    metadata_values: list[str] = field(default_factory=list)
    # 0 = stateless SHARED-pool stream; non-zero = session-bound.
    session_id: int = 0

    def encode(self) -> bytes:
        try:
            return _native.encode_stream_header(
                self.service, self.method, self.version, self.call_id,
                self.deadline, self.serialization_mode,
                list(self.metadata_keys), list(self.metadata_values),
                self.session_id,
            )
        except (ValueError, OverflowError, TypeError) as e:
            raise EnvelopeError(str(e)) from None

    @classmethod
    def decode(cls, data: bytes) -> "StreamHeader":
        try:
            return cls(*_native.decode_stream_header(data))
        except ValueError as e:
            raise EnvelopeError(str(e)) from None


@dataclass(slots=True)
class CallHeader:
    """Per-call header within a session stream (``CALL`` flag)."""

    method: str = ""
    call_id: int = 0
    deadline: int = 0  # relative seconds, 0 = no deadline
    metadata_keys: list[str] = field(default_factory=list)
    metadata_values: list[str] = field(default_factory=list)

    def encode(self) -> bytes:
        try:
            return _native.encode_call_header(
                self.method, self.call_id, self.deadline,
                list(self.metadata_keys), list(self.metadata_values),
            )
        except (ValueError, OverflowError, TypeError) as e:
            raise EnvelopeError(str(e)) from None

    @classmethod
    def decode(cls, data: bytes) -> "CallHeader":
        try:
            return cls(*_native.decode_call_header(data))
        except ValueError as e:
            raise EnvelopeError(str(e)) from None


@dataclass(slots=True)
class RpcStatus:
    """Trailing status frame (``TRAILER`` flag)."""

    code: int = 0
    message: str = ""
    detail_keys: list[str] = field(default_factory=list)
    detail_values: list[str] = field(default_factory=list)

    def encode(self) -> bytes:
        try:
            return _native.encode_rpc_status(
                int(self.code), self.message,
                list(self.detail_keys), list(self.detail_values),
            )
        except (ValueError, OverflowError, TypeError) as e:
            raise EnvelopeError(str(e)) from None

    @classmethod
    def decode(cls, data: bytes) -> "RpcStatus":
        try:
            return cls(*_native.decode_rpc_status(data))
        except ValueError as e:
            raise EnvelopeError(str(e)) from None


# ── Compression (Aster-SPEC §5.6) ────────────────────────────────────────────


def compress_payload(
    payload: bytes, threshold: int = COMPRESSION_THRESHOLD,
) -> tuple[bytes, bool]:
    """Return ``(bytes_to_send, compressed)``.

    Compresses only when ``len(payload) > threshold`` and the result is
    smaller. A negative *threshold* disables compression.
    """
    if threshold < 0 or len(payload) <= threshold:
        return payload, False
    out = _native.maybe_compress(payload, threshold)
    if out is None:
        return payload, False
    return out, True


def decompress_payload(data: bytes, limit: int = MAX_DECOMPRESSED_SIZE) -> bytes:
    """Decompress a ``COMPRESSED`` payload, refusing more than *limit* bytes.

    Raises:
        LimitExceeded: the output would exceed *limit*.
        ValueError: *data* is not a valid zstd frame.
    """
    try:
        return _native.decompress(data, limit)
    except OverflowError:
        from aster.limits import LimitExceeded

        raise LimitExceeded("decompressed payload", limit, limit + 1) from None


def payload_of(payload: bytes, flags: int) -> bytes:
    """The logical payload of a received frame (decompressed if flagged)."""
    if flags & COMPRESSED:
        return decompress_payload(payload)
    return payload


# ── Frames (Aster-SPEC §6.1) ─────────────────────────────────────────────────


@runtime_checkable
class SendStream(Protocol):
    """Minimal async send-stream interface (matches IrohSendStream)."""

    async def write_all(self, data: bytes) -> None: ...


@runtime_checkable
class RecvStream(Protocol):
    """Minimal async recv-stream interface (matches IrohRecvStream)."""

    async def read_exact(self, n: int) -> bytes: ...


def encode_frame(payload: bytes, flags: int = 0) -> bytes:
    """Return ``[u32 LE length][flags][payload]``.

    Empty payloads are allowed only on TRAILER / CANCEL / END_STREAM frames.
    """
    try:
        return _native.encode_frame(payload, flags)
    except ValueError as e:
        raise FramingError(str(e)) from None


def decode_frame(data: bytes) -> tuple[bytes, int, int]:
    """Decode one frame from the front of *data* → ``(payload, flags, consumed)``."""
    try:
        return _native.decode_frame(data)
    except ValueError as e:
        raise FramingError(str(e)) from None


async def write_frame(stream: SendStream, payload: bytes, flags: int = 0) -> None:
    """Write a single frame to *stream*."""
    await stream.write_all(encode_frame(payload, flags))


async def read_frame(
    stream: RecvStream,
    timeout_s: float | None = None,
) -> tuple[bytes, int] | None:
    """Read one frame from *stream*.

    Returns ``(payload, flags)``, or ``None`` when the stream ended cleanly
    before a length prefix. Pass ``timeout_s > 0`` for a per-read timeout
    (off by default: the QUIC idle timeout already bounds hung peers).

    Raises:
        FramingError: zero / oversized length, truncated body, or timeout.
    """
    use_timeout = timeout_s is not None and timeout_s > 0
    try:
        if use_timeout:
            length_bytes = await asyncio.wait_for(
                stream.read_exact(_LENGTH_SIZE), timeout=timeout_s
            )
        else:
            length_bytes = await stream.read_exact(_LENGTH_SIZE)
    except asyncio.TimeoutError:
        raise FramingError("frame read timed out waiting for length header")
    except Exception:
        # Stream ended or was reset -- treat as clean EOF.
        return None

    if len(length_bytes) < _LENGTH_SIZE:
        return None

    body_len = int.from_bytes(length_bytes, "little")
    if body_len == 0:
        raise FramingError("received zero-length frame")
    if body_len > MAX_FRAME_SIZE:
        raise FramingError(f"frame size {body_len} exceeds maximum {MAX_FRAME_SIZE}")

    try:
        if use_timeout:
            body = await asyncio.wait_for(stream.read_exact(body_len), timeout=timeout_s)
        else:
            body = await stream.read_exact(body_len)
    except asyncio.TimeoutError:
        raise FramingError(f"frame read timed out waiting for {body_len} bytes of body")

    if len(body) < body_len:
        raise FramingError(f"incomplete frame: expected {body_len} bytes, got {len(body)}")

    return body[1:], body[0]


__all__ = [
    "COMPRESSED", "TRAILER", "HEADER", "ROW_SCHEMA", "CALL", "CANCEL",
    "END_STREAM", "TUNNEL", "MAX_FRAME_SIZE", "COMPRESSION_THRESHOLD",
    "ZSTD_LEVEL", "MAX_DECOMPRESSED_SIZE", "FORY_VERSION",
    "FramingError", "EnvelopeError",
    "StreamHeader", "CallHeader", "RpcStatus",
    "compress_payload", "decompress_payload", "payload_of",
    "SendStream", "RecvStream",
    "encode_frame", "decode_frame", "write_frame", "read_frame",
]
