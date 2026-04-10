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
from typing import Any

from aster.codec import wire_type

logger = logging.getLogger(__name__)

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
                return list
            if kind == "map":
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
        """Create a dataclass with the given wire_type tag and fields."""
        dc_fields: list[tuple[str, type, Any]] = []
        for f in fields:
            py_type = self._resolve_field_type(f)
            default = self._resolve_field_default(f)
            if isinstance(default, dataclasses.Field):
                dc_fields.append((f["name"], py_type, default))
            else:
                dc_fields.append((f["name"], py_type, dataclasses.field(default=default)))

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
