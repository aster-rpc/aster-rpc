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
        self._types: dict[str, type] = {}  # wire_tag → synthesized type
        self._method_map: dict[str, dict[str, Any]] = {}  # "Service:method" → method meta

    def register_from_manifest(self, methods: list[dict[str, Any]]) -> None:
        """Register synthesized types from manifest method descriptors.

        Args:
            methods: List of method dicts from ContractManifest.methods.
                     Each must have request_wire_tag, fields, and optionally
                     response_wire_tag + response_fields.
        """
        for method in methods:
            # Synthesize element types first (needed before the parent type)
            for field_list in (method.get("fields", []), method.get("response_fields", [])):
                self._synthesize_element_types(field_list)

            # Synthesize request type
            req_tag = method.get("request_wire_tag", "")
            req_name = method.get("request_type", "")
            fields = method.get("fields", [])

            if req_tag and req_name and req_tag not in self._types:
                req_cls = self._synthesize_type(req_tag, req_name, fields)
                self._types[req_tag] = req_cls
                logger.debug("Synthesized request type: %s (%s)", req_name, req_tag)

            # Synthesize response type
            resp_tag = method.get("response_wire_tag", "")
            resp_name = method.get("response_type", "")
            resp_fields = method.get("response_fields", [])

            if resp_tag and resp_name and resp_tag not in self._types:
                resp_cls = self._synthesize_type(resp_tag, resp_name, resp_fields)
                self._types[resp_tag] = resp_cls
                logger.debug("Synthesized response type: %s (%s)", resp_name, resp_tag)

    def _synthesize_element_types(self, fields: list[dict[str, Any]]) -> None:
        """Pre-synthesize element types for list[X] fields."""
        for f in fields:
            elem_tag = f.get("element_wire_tag", "")
            elem_name = f.get("element_type", "")
            elem_fields = f.get("element_fields", [])
            if elem_tag and elem_name and elem_tag not in self._types:
                elem_cls = self._synthesize_type(elem_tag, elem_name, elem_fields)
                self._types[elem_tag] = elem_cls
                logger.debug("Synthesized element type: %s (%s)", elem_name, elem_tag)

    def _synthesize_type(
        self, tag: str, name: str, fields: list[dict[str, Any]]
    ) -> type:
        """Create a dataclass with the given wire_type tag and fields."""
        # Build field specifications for make_dataclass
        dc_fields: list[tuple[str, type, Any]] = []
        for f in fields:
            elem_tag = f.get("element_wire_tag", "")
            if elem_tag and elem_tag in self._types:
                # Parameterized list: list[ElementType]
                py_type = list[self._types[elem_tag]]
            else:
                py_type = _resolve_type(f.get("type", "str"))
            default = _resolve_default(f.get("type", "str"), f.get("default"))
            dc_fields.append((f["name"], py_type, dataclasses.field(default=default)))

        # Create the dataclass
        cls = dataclasses.make_dataclass(name, dc_fields)

        # Apply wire_type tag
        cls.__wire_type__ = tag
        # Split tag into namespace/typename for Fory registration
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
