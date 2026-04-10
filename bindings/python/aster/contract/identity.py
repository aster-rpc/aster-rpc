"""
aster.contract.identity -- Contract identity types, canonical serialization, and hashing.

Spec reference: Aster-ContractIdentity.md §11.3

Provides:
- Enum types for the canonical schema (TypeKind, ContainerKind, etc.)
- Dataclass types (FieldDef, TypeDef, MethodDef, ServiceContract, etc.)
- Canonical byte serialization for each type
- Type graph resolution with SCC cycle-breaking (Tarjan's algorithm)
- BLAKE3 hashing utilities
"""

from __future__ import annotations

import dataclasses
import unicodedata
import warnings
from dataclasses import dataclass, field
from enum import IntEnum
from typing import Any, get_type_hints


# ── Enum types (§11.3.3, fixed normative values) ─────────────────────────────


class TypeKind(IntEnum):
    PRIMITIVE = 0  # int32/string/etc, carried in type_primitive
    REF = 1        # reference to TypeDef by hash (type_ref field)
    SELF_REF = 2   # back-edge in a cycle (self_ref_name field)
    ANY = 3        # Fory `Any`


class ContainerKind(IntEnum):
    NONE = 0
    LIST = 1
    SET = 2
    MAP = 3


class TypeDefKind(IntEnum):
    MESSAGE = 0  # struct-like
    ENUM = 1
    UNION = 2


class MethodPattern(IntEnum):
    UNARY = 0
    SERVER_STREAM = 1
    CLIENT_STREAM = 2
    BIDI_STREAM = 3


class CapabilityKind(IntEnum):
    ROLE = 0
    ANY_OF = 1
    ALL_OF = 2


class ScopeKind(IntEnum):
    SHARED = 0
    SESSION = 1
    # Legacy alias kept for back-compat with code that imported ScopeKind.STREAM.
    # Resolves to the same integer (1), so contract ids are unaffected.
    STREAM = 1


# ── Identifier normalization (§11.3.3) ───────────────────────────────────────

# Unicode script ranges for mixed-script detection (simplified)
_LATIN_RANGE = range(0x0041, 0x024F + 1)      # Basic Latin + Latin Extended
_CYRILLIC_RANGE = range(0x0400, 0x04FF + 1)   # Cyrillic
_GREEK_RANGE = range(0x0370, 0x03FF + 1)       # Greek


def _char_script(c: str) -> str | None:
    """Return a simplified script name for a character, or None for neutral."""
    cp = ord(c)
    if cp in _LATIN_RANGE:
        return "Latin"
    if cp in _CYRILLIC_RANGE:
        return "Cyrillic"
    if cp in _GREEK_RANGE:
        return "Greek"
    return None


def normalize_identifier(s: str) -> str:
    """Normalize an identifier to NFC form, validating it first.

    Args:
        s: The identifier string to normalize.

    Returns:
        NFC-normalized identifier.

    Raises:
        ValueError: If *s* is not a valid Python identifier.
    """
    if not s.isidentifier():
        raise ValueError(f"Not a valid identifier: {s!r}")

    normalized = unicodedata.normalize("NFC", s)

    # Mixed-script detection (non-fatal warning)
    scripts: set[str] = set()
    for c in normalized:
        script = _char_script(c)
        if script is not None:
            scripts.add(script)
    if len(scripts) >= 2:
        warnings.warn(
            f"Identifier {s!r} uses characters from multiple Unicode scripts: "
            f"{sorted(scripts)}. This may indicate a homoglyph attack.",
            stacklevel=2,
        )

    return normalized


# ── Field dataclasses (§11.3.3) ──────────────────────────────────────────────


@dataclass
class FieldDef:
    """Describes a single field in a MESSAGE TypeDef.

    Field IDs are normative and match the canonical serialization order.
    """

    id: int                         # field 1: ZigZag varint
    name: str                       # field 2: UTF-8 string
    type_kind: TypeKind             # field 3: unsigned varint
    type_primitive: str             # field 4: UTF-8 string (empty when unused)
    type_ref: bytes                 # field 5: bytes (empty b"" when unused)
    self_ref_name: str              # field 6: UTF-8 string (empty when unused)
    optional: bool                  # field 7: bool
    ref_tracked: bool               # field 8: bool
    container: ContainerKind        # field 9: unsigned varint
    container_key_kind: TypeKind    # field 10: unsigned varint
    container_key_primitive: str    # field 11: UTF-8 string
    container_key_ref: bytes        # field 12: bytes


@dataclass
class EnumValueDef:
    """Describes a single value in an ENUM TypeDef."""

    name: str    # field 1: UTF-8 string
    value: int   # field 2: ZigZag varint


@dataclass
class UnionVariantDef:
    """Describes a single variant in a UNION TypeDef."""

    name: str       # field 1: UTF-8 string
    id: int         # field 2: ZigZag varint
    type_ref: bytes  # field 3: bytes


@dataclass
class TypeDef:
    """Describes a user-defined type (message, enum, or union)."""

    kind: TypeDefKind                        # field 1: unsigned varint
    package: str                             # field 2: UTF-8 string
    name: str                                # field 3: UTF-8 string
    fields: list[FieldDef] = field(default_factory=list)           # field 4: sorted by id
    enum_values: list[EnumValueDef] = field(default_factory=list)  # field 5: sorted by value
    union_variants: list[UnionVariantDef] = field(default_factory=list)  # field 6: sorted by id


@dataclass
class CapabilityRequirement:
    """Describes a capability requirement (role check)."""

    kind: CapabilityKind     # field 1: unsigned varint
    roles: list[str]         # field 2: NFC-normalized + sorted by Unicode codepoint


@dataclass
class MethodDef:
    """Describes a single RPC method in a ServiceContract."""

    name: str                                   # field 1: UTF-8 string
    pattern: MethodPattern                      # field 2: unsigned varint
    request_type: bytes                         # field 3: 32-byte BLAKE3 hash
    response_type: bytes                        # field 4: 32-byte BLAKE3 hash
    idempotent: bool                            # field 5: bool
    default_timeout: float                      # field 6: float64 (8 bytes LE)
    requires: CapabilityRequirement | None = None  # field 7: optional


@dataclass
class ServiceContract:
    """The top-level contract descriptor for a service.

    This is the canonical type that gets hashed to produce the contract_id.
    """

    name: str                                    # field 1: UTF-8 string
    version: int                                 # field 2: ZigZag varint
    methods: list[MethodDef] = field(default_factory=list)             # field 3: sorted by NFC name
    serialization_modes: list[str] = field(default_factory=list)       # field 4
    scoped: ScopeKind = ScopeKind.SHARED         # field 5: unsigned varint
    requires: CapabilityRequirement | None = None  # field 6: optional

    @classmethod
    def from_service_info(
        cls,
        service_info: Any,
        type_hashes: dict[str, bytes] | None = None,
    ) -> "ServiceContract":
        """Build a ServiceContract from a ServiceInfo (from Phase 4 @service decorator).

        Args:
            service_info: A ServiceInfo object from aster.service.
            type_hashes: Optional mapping of fully-qualified type name to 32-byte hash.

        Returns:
            A ServiceContract ready for canonical serialization.
        """
        from aster.service import ServiceInfo

        if not isinstance(service_info, ServiceInfo):
            raise TypeError(f"Expected ServiceInfo, got {type(service_info).__name__}")

        if type_hashes is None:
            type_hashes = {}

        # Map scoped string to ScopeKind. Accept both the canonical "session"
        # value and the legacy "stream" alias on input so older callers keep
        # working.
        scoped_str = getattr(service_info, "scoped", "shared")
        scoped = (
            ScopeKind.SESSION
            if scoped_str in ("session", "stream")
            else ScopeKind.SHARED
        )

        # Map serialization modes
        ser_modes: list[str] = []
        for mode in service_info.serialization_modes:
            if hasattr(mode, "name"):
                # Enum with name attribute: use lowercase name (XLANG → "xlang")
                ser_modes.append(mode.name.lower())
            elif hasattr(mode, "value") and isinstance(mode.value, str):
                ser_modes.append(mode.value)
            elif isinstance(mode, str):
                ser_modes.append(mode)
            else:
                ser_modes.append(str(mode))
        if not ser_modes:
            ser_modes = ["xlang"]

        # Build MethodDef list
        method_defs: list[MethodDef] = []
        for method_name, method_info in service_info.methods.items():
            # Map pattern string to MethodPattern
            pattern_str = method_info.pattern
            if pattern_str == "server_stream":
                pattern = MethodPattern.SERVER_STREAM
            elif pattern_str == "client_stream":
                pattern = MethodPattern.CLIENT_STREAM
            elif pattern_str == "bidi_stream":
                pattern = MethodPattern.BIDI_STREAM
            else:
                pattern = MethodPattern.UNARY

            # Resolve request/response type hashes
            req_hash = _resolve_type_hash(method_info.request_type, type_hashes)
            resp_hash = _resolve_type_hash(method_info.response_type, type_hashes)

            timeout = method_info.timeout if method_info.timeout is not None else 0.0

            method_defs.append(MethodDef(
                name=method_name,
                pattern=pattern,
                request_type=req_hash,
                response_type=resp_hash,
                idempotent=method_info.idempotent,
                default_timeout=timeout,
                requires=None,
            ))

        # Sort methods by NFC-normalized name
        method_defs.sort(key=lambda m: unicodedata.normalize("NFC", m.name))

        return cls(
            name=service_info.name,
            version=service_info.version,
            methods=method_defs,
            serialization_modes=ser_modes,
            scoped=scoped,
            requires=None,
        )


def _resolve_type_hash(tp: type | None, type_hashes: dict[str, bytes]) -> bytes:
    """Resolve a Python type to its 32-byte canonical hash.

    Returns 32 zero bytes if the type is unknown (primitive types get
    a placeholder hash).

    Args:
        tp: The Python type to resolve. May be a string (forward reference).
        type_hashes: Mapping of fully-qualified name to 32-byte hash.

    Returns:
        32-byte hash.
    """
    if tp is None:
        return b"\x00" * 32

    # Handle string forward references (unresolved type annotations)
    if isinstance(tp, str):
        # Check type_hashes by short name
        for fqn, h in type_hashes.items():
            if fqn.endswith(f".{tp}") or fqn == tp:
                return h
        return b"\x00" * 32

    # Primitives get a well-known placeholder
    _PRIMITIVE_TYPES = {int, float, str, bool, bytes}
    if tp in _PRIMITIVE_TYPES:
        # Hash of the primitive name string as a stand-in
        name = {int: "int64", float: "float64", str: "string",
                bool: "bool", bytes: "binary"}[tp]
        return compute_type_hash(name.encode("utf-8"))

    # Look up by fully-qualified name
    fqn = f"{tp.__module__}.{tp.__qualname__}"
    if fqn in type_hashes:
        return type_hashes[fqn]

    # Try just the qualified name
    if tp.__qualname__ in type_hashes:
        return type_hashes[tp.__qualname__]

    # Unknown type: return zeros
    return b"\x00" * 32


# ── Canonical serialization (delegated to Rust core) ─────────────────────────


def canonical_xlang_bytes(obj: Any) -> bytes:
    """Serialize an object to canonical XLANG bytes.

    Delegates to the Rust core implementation via ``_aster.contract``.
    Supported types: ServiceContract, TypeDef, MethodDef.

    Args:
        obj: The object to serialize.

    Returns:
        Canonical byte representation.

    Raises:
        TypeError: If the object type is not supported.
    """
    if isinstance(obj, ServiceContract):
        return _canonical_bytes_via_rust("ServiceContract", obj)
    if isinstance(obj, TypeDef):
        return _canonical_bytes_via_rust("TypeDef", obj)
    if isinstance(obj, MethodDef):
        return _canonical_bytes_via_rust("MethodDef", obj)
    raise TypeError(
        f"No canonical writer for type {type(obj).__name__}. "
        f"Supported: ServiceContract, TypeDef, MethodDef"
    )


def _canonical_bytes_via_rust(type_name: str, obj: Any) -> bytes:
    """Serialize via Rust core: convert to JSON, pass to _aster.contract."""
    import aster._aster as _native
    json_str = _to_json(obj)
    return bytes(_native.contract.canonical_bytes_from_json(type_name, json_str))


_ENUM_TO_SERDE: dict[int, str] = {
    # MethodPattern
    0: "unary",
    1: "server_stream",
    2: "client_stream",
    3: "bidi_stream",
}

_SCOPE_TO_SERDE: dict[int, str] = {
    0: "shared",
    1: "session",
}

_CAP_TO_SERDE: dict[int, str] = {
    0: "role",
    1: "any_of",
    2: "all_of",
}


def _to_json(obj: Any) -> str:
    """Convert a contract dataclass to JSON for the Rust core (serde-compatible)."""
    import json

    _TYPEDEF_KIND_TO_SERDE = {0: "message", 1: "enum", 2: "union"}
    _TYPEKIND_TO_SERDE = {0: "primitive", 1: "ref", 2: "self_ref", 3: "any"}
    _CONTAINER_TO_SERDE = {0: "none", 1: "list", 2: "set", 3: "map"}

    def _convert(o: Any, field_name: str = "", parent_type: str = "") -> Any:
        if isinstance(o, bytes):
            return o.hex()
        if isinstance(o, CapabilityRequirement):
            return {
                "kind": _CAP_TO_SERDE.get(int(o.kind), str(o.kind)),
                "roles": [_convert(r) for r in o.roles],
            }
        if dataclasses.is_dataclass(o) and not isinstance(o, type):
            d = {}
            type_name = type(o).__name__
            for f in dataclasses.fields(o):
                val = getattr(o, f.name)
                d[f.name] = _convert(val, f.name, type_name)
            return d
        if isinstance(o, list):
            return [_convert(item) for item in o]
        if isinstance(o, (str, bool)):
            return o
        if isinstance(o, (int, float)):
            if field_name == "pattern":
                return _ENUM_TO_SERDE.get(int(o), int(o))
            if field_name == "scoped":
                return _SCOPE_TO_SERDE.get(int(o), int(o))
            if field_name == "kind" and parent_type == "TypeDef":
                return _TYPEDEF_KIND_TO_SERDE.get(int(o), int(o))
            if field_name == "kind":
                return _CAP_TO_SERDE.get(int(o), int(o))
            if field_name == "container":
                return _CONTAINER_TO_SERDE.get(int(o), int(o))
            if field_name in ("type_kind", "container_key_kind"):
                return _TYPEKIND_TO_SERDE.get(int(o), int(o))
            return o
        if o is None:
            return None
        if hasattr(o, "value"):
            return o.value
        return str(o)

    return json.dumps(_convert(obj))


# ── Hashing ───────────────────────────────────────────────────────────────────


def compute_type_hash(canonical_bytes: bytes) -> bytes:
    """Compute BLAKE3 hash of canonical bytes, returning 32-byte digest.

    Delegates to Rust core for the hash computation.
    """
    import aster._aster as _native
    return bytes(_native.contract.compute_type_hash(canonical_bytes))


def compute_contract_id(contract_bytes: bytes) -> str:
    """Compute BLAKE3 hash of contract bytes, returning 64-char hex string.

    Delegates to Rust core for the hash computation.
    """
    import aster._aster as _native
    digest = bytes(_native.contract.compute_type_hash(contract_bytes))
    return digest.hex()


def contract_id_from_service(service_cls: type) -> str:
    """Derive the per-spec ``contract_id`` for a ``@service``-decorated class.

    Composes :func:`build_type_graph`, :func:`resolve_with_cycles`,
    :meth:`ServiceContract.from_service_info`, :func:`canonical_xlang_bytes`
    and :func:`compute_contract_id` into a single call so callers (examples,
    high-level servers, clients) don't have to reassemble the pipeline.

    Args:
        service_cls: A class decorated with :func:`aster.decorators.service`.

    Returns:
        64-character hex BLAKE3 digest of the canonical ``ServiceContract``.

    Raises:
        TypeError: if ``service_cls`` has no ``__aster_service_info__``.
    """
    service_info = getattr(service_cls, "__aster_service_info__", None)
    if service_info is None:
        raise TypeError(
            f"{service_cls!r} is not @service-decorated "
            f"(missing __aster_service_info__)"
        )

    # 1. Collect root dataclass types from all method signatures.
    root_types: list[type] = []
    for method_info in service_info.methods.values():
        if method_info.request_type is not None and isinstance(method_info.request_type, type):
            root_types.append(method_info.request_type)
        if method_info.response_type is not None and isinstance(method_info.response_type, type):
            root_types.append(method_info.response_type)

    # 2. Walk the type graph, resolve with cycles → TypeDef per FQN.
    types = build_type_graph(root_types)
    type_defs = resolve_with_cycles(types)

    # 3. Derive type_hashes from the canonical TypeDef bytes.
    type_hashes: dict[str, bytes] = {
        fqn: compute_type_hash(canonical_xlang_bytes(td))
        for fqn, td in type_defs.items()
    }

    # 4. Build ServiceContract and hash its canonical bytes.
    contract = ServiceContract.from_service_info(service_info, type_hashes)
    return compute_contract_id(canonical_xlang_bytes(contract))


# ── Type graph construction ───────────────────────────────────────────────────

# Mapping of Python primitive types to XLANG primitive names
_PYTHON_TO_XLANG_PRIMITIVE: dict[type, str] = {
    str: "string",
    int: "int64",
    float: "float64",
    bool: "bool",
    bytes: "binary",
    bytearray: "binary",
}


def _get_type_args(tp: Any) -> tuple[Any, ...]:
    """Return the type arguments of a generic alias."""
    return getattr(tp, "__args__", None) or ()


def _get_origin(tp: Any) -> Any:
    """Return the __origin__ of a generic alias."""
    return getattr(tp, "__origin__", None)


def _is_optional(tp: Any) -> tuple[bool, Any]:
    """Check if *tp* is Optional[X] (i.e. Union[X, None]).

    Returns:
        (is_optional, inner_type)
    """
    import types as _types

    origin = _get_origin(tp)
    # Python 3.10+ union: X | None
    if origin is _types.UnionType:
        args = _get_type_args(tp)
        non_none = [a for a in args if a is not type(None)]
        if len(non_none) == 1 and type(None) in args:
            return True, non_none[0]
        return False, tp

    # typing.Union[X, None]
    import typing
    if origin is typing.Union:
        args = _get_type_args(tp)
        non_none = [a for a in args if a is not type(None)]
        if len(non_none) == 1 and type(None) in args:
            return True, non_none[0]
        return False, tp

    return False, tp


def _get_fqn(cls: type) -> str:
    """Return fully-qualified name for a type."""
    return f"{cls.__module__}.{cls.__qualname__}"


def _get_package_name(cls: type) -> str:
    """Return the package name for a type, using wire_type if available."""
    if hasattr(cls, "__fory_namespace__") and cls.__fory_namespace__:
        return cls.__fory_namespace__
    return cls.__module__ or ""


def _get_type_name(cls: type) -> str:
    """Return the simple type name, using wire_type if available."""
    if hasattr(cls, "__fory_typename__"):
        return cls.__fory_typename__
    return cls.__qualname__


def _build_field_def_for(
    field_obj: dataclasses.Field,
    field_index: int,
    tp: Any,
    back_edges: set[str],
    type_hashes: dict[str, bytes],
    scc_members: set[str] | None = None,
) -> FieldDef:
    """Build a FieldDef for a dataclass field.

    Args:
        field_obj: The dataclasses.Field object.
        field_index: 1-based field ID.
        tp: The field's type annotation.
        back_edges: Set of FQNs that are back-edges (should use SELF_REF).
        type_hashes: Already-computed type hashes for REF resolution.
        scc_members: FQNs of all members in the current SCC.

    Returns:
        FieldDef with all 12 fields populated.
    """
    is_opt, inner_tp = _is_optional(tp)
    container = ContainerKind.NONE
    type_kind = TypeKind.PRIMITIVE
    type_primitive = ""
    type_ref = b""
    self_ref_name = ""
    container_key_kind = TypeKind.PRIMITIVE
    container_key_primitive = ""
    container_key_ref = b""

    # Peel container
    origin = _get_origin(inner_tp if is_opt else tp)
    actual_tp = inner_tp if is_opt else tp

    import typing
    if origin in (list, typing.List):
        container = ContainerKind.LIST
        args = _get_type_args(actual_tp)
        element_tp = args[0] if args else None
        type_kind, type_primitive, type_ref, self_ref_name = _resolve_field_type(
            element_tp, back_edges, type_hashes, scc_members
        )

    elif origin in (set, typing.Set, frozenset):
        container = ContainerKind.SET
        args = _get_type_args(actual_tp)
        element_tp = args[0] if args else None
        type_kind, type_primitive, type_ref, self_ref_name = _resolve_field_type(
            element_tp, back_edges, type_hashes, scc_members
        )

    elif origin in (dict, typing.Dict):
        container = ContainerKind.MAP
        args = _get_type_args(actual_tp)
        key_tp = args[0] if args else None
        val_tp = args[1] if len(args) > 1 else None

        # Key type
        container_key_kind, container_key_primitive, container_key_ref, _ = (
            _resolve_field_type(key_tp, back_edges, type_hashes, scc_members)
        )
        # Value type
        type_kind, type_primitive, type_ref, self_ref_name = _resolve_field_type(
            val_tp, back_edges, type_hashes, scc_members
        )

    else:
        type_kind, type_primitive, type_ref, self_ref_name = _resolve_field_type(
            actual_tp, back_edges, type_hashes, scc_members
        )

    return FieldDef(
        id=field_index,
        name=field_obj.name,
        type_kind=type_kind,
        type_primitive=type_primitive,
        type_ref=type_ref,
        self_ref_name=self_ref_name,
        optional=is_opt,
        ref_tracked=False,
        container=container,
        container_key_kind=container_key_kind,
        container_key_primitive=container_key_primitive,
        container_key_ref=container_key_ref,
    )


def _resolve_field_type(
    tp: Any,
    back_edges: set[str],
    type_hashes: dict[str, bytes],
    scc_members: set[str] | None,
) -> tuple[TypeKind, str, bytes, str]:
    """Resolve a Python type to (type_kind, type_primitive, type_ref, self_ref_name).

    Args:
        tp: Python type to resolve.
        back_edges: FQNs that are back-edges within the current SCC.
        type_hashes: Already-computed hashes for REF resolution.
        scc_members: FQNs of members in the current SCC.

    Returns:
        (TypeKind, primitive_name, type_ref_bytes, self_ref_name)
    """
    if tp is None or tp is type(None):
        return TypeKind.PRIMITIVE, "null", b"", ""

    if tp in _PYTHON_TO_XLANG_PRIMITIVE:
        return TypeKind.PRIMITIVE, _PYTHON_TO_XLANG_PRIMITIVE[tp], b"", ""

    if dataclasses.is_dataclass(tp) and isinstance(tp, type):
        fqn = _get_fqn(tp)
        if fqn in back_edges:
            return TypeKind.SELF_REF, "", b"", fqn
        if fqn in type_hashes:
            return TypeKind.REF, "", type_hashes[fqn], ""
        # Unknown (forward ref or missing hash)
        return TypeKind.REF, "", b"\x00" * 32, ""

    # Any other type: treat as ANY
    return TypeKind.ANY, "", b"", ""


def _build_type_def(
    cls: type,
    back_edges: set[str],
    type_hashes: dict[str, bytes],
    scc_members: set[str] | None = None,
) -> TypeDef:
    """Build a TypeDef from a Python dataclass.

    Args:
        cls: Python dataclass type.
        back_edges: Set of FQNs that are back-edges (use SELF_REF).
        type_hashes: Already-computed type hashes for REF resolution.
        scc_members: FQNs of all members in the current SCC.

    Returns:
        TypeDef for the given class.
    """
    package = _get_package_name(cls)
    name = _get_type_name(cls)

    # Get field type hints
    try:
        hints = get_type_hints(cls)
    except Exception:
        hints = {}

    fields: list[FieldDef] = []
    for idx, dc_field in enumerate(dataclasses.fields(cls), start=1):
        field_type = hints.get(dc_field.name, dc_field.type)
        fd = _build_field_def_for(
            dc_field, idx, field_type, back_edges, type_hashes, scc_members
        )
        fields.append(fd)

    return TypeDef(
        kind=TypeDefKind.MESSAGE,
        package=package,
        name=name,
        fields=fields,
        enum_values=[],
        union_variants=[],
    )


# ── Reference graph builder ───────────────────────────────────────────────────


def _collect_refs(cls: type) -> set[str]:
    """Return FQNs of all dataclass types directly referenced by cls's fields."""
    refs: set[str] = set()
    try:
        hints = get_type_hints(cls)
    except Exception:
        hints = {}

    def _scan(tp: Any) -> None:
        if tp is None:
            return
        origin = _get_origin(tp)
        if origin is not None:
            for arg in _get_type_args(tp):
                _scan(arg)
            return
        # Check for Union / Optional
        _, inner = _is_optional(tp)
        if inner is not tp:
            _scan(inner)
            return
        if isinstance(tp, type) and dataclasses.is_dataclass(tp):
            refs.add(_get_fqn(tp))

    for dc_field in dataclasses.fields(cls):
        field_type = hints.get(dc_field.name, dc_field.type)
        _scan(field_type)

    return refs


# ── Tarjan's SCC algorithm ────────────────────────────────────────────────────


def _tarjan_scc(graph: dict[str, set[str]]) -> list[list[str]]:
    """Return SCCs in reverse topological order (leaves first).

    Args:
        graph: Adjacency map: node -> set of reachable nodes.

    Returns:
        List of SCCs; each SCC is a list of node names.
        Returned in reverse topological order (leaves first, roots last).
    """
    index_counter = [0]
    stack: list[str] = []
    lowlink: dict[str, int] = {}
    index: dict[str, int] = {}
    on_stack: dict[str, bool] = {}
    sccs: list[list[str]] = []

    def strongconnect(v: str) -> None:
        index[v] = index_counter[0]
        lowlink[v] = index_counter[0]
        index_counter[0] += 1
        stack.append(v)
        on_stack[v] = True

        for w in sorted(graph.get(v, set())):  # sorted for determinism
            if w not in index:
                strongconnect(w)
                lowlink[v] = min(lowlink[v], lowlink[w])
            elif on_stack.get(w, False):
                lowlink[v] = min(lowlink[v], index[w])

        if lowlink[v] == index[v]:
            scc: list[str] = []
            while True:
                w = stack.pop()
                on_stack[w] = False
                scc.append(w)
                if w == v:
                    break
            sccs.append(scc)

    for v in sorted(graph.keys()):
        if v not in index:
            strongconnect(v)

    return sccs  # Already in reverse topological order from Tarjan's


def _spanning_tree_dfs(
    start: str,
    members: list[str],
    graph: dict[str, set[str]],
) -> set[tuple[str, str]]:
    """Compute back-edges within an SCC using DFS from start node.

    Args:
        start: Starting node (smallest by NFC codepoint).
        members: All SCC members.
        graph: Reference graph.

    Returns:
        Set of (from, to) pairs that are back-edges (should use SELF_REF).
    """
    member_set = set(members)
    visited: set[str] = set()
    back_edges: set[tuple[str, str]] = set()

    def dfs(v: str) -> None:
        visited.add(v)
        for w in sorted(graph.get(v, set()) & member_set):
            if w not in visited:
                dfs(w)
            else:
                back_edges.add((v, w))

    dfs(start)
    return back_edges


def build_type_graph(root_types: list[type]) -> dict[str, type]:
    """Walk the type graph starting from root_types, returning all encountered types.

    Args:
        root_types: Starting point types (e.g. from service method signatures).

    Returns:
        Dict mapping FQN to type for all discovered dataclass types.
    """
    result: dict[str, type] = {}
    visited: set[str] = set()

    def _visit(tp: Any) -> None:
        origin = _get_origin(tp)
        if origin is not None:
            for arg in _get_type_args(tp):
                _visit(arg)
            return

        is_opt, inner = _is_optional(tp)
        if is_opt:
            _visit(inner)
            return

        if not isinstance(tp, type):
            return
        if tp in _PYTHON_TO_XLANG_PRIMITIVE or tp is type(None):
            return

        if not dataclasses.is_dataclass(tp):
            return

        fqn = _get_fqn(tp)
        if fqn in visited:
            return
        visited.add(fqn)
        result[fqn] = tp

        # Recurse into fields
        try:
            hints = get_type_hints(tp)
        except Exception:
            hints = {}

        for dc_field in dataclasses.fields(tp):
            field_type = hints.get(dc_field.name, dc_field.type)
            _visit(field_type)

    for t in root_types:
        _visit(t)

    return result


def resolve_with_cycles(types: dict[str, type]) -> dict[str, TypeDef]:
    """Resolve all types to TypeDef, handling cycles via Tarjan's SCC algorithm.

    Algorithm:
    1. Build reference graph
    2. Find SCCs with Tarjan's
    3. For each SCC of size >= 1 with cycles:
       - Sort members by NFC codepoint
       - DFS from smallest to find spanning tree
       - Back-edges become SELF_REF
    4. Hash bottom-up over condensation DAG

    Args:
        types: Dict of FQN -> type from build_type_graph.

    Returns:
        Dict mapping FQN to TypeDef (with type_refs resolved to hashes
        where possible, and SELF_REF for back-edges).
    """
    # Build reference graph (only edges within our type set)
    graph: dict[str, set[str]] = {}
    for fqn, cls in types.items():
        refs = _collect_refs(cls) & set(types.keys())
        graph[fqn] = refs
        # Ensure all nodes appear in graph
        for ref in refs:
            if ref not in graph:
                graph[ref] = set()

    # Add isolated nodes
    for fqn in types:
        if fqn not in graph:
            graph[fqn] = set()

    # Find SCCs (returned in reverse topological order -- leaves first)
    sccs = _tarjan_scc(graph)

    # Process SCCs in order (leaves first -- correct for bottom-up hashing)
    type_hashes: dict[str, bytes] = {}
    type_defs: dict[str, TypeDef] = {}

    for scc in sccs:
        scc_set = set(scc)

        # Determine back-edges within this SCC
        back_edges: set[tuple[str, str]] = set()

        if len(scc) == 1:
            fqn = scc[0]
            # Check for self-edge
            if fqn in graph.get(fqn, set()):
                back_edges.add((fqn, fqn))
        else:
            # Multi-node SCC: find spanning tree via DFS from smallest member
            sorted_members = sorted(
                scc,
                key=lambda s: [ord(c) for c in unicodedata.normalize("NFC", s)],
            )
            start = sorted_members[0]
            back_edges = _spanning_tree_dfs(start, sorted_members, graph)

        # Collect all back-edge target FQNs for each source
        back_edge_targets: dict[str, set[str]] = {}
        for (src, tgt) in back_edges:
            back_edge_targets.setdefault(src, set()).add(tgt)

        # Build TypeDef for each member, in reverse topological order within SCC
        # (for multi-node SCCs: nodes that are hashed first are those with
        # only back-edges leaving the SCC)
        if len(scc) == 1:
            fqn = scc[0]
            cls = types[fqn]
            back_edge_set = back_edge_targets.get(fqn, set())
            td = _build_type_def(cls, back_edge_set, type_hashes, scc_set)
            td_bytes = canonical_xlang_bytes(td)
            h = compute_type_hash(td_bytes)
            type_hashes[fqn] = h
            type_defs[fqn] = td
        else:
            # For multi-node SCC: process in reverse topological order
            # Nodes that appear last in DFS (deepest) get hashed first
            # We use the spanning tree structure: reverse post-order of the SCC
            sorted_members = sorted(
                scc,
                key=lambda s: [ord(c) for c in unicodedata.normalize("NFC", s)],
            )
            # Determine processing order: nodes that have no spanning-tree
            # successors within SCC go first
            # We reverse the DFS post-order: deepest (last visited) goes first
            processing_order = _scc_processing_order(
                sorted_members[0], sorted_members, graph, back_edges
            )

            for fqn in processing_order:
                cls = types[fqn]
                back_edge_set = back_edge_targets.get(fqn, set())
                td = _build_type_def(cls, back_edge_set, type_hashes, scc_set)
                td_bytes = canonical_xlang_bytes(td)
                h = compute_type_hash(td_bytes)
                type_hashes[fqn] = h
                type_defs[fqn] = td

    return type_defs


def _scc_processing_order(
    start: str,
    members: list[str],
    graph: dict[str, set[str]],
    back_edges: set[tuple[str, str]],
) -> list[str]:
    """Return processing order for SCC members (nodes hashed first come first).

    Nodes that are leaves in the spanning tree (no spanning-tree outgoing
    edges within the SCC) should be processed first (deepest in DFS tree).

    Args:
        start: Starting DFS node.
        members: All SCC member FQNs.
        graph: Reference graph.
        back_edges: Set of (src, tgt) back-edges.

    Returns:
        List of FQNs in processing order (leaves-first = reverse post-order).
    """
    member_set = set(members)
    # Spanning tree edges = all edges in SCC minus back-edges
    spanning_tree_edges: set[tuple[str, str]] = set()
    for fqn in members:
        for target in graph.get(fqn, set()) & member_set:
            if (fqn, target) not in back_edges:
                spanning_tree_edges.add((fqn, target))

    # DFS post-order on spanning tree
    visited: set[str] = set()
    post_order: list[str] = []

    def dfs_post(v: str) -> None:
        visited.add(v)
        # Visit spanning-tree successors in sorted order
        successors = sorted(
            [tgt for (src, tgt) in spanning_tree_edges if src == v and tgt not in visited],
            key=lambda s: [ord(c) for c in unicodedata.normalize("NFC", s)],
        )
        for w in successors:
            if w not in visited:
                dfs_post(w)
        post_order.append(v)

    dfs_post(start)

    # Reverse post-order = processing order (leaves first)
    return list(reversed(post_order))
