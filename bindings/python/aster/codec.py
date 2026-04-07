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
from dataclasses import dataclass, field
from typing import Any, get_type_hints

import pyfory
import pyfory.format as pyfory_format
import zstandard

from aster.types import SerializationMode

# ── Default compression threshold ────────────────────────────────────────────

DEFAULT_COMPRESSION_THRESHOLD: int = 4096  # bytes


@dataclass(slots=True)
class ForyConfig:
    """Configuration for constructing the underlying ``pyfory.Fory`` instance.

    Args:
        xlang: Whether to enable Fory's cross-language mode. If ``None``,
            :class:`ForyCodec` chooses a mode-appropriate default:
            ``True`` for :class:`SerializationMode.XLANG`, otherwise ``False``.
        extra_kwargs: Additional keyword arguments forwarded to
            ``pyfory.Fory(...)``.
    """

    xlang: bool | None = None
    extra_kwargs: dict[str, Any] = field(default_factory=dict)

    def resolved_xlang(self, mode: SerializationMode) -> bool:
        """Return the effective xlang setting for a codec mode."""
        if self.xlang is not None:
            return self.xlang
        return mode == SerializationMode.XLANG

    def to_kwargs(self, mode: SerializationMode) -> dict[str, Any]:
        """Convert the config into ``pyfory.Fory`` constructor kwargs."""
        kwargs = dict(self.extra_kwargs)
        kwargs.setdefault("xlang", self.resolved_xlang(mode))
        return kwargs


# ── @wire_type decorator ─────────────────────────────────────────────────────


def wire_type(tag: str, *, metadata: dict | None = None):
    """Declare a stable wire identity for a dataclass.

    The *tag* string is split on the last ``/`` into
    ``(namespace, typename)``.  If there is no ``/``
    the namespace is the empty string.

    Use ``@wire_type`` when you need explicit control over the wire
    identity of a type (production services, cross-language compat).
    If omitted, the ``@service`` decorator auto-derives a tag from the
    module + class name at decoration time.

    Args:
        tag: Wire type tag (e.g., "billing/Invoice").
        metadata: Optional dict mapping field names to Metadata objects
            for describing individual fields to AI agents.

    Example::

        @wire_type("billing/Invoice", metadata={
            "amount": Metadata(description="Total in cents (USD)"),
        })
        @dataclass
        class Invoice:
            amount: float = 0.0
    """

    def decorator(cls):
        parts = tag.rsplit("/", 1)
        if len(parts) == 2:
            cls.__fory_namespace__ = parts[0]
            cls.__fory_typename__ = parts[1]
        else:
            cls.__fory_namespace__ = ""
            cls.__fory_typename__ = tag
        cls.__wire_type__ = tag
        if metadata:
            cls.__wire_type_field_metadata__ = metadata
        return cls

    return decorator


def _auto_apply_wire_type(cls: type) -> None:
    """Apply a module-based default wire identity to a dataclass.

    Default tag: ``{cls.__module__}.{cls.__qualname__}``

    Module-based (not service-name-based) because:
    - Globally unique without depending on which service uses the type
    - Stable across service renames
    - Same identity Python itself uses for pickling
    """
    module = cls.__module__ or ""
    qualname = cls.__qualname__
    tag = f"{module}.{qualname}" if module else qualname
    cls.__wire_type__ = tag
    cls.__fory_namespace__ = module
    cls.__fory_typename__ = qualname


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
    """Raise ``TypeError`` if any dataclass type lacks a ``@wire_type``.

    Only called for XLANG mode where tag-based registration is required.
    """
    for tp in types:
        if not dataclasses.is_dataclass(tp):
            continue
        if not hasattr(tp, "__wire_type__"):
            raise TypeError(
                f"Type {tp.__qualname__} is used in XLANG mode but has no "
                f"@wire_type decorator. All types must be tagged for XLANG "
                f"serialization."
            )


# ── Framework-internal types ─────────────────────────────────────────────────

# These are registered automatically by ForyCodec.
# Import here to avoid circular imports — the types themselves are defined
# in protocol.py which uses the wire_type from this module (or its own copy).
_INTERNAL_TYPES: list[type] | None = None


def _get_internal_types() -> list[type]:
    """Lazily import and return framework-internal protocol types."""
    global _INTERNAL_TYPES
    if _INTERNAL_TYPES is None:
        from aster.protocol import StreamHeader, CallHeader, RpcStatus
        _INTERNAL_TYPES = [StreamHeader, CallHeader, RpcStatus]
    return _INTERNAL_TYPES


# ── ForyCodec ────────────────────────────────────────────────────────────────


class ForyCodec:
    """Serialization codec wrapping Apache Fory (pyfory).

    Supports XLANG, NATIVE, and ROW serialization modes.

    Args:
        mode: The serialization mode to use.
        types: User-defined types that will be serialized/deserialized.
            For XLANG mode, all types must have ``@wire_type`` decorators.
            For NATIVE mode, tags are optional.
        compression_threshold: Payloads larger than this (in bytes) are
            zstd-compressed.  Set to ``-1`` to disable compression.
        fory_config: Optional configuration forwarded to ``pyfory.Fory``.
            By default, XLANG codecs enable ``xlang=True`` and other modes
            leave it disabled.
    """

    def __init__(
        self,
        mode: SerializationMode = SerializationMode.XLANG,
        types: list[type] | None = None,
        compression_threshold: int = DEFAULT_COMPRESSION_THRESHOLD,
        fory_config: ForyConfig | None = None,
    ) -> None:
        self.mode = mode
        self.compression_threshold = compression_threshold
        self.fory_config = fory_config or ForyConfig()
        from aster.limits import MAX_DECOMPRESSED_SIZE

        self._cctx = zstandard.ZstdCompressor()
        self._dctx = zstandard.ZstdDecompressor()
        self._max_decompress = MAX_DECOMPRESSED_SIZE

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

        # §5.3.1: Detect duplicate wire_type tags before registration.
        self._tag_to_type: dict[str, type] = {}
        for tp in all_types:
            tag = getattr(tp, "__wire_type__", None)
            if tag is not None:
                existing = self._tag_to_type.get(tag)
                if existing is not None and existing is not tp:
                    raise ValueError(
                        f"Duplicate wire_type tag {tag!r}: already registered "
                        f"to {existing.__qualname__}, cannot register "
                        f"{tp.__qualname__} with the same tag"
                    )
                self._tag_to_type[tag] = tp

        # Create the Fory instance.
        self._fory = self._create_fory_instance()

        # Register all discovered types.
        for tp in all_types:
            tag = getattr(tp, "__wire_type__", None)
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
        self._row_schema = None
        self._row_type = None
        if mode == SerializationMode.ROW:
            self._setup_row(user_types)

    def _create_fory_instance(self) -> Any:
        """Create and return the configured ``pyfory.Fory`` instance."""
        kwargs = self.fory_config.to_kwargs(self.mode)
        try:
            return pyfory.Fory(**kwargs)
        except TypeError as exc:
            raise TypeError(
                f"Failed to construct pyfory.Fory with kwargs {kwargs!r}"
            ) from exc

    def _setup_row(self, types: list[type]) -> None:
        """Set up ROW-mode encoder/decoder if pyfory supports it."""
        if not types:
            raise ValueError("ROW mode requires at least one root type")

        if len(types) != 1:
            raise ValueError(
                "ROW mode currently supports exactly one root type per codec"
            )

        row_type = types[0]
        if not dataclasses.is_dataclass(row_type):
            raise TypeError("ROW mode requires a dataclass root type")

        self._row_type = row_type
        self._row_schema = pyfory_format.infer_schema(row_type)
        self._row_encoder = pyfory_format.create_row_encoder(self._row_schema)

    def encode(self, obj: Any) -> bytes:
        """Serialize an object according to the configured mode.

        Returns the raw serialized bytes (without compression).
        Use :meth:`encode_compressed` if you want automatic compression.
        """
        if self.mode == SerializationMode.ROW and self._row_encoder is not None:
            row = self._row_encoder.to_row(obj)
            return bytes(row.to_bytes())

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
        if self.mode == SerializationMode.ROW and self._row_encoder is not None:
            row = pyfory_format.RowData(self._row_schema, data)
            result = self._row_encoder.from_row(row)
            if (
                expected_type is not None
                and dataclasses.is_dataclass(expected_type)
                and not isinstance(result, expected_type)
            ):
                result = self._row_to_dataclass(row, expected_type)
        else:
            result = self._fory.deserialize(data)

        if expected_type is not None and not isinstance(result, expected_type):
            raise TypeError(
                f"Expected {expected_type.__qualname__}, "
                f"got {type(result).__qualname__}"
            )
        return result

    def _row_to_dataclass(self, row: pyfory_format.RowData, cls: type) -> Any:
        """Rehydrate a dataclass instance from a pyfory RowData view."""
        kwargs: dict[str, Any] = {}
        hints = get_type_hints(cls)

        for field in dataclasses.fields(cls):
            field_type = hints.get(field.name, field.type)
            index = row.schema.get_field_index(field.name)
            kwargs[field.name] = self._row_field_value(row, index, field_type)

        return cls(**kwargs)

    def _row_field_value(
        self,
        row: pyfory_format.RowData,
        index: int,
        field_type: Any,
    ) -> Any:
        """Read a field value from RowData using a best-effort type mapping."""
        origin = getattr(field_type, "__origin__", None)
        if origin is not None:
            args = [a for a in getattr(field_type, "__args__", ()) if a is not type(None)]
            if len(args) == 1:
                field_type = args[0]

        if field_type is bool:
            return row.get_boolean(index)
        if field_type is str:
            return row.get_str(index)
        if field_type is int:
            return row.get_int64(index)
        if field_type is float:
            return row.get_double(index)
        if field_type is bytes:
            return row.get_binary(index)

        # Fallback to generic getter for types we don't explicitly map yet.
        return row.get(index)

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
            data = self._safe_decompress(data)
        return self.decode(data, expected_type)

    def compress(self, data: bytes) -> bytes:
        """Compress raw bytes with zstd."""
        return self._cctx.compress(data)

    def decompress(self, data: bytes) -> bytes:
        """Decompress zstd-compressed bytes. Enforces MAX_DECOMPRESSED_SIZE."""
        return self._safe_decompress(data)

    def _safe_decompress(self, data: bytes) -> bytes:
        """Decompress with size limit enforcement.

        Uses streaming decompression to enforce the limit without trusting
        the content-size header in the compressed data.
        """
        from aster.limits import LimitExceeded

        reader = self._dctx.stream_reader(data)
        chunks: list[bytes] = []
        total = 0
        while True:
            chunk = reader.read(65536)
            if not chunk:
                break
            total += len(chunk)
            if total > self._max_decompress:
                raise LimitExceeded(
                    "decompressed payload", self._max_decompress, total
                )
            chunks.append(chunk)
        return b"".join(chunks)

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

        if self._row_schema is None:
            raise NotImplementedError("ROW schema is not initialized")

        return bytes(self._row_schema.to_bytes())

    def decode_row_data(self, data: bytes) -> pyfory_format.RowData:
        """Decode raw ROW bytes into a RowData view for random-access reads."""
        if self.mode != SerializationMode.ROW:
            raise ValueError("decode_row_data() is only valid in ROW mode")
        if self._row_schema is None:
            raise NotImplementedError("ROW schema is not initialized")
        return pyfory_format.RowData(self._row_schema, data)

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