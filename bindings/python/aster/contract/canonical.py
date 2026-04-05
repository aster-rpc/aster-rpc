"""
aster.contract.canonical — Low-level canonical byte writers.

Spec reference: Aster-ContractIdentity.md §11.3.2

Implements the custom byte-level encoding for the Fory XLANG canonical
serialization format. These are hand-written primitives — NOT wrappers
around pyfory.

Encoding rules:
- Varint: unsigned LEB128
- ZigZag i32/i64: (n << 1) ^ (n >> 31/63) then varint
- String: varint((utf8_len << 2) | 2) then UTF-8 bytes
- Bool: 0x01 / 0x00
- Bytes (hash field): varint(len) then raw bytes
- Float64: 8 bytes little-endian IEEE 754
- List: varint(length) then 0x0C then elements
- Optional absent: 0xFD (NULL_FLAG)
- Optional present: 0x00 then value
- Enum: unsigned varint of integer value
"""

from __future__ import annotations

import io
import struct


# ── Null / presence flags ────────────────────────────────────────────────────

NULL_FLAG: int = 0xFD
"""Sentinel byte written for absent optional fields."""

LIST_ELEMENT_HEADER: int = 0x0C
"""Homogeneous declared-type non-null non-ref element header byte."""


# ── Primitive writers ─────────────────────────────────────────────────────────


def write_varint(w: io.BytesIO, value: int) -> None:
    """Write an unsigned LEB128 variable-length integer.

    Args:
        w: Output buffer.
        value: Non-negative integer to encode.
    """
    if value < 0:
        raise ValueError(f"write_varint: value must be non-negative, got {value}")
    while True:
        byte = value & 0x7F
        value >>= 7
        if value != 0:
            w.write(bytes([byte | 0x80]))
        else:
            w.write(bytes([byte]))
            break


def write_zigzag_i32(w: io.BytesIO, value: int) -> None:
    """Write a ZigZag-encoded signed 32-bit integer as varint.

    ZigZag(n) = (n << 1) ^ (n >> 31)

    Args:
        w: Output buffer.
        value: Signed 32-bit integer.
    """
    # Ensure 32-bit arithmetic with sign extension
    value = value & 0xFFFFFFFF
    if value >= 0x80000000:
        value -= 0x100000000
    zigzag = (value << 1) ^ (value >> 31)
    write_varint(w, zigzag & 0xFFFFFFFF)


def write_zigzag_i64(w: io.BytesIO, value: int) -> None:
    """Write a ZigZag-encoded signed 64-bit integer as varint.

    ZigZag(n) = (n << 1) ^ (n >> 63)

    Args:
        w: Output buffer.
        value: Signed 64-bit integer.
    """
    zigzag = (value << 1) ^ (value >> 63)
    # Mask to 64-bit unsigned
    write_varint(w, zigzag & 0xFFFFFFFFFFFFFFFF)


def write_string(w: io.BytesIO, s: str) -> None:
    """Write a UTF-8 Fory XLANG string.

    Format: varint((utf8_byte_length << 2) | 2) followed by UTF-8 bytes.
    - Empty string "": 0x02 (no bytes follow)
    - "xlang" (5 bytes): 0x16 then b"xlang"
    - "EmptyService" (12 bytes): 0x32 then b"EmptyService"

    Args:
        w: Output buffer.
        s: String to encode.
    """
    encoded = s.encode("utf-8")
    header = (len(encoded) << 2) | 2
    write_varint(w, header)
    if encoded:
        w.write(encoded)


def write_bytes_field(w: io.BytesIO, data: bytes) -> None:
    """Write a raw bytes field (used for hash fields).

    Format: varint(length) followed by raw bytes.
    - Empty b"": varint(0) = 0x00
    - 32-byte hash: varint(32) = 0x20 then 32 bytes

    Args:
        w: Output buffer.
        data: Bytes to encode.
    """
    write_varint(w, len(data))
    if data:
        w.write(data)


def write_bool(w: io.BytesIO, value: bool) -> None:
    """Write a boolean as a single byte: 0x01 for True, 0x00 for False.

    Args:
        w: Output buffer.
        value: Boolean value.
    """
    w.write(b"\x01" if value else b"\x00")


def write_float64(w: io.BytesIO, value: float) -> None:
    """Write a float64 as 8 bytes little-endian IEEE 754.

    Args:
        w: Output buffer.
        value: Float to encode.
    """
    w.write(struct.pack("<d", value))


def write_list_header(w: io.BytesIO, length: int) -> None:
    """Write a list header: varint(length) then 0x0C element header.

    From Appendix A.2:
      Field 3 (methods: list<MethodDef>, empty):
          varuint32(0)       // length = 0
          0x0C               // elements header

    Args:
        w: Output buffer.
        length: Number of elements.
    """
    write_varint(w, length)
    w.write(bytes([LIST_ELEMENT_HEADER]))


def write_optional_absent(w: io.BytesIO) -> None:
    """Write a NULL_FLAG (0xFD) for an absent optional field.

    Args:
        w: Output buffer.
    """
    w.write(bytes([NULL_FLAG]))


def write_optional_present_prefix(w: io.BytesIO) -> None:
    """Write 0x00 presence byte before a present optional field's value.

    Args:
        w: Output buffer.
    """
    w.write(b"\x00")


# ── CanonicalWriter ───────────────────────────────────────────────────────────


class CanonicalWriter:
    """Stateful writer that accumulates canonical bytes in a BytesIO buffer.

    Convenience wrapper around the module-level write_* functions for
    callers that prefer an object-oriented interface.

    Example::

        cw = CanonicalWriter()
        cw.string("hello")
        cw.zigzag_i32(42)
        data = cw.getvalue()
    """

    def __init__(self) -> None:
        self._buf = io.BytesIO()

    def varint(self, value: int) -> "CanonicalWriter":
        write_varint(self._buf, value)
        return self

    def zigzag_i32(self, value: int) -> "CanonicalWriter":
        write_zigzag_i32(self._buf, value)
        return self

    def zigzag_i64(self, value: int) -> "CanonicalWriter":
        write_zigzag_i64(self._buf, value)
        return self

    def string(self, s: str) -> "CanonicalWriter":
        write_string(self._buf, s)
        return self

    def bytes_field(self, data: bytes) -> "CanonicalWriter":
        write_bytes_field(self._buf, data)
        return self

    def bool_(self, value: bool) -> "CanonicalWriter":
        write_bool(self._buf, value)
        return self

    def float64(self, value: float) -> "CanonicalWriter":
        write_float64(self._buf, value)
        return self

    def list_header(self, length: int) -> "CanonicalWriter":
        write_list_header(self._buf, length)
        return self

    def optional_absent(self) -> "CanonicalWriter":
        write_optional_absent(self._buf)
        return self

    def optional_present_prefix(self) -> "CanonicalWriter":
        write_optional_present_prefix(self._buf)
        return self

    def raw(self, data: bytes) -> "CanonicalWriter":
        """Write raw bytes directly."""
        self._buf.write(data)
        return self

    def getvalue(self) -> bytes:
        """Return the accumulated bytes."""
        return self._buf.getvalue()

    def __len__(self) -> int:
        return len(self.getvalue())
