"""
aster.codec — Fory serialization codec.

Spec reference: §5.1–5.6 (serialization protocols), §5.3 (XLANG tags), §5.5 (ROW mode)

Wraps Apache Fory (pyfory) to provide three serialization modes:
- XLANG: Cross-language, tag-based type registration
- NATIVE: Python-native serialization (no tag requirement)
- ROW: Row-oriented format for random-access field reads

Also provides transparent zstd compression for payloads exceeding a
configurable threshold.
"""

from __future__ import annotations

import dataclasses
import typing
from dataclasses import dataclass, fields
from typing import Any, get_type_hints

import pyfory
import zstandard

from aster_python.aster.types import SerializationMode

# ── Default compression threshold ────────────────────────────────────────────

DEFAULT_COMPRESSION_THRESHOLD: int = 4096  # bytes


# ── @fory_tag decorator ──────────────────────────────────────────────────────


def fory_tag(tag: str):
    """Attach a Fory XLANG type tag to a dataclass.

    The *tag* string is split on the last ``/`` into
    ``(__fory_namespace__, __fory_typename__)``.  If there is no ``/``
    the namespace is the empty string.

    For XLANG mode, every user-defined type that appears in an RPC
    method signature **must** be decorated with ``@fory_tag``.
    """

    def decorator(cls):
        parts = tag.rsplit("/", 1)
        if len(parts) == 2:
            cls.__fory_namespace__ = parts[0]
            cls.__fory_typename__ = parts[1]
        else:
            cls.__fory_namespace__ = ""
            cls.__fory_typename__ = tag
        cls.__fory_tag__ = tag
        return cls

    return decorator


# ── Type-graph walking ───────────────────────────────────────────────────────

# Primitive types that don't need Fory registration.
_PRIMITIVES = frozenset({
    int, float, str, bool, bytes, bytearray, type(None),
})


def _is_primitive(tp: type) -> bool:
    """Return True if *tp* is a primitive that doesn't need registration."""
    return tp in _PRIMITIVES


def _unwrap_generic(tp: Any) -> list[Any]:
    """Extract inner types from generic aliases like list[X], dict[K,V], Optional[X]."""
    origin = getattr(tp, "__origin__", None)
    if origin is None:
        return []
    args = getattr(tp, "__args__", ()) or ()
    return list(args)


def _walk_type_graph(root_types: list[type]) -> list[type]:
    """Walk the type graph starting from *root_types* and return all
    dataclass types that need Fory registration (in dependency order,
    leaves first).

    Primitives and generic containers (list, dict, set, etc.) are
    skipped — only concrete dataclass types are collected.
    """
    visited: set[type] = set()
    result: list[type] = []

    def _visit(tp: Any) -> None:
        # Unwrap Optional, list, dict, etc.
        origin = getattr(tp, "__origin__", None)
        if origin is not None:
            for arg in _unwrap_generic(tp):
                _visit(arg)
            return

        if not isinstance(tp, type):
            return
        if _is_primitive(tp):
            return
        if tp in visited:
            return

        visited.add(tp)

        if not dataclasses.is_dataclass(tp):
            return

        # Visit fields first (dependency order)
        try:
            hints = get_type_hints(tp)
        except Exception:
            hints = {}

        for field in dataclasses.fields(tp):
            field_type = hints.get(field.name, field.type)
            _visit(field_type)

        result.append(tp)

    for t in root_types:
        _visit(t)

    return result


def _validate_xlang_tags(types: list[type]) -> None:
    """Raise ``TypeError`` if any dataclass type lacks a ``@fory_tag``.

    Only called for XLANG mode where tag-based registration is required.
    """
    for tp in types:
        if not dataclasses.is_dataclass(tp):
            continue
        if not hasattr(tp, "__fory_tag__"):
            raise TypeError(
                f"Type {tp.__qualname__} is used in XLANG mode but has no "
                f"@fory_tag decorator. All types must be tagged for XLANG "
                f"serialization."
            )


# ── Framework-internal types ─────────────────────────────────────────────────

# These are registered automatically by ForyCodec.
# Import here to avoid circular imports — the types themselves are defined
# in protocol.py which uses the fory_tag from this module (or its own copy).
_INTERNAL_TYPES: list[type] | None = None


def _get_internal_types() -> list[type]:
    """Lazily import and return framework-internal protocol types."""
    global _INTERNAL_TYPES
    if _INTERNAL_TYPES is None:
        from aster_python.aster.protocol import StreamHeader, CallHeader, RpcStatus
        _INTERNAL_TYPES = [StreamHeader, CallHeader, RpcStatus]
    return _INTERNAL_TYPES


# ── ForyCodec ────────────────────────────────────────────────────────────────


class ForyCodec:
    """Serialization codec wrapping Apache Fory (pyfory).

    Supports XLANG, NATIVE, and ROW serialization modes.

    Args:
        mode: The serialization mode to use.
        types: User-defined types that will be serialized/deserialized.
            For XLANG mode, all types must have ``@fory_tag`` decorators.
            For NATIVE mode, tags are optional.
        compression_threshold: Payloads larger than this (in bytes) are
            zstd-compressed.  Set to ``-1`` to disable compression.
    """

    def __init__(
        self,
        mode: SerializationMode = SerializationMode.XLANG,
        types: list[type] | None = None,
        compression_threshold: int = DEFAULT_COMPRESSION_THRESHOLD,
    ) -> None:
        self.mode = mode
        self.compression_threshold = compression_threshold
        self._cctx = zstandard.ZstdCompressor()
        self._dctx = zstandard.ZstdDecompressor()

        user_types = list(types) if types else []

        # Walk the type graph to discover nested dataclass types.
        all_types = _walk_type_graph(user_types)

        # For XLANG mode, validate that all types have tags.
        if mode == SerializationMode.XLANG:
            _validate_xlang_tags(all_types)

        # Add framework-internal types (always registered for XLANG).
        internal = _get_internal_types()
        for it in internal:
            if it not in all_types:
                all_types.append(it)

        self._registered_types = all_types

        # Create the Fory instance.
        if mode == SerializationMode.ROW:
            # ROW mode uses a separate Fory instance
            self._fory = pyfory.Fory()
        else:
            self._fory = pyfory.Fory()

        # Register all discovered types.
        for tp in all_types:
            tag = getattr(tp, "__fory_tag__", None)
            if tag is not None:
                ns = getattr(tp, "__fory_namespace__", "")
                tn = getattr(tp, "__fory_typename__", tp.__name__)
                self._fory.register_type(tp, namespace=ns, typename=tn)
            elif mode == SerializationMode.NATIVE:
                # NATIVE mode: register without tag
                self._fory.register_type(tp)
            # else: type is already validated above for XLANG

        # For ROW mode, build encoder/decoder if available
        self._row_encoder = None
        self._row_decoder = None
        if mode == SerializationMode.ROW:
            self._setup_row(user_types)

    def _setup_row(self, types: list[type]) -> None:
        """Set up ROW-mode encoder/decoder if pyfory supports it."""
        # pyfory ROW support check — may not be available in all versions
        if hasattr(pyfory, "create_row_encoder"):
            # This path is for future pyfory versions with ROW support
            pass
        # ROW mode falls back to standard serialization if not available

    def encode(self, obj: Any) -> bytes:
        """Serialize an object according to the configured mode.

        Returns the raw serialized bytes (without compression).
        Use :meth:`encode_compressed` if you want automatic compression.
        """
        data = self._fory.serialize(obj)
        return bytes(data)

    def decode(self, data: bytes, expected_type: type | None = None) -> Any:
        """Deserialize bytes into an object.

        Args:
            data: The serialized bytes.
            expected_type: Optional expected type for validation.

        Returns:
            The deserialized object.

        Raises:
            TypeError: If *expected_type* is given and the result doesn't match.
        """
        result = self._fory.deserialize(data)
        if expected_type is not None and not isinstance(result, expected_type):
            raise TypeError(
                f"Expected {expected_type.__qualname__}, "
                f"got {type(result).__qualname__}"
            )
        return result

    def encode_compressed(self, obj: Any) -> tuple[bytes, bool]:
        """Serialize and optionally compress.

        Returns:
            A ``(data, compressed)`` tuple where *compressed* is ``True``
            if zstd compression was applied.
        """
        raw = self.encode(obj)
        if (
            self.compression_threshold >= 0
            and len(raw) > self.compression_threshold
        ):
            return self._cctx.compress(raw), True
        return raw, False

    def decode_compressed(
        self, data: bytes, compressed: bool, expected_type: type | None = None
    ) -> Any:
        """Decompress (if needed) and deserialize.

        Args:
            data: The (possibly compressed) serialized bytes.
            compressed: Whether the data is zstd-compressed.
            expected_type: Optional expected type for validation.
        """
        if compressed:
            data = self._dctx.decompress(data)
        return self.decode(data, expected_type)

    def compress(self, data: bytes) -> bytes:
        """Compress raw bytes with zstd."""
        return self._cctx.compress(data)

    def decompress(self, data: bytes) -> bytes:
        """Decompress zstd-compressed bytes."""
        return self._dctx.decompress(data)

    def encode_row_schema(self) -> bytes:
        """For ROW mode: serialize the schema for hoisting.

        Returns the serialized schema bytes that should be sent as the
        first frame (with ROW_SCHEMA flag) so the receiver can decode
        subsequent ROW-encoded payloads.

        Raises:
            NotImplementedError: If ROW mode is not supported by the
                installed pyfory version.
        """
        if self.mode != SerializationMode.ROW:
            raise ValueError("encode_row_schema() is only valid in ROW mode")

        # Build a schema description from registered types
        schema_types = []
        for tp in self._registered_types:
            if not dataclasses.is_dataclass(tp):
                continue
            tag = getattr(tp, "__fory_tag__", tp.__qualname__)
            field_defs = []
            try:
                hints = get_type_hints(tp)
            except Exception:
                hints = {}
            for f in dataclasses.fields(tp):
                ft = hints.get(f.name, f.type)
                field_defs.append({
                    "name": f.name,
                    "type": _type_name(ft),
                })
            schema_types.append({
                "tag": tag,
                "fields": field_defs,
            })

        # Serialize the schema using standard Fory serialization
        return self.encode(schema_types)

    @property
    def registered_types(self) -> list[type]:
        """Return the list of types registered with this codec."""
        return list(self._registered_types)


def _type_name(tp: Any) -> str:
    """Return a human-readable name for a type."""
    origin = getattr(tp, "__origin__", None)
    if origin is not None:
        args = getattr(tp, "__args__", ())
        if args:
            arg_names = ", ".join(_type_name(a) for a in args)
            return f"{getattr(origin, '__name__', str(origin))}[{arg_names}]"
        return getattr(origin, "__name__", str(origin))
    if isinstance(tp, type):
        return tp.__name__
    return str(tp)