"""
tests/conformance/wire/test_wire_vectors.py

Phase 13 wire frame conformance tests.

Verifies the exact byte layout of control frames and that encode/decode
is symmetric.  Binary fixture files (*.bin) for simple control frames are
generated once by the conftest.py in this directory, and then read back here
to validate byte-level stability.

Spec reference: Aster-SPEC.md §6.1 (stream framing)
"""

from __future__ import annotations

import io
import struct
from pathlib import Path

import pytest

from aster_python.aster.framing import (
    CALL,
    CANCEL,
    COMPRESSED,
    HEADER,
    ROW_SCHEMA,
    TRAILER,
    FramingError,
    read_frame,
    write_frame,
)

# ── Constants ─────────────────────────────────────────────────────────────────

_FIXTURES_DIR = Path(__file__).parent


# ── Fake in-memory stream helpers ─────────────────────────────────────────────


class _MemSendStream:
    """Accumulates write_all calls into a bytes buffer."""

    def __init__(self) -> None:
        self._buf = io.BytesIO()

    async def write_all(self, data: bytes) -> None:
        self._buf.write(data)

    def getvalue(self) -> bytes:
        return self._buf.getvalue()


class _MemRecvStream:
    """Reads from a fixed bytes buffer."""

    def __init__(self, data: bytes) -> None:
        self._buf = io.BytesIO(data)

    async def read_exact(self, n: int) -> bytes:
        chunk = self._buf.read(n)
        if len(chunk) < n:
            raise EOFError(f"stream ended: wanted {n}, got {len(chunk)}")
        return chunk


# ── CANCEL flags-only frame ───────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_cancel_frame_is_exactly_5_bytes():
    """A CANCEL flags-only frame is exactly 5 bytes: 4-byte length + 1-byte flags."""
    send = _MemSendStream()
    await write_frame(send, b"", flags=CANCEL)
    raw = send.getvalue()
    # Expected: \x01\x00\x00\x00\x20 (length=1, flags=0x20)
    assert len(raw) == 5
    assert raw == b"\x01\x00\x00\x00\x20"


@pytest.mark.asyncio
async def test_cancel_frame_matches_fixture():
    """CANCEL frame bytes match the committed binary fixture."""
    fixture = _FIXTURES_DIR / "cancel_flags_only.bin"
    assert fixture.exists(), f"Fixture not found: {fixture}"
    expected = fixture.read_bytes()

    send = _MemSendStream()
    await write_frame(send, b"", flags=CANCEL)
    assert send.getvalue() == expected


@pytest.mark.asyncio
async def test_cancel_frame_decodes_correctly():
    """A CANCEL frame written and then read back yields empty payload with CANCEL flag."""
    send = _MemSendStream()
    await write_frame(send, b"", flags=CANCEL)

    recv = _MemRecvStream(send.getvalue())
    result = await read_frame(recv)
    assert result is not None
    payload, flags = result
    assert payload == b""
    assert flags == CANCEL


# ── TRAILER empty frame ───────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_trailer_empty_frame_is_exactly_5_bytes():
    """A TRAILER frame with no payload is exactly 5 bytes."""
    send = _MemSendStream()
    await write_frame(send, b"", flags=TRAILER)
    raw = send.getvalue()
    # Expected: \x01\x00\x00\x00\x02 (length=1, flags=0x02)
    assert len(raw) == 5
    assert raw == b"\x01\x00\x00\x00\x02"


@pytest.mark.asyncio
async def test_trailer_empty_frame_matches_fixture():
    """TRAILER-empty frame bytes match the committed binary fixture."""
    fixture = _FIXTURES_DIR / "trailer_ok.bin"
    assert fixture.exists(), f"Fixture not found: {fixture}"
    expected = fixture.read_bytes()

    send = _MemSendStream()
    await write_frame(send, b"", flags=TRAILER)
    assert send.getvalue() == expected


@pytest.mark.asyncio
async def test_trailer_frame_decodes_correctly():
    """A TRAILER frame written and read back yields empty payload with TRAILER flag."""
    send = _MemSendStream()
    await write_frame(send, b"", flags=TRAILER)

    recv = _MemRecvStream(send.getvalue())
    result = await read_frame(recv)
    assert result is not None
    payload, flags = result
    assert payload == b""
    assert flags == TRAILER


# ── HEADER frame structure ────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_header_frame_structure_4byte_length_prefix():
    """HEADER frame has a 4-byte LE u32 length prefix."""
    payload = b"header payload bytes"
    send = _MemSendStream()
    await write_frame(send, payload, flags=HEADER)
    raw = send.getvalue()

    length = struct.unpack("<I", raw[:4])[0]
    # Length = flags byte (1) + len(payload)
    assert length == 1 + len(payload)


@pytest.mark.asyncio
async def test_header_frame_flags_byte_is_0x04():
    """HEADER frame flags byte is exactly 0x04."""
    payload = b"some data"
    send = _MemSendStream()
    await write_frame(send, payload, flags=HEADER)
    raw = send.getvalue()

    flags_byte = raw[4]
    assert flags_byte == HEADER  # 0x04


@pytest.mark.asyncio
async def test_header_frame_round_trips():
    """HEADER frame can be written and read back with the same payload."""
    original_payload = b"\x00\x01\x02\x03fake serialized header"
    send = _MemSendStream()
    await write_frame(send, original_payload, flags=HEADER)

    recv = _MemRecvStream(send.getvalue())
    result = await read_frame(recv)
    assert result is not None
    payload, flags = result
    assert flags == HEADER
    assert payload == original_payload


# ── CALL frame ────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_call_frame_flags_is_0x10():
    """In-session CALL frame has flags=0x10."""
    payload = b"call header bytes"
    send = _MemSendStream()
    await write_frame(send, payload, flags=CALL)
    raw = send.getvalue()

    flags_byte = raw[4]
    assert flags_byte == CALL  # 0x10


@pytest.mark.asyncio
async def test_call_frame_round_trips():
    """CALL frame written and read back yields the same payload."""
    original = b"some_method_call_header"
    send = _MemSendStream()
    await write_frame(send, original, flags=CALL)

    recv = _MemRecvStream(send.getvalue())
    result = await read_frame(recv)
    assert result is not None
    payload, flags = result
    assert flags == CALL
    assert payload == original


# ── Multiple frames on the same stream ────────────────────────────────────────


@pytest.mark.asyncio
async def test_multiple_frames_sequential_read():
    """Multiple frames written sequentially to the same stream can be read back in order."""
    send = _MemSendStream()
    frames = [
        (b"first frame data", HEADER),
        (b"second frame data", CALL),
        (b"", TRAILER),
    ]
    for payload, flags in frames:
        await write_frame(send, payload, flags=flags)

    recv = _MemRecvStream(send.getvalue())
    for expected_payload, expected_flags in frames:
        result = await read_frame(recv)
        assert result is not None
        got_payload, got_flags = result
        assert got_payload == expected_payload
        assert got_flags == expected_flags


# ── Error cases ────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_write_frame_zero_payload_non_control_raises():
    """write_frame raises FramingError for a zero-length payload on a non-control frame."""
    send = _MemSendStream()
    with pytest.raises(FramingError):
        await write_frame(send, b"", flags=0)


@pytest.mark.asyncio
async def test_read_frame_returns_none_on_empty_stream():
    """read_frame returns None when the stream is empty (clean EOF)."""
    recv = _MemRecvStream(b"")
    result = await read_frame(recv)
    assert result is None


# ── Flag constants sanity checks ─────────────────────────────────────────────


def test_flag_constants_are_distinct_powers_of_two():
    """All flag constants are distinct and each is a power of two."""
    flag_values = [COMPRESSED, TRAILER, HEADER, ROW_SCHEMA, CALL, CANCEL]
    assert len(set(flag_values)) == len(flag_values), "Flag values must be distinct"
    for val in flag_values:
        assert val > 0 and (val & (val - 1)) == 0, f"Flag {val} is not a power of two"


def test_cancel_flag_value():
    """CANCEL flag is exactly 0x20."""
    assert CANCEL == 0x20


def test_trailer_flag_value():
    """TRAILER flag is exactly 0x02."""
    assert TRAILER == 0x02


def test_header_flag_value():
    """HEADER flag is exactly 0x04."""
    assert HEADER == 0x04


def test_call_flag_value():
    """CALL flag is exactly 0x10."""
    assert CALL == 0x10
