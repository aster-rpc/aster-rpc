"""
aster.codec -- Fory serialization codec.

Spec reference: §5.1--5.6 (serialization protocols), §5.3 (XLANG tags), §5.5 (ROW mode)

Wraps Apache Fory (pyfory 1.7.x, the release line pinned by Rust core in
``core/src/xlang.rs``) for RPC **payloads**:

- XLANG: cross-language, named type registration. The only interoperable
  mode; options are fixed to Aster's standard (xlang on, compatible on,
  reference tracking off), matching ``core::xlang::new_fory``.
- NATIVE / ROW: reserved by the spec, Python-only.

The RPC envelope is not Fory-encoded (see :mod:`aster.wire`). Payload
compression delegates to Rust core (Aster-SPEC §5.6).
"""

from __future__ import annotations

import dataclasses
import enum
import typing
from dataclasses import dataclass, field
from typing import Any, get_type_hints

import pyfory
import pyfory.format as pyfory_format

from aster import wire as _wire
from aster.rpc_types import SerializationMode

# ── Default compression threshold ────────────────────────────────────────────

DEFAULT_COMPRESSION_THRESHOLD: int = _wire.COMPRESSION_THRESHOLD  # 4096 bytes


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
        """Convert the config into ``pyfory.Fory`` constructor kwargs.

        Aster's payload Fory options are fixed (Aster-SPEC §5.2,
        ``core/src/xlang.rs::new_fory``): ``xlang=True``, ``compatible=True``
        (NAMED_COMPATIBLE_STRUCT, field add/remove tolerated) and
        ``ref=False`` (no shared/cyclic graphs on the wire). They feed the
        encoded bytes, so drift between producer and consumer is a wire
        break. ``extra_kwargs`` may add other pyfory options but cannot
        override these for XLANG.
        """
        kwargs = dict(self.extra_kwargs)
        xlang = self.resolved_xlang(mode)
        kwargs["xlang"] = xlang
        kwargs.setdefault("strict", True)
        if xlang:
            kwargs["ref"] = False
            kwargs["compatible"] = True
        else:
            kwargs.setdefault("ref", False)
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
        tag: Wire type tag (e.g., ``"billing/Invoice"``).
        metadata: Optional dict mapping field names to ``Metadata`` objects
            for describing individual fields to AI agents.

    Example::

        @wire_type("billing/Invoice")
        @dataclass
        class Invoice:
            customer: str = ""
            amount: float = 0.0
            paid: bool = False

    Serialization rules:

    - **Supported field types:** ``str``, ``int``, ``float``, ``bool``,
      ``bytes``, ``list[T]``, ``dict[str, V]``, ``Optional[T]``,
      and other ``@wire_type`` dataclasses.

    - **Use ``Optional[T]`` for nullable fields.**

    - **Integer widths:** a plain ``int`` is a zig-zag ``varint64`` (Rust
      ``i64``). Use ``pyfory.Int32`` for ``varint32`` (Rust ``i32``),
      ``pyfory.FixedInt32`` for a fixed 4-byte ``int32``, ``pyfory.Int8`` /
      ``pyfory.Int16`` / ``pyfory.Float32`` etc. for the other widths.

    - **Every field must have a default value.** Fory XLANG needs defaults
      for construction. Use ``""`` for strings, ``0`` for ints, ``False``
      for bools, and ``field(default_factory=list)`` for collections. Such
      defaults are *not* part of the contract (the field is published as
      required); use :func:`contract_default` to publish one.

    - **``@rpc`` methods must return a ``@wire_type`` dataclass, not None.**
      Even if your response has no fields, define an empty response type::

          @wire_type("myapp/AckResult")
          @dataclass
          class AckResult:
              ok: bool = True

    - **Generic wrapper types** (e.g., ``SignedRequest[T]``) are supported.
      The ``@wire_type`` tag is on the outer class; the type parameter is
      resolved at manifest extraction time.
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


# ── Contract defaults (Aster-ContractIdentity §11.3.2.3) ────────────────────

#: ``dataclasses.Field.metadata`` key marking a contract default.
CONTRACT_DEFAULT_KEY = "aster.contract_default"

_FORY_FIELD_KWARGS = frozenset({"id", "nullable", "ref", "ignore", "dynamic"})


def contract_default(
    default: Any = dataclasses.MISSING,
    *,
    default_factory: Any = dataclasses.MISSING,
    **field_kwargs: Any,
) -> Any:
    """Declare a field default that is **part of the contract**.

    Every ``@wire_type`` dataclass field needs a Python construction default,
    but a plain default (``name: str = ""``) does *not* enter the contract:
    the field is published as ``required``. Wrap the default in
    ``contract_default`` to publish it (``required = false`` plus the
    canonical default bytes), exactly like Rust's ``#[aster(default = …)]``::

        @wire_type("defaults.v1/Settings")
        @dataclass
        class Settings:
            name: str = ""                                   # required
            state: str = contract_default("idle")
            retries: pyfory.Int32 = contract_default(-3)
            tags: list[str] = contract_default(default_factory=list)

    Changing a contract default changes ``contract_id`` (it is a breaking
    change); Fory payload bytes are unaffected. Supported in v1: ``str``,
    ``bool``, ``pyfory.Int32`` (varint32), ``int`` (varint64), ``float``
    (float64), ``Optional`` of those with a non-``None`` value, and empty
    ``list`` / ``set`` / ``dict``. Anything else raises ``TypeError`` when the
    contract is computed.

    Extra keyword arguments go to :func:`pyfory.field` when they are pyfory
    field options (``id``, ``nullable``, ...), else to
    :func:`dataclasses.field`.
    """
    if (default is dataclasses.MISSING) == (default_factory is dataclasses.MISSING):
        raise TypeError("contract_default() takes exactly one of default / default_factory")
    metadata = dict(field_kwargs.pop("metadata", None) or {})
    metadata[CONTRACT_DEFAULT_KEY] = True
    kwargs: dict[str, Any] = {"metadata": metadata, **field_kwargs}
    if default is not dataclasses.MISSING:
        kwargs["default"] = default
    else:
        kwargs["default_factory"] = default_factory
    if _FORY_FIELD_KWARGS & field_kwargs.keys():
        return pyfory.field(**kwargs)
    return dataclasses.field(**kwargs)


def fory_registration_name(namespace: str, typename: str) -> str:
    """The Fory named-registration string for ``namespace`` + ``typename``.

    Matches Rust's ``register_by_name(format!("{package}.{name}"))``: Fory
    splits the name on its **last** ``.`` into (namespace, type name). So the
    tag ``"billing/Invoice"`` registers as ``"billing.Invoice"`` in every
    binding.
    """
    return f"{namespace}.{typename}" if namespace else typename


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


def _is_enum(tp: type) -> bool:
    """Return True if *tp* is a user-defined Enum subclass."""
    return isinstance(tp, type) and issubclass(tp, enum.Enum) and tp is not enum.Enum


def _walk_type_graph(root_types: list[type]) -> list[type]:
    """Walk the type graph starting from *root_types* and return all
    dataclass and enum types that need Fory registration (in dependency
    order, leaves first).

    Primitives and generic containers (list, dict, set, etc.) are
    skipped -- only concrete dataclass and enum types are collected.
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

        # Enum types are leaf nodes -- collect and return
        if _is_enum(tp):
            result.append(tp)
            return

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


# Per-class caches used by `_coerce_enum_fields` to avoid two
# expensive-per-call operations:
#
#   1. `typing.get_type_hints(cls)` re-parses string forward refs via
#      `compile` + `eval` on every call. The stdlib does not memoize
#      forward-ref resolution, so every call pays ~40 µs on M2 for a small
#      dataclass.
#   2. Even with hints cached, walking dataclass fields to check for enums is
#      pure waste for types that have no enum fields anywhere in their graph.
#
# On first decode of a given class we compute (and cache):
#   - The resolved type hints dict from `get_type_hints`.
#   - A bool: does this class (or any nested dataclass it reaches) have any
#     enum-typed field? If False, `_coerce_enum_fields` short-circuits to an
#     early return without walking `dataclasses.fields` at all.
#
# `_coerce_enum_fields` is called on every Fory-decoded dataclass -- twice
# per unary client call (response + trailer), once per unary server call
# (request), plus every streaming frame. Uncached, this was the largest
# per-call Python cost on the typed Fory path: fixing it recovered ~230 us
# per call p50 and ~38% sequential throughput on the MC getStatus benchmark
# (Apple M2).
#
# Most RPC payload types have zero enum fields and hit the early return
# path after paying one walk at first decode, then ~50 ns per call forever
# after (dict lookup + branch).
_enum_hints_cache: dict[type, dict[str, Any]] = {}
_needs_coerce_cache: dict[type, bool] = {}


def _class_needs_enum_coercion(
    cls: type, _seen: set[type] | None = None,
) -> bool:
    """Return True iff `cls` or any dataclass nested under it has an enum field.

    Walked once per class on first decode; result is cached in
    `_needs_coerce_cache` so subsequent calls to `_coerce_enum_fields` can
    skip the entire field walk when this returns False.
    """
    if _seen is None:
        _seen = set()
    if cls in _seen:
        return False
    _seen.add(cls)

    if not dataclasses.is_dataclass(cls):
        return False

    hints = _enum_hints_cache.get(cls)
    if hints is None:
        try:
            hints = get_type_hints(cls)
        except Exception:
            hints = {}
        _enum_hints_cache[cls] = hints

    for f in dataclasses.fields(cls):
        field_type = hints.get(f.name, f.type)
        # Unwrap Optional[X] / Union[X, None]
        resolved = field_type
        origin = getattr(field_type, "__origin__", None)
        if origin is typing.Union:
            args = [
                a for a in getattr(field_type, "__args__", ())
                if a is not type(None)
            ]
            if len(args) == 1:
                resolved = args[0]
        if isinstance(resolved, type):
            if issubclass(resolved, enum.Enum):
                return True
            if dataclasses.is_dataclass(resolved):
                if _class_needs_enum_coercion(resolved, _seen):
                    return True
    return False


def _coerce_enum_fields(obj: Any) -> None:
    """Coerce raw primitive values to enum members in-place on a dataclass.

    When a cross-language producer (e.g. TypeScript) serializes an enum as its
    primitive value (string or int), the Python deserializer yields a plain
    str/int.  This function walks the dataclass fields and converts them back
    to the expected ``enum.Enum`` member.  Operates recursively on nested
    dataclasses.

    Fast path: types with no enum fields anywhere in their graph are an early
    return after a single dict lookup. See the cache comment above for the
    measured impact.
    """
    cls = type(obj)
    needs = _needs_coerce_cache.get(cls)
    if needs is None:
        needs = _class_needs_enum_coercion(cls)
        _needs_coerce_cache[cls] = needs
    if not needs:
        return

    hints = _enum_hints_cache.get(cls)
    if hints is None:
        try:
            hints = get_type_hints(cls)
        except Exception:
            hints = {}
        _enum_hints_cache[cls] = hints

    for f in dataclasses.fields(obj):
        value = getattr(obj, f.name)
        field_type = hints.get(f.name, f.type)

        # Unwrap Optional[X]
        resolved = field_type
        origin = getattr(field_type, "__origin__", None)
        if origin is typing.Union:
            args = [a for a in getattr(field_type, "__args__", ()) if a is not type(None)]
            if len(args) == 1:
                resolved = args[0]

        if value is None:
            continue

        # If the field type is an enum and the value is a raw primitive, coerce
        if isinstance(resolved, type) and issubclass(resolved, enum.Enum):
            if not isinstance(value, enum.Enum):
                try:
                    object.__setattr__(obj, f.name, resolved(value))
                except (ValueError, KeyError):
                    pass  # leave as-is if value doesn't match any member
        elif dataclasses.is_dataclass(value) and not isinstance(value, type):
            _coerce_enum_fields(value)


def _validate_xlang_tags(types: list[type]) -> None:
    """Raise ``TypeError`` if any dataclass type lacks a ``@wire_type``.

    Only called for XLANG mode where tag-based registration is required.
    Enum types are exempt -- they are registered by module/qualname.
    """
    for tp in types:
        if _is_enum(tp):
            continue
        if not dataclasses.is_dataclass(tp):
            continue
        if not hasattr(tp, "__wire_type__"):
            raise TypeError(
                f"Type {tp.__qualname__} is used in XLANG mode but has no "
                f"@wire_type decorator. All types must be tagged for XLANG "
                f"serialization."
            )


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

        self._max_decompress = MAX_DECOMPRESSED_SIZE

        user_types = list(types) if types else []

        # Walk the type graph to discover nested dataclass types.
        all_types = _walk_type_graph(user_types)

        # For XLANG mode, validate that all types have tags.
        if mode == SerializationMode.XLANG:
            _validate_xlang_tags(all_types)

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
                self._fory.register(tp, name=fory_registration_name(ns, tn))
            elif _is_enum(tp):
                # Enums: derive namespace/typename like auto wire_type
                self._fory.register(
                    tp,
                    name=fory_registration_name(tp.__module__ or "", tp.__qualname__),
                )
            elif mode == SerializationMode.NATIVE:
                # NATIVE mode: register without tag
                self._fory.register(tp)
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
        # Unwrap generic aliases (e.g., SignedRequest[JoinPayload] -> SignedRequest)
        # so isinstance() checks don't fail with "Subscripted generics cannot be
        # used with class and instance checks".
        check_type = expected_type
        if check_type is not None:
            origin = getattr(check_type, "__origin__", None)
            if origin is not None and isinstance(origin, type):
                check_type = origin

        if self.mode == SerializationMode.ROW and self._row_encoder is not None:
            row = pyfory_format.RowData(self._row_schema, data)
            result = self._row_encoder.from_row(row)
            if (
                check_type is not None
                and dataclasses.is_dataclass(check_type)
                and not isinstance(result, check_type)
            ):
                result = self._row_to_dataclass(row, check_type)
        else:
            result = self._fory.deserialize(data)

        # Coerce raw primitives (str/int) into enum members when the target
        # type expects enums.  This handles cross-language interop where the
        # producer (e.g. TypeScript) serializes enums as their primitive value
        # because Fory JS lacks a native NAMED_ENUM wire type.
        # TODO: add cross-language enum round-trip tests (Python ↔ TS)
        if dataclasses.is_dataclass(result) and not isinstance(result, type):
            _coerce_enum_fields(result)

        if check_type is not None and not isinstance(result, check_type):
            raise TypeError(
                f"Expected {(expected_type or check_type).__qualname__}, "
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
        """Serialize and optionally compress (Aster-SPEC §5.6).

        Returns:
            A ``(data, compressed)`` tuple where *compressed* is ``True``
            if zstd compression was applied (payload strictly larger than
            the threshold and the compressed form smaller).
        """
        return _wire.compress_payload(self.encode(obj), self.compression_threshold)

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
        """Compress raw bytes with zstd (level 3, unconditionally)."""
        out, compressed = _wire.compress_payload(data, 0)
        if not compressed:
            raise ValueError("payload does not compress")
        return out

    def decompress(self, data: bytes) -> bytes:
        """Decompress zstd-compressed bytes. Enforces MAX_DECOMPRESSED_SIZE."""
        return self._safe_decompress(data)

    def _safe_decompress(self, data: bytes) -> bytes:
        """Streaming decompression with the size limit enforced in Rust core
        (the zstd frame's content-size header is never trusted)."""
        return _wire.decompress_payload(data, self._max_decompress)

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