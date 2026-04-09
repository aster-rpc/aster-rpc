"""
aster.json_codec -- JSON serialization for cross-language RPC payloads.

Used when a client requests SerializationMode.JSON (mode 3) in the
StreamHeader. Converts between JSON bytes and @wire_type dataclasses.

The StreamHeader and RpcStatus protocol frames always use Fory XLANG
regardless of payload mode.
"""

from __future__ import annotations

import dataclasses
import json
from typing import Any, Optional, get_type_hints


def safe_decompress(data: bytes) -> bytes:
    """Decompress zstd bytes with a size limit to prevent decompression bombs."""
    import zstandard
    from aster.limits import MAX_DECOMPRESSED_SIZE, LimitExceeded

    dctx = zstandard.ZstdDecompressor()
    reader = dctx.stream_reader(data)
    chunks: list[bytes] = []
    total = 0
    while True:
        chunk = reader.read(65536)
        if not chunk:
            break
        total += len(chunk)
        if total > MAX_DECOMPRESSED_SIZE:
            raise LimitExceeded(
                f"Decompressed size exceeds {MAX_DECOMPRESSED_SIZE} bytes"
            )
        chunks.append(chunk)
    return b"".join(chunks)


class JsonProxyCodec:
    """JSON codec for the proxy client.

    Encodes all frames (including StreamHeader/RpcStatus) as JSON.
    Sets serializationMode=3 so the server uses JSON decoding for payloads.
    """

    def encode(self, obj: Any) -> bytes:
        return json_encode(obj)

    def encode_compressed(self, obj: Any) -> tuple[bytes, bool]:
        return json_encode(obj), False

    def decode(self, data: bytes, expected_type: type | None = None) -> Any:
        # Handle Fory XLANG error trailers from pre-JSON-mode error paths.
        # Fory starts with 0x02; JSON starts with '{'.
        if data and data[0:1] != b'{':
            # Fall back to Fory decode for protocol frames
            try:
                from aster.codec import ForyCodec
                return ForyCodec().decode(data, expected_type)
            except Exception:
                pass
        return json_decode(data, expected_type)

    def decode_compressed(
        self, data: bytes, compressed: bool, expected_type: type | None = None
    ) -> Any:
        if compressed:
            data = safe_decompress(data)
        return json_decode(data, expected_type)


def json_encode(obj: Any) -> bytes:
    """Serialize a dataclass or dict to JSON bytes."""
    if dataclasses.is_dataclass(obj) and not isinstance(obj, type):
        return json.dumps(dataclasses.asdict(obj), separators=(",", ":")).encode("utf-8")
    if isinstance(obj, dict):
        return json.dumps(obj, separators=(",", ":")).encode("utf-8")
    return json.dumps(obj).encode("utf-8")


def _camel_to_snake(name: str) -> str:
    """Convert camelCase to snake_case."""
    import re
    s1 = re.sub(r"(.)([A-Z][a-z]+)", r"\1_\2", name)
    return re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", s1).lower()


def _normalize_keys(d: dict) -> dict:
    """Convert camelCase keys to snake_case for cross-language compat."""
    return {_camel_to_snake(k): v for k, v in d.items()}


def json_decode(data: bytes | str, expected_type: type | None = None) -> Any:
    """Deserialize JSON bytes into a dataclass instance or plain dict.

    If expected_type is a dataclass, constructs an instance from the JSON.
    Handles nested @wire_type dataclasses and Optional fields.
    """
    # Explicit UTF-8 decode to avoid Python's detect_encoding
    # misidentifying short byte sequences as UTF-32
    if isinstance(data, (bytes, bytearray, memoryview)):
        data = bytes(data).decode("utf-8")
    raw = json.loads(data)

    if expected_type is None or not dataclasses.is_dataclass(expected_type):
        return raw

    # Unwrap generic aliases (e.g., SignedRequest[T])
    origin = getattr(expected_type, "__origin__", None)
    if origin is not None and isinstance(origin, type):
        expected_type = origin

    return _dict_to_dataclass(raw, expected_type)


def _dict_to_dataclass(d: dict, cls: type) -> Any:
    """Recursively construct a dataclass from a dict."""
    if not isinstance(d, dict):
        return d

    try:
        hints = get_type_hints(cls)
    except Exception:
        hints = {}

    kwargs = {}
    for f in dataclasses.fields(cls):
        if f.name not in d:
            continue
        value = d[f.name]
        field_type = hints.get(f.name, f.type)

        # Unwrap Optional
        origin = getattr(field_type, "__origin__", None)
        if origin is type(None):
            kwargs[f.name] = value
            continue

        # Handle Optional[X] -- args = (X, NoneType)
        args = getattr(field_type, "__args__", None)
        if args and type(None) in args:
            if value is None:
                kwargs[f.name] = None
                continue
            # Get the non-None type
            inner = next((a for a in args if a is not type(None)), None)
            if inner and dataclasses.is_dataclass(inner) and isinstance(value, dict):
                kwargs[f.name] = _dict_to_dataclass(value, inner)
                continue

        # Handle nested dataclass
        actual_type = field_type
        if origin is list and args:
            # list[X] -- convert inner dicts
            elem_type = args[0]
            if dataclasses.is_dataclass(elem_type) and isinstance(value, list):
                kwargs[f.name] = [
                    _dict_to_dataclass(item, elem_type) if isinstance(item, dict) else item
                    for item in value
                ]
                continue

        if dataclasses.is_dataclass(actual_type) and isinstance(value, dict):
            kwargs[f.name] = _dict_to_dataclass(value, actual_type)
            continue

        kwargs[f.name] = value

    return cls(**kwargs)
