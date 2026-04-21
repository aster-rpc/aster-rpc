"""
aster.dynamic -- Dynamic type synthesis from contract manifests.

Enables invocation of Aster services without needing local Python type
definitions. Reads method schemas from manifests and synthesizes
wire-compatible dataclasses at runtime.

Usage::

    from aster.dynamic import DynamicTypeFactory

    factory = DynamicTypeFactory()
    factory.register_from_manifest(manifest_methods)

    # Get a synthesized request type
    HelloRequest = factory.get_type("_hello_service.HelloRequest")
    req = HelloRequest(name="World")  # works like a normal dataclass

    # Or build from a dict
    req = factory.build_request("HelloService", "say_hello", {"name": "World"})
"""

from __future__ import annotations

import dataclasses
import logging
import re
from typing import Any

from aster.codec import wire_type


_CAMEL_SPLIT_1 = re.compile(r"(.)([A-Z][a-z]+)")
_CAMEL_SPLIT_2 = re.compile(r"([a-z0-9])([A-Z])")


def _to_snake_case(name: str) -> str:
    """Convert camelCase / PascalCase to snake_case.

    Idempotent on already-snake_case names. Matches Fory Java / Fory TS's
    automatic fingerprint casing so the synthesized Python type's
    attribute names align with the cross-binding wire token set.
    """
    s1 = _CAMEL_SPLIT_1.sub(r"\1_\2", name)
    return _CAMEL_SPLIT_2.sub(r"\1_\2", s1).lower()

logger = logging.getLogger(__name__)

# V1 schema `kind` → Python type (for dict/list inner-type annotations).
# Must keep Fory field-hash parity with the codegen path in
# cli/aster_cli/codegen.py -- same mapping there as `_KIND_TO_PY`.
_KIND_TO_PY: dict[str, type] = {
    "string": str,
    "int": int,
    "float": float,
    "bool": bool,
    "bytes": bytes,
}

# Aster type name → Python type for dataclass field annotations
_TYPE_MAP: dict[str, type] = {
    "str": str,
    "string": str,
    "int": int,
    "int32": int,
    "int64": int,
    "float": float,
    "float32": float,
    "float64": float,
    "double": float,
    "bool": bool,
    "boolean": bool,
    "bytes": bytes,
}

# Default values for each Python type
_DEFAULT_MAP: dict[type, Any] = {
    str: "",
    int: 0,
    float: 0.0,
    bool: False,
    bytes: b"",
}


def _resolve_type(type_name: str) -> type:
    """Resolve an Aster type name to a Python type."""
    lower = type_name.lower()
    if lower in _TYPE_MAP:
        return _TYPE_MAP[lower]

    # list[X] → list
    if type_name.lower().startswith("list["):
        return list

    # dict[str, X] → dict
    if type_name.lower().startswith("dict["):
        return dict

    # Optional[X] → resolve inner
    if type_name.lower().startswith("optional["):
        return _resolve_type(type_name[9:-1])

    # Unknown → Any (treated as object)
    return object


def _resolve_default(type_name: str, field_default: Any = dataclasses.MISSING) -> Any:
    """Resolve a default value for a field."""
    if field_default is not dataclasses.MISSING and field_default is not None:
        return field_default

    py_type = _resolve_type(type_name)
    return _DEFAULT_MAP.get(py_type, None)


class DynamicTypeFactory:
    """Creates wire-compatible dataclasses from manifest method schemas.

    Synthesized types have the same ``@wire_type`` tag, field names, and
    field types as the original producer-side types. Fory XLANG serializes
    them identically -- the server sees a valid request.
    """

    def __init__(self) -> None:
        self._types: dict[str, type] = {}  # wire_tag -> synthesized type
        self._type_defs: dict[str, tuple[str, list[dict[str, Any]]]] = {}  # wire_tag -> (name, fields)
        self._method_map: dict[str, dict[str, Any]] = {}  # "Service:method" -> method meta

    def register_from_manifest(self, methods: list[dict[str, Any]]) -> None:
        """Register synthesized types from manifest method descriptors.

        Reads v1 field schema (kind/ref_name/item_ref/etc.) with fallback
        to legacy keys for older manifests. Walks all methods to build a
        wire_tag -> fields map, then synthesizes types with cross-method
        ref resolution.
        """
        # First pass: collect all type definitions across all methods
        for method in methods:
            req_tag = method.get("request_wire_tag", "")
            req_name = method.get("request_type", "")
            if req_tag and req_name and req_tag not in self._type_defs:
                self._type_defs[req_tag] = (req_name, method.get("fields", []))

            resp_tag = method.get("response_wire_tag", "")
            resp_name = method.get("response_type", "")
            if resp_tag and resp_name and resp_tag not in self._type_defs:
                self._type_defs[resp_tag] = (resp_name, method.get("response_fields", []))

        # Second pass: synthesize each type. _ensure_type recurses into
        # nested ref types as needed.
        for tag in list(self._type_defs.keys()):
            if tag not in self._types:
                self._ensure_type(tag)

    def _ensure_type(self, tag: str) -> type | None:
        """Synthesize the type for a wire tag if not already done."""
        if tag in self._types:
            return self._types[tag]
        defn = self._type_defs.get(tag)
        if defn is None:
            return None
        name, fields = defn
        cls = self._synthesize_type(tag, name, fields)
        self._types[tag] = cls
        logger.debug("Synthesized type: %s (%s)", name, tag)
        return cls

    def _resolve_field_type(self, f: dict[str, Any]) -> type:
        """Resolve a v1 schema field to a Python type for the dataclass."""
        kind = f.get("kind")
        if kind is not None:
            # V1 schema path
            if kind == "string":
                return str
            if kind == "int":
                return int
            if kind == "float":
                return float
            if kind == "bool":
                return bool
            if kind == "bytes":
                return bytes
            if kind == "ref":
                ref_tag = f.get("wire_tag", "")
                if ref_tag:
                    nested = self._ensure_type(ref_tag)
                    if nested is not None:
                        return nested
                return object
            if kind == "enum":
                return str
            if kind == "list":
                item_kind = f.get("item_kind", "string")
                if item_kind == "ref":
                    item_tag = f.get("item_wire_tag", "")
                    if item_tag:
                        nested = self._ensure_type(item_tag)
                        if nested is not None:
                            return list[nested]
                inner = _KIND_TO_PY.get(item_kind)
                if inner is not None:
                    return list[inner]
                return list
            if kind == "map":
                key_py = _KIND_TO_PY.get(f.get("key_kind", "string"), str)
                val_kind = f.get("value_kind", "string")
                if val_kind == "ref":
                    val_tag = f.get("value_wire_tag", "")
                    if val_tag:
                        nested = self._ensure_type(val_tag)
                        if nested is not None:
                            return dict[key_py, nested]
                    return dict
                val_py = _KIND_TO_PY.get(val_kind)
                if val_py is not None:
                    return dict[key_py, val_py]
                return dict
            return object

        # Legacy path
        elem_tag = f.get("element_wire_tag", "")
        if elem_tag:
            nested = self._ensure_type(elem_tag)
            if nested is not None:
                return list[nested]
        return _resolve_type(f.get("type", "str"))

    def _resolve_field_default(self, f: dict[str, Any]) -> Any:
        """Resolve a v1 schema field's default value (or dataclass field)."""
        dk = f.get("default_kind")
        if dk is not None:
            # V1 schema path
            if dk == "value":
                dv = f.get("default_value")
                if dv is not None:
                    return dv
            elif dk == "empty_list":
                return dataclasses.field(default_factory=list)
            elif dk == "empty_map":
                return dataclasses.field(default_factory=dict)
            elif dk == "null":
                return None

            # Type-based default for v1 fields without explicit default_value
            kind = f.get("kind", "string")
            if kind == "string":
                return ""
            if kind == "int":
                return 0
            if kind == "float":
                return 0.0
            if kind == "bool":
                return False
            if kind == "bytes":
                return b""
            if kind == "list":
                return dataclasses.field(default_factory=list)
            if kind == "map":
                return dataclasses.field(default_factory=dict)
            return None

        # Legacy path
        return _resolve_default(f.get("type", "str"), f.get("default"))

    def _synthesize_type(
        self, tag: str, name: str, fields: list[dict[str, Any]]
    ) -> type:
        """Create a dataclass with the given wire_type tag and fields.

        Field names are snake-cased from whatever the manifest declares,
        so a Java-published contract with ``agentId`` surfaces as a
        Python attribute ``agent_id``. This keeps the Python wire
        fingerprint (pyfory uses the raw attribute name) aligned with
        Java/TS Fory's automatic snake-case fingerprinting, and it keeps
        user code idiomatic (dicts keyed by ``agent_id`` work against
        any-binding contracts).
        """
        dc_fields: list[tuple[str, type, Any]] = []
        for f in fields:
            py_type = self._resolve_field_type(f)
            default = self._resolve_field_default(f)
            pyname = _to_snake_case(f["name"])
            if isinstance(default, dataclasses.Field):
                dc_fields.append((pyname, py_type, default))
            else:
                dc_fields.append((pyname, py_type, dataclasses.field(default=default)))

        cls = dataclasses.make_dataclass(name, dc_fields)

        cls.__wire_type__ = tag
        if "/" in tag:
            ns, tn = tag.rsplit("/", 1)
        elif "." in tag:
            parts = tag.rsplit(".", 1)
            ns = parts[0]
            tn = parts[1]
        else:
            ns = ""
            tn = tag
        cls.__fory_namespace__ = ns
        cls.__fory_typename__ = tn

        return cls

    def get_type(self, wire_tag: str) -> type | None:
        """Get a synthesized type by its wire_type tag."""
        return self._types.get(wire_tag)

    def get_all_types(self) -> list[type]:
        """Get all synthesized types (for codec registration)."""
        return list(self._types.values())

    def build_request(
        self, method_meta: dict[str, Any], args: dict[str, Any]
    ) -> Any:
        """Build a typed request object from a dict of arguments.

        Args:
            method_meta: Method descriptor from the manifest.
            args: Key-value arguments from the user/agent.

        Returns:
            An instance of the synthesized request dataclass.

        Raises:
            KeyError: If the request type hasn't been registered.
        """
        req_tag = method_meta.get("request_wire_tag", "")
        if not req_tag:
            raise KeyError(f"No request_wire_tag in method metadata")

        cls = self._types.get(req_tag)
        if cls is None:
            raise KeyError(f"Type {req_tag!r} not registered")

        # Filter args to only known fields
        known = {f.name for f in dataclasses.fields(cls)}
        filtered = {k: v for k, v in args.items() if k in known}
        return cls(**filtered)

    @property
    def type_count(self) -> int:
        return len(self._types)

    def register_from_type_defs(
        self,
        roots: list[str],
        type_defs_by_tag: dict[str, dict[str, Any]],
        type_defs_by_hash: dict[str, dict[str, Any]],
    ) -> set[str]:
        """Synthesize Python dataclasses from canonical ``TypeDef`` dicts.

        The flat-manifest path (:meth:`register_from_manifest`) loses
        information whenever a field references a nested struct: the
        manifest only carries leaf ``kind`` strings, so the dynamic
        proxy has to fall back to ``object`` for refs and the producer-
        side Fory hash diverges from what we build at the consumer.
        The TypeDef graph carries the full tree -- hex-hash refs for
        every nested type -- so the hash we compute here matches the
        producer byte-for-byte.

        Mirrors the TS ``DynamicTypeFactory.registerFromTypeDefs``
        (bindings/typescript/packages/aster/src/dynamic.ts) so all
        bindings that consume ``types/{hash}.bin`` blobs converge on a
        single code path.

        Args:
            roots: Wire tags ("package/name") to start the walk from --
                typically every method's request/response wire tag.
            type_defs_by_tag: Tag -> decoded TypeDef dict.
            type_defs_by_hash: Hex BLAKE3 hash -> decoded TypeDef dict.
                Nested ``ref`` fields carry the hex hash of the target
                type, so we resolve them by hash.

        Returns:
            The set of wire tags that were reached + registered. Callers
            can diff against ``roots`` to decide whether to fall back
            to the flat manifest path for any unresolved roots.
        """
        resolved: set[str] = set()
        ordered = _topo_sort_reachable(roots, type_defs_by_tag, type_defs_by_hash)

        for tag in ordered:
            if tag in self._types:
                resolved.add(tag)
                continue
            td = type_defs_by_tag.get(tag)
            if td is None:
                continue
            name = td.get("name", "")
            if not name:
                continue
            cls = self._synthesize_from_type_def(tag, name, td, type_defs_by_hash)
            self._types[tag] = cls
            resolved.add(tag)

        return resolved

    def _synthesize_from_type_def(
        self,
        tag: str,
        name: str,
        td: dict[str, Any],
        type_defs_by_hash: dict[str, dict[str, Any]],
    ) -> type:
        """Build a dataclass for one canonical TypeDef.

        Field types are resolved from ``(type_kind, type_primitive,
        type_ref, container, container_key_*)``. Nested refs are looked
        up by hex hash in ``type_defs_by_hash``; topo-sort guarantees
        leaf types are already in ``self._types`` when we reach a
        parent that references them.
        """
        dc_fields: list[tuple[str, type, Any]] = []
        for f in td.get("fields", []):
            py_type = self._canonical_field_type(f, type_defs_by_hash)
            default = self._canonical_field_default(f)
            pyname = _to_snake_case(f["name"])
            if isinstance(default, dataclasses.Field):
                dc_fields.append((pyname, py_type, default))
            else:
                dc_fields.append((pyname, py_type, dataclasses.field(default=default)))

        cls = dataclasses.make_dataclass(name, dc_fields)
        cls.__wire_type__ = tag
        if "/" in tag:
            ns, tn = tag.rsplit("/", 1)
        elif "." in tag:
            parts = tag.rsplit(".", 1)
            ns = parts[0]
            tn = parts[1]
        else:
            ns = ""
            tn = tag
        cls.__fory_namespace__ = ns
        cls.__fory_typename__ = tn
        return cls

    def _canonical_field_type(
        self,
        f: dict[str, Any],
        type_defs_by_hash: dict[str, dict[str, Any]],
    ) -> type:
        container = f.get("container", "none")
        if container in ("list", "set"):
            elem = self._canonical_leaf_type(
                f.get("type_kind", "primitive"),
                f.get("type_primitive", "string"),
                f.get("type_ref", ""),
                type_defs_by_hash,
            )
            return list[elem] if elem is not object else list
        if container == "map":
            key = self._canonical_leaf_type(
                f.get("container_key_kind", "primitive"),
                f.get("container_key_primitive", "string"),
                f.get("container_key_ref", ""),
                type_defs_by_hash,
            )
            val = self._canonical_leaf_type(
                f.get("type_kind", "primitive"),
                f.get("type_primitive", "string"),
                f.get("type_ref", ""),
                type_defs_by_hash,
            )
            if key is object or val is object:
                return dict
            return dict[key, val]
        return self._canonical_leaf_type(
            f.get("type_kind", "primitive"),
            f.get("type_primitive", "string"),
            f.get("type_ref", ""),
            type_defs_by_hash,
        )

    def _canonical_leaf_type(
        self,
        type_kind: str,
        type_primitive: str,
        type_ref_hex: str,
        type_defs_by_hash: dict[str, dict[str, Any]],
    ) -> Any:
        if type_kind == "primitive":
            return _primitive_to_py().get(type_primitive, object)
        if type_kind == "ref":
            nested_td = type_defs_by_hash.get(type_ref_hex)
            if nested_td is None:
                return object
            nested_tag = f"{nested_td.get('package', '')}/{nested_td.get('name', '')}"
            nested_cls = self._types.get(nested_tag)
            return nested_cls if nested_cls is not None else object
        return object

    @staticmethod
    def _canonical_field_default(f: dict[str, Any]) -> Any:
        container = f.get("container", "none")
        if container in ("list", "set"):
            return dataclasses.field(default_factory=list)
        if container == "map":
            return dataclasses.field(default_factory=dict)
        type_kind = f.get("type_kind", "primitive")
        if type_kind == "primitive":
            prim = f.get("type_primitive", "string")
            if prim in _PRIMITIVE_DEFAULTS:
                return _PRIMITIVE_DEFAULTS[prim]
        return None


# Canonical ``type_primitive`` → Python annotation. pyfory distinguishes
# bit-widths through its ``TypeVar`` markers (``pyfory.int32`` etc.): a
# struct declared with ``accepted: int`` hashes differently from one
# declared with ``accepted: pyfory.int32``, so if we want the client-
# side synthesised dataclass to interop byte-for-byte with a producer
# that used ``pyfory.int32``, we must surface the same TypeVar here.
# Missing entries fall through to ``object``.
def _build_primitive_map() -> dict[str, Any]:
    # Imported lazily so pyfory remains an optional dependency path-wise
    # -- ``register_from_manifest`` alone doesn't need these markers.
    import pyfory

    return {
        "bool": bool,
        "string": str,
        "binary": bytes,
        "int8": pyfory.int8,
        "int16": pyfory.int16,
        "int32": pyfory.int32,
        "int64": pyfory.int64,
        # Fory xlang models uint via the signed int of the same width.
        "uint8": pyfory.int8,
        "uint16": pyfory.int16,
        "uint32": pyfory.int32,
        "uint64": pyfory.int64,
        "float32": pyfory.float32,
        "float64": pyfory.float64,
        # Timestamps are carried as i64 millis in pyfory's xlang.
        "timestamp": pyfory.int64,
        "uuid": str,
    }


_PRIMITIVE_TO_PY: dict[str, Any] | None = None


def _primitive_to_py() -> dict[str, Any]:
    global _PRIMITIVE_TO_PY
    if _PRIMITIVE_TO_PY is None:
        _PRIMITIVE_TO_PY = _build_primitive_map()
    return _PRIMITIVE_TO_PY


# Default values per primitive. pyfory's TypeVar markers aren't hashable
# as plain ``type`` instances, so we key by name instead.
_PRIMITIVE_DEFAULTS: dict[str, Any] = {
    "bool": False,
    "string": "",
    "binary": b"",
    "int8": 0,
    "int16": 0,
    "int32": 0,
    "int64": 0,
    "uint8": 0,
    "uint16": 0,
    "uint32": 0,
    "uint64": 0,
    "float32": 0.0,
    "float64": 0.0,
    "timestamp": 0,
    "uuid": "",
}


def _topo_sort_reachable(
    roots: list[str],
    by_tag: dict[str, dict[str, Any]],
    by_hash: dict[str, dict[str, Any]],
) -> list[str]:
    """Return wire tags reachable from ``roots``, leaves first.

    Back-edges (cycles via ``self_ref``) are broken by visit order: a
    node already on the DFS stack is treated as a self-reference and
    skipped. Mirrors the TS ``topoSortReachable`` in dynamic.ts.
    """
    ordered: list[str] = []
    visited: set[str] = set()
    on_stack: set[str] = set()

    def ref_tag(hash_hex: str) -> str | None:
        nested = by_hash.get(hash_hex)
        if nested is None:
            return None
        return f"{nested.get('package', '')}/{nested.get('name', '')}"

    def collect_children(td: dict[str, Any]) -> list[str]:
        out: list[str] = []
        for f in td.get("fields", []):
            if f.get("type_kind") == "ref":
                tag = ref_tag(f.get("type_ref", ""))
                if tag:
                    out.append(tag)
            if f.get("container", "none") != "none" and f.get("container_key_kind") == "ref":
                tag = ref_tag(f.get("container_key_ref", ""))
                if tag:
                    out.append(tag)
        return out

    def visit(tag: str) -> None:
        if tag in visited or tag in on_stack:
            return
        td = by_tag.get(tag)
        if td is None:
            return
        on_stack.add(tag)
        for child in collect_children(td):
            visit(child)
        on_stack.discard(tag)
        visited.add(tag)
        ordered.append(tag)

    for r in roots:
        visit(r)
    return ordered
