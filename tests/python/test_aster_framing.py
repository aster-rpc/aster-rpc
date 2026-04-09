"""
Phase 1 tests: Wire Protocol & Framing.

Tests cover:
- Frame round-trip encoding/decoding
- Flag parsing
- Max-size rejection
- Zero-length frame rejection
- StreamHeader/CallHeader/RpcStatus Fory serialization round-trip
"""

from __future__ import annotations

import asyncio
import struct
from io import BytesIO

import pytest

from aster.status import (
    StatusCode,
    RpcError,
    NotFoundError,
    InvalidArgumentError,
)
from aster.rpc_types import SerializationMode, RetryPolicy, ExponentialBackoff
from aster.framing import (
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
from aster.protocol import StreamHeader, CallHeader, RpcStatus, wire_type

# ── In-memory async stream helpers ──────────────────────────────────────────


class MemSendStream:
    """In-memory async send stream for testing."""

    def __init__(self) -> None:
        self.buf = bytearray()

    async def write_all(self, data: bytes) -> None:
        self.buf.extend(data)


class MemRecvStream:
    """In-memory async recv stream for testing."""

    def __init__(self, data: bytes) -> None:
        self._data = memoryview(data)
        self._pos = 0

    async def read_exact(self, n: int) -> bytes:
        if self._pos + n > len(self._data):
            raise EOFError("end of stream")
        chunk = bytes(self._data[self._pos : self._pos + n])
        self._pos += n
        return chunk


class EmptyRecvStream:
    """Recv stream that immediately raises (simulating EOF)."""

    async def read_exact(self, n: int) -> bytes:
        raise EOFError("end of stream")


# ── StatusCode tests ────────────────────────────────────────────────────────


class TestStatusCode:
    def test_all_codes_exist(self):
        """All 17 status codes (0--16) are defined."""
        assert len(StatusCode) == 17
        assert StatusCode.OK == 0
        assert StatusCode.UNAUTHENTICATED == 16

    def test_codes_are_ints(self):
        for code in StatusCode:
            assert isinstance(code, int)

    def test_code_names(self):
        expected = [
            "OK", "CANCELLED", "UNKNOWN", "INVALID_ARGUMENT",
            "DEADLINE_EXCEEDED", "NOT_FOUND", "ALREADY_EXISTS",
            "PERMISSION_DENIED", "RESOURCE_EXHAUSTED", "FAILED_PRECONDITION",
            "ABORTED", "OUT_OF_RANGE", "UNIMPLEMENTED", "INTERNAL",
            "UNAVAILABLE", "DATA_LOSS", "UNAUTHENTICATED",
        ]
        assert [c.name for c in StatusCode] == expected


# ── RpcError tests ──────────────────────────────────────────────────────────


class TestRpcError:
    def test_basic_construction(self):
        err = RpcError(StatusCode.NOT_FOUND, "thing missing")
        assert err.code == StatusCode.NOT_FOUND
        assert err.message == "thing missing"
        assert err.details == {}
        assert "NOT_FOUND" in str(err)

    def test_with_details(self):
        err = RpcError(
            StatusCode.INTERNAL, "boom", details={"trace_id": "abc"}
        )
        assert err.details == {"trace_id": "abc"}

    def test_is_exception(self):
        err = RpcError(StatusCode.UNKNOWN, "oops")
        assert isinstance(err, Exception)

    def test_repr(self):
        err = RpcError(StatusCode.OK, "fine")
        r = repr(err)
        assert "RpcError" in r
        assert "OK" in r

    def test_from_status_returns_specific_subclass(self):
        err = RpcError.from_status(StatusCode.NOT_FOUND, "missing")
        assert isinstance(err, NotFoundError)
        assert err.code == StatusCode.NOT_FOUND

    def test_specific_subclass_preserves_status(self):
        err = InvalidArgumentError("bad input")
        assert isinstance(err, RpcError)
        assert err.code == StatusCode.INVALID_ARGUMENT
        assert err.message == "bad input"


# ── SerializationMode tests ─────────────────────────────────────────────────


class TestSerializationMode:
    def test_values(self):
        assert SerializationMode.XLANG == 0
        assert SerializationMode.NATIVE == 1
        assert SerializationMode.ROW == 2


# ── RetryPolicy tests ──────────────────────────────────────────────────────


class TestRetryPolicy:
    def test_defaults(self):
        p = RetryPolicy()
        assert p.max_attempts == 3
        assert p.backoff.initial_ms == 100
        assert p.backoff.multiplier == 2.0

    def test_custom(self):
        p = RetryPolicy(
            max_attempts=5,
            backoff=ExponentialBackoff(initial_ms=200, max_ms=60_000),
        )
        assert p.max_attempts == 5
        assert p.backoff.max_ms == 60_000


# ── Flag constant tests ────────────────────────────────────────────────────


class TestFlagConstants:
    def test_flag_values(self):
        assert COMPRESSED == 0x01
        assert TRAILER == 0x02
        assert HEADER == 0x04
        assert ROW_SCHEMA == 0x08
        assert CALL == 0x10
        assert CANCEL == 0x20

    def test_flags_are_distinct_bits(self):
        """Each flag occupies a unique bit position."""
        all_flags = [COMPRESSED, TRAILER, HEADER, ROW_SCHEMA, CALL, CANCEL]
        combined = 0
        for f in all_flags:
            assert combined & f == 0, f"flag {f:#x} overlaps"
            combined |= f

    def test_flag_combinations(self):
        """Flags can be OR-combined."""
        combo = HEADER | COMPRESSED
        assert combo & HEADER
        assert combo & COMPRESSED
        assert not (combo & TRAILER)


# ── Frame round-trip tests ──────────────────────────────────────────────────


class TestFrameRoundTrip:
    @pytest.mark.asyncio
    async def test_simple_payload(self):
        """A simple payload survives write → read."""
        send = MemSendStream()
        payload = b"hello world"
        await write_frame(send, payload, flags=0)

        recv = MemRecvStream(bytes(send.buf))
        result = await read_frame(recv)
        assert result is not None
        got_payload, got_flags = result
        assert got_payload == payload
        assert got_flags == 0

    @pytest.mark.asyncio
    async def test_with_header_flag(self):
        """HEADER flag is preserved."""
        send = MemSendStream()
        payload = b"\x01\x02\x03"
        await write_frame(send, payload, flags=HEADER)

        recv = MemRecvStream(bytes(send.buf))
        result = await read_frame(recv)
        assert result is not None
        got_payload, got_flags = result
        assert got_payload == payload
        assert got_flags == HEADER

    @pytest.mark.asyncio
    async def test_with_combined_flags(self):
        """Multiple flags combined are preserved."""
        send = MemSendStream()
        flags = HEADER | COMPRESSED
        payload = b"compressed-header-data"
        await write_frame(send, payload, flags=flags)

        recv = MemRecvStream(bytes(send.buf))
        result = await read_frame(recv)
        assert result is not None
        got_payload, got_flags = result
        assert got_payload == payload
        assert got_flags == flags

    @pytest.mark.asyncio
    async def test_trailer_empty_payload(self):
        """TRAILER flag allows an empty payload (status-only frame)."""
        send = MemSendStream()
        await write_frame(send, b"", flags=TRAILER)

        recv = MemRecvStream(bytes(send.buf))
        result = await read_frame(recv)
        assert result is not None
        got_payload, got_flags = result
        assert got_payload == b""
        assert got_flags == TRAILER

    @pytest.mark.asyncio
    async def test_cancel_empty_payload(self):
        """CANCEL flag allows an empty payload (flags-only cancel frame, spec §5.2)."""
        send = MemSendStream()
        await write_frame(send, b"", flags=CANCEL)

        recv = MemRecvStream(bytes(send.buf))
        result = await read_frame(recv)
        assert result is not None
        got_payload, got_flags = result
        assert got_payload == b""
        assert got_flags == CANCEL

    @pytest.mark.asyncio
    async def test_large_payload(self):
        """A payload near-but-under the max frame size works."""
        send = MemSendStream()
        # 1 byte for flags, so max payload is MAX_FRAME_SIZE - 1
        payload = b"\xAB" * (MAX_FRAME_SIZE - 1)
        await write_frame(send, payload, flags=0)

        recv = MemRecvStream(bytes(send.buf))
        result = await read_frame(recv)
        assert result is not None
        got_payload, _ = result
        assert len(got_payload) == MAX_FRAME_SIZE - 1

    @pytest.mark.asyncio
    async def test_multiple_frames(self):
        """Multiple frames written sequentially can all be read back."""
        send = MemSendStream()
        payloads = [b"frame0", b"frame1", b"frame2"]
        for p in payloads:
            await write_frame(send, p, flags=0)

        recv = MemRecvStream(bytes(send.buf))
        for expected in payloads:
            result = await read_frame(recv)
            assert result is not None
            got_payload, _ = result
            assert got_payload == expected

    @pytest.mark.asyncio
    async def test_all_flag_values(self):
        """Each flag value round-trips correctly."""
        for flag in [COMPRESSED, TRAILER, HEADER, ROW_SCHEMA, CALL, CANCEL]:
            send = MemSendStream()
            payload = b"test" if flag != TRAILER else b""
            await write_frame(send, payload, flags=flag)

            recv = MemRecvStream(bytes(send.buf))
            result = await read_frame(recv)
            assert result is not None
            _, got_flags = result
            assert got_flags == flag


# ── Max-size rejection tests ────────────────────────────────────────────────


class TestMaxSizeRejection:
    @pytest.mark.asyncio
    async def test_write_oversized_frame(self):
        """Writing a frame larger than MAX_FRAME_SIZE raises FramingError."""
        send = MemSendStream()
        payload = b"\x00" * MAX_FRAME_SIZE  # +1 for flags = over limit
        with pytest.raises(FramingError, match="exceeds maximum"):
            await write_frame(send, payload, flags=0)

    @pytest.mark.asyncio
    async def test_read_oversized_frame(self):
        """Reading a frame with a length header > MAX_FRAME_SIZE raises FramingError."""
        # Craft a raw frame with an oversized length
        bad_length = MAX_FRAME_SIZE + 1
        raw = struct.pack("<I", bad_length)
        recv = MemRecvStream(raw)
        with pytest.raises(FramingError, match="exceeds maximum"):
            await read_frame(recv)


# ── Zero-length frame rejection tests ───────────────────────────────────────


class TestZeroLengthRejection:
    @pytest.mark.asyncio
    async def test_write_empty_payload_no_trailer(self):
        """Writing an empty payload without TRAILER flag raises FramingError."""
        send = MemSendStream()
        with pytest.raises(FramingError, match="zero-length"):
            await write_frame(send, b"", flags=0)

    @pytest.mark.asyncio
    async def test_read_zero_length_frame(self):
        """Reading a frame with Length=0 raises FramingError."""
        raw = struct.pack("<I", 0)
        recv = MemRecvStream(raw)
        with pytest.raises(FramingError, match="zero-length"):
            await read_frame(recv)


# ── EOF handling tests ──────────────────────────────────────────────────────


class TestEOFHandling:
    @pytest.mark.asyncio
    async def test_empty_stream_returns_none(self):
        """read_frame on an empty stream returns None."""
        recv = EmptyRecvStream()
        result = await read_frame(recv)
        assert result is None


# ── Protocol type tests ─────────────────────────────────────────────────────


class TestProtocolTypes:
    def test_stream_header_defaults(self):
        h = StreamHeader()
        assert h.service == ""
        assert h.method == ""
        assert h.version == 0
        assert h.metadata_keys == []

    def test_stream_header_construction(self):
        h = StreamHeader(
            service="MyService",
            method="do_thing",
            version=1,
            call_id="call-1",
            deadline_epoch_ms=1000,
            serialization_mode=0,
            metadata_keys=["key"],
            metadata_values=["val"],
        )
        assert h.service == "MyService"
        assert h.method == "do_thing"
        assert h.metadata_keys == ["key"]

    def test_call_header_defaults(self):
        h = CallHeader()
        assert h.method == ""
        assert h.call_id == ""

    def test_rpc_status_defaults(self):
        s = RpcStatus()
        assert s.code == 0
        assert s.message == ""

    def test_rpc_status_construction(self):
        s = RpcStatus(
            code=StatusCode.INTERNAL,
            message="oops",
            detail_keys=["k"],
            detail_values=["v"],
        )
        assert s.code == 13
        assert s.message == "oops"


# ── Fory tag tests ──────────────────────────────────────────────────────────


class TestForyTags:
    def test_stream_header_tag(self):
        assert StreamHeader.__wire_type__ == "_aster/StreamHeader"
        assert StreamHeader.__fory_namespace__ == "_aster"
        assert StreamHeader.__fory_typename__ == "StreamHeader"

    def test_call_header_tag(self):
        assert CallHeader.__wire_type__ == "_aster/CallHeader"
        assert CallHeader.__fory_namespace__ == "_aster"
        assert CallHeader.__fory_typename__ == "CallHeader"

    def test_rpc_status_tag(self):
        assert RpcStatus.__wire_type__ == "_aster/RpcStatus"
        assert RpcStatus.__fory_namespace__ == "_aster"
        assert RpcStatus.__fory_typename__ == "RpcStatus"

    def test_custom_wire_type(self):
        @wire_type("myapp.pkg/MyType")
        class MyType:
            pass

        assert MyType.__wire_type__ == "myapp.pkg/MyType"
        assert MyType.__fory_namespace__ == "myapp.pkg"
        assert MyType.__fory_typename__ == "MyType"

    def test_wire_type_no_namespace(self):
        @wire_type("SimpleTag")
        class Simple:
            pass

        assert Simple.__wire_type__ == "SimpleTag"
        assert Simple.__fory_namespace__ == ""
        assert Simple.__fory_typename__ == "SimpleTag"


# ── Fory serialization round-trip tests ─────────────────────────────────────

try:
    import pyfory

    HAS_PYFORY = True
except ImportError:
    HAS_PYFORY = False


@pytest.mark.skipif(not HAS_PYFORY, reason="pyfory not installed")
class TestForySerializationRoundTrip:
    """Verify that protocol types round-trip through Fory XLANG."""

    def _create_fory(self, *types):
        f = pyfory.Fory()
        for cls in types:
            f.register_type(
                cls,
                namespace=cls.__fory_namespace__,
                typename=cls.__fory_typename__,
            )
        return f

    def test_stream_header_round_trip(self):
        f = self._create_fory(StreamHeader)
        original = StreamHeader(
            service="TestService",
            method="test_method",
            version=1,
            call_id="call-001",
            deadline_epoch_ms=1712000000000,
            serialization_mode=0,
            metadata_keys=["trace_id", "auth"],
            metadata_values=["t1", "token"],
        )
        data = f.serialize(original)
        assert isinstance(data, (bytes, bytearray))
        assert len(data) > 0

        restored = f.deserialize(data)
        assert isinstance(restored, StreamHeader)
        assert restored.service == original.service
        assert restored.method == original.method
        assert restored.version == original.version
        assert restored.call_id == original.call_id
        assert restored.deadline_epoch_ms == original.deadline_epoch_ms
        assert restored.serialization_mode == original.serialization_mode
        assert restored.metadata_keys == original.metadata_keys
        assert restored.metadata_values == original.metadata_values

    def test_call_header_round_trip(self):
        f = self._create_fory(CallHeader)
        original = CallHeader(
            method="assign_task",
            call_id="call-002",
            deadline_epoch_ms=1712000001000,
            metadata_keys=["k1"],
            metadata_values=["v1"],
        )
        data = f.serialize(original)
        restored = f.deserialize(data)
        assert isinstance(restored, CallHeader)
        assert restored.method == original.method
        assert restored.call_id == original.call_id
        assert restored.deadline_epoch_ms == original.deadline_epoch_ms
        assert restored.metadata_keys == original.metadata_keys
        assert restored.metadata_values == original.metadata_values

    def test_rpc_status_round_trip(self):
        f = self._create_fory(RpcStatus)
        original = RpcStatus(
            code=StatusCode.DEADLINE_EXCEEDED,
            message="timed out after 30s",
            detail_keys=["retry_after_ms"],
            detail_values=["5000"],
        )
        data = f.serialize(original)
        restored = f.deserialize(data)
        assert isinstance(restored, RpcStatus)
        assert restored.code == original.code
        assert restored.message == original.message
        assert restored.detail_keys == original.detail_keys
        assert restored.detail_values == original.detail_values

    def test_stream_header_determinism(self):
        """Same StreamHeader serializes to identical bytes."""
        f = self._create_fory(StreamHeader)
        h = StreamHeader(
            service="Svc", method="m", version=1,
            call_id="c1",
        )
        b1 = f.serialize(h)
        b2 = f.serialize(h)
        assert b1 == b2

    def test_rpc_status_determinism(self):
        """Same RpcStatus serializes to identical bytes."""
        f = self._create_fory(RpcStatus)
        s = RpcStatus(code=0, message="ok")
        b1 = f.serialize(s)
        b2 = f.serialize(s)
        assert b1 == b2

    def test_rpc_status_ok(self):
        """An OK status round-trips correctly."""
        f = self._create_fory(RpcStatus)
        ok = RpcStatus(code=StatusCode.OK, message="")
        data = f.serialize(ok)
        restored = f.deserialize(data)
        assert restored.code == 0
        assert restored.message == ""

    def test_stream_header_empty_metadata(self):
        """StreamHeader with no metadata round-trips."""
        f = self._create_fory(StreamHeader)
        h = StreamHeader(service="S", method="m")
        data = f.serialize(h)
        restored = f.deserialize(data)
        assert restored.metadata_keys == []
        assert restored.metadata_values == []