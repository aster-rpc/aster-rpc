"""
aster.contract.manifest -- ContractManifest and verification.

Spec reference: Aster-ContractIdentity.md §11.4

Provides:
- ContractManifest: dataclass for persisting contract identity info
- FatalContractMismatch: raised when a live contract doesn't match the manifest
- verify_manifest_or_fatal: strict identity check at startup
"""

from __future__ import annotations

import json
from typing import Any
import dataclasses
from dataclasses import asdict, dataclass, field

import blake3

FIELD_SCHEMA_VERSION = 1


@dataclass
class ContractManifest:
    """Persisted record of a service contract's canonical identity.

    Write with ``json.dumps(asdict(manifest))`` and read back with
    ``ContractManifest(**json.loads(text))``.
    """

    v: int = FIELD_SCHEMA_VERSION
    """Field schema version. 1 = structured kind system."""

    service: str = ""
    """Service name."""

    version: int = 1
    """Service version integer."""

    contract_id: str = ""
    """64-char hex string (full BLAKE3 digest of canonical ServiceContract bytes)."""

    canonical_encoding: str = "fory-xlang/0.15"
    """Encoding scheme identifier."""

    type_count: int = 0
    """Number of user-defined types referenced by this contract."""

    type_hashes: list[str] = field(default_factory=list)
    """Hex-encoded BLAKE3 hashes of each referenced TypeDef (sorted)."""

    method_count: int = 0
    """Number of methods in the service."""

    methods: list[dict] = field(default_factory=list)
    """Method descriptors: [{name, pattern, request_type, response_type, timeout, idempotent, fields}].

    Each entry provides enough information for dynamic invocation and shell
    autocomplete without needing the Python type definitions locally.
    ``fields`` is a list of ``{name, type, required, default}`` dicts describing
    the request type's fields (empty if type info is unavailable).
    """

    serialization_modes: list[str] = field(default_factory=list)
    """Supported serialization mode strings (e.g. ["xlang"])."""

    producer_language: str = ""
    """Producer language identifier. REQUIRED when "native" in serialization_modes,
    empty string otherwise. One of "python", "typescript", "java", "csharp", "go".
    See spec 11.3.2.3 Serialization Modes. Validated by the Rust canonicalizer."""

    scoped: str = "shared"
    """Service scope: "shared" or "session"."""

    deprecated: bool = False
    """Whether this contract version is deprecated."""

    semver: str | None = None
    """Optional semantic version string."""

    vcs_revision: str | None = None
    """Optional VCS commit hash."""

    vcs_tag: str | None = None
    """Optional VCS tag."""

    vcs_url: str | None = None
    """Optional VCS repository URL."""

    changelog: str | None = None
    """Optional free-form changelog entry."""

    published_by: str = ""
    """Identity of the publisher (node public key hex or human name)."""

    published_at_epoch_ms: int = 0
    """Publication timestamp in milliseconds since Unix epoch."""

    def to_json(self, indent: int | None = 2) -> str:
        """Serialize to a JSON string.

        Args:
            indent: JSON indentation level. None for compact.

        Returns:
            JSON string representation.
        """
        return json.dumps(asdict(self), indent=indent)

    @classmethod
    def from_json(cls, text: str) -> "ContractManifest":
        """Deserialize from a JSON string.

        Args:
            text: JSON string.

        Returns:
            ContractManifest instance.
        """
        from aster.limits import (
            MAX_MANIFEST_METHODS,
            MAX_MANIFEST_TYPE_HASHES,
            MAX_MANIFEST_FIELDS_PER_METHOD,
        )

        data = json.loads(text)

        # Validate and cap list sizes from untrusted input
        if "methods" in data and len(data["methods"]) > MAX_MANIFEST_METHODS:
            data["methods"] = data["methods"][:MAX_MANIFEST_METHODS]
        if "type_hashes" in data and len(data["type_hashes"]) > MAX_MANIFEST_TYPE_HASHES:
            data["type_hashes"] = data["type_hashes"][:MAX_MANIFEST_TYPE_HASHES]
        for m in data.get("methods", []):
            if "fields" in m and len(m["fields"]) > MAX_MANIFEST_FIELDS_PER_METHOD:
                m["fields"] = m["fields"][:MAX_MANIFEST_FIELDS_PER_METHOD]

        # Validate critical field types
        if "version" in data:
            data["version"] = int(data["version"])
        if "method_count" in data:
            data["method_count"] = int(data["method_count"])
        if "type_count" in data:
            data["type_count"] = int(data["type_count"])

        # Upgrade legacy (unversioned) manifests to v1 field schema
        if data.get("v") is None:
            data["v"] = FIELD_SCHEMA_VERSION
            for m in data.get("methods", []):
                for key in ("fields", "response_fields"):
                    if key in m:
                        m[key] = [
                            upgrade_legacy_field(f) if "kind" not in f else f
                            for f in m[key]
                        ]

        return cls(**data)

    @classmethod
    def from_file(cls, path: str) -> "ContractManifest":
        """Load a ContractManifest from a JSON file.

        Args:
            path: Path to the manifest JSON file.

        Returns:
            ContractManifest instance.
        """
        with open(path, encoding="utf-8") as f:
            return cls.from_json(f.read())

    def save(self, path: str) -> None:
        """Save the manifest to a JSON file.

        Args:
            path: Destination path for the JSON file.
        """
        with open(path, "w", encoding="utf-8") as f:
            f.write(self.to_json())


# ── Method extraction ─────────────────────────────────────────────────────────


def extract_method_descriptors(service_info: object) -> list[dict]:
    """Extract method descriptors from a ServiceInfo for manifest storage.

    Introspects request types to extract field definitions (name, type, required,
    default) so that dynamic clients and the shell can build payloads without
    needing the Python types locally.

    Args:
        service_info: A ServiceInfo object from aster.service.

    Returns:
        List of method descriptor dicts, sorted by name.
    """
    import dataclasses
    import inspect
    from typing import get_type_hints

    methods_out: list[dict] = []

    for method_name, method_info in getattr(service_info, "methods", {}).items():
        fields: list[dict] = []

        # Extract fields from the request type.
        # Unwrap generic aliases (e.g., SignedRequest[PayloadT] -> SignedRequest)
        # so we can access @wire_type and dataclass fields.
        req_type = getattr(method_info, "request_type", None)
        if isinstance(req_type, str):
            req_type = _resolve_type_by_name(req_type)
        if req_type is not None:
            req_type = _unwrap_generic(req_type)
        if req_type is not None and dataclasses.is_dataclass(req_type):
            try:
                hints = get_type_hints(req_type)
            except Exception:
                hints = {}
            for f in dataclasses.fields(req_type):
                ftype = hints.get(f.name, f.type)
                fields.append(build_field_v1(f, ftype))

        resp_type = getattr(method_info, "response_type", None)

        # Resolve forward references (strings) to actual classes.
        # When `from __future__ import annotations` is used, type hints
        # are strings. The response type might be in any imported module
        # (e.g., JoinResult in identity.py, not common.py where
        # SignedRequest lives). Search loaded modules for the class.
        if isinstance(resp_type, str):
            resp_type = _resolve_type_by_name(resp_type, req_type)

        # Unwrap generic aliases for response type too
        if resp_type is not None:
            resp_type = _unwrap_generic(resp_type)

        # Extract response type fields for dynamic invocation
        resp_fields: list[dict] = []
        if resp_type is not None and dataclasses.is_dataclass(resp_type):
            try:
                resp_hints = get_type_hints(resp_type)
            except Exception:
                resp_hints = {}
            for f in dataclasses.fields(resp_type):
                ftype = resp_hints.get(f.name, f.type)
                resp_fields.append(build_field_v1(f, ftype))

        # Capture Mode 2 inline params so codegen and the shell can render
        # inline signatures. For Mode 1 we leave this empty and the consumer
        # of the manifest treats the method as taking an explicit request
        # class.
        request_style = getattr(method_info, "request_style", "explicit")
        inline_param_dicts: list[dict] = []
        if request_style == "inline":
            raw_params = getattr(method_info, "inline_params", []) or []
            # Build a lookup from field name → field dict already extracted
            # from the synthesized request class, so codegen gets the same
            # type/required/default representation as for explicit types.
            by_name = {f.get("name"): f for f in fields}
            for pname, _ptype in raw_params:
                fdict = by_name.get(pname)
                if fdict is not None:
                    inline_param_dicts.append(fdict)

        methods_out.append({
            "name": method_name,
            "pattern": getattr(method_info, "pattern", "unary"),
            "request_type": _type_display_name(req_type) if req_type else "",
            "response_type": _type_display_name(resp_type) if resp_type else "",
            "request_wire_tag": getattr(req_type, "__wire_type__", "") if req_type else "",
            "response_wire_tag": getattr(resp_type, "__wire_type__", "") if resp_type else "",
            "timeout": getattr(method_info, "timeout", None),
            "idempotent": getattr(method_info, "idempotent", False),
            "fields": fields,
            "response_fields": resp_fields,
            "request_style": request_style,
            "inline_params": inline_param_dicts,
        })

    methods_out.sort(key=lambda m: m["name"])
    return methods_out


def _resolve_type_by_name(name: str, hint_type: type | None = None) -> type | str:
    """Resolve a string type name to an actual class.

    Searches the hint type's module first, then all loaded modules.
    Returns the original string if resolution fails.

    Handles PEP 563 string annotations including generic wrappers --
    ``AsyncIterator[CommandResult]`` is unwrapped to ``CommandResult``
    before lookup, because the wrapper carries no information beyond
    what the method's ``pattern`` field already encodes (the codegen
    knows that a server_stream method yields the response type, that
    a bidi_stream method takes an AsyncIterator of the request type
    and yields the response type, etc.).
    """
    import sys
    import dataclasses

    # Strip generic wrappers to get at the wire type. We unwrap from the
    # outside in: AsyncIterator[CommandResult] -> CommandResult,
    # Optional[Foo] -> Foo, list[Foo] -> Foo. A bare class name passes
    # through unchanged.
    bare = _strip_generic_wrapper(name)

    # Try the hint type's module first
    if hint_type is not None and hasattr(hint_type, "__module__"):
        mod = sys.modules.get(hint_type.__module__)
        if mod is not None:
            candidate = getattr(mod, bare, None)
            if candidate is not None and (isinstance(candidate, type) or dataclasses.is_dataclass(candidate)):
                return candidate

    # Search loaded modules for a dataclass with this name
    for mod in sys.modules.values():
        if mod is None:
            continue
        candidate = getattr(mod, bare, None)
        if candidate is not None and dataclasses.is_dataclass(candidate):
            return candidate

    # Resolution failed -- return the bare (unwrapped) name so downstream
    # consumers don't see the broken generic-syntax form.
    return bare


def _strip_generic_wrapper(name: str) -> str:
    """Pull the inner type out of a generic wrapper string.

    ``AsyncIterator[CommandResult]`` -> ``CommandResult``
    ``Optional[Foo]`` -> ``Foo``
    ``list[Bar]`` -> ``Bar``
    ``Foo`` -> ``Foo``

    Recurses for nested wrappers (``AsyncIterator[Optional[X]]`` -> ``X``).
    For union/comma-separated forms, picks the first non-None component.
    """
    s = name.strip()
    while "[" in s and s.endswith("]"):
        bracket = s.index("[")
        s = s[bracket + 1:-1].strip()
        if "," in s:
            parts = [p.strip() for p in s.split(",")]
            s = next((p for p in parts if p and p != "None"), parts[0])
    return s


def _unwrap_generic(t: object) -> type:
    """Unwrap a generic alias (e.g., SignedRequest[Payload]) to its origin class."""
    origin = getattr(t, "__origin__", None)
    if origin is not None and isinstance(origin, type):
        return origin
    return t


def _extract_list_element_type(t: object) -> type | None:
    """Extract the element type from list[X], or None if not a parameterized list."""
    import typing
    origin = getattr(t, "__origin__", None)
    if origin is list:
        args = getattr(t, "__args__", ())
        if args:
            return args[0]
    return None


def _type_display_name(t: object) -> str:
    """Human-readable name for a type."""
    if t is None:
        return ""
    if hasattr(t, "__name__"):
        return t.__name__
    return str(t)


def _is_json_safe(val: object) -> bool:
    """Check if a value is safely JSON-serializable."""
    return val is None or isinstance(val, (str, int, float, bool))


# ── V1 field schema ──────────────────────────────────────────────────────────

_PY_TO_KIND: dict[type, str] = {
    str: "string",
    int: "int",
    float: "float",
    bool: "bool",
    bytes: "bytes",
}


def _classify_type(tp: object) -> dict[str, Any]:
    """Classify a Python type into the v1 field schema kind system.

    Returns a partial field dict with kind, nullable, and type-specific
    keys (item_kind, ref_name, enum_values, etc.).
    """
    import enum
    import typing

    result: dict[str, Any] = {}

    # Unwrap Optional / X | None
    origin = getattr(tp, "__origin__", None)
    args = getattr(tp, "__args__", ())

    is_optional = False
    if origin is typing.Union and type(None) in args:
        non_none = [a for a in args if a is not type(None)]
        if len(non_none) == 1:
            is_optional = True
            tp = non_none[0]
            origin = getattr(tp, "__origin__", None)
            args = getattr(tp, "__args__", ())

    result["nullable"] = is_optional

    # Primitives
    if tp in _PY_TO_KIND:
        result["kind"] = _PY_TO_KIND[tp]
        return result

    # Bare dict (no type params): treat as map[string, string]
    if tp is dict:
        result["kind"] = "map"
        result["key_kind"] = "string"
        result["value_kind"] = "string"
        result["value_nullable"] = False
        return result

    # Bare list (no type params): treat as list[string]
    if tp is list:
        result["kind"] = "list"
        result["item_kind"] = "string"
        result["item_nullable"] = False
        return result

    # Enum
    if isinstance(tp, type) and issubclass(tp, enum.Enum):
        result["kind"] = "enum"
        result["ref_name"] = tp.__name__
        result["enum_values"] = [e.value for e in tp]
        return result

    # list[X]
    if origin is list:
        result["kind"] = "list"
        if args:
            elem = _classify_type(args[0])
            result["item_kind"] = elem.get("kind", "string")
            result["item_nullable"] = elem.get("nullable", False)
            if elem.get("ref_name"):
                result["item_ref"] = elem["ref_name"]
            if elem.get("wire_tag"):
                result["item_wire_tag"] = elem["wire_tag"]
        else:
            result["item_kind"] = "string"
            result["item_nullable"] = False
        return result

    # dict[K, V]
    if origin is dict:
        result["kind"] = "map"
        if len(args) >= 2:
            key_info = _classify_type(args[0])
            val_info = _classify_type(args[1])
            result["key_kind"] = key_info.get("kind", "string")
            result["value_kind"] = val_info.get("kind", "string")
            result["value_nullable"] = val_info.get("nullable", False)
            if val_info.get("ref_name"):
                result["value_ref"] = val_info["ref_name"]
        else:
            result["key_kind"] = "string"
            result["value_kind"] = "string"
            result["value_nullable"] = False
        return result

    # Dataclass reference
    if dataclasses.is_dataclass(tp) and isinstance(tp, type):
        result["kind"] = "ref"
        result["ref_name"] = tp.__name__
        result["wire_tag"] = getattr(tp, "__wire_type__", "")
        return result

    # Fallback: treat as string
    result["kind"] = "string"
    return result


def build_field_v1(f: dataclasses.Field, resolved_type: object) -> dict[str, Any]:
    """Build a v1 schema field dict from a dataclass field.

    Args:
        f: The dataclass Field object.
        resolved_type: The resolved type hint for this field.

    Returns:
        A v1 field schema dict.
    """
    info = _classify_type(resolved_type)

    has_default = (
        f.default is not dataclasses.MISSING
        or f.default_factory is not dataclasses.MISSING
    )

    default_value = None
    default_kind = "none"

    if f.default is not dataclasses.MISSING:
        default_value = f.default if _is_json_safe(f.default) else str(f.default)
        default_kind = "value"
    elif f.default_factory is not dataclasses.MISSING:
        factory = f.default_factory
        if factory is list:
            default_kind = "empty_list"
        elif factory is dict:
            default_kind = "empty_map"
        else:
            default_kind = "factory"

    field_dict: dict[str, Any] = {
        "name": f.name,
        "kind": info.get("kind", "string"),
        "nullable": info.get("nullable", False),
        "required": not has_default,
        "default_value": default_value,
        "default_kind": default_kind,
    }

    # Type-specific keys (only include when set)
    for key in ("ref_name", "wire_tag", "enum_values",
                "item_kind", "item_ref", "item_wire_tag", "item_nullable",
                "key_kind", "value_kind", "value_ref", "value_nullable"):
        if key in info:
            field_dict[key] = info[key]

    field_dict["properties"] = {}
    return field_dict


def upgrade_legacy_field(old: dict[str, Any]) -> dict[str, Any]:
    """Convert an unversioned legacy field dict to v1 schema.

    Parses the Python type string and maps to the structured kind system.
    """
    name = old.get("name", "")
    type_str = str(old.get("type", "string")).lower()
    required = old.get("required", False)
    default = old.get("default")

    kind = "string"
    extra: dict[str, Any] = {}

    if type_str in ("str", "string"):
        kind = "string"
    elif type_str in ("int", "integer"):
        kind = "int"
    elif type_str in ("float", "double"):
        kind = "float"
    elif type_str in ("bool", "boolean"):
        kind = "bool"
    elif type_str == "bytes":
        kind = "bytes"
    elif type_str.startswith("list"):
        kind = "list"
        extra["item_kind"] = "string"
        extra["item_nullable"] = False
        elem_wire_tag = old.get("element_wire_tag", "")
        elem_type = old.get("element_type", "")
        if elem_wire_tag or elem_type:
            extra["item_kind"] = "ref"
            extra["item_ref"] = elem_type
            extra["item_wire_tag"] = elem_wire_tag
    elif type_str.startswith("dict") or type_str.startswith("map"):
        kind = "map"
        extra["key_kind"] = "string"
        extra["value_kind"] = "string"
        extra["value_nullable"] = False
    elif type_str.startswith("optional"):
        kind = "string"
        extra["nullable"] = True
    else:
        kind = "ref"
        extra["ref_name"] = old.get("type", "")
        extra["wire_tag"] = old.get("wire_tag", "")

    nullable = extra.pop("nullable", False)
    default_kind = "none"
    default_value = None
    if default is not None:
        default_kind = "value"
        default_value = default
    elif not required:
        if kind == "list":
            default_kind = "empty_list"
        elif kind == "map":
            default_kind = "empty_map"
        elif nullable:
            default_kind = "null"

    result: dict[str, Any] = {
        "name": name,
        "kind": kind,
        "nullable": nullable,
        "required": required,
        "default_value": default_value,
        "default_kind": default_kind,
        **extra,
        "properties": {},
    }
    return result


# ── FatalContractMismatch ─────────────────────────────────────────────────────


class FatalContractMismatch(Exception):
    """Raised when the live contract hash doesn't match the committed manifest.

    This indicates a breaking change that wasn't recorded. The developer
    must rerun ``aster contract gen`` and commit the updated manifest.
    """

    def __init__(
        self,
        service_name: str,
        version: int,
        expected_id: str,
        actual_id: str,
        manifest_path: str,
    ) -> None:
        self.service_name = service_name
        self.version = version
        self.expected_id = expected_id
        self.actual_id = actual_id
        self.manifest_path = manifest_path

        super().__init__(
            f"Contract identity mismatch for {service_name!r} v{version}:\n"
            f"  Expected: {expected_id}\n"
            f"  Actual:   {actual_id}\n"
            f"  Manifest: {manifest_path}\n"
            f"  → The service interface has changed without updating the manifest.\n"
            f"    Rerun `aster contract gen` and commit the updated manifest."
        )


# ── Verification ──────────────────────────────────────────────────────────────


def verify_manifest_or_fatal(
    live_contract_bytes: bytes,
    manifest_path: str,
) -> ContractManifest:
    """Verify that the live contract bytes match the committed manifest.

    Loads the manifest from *manifest_path*, computes BLAKE3 of
    *live_contract_bytes*, and checks for equality.

    Args:
        live_contract_bytes: Canonical bytes of the live ServiceContract.
        manifest_path: Path to the committed manifest JSON file.

    Returns:
        The loaded ContractManifest on success.

    Raises:
        FatalContractMismatch: If the hashes differ.
        FileNotFoundError: If the manifest file does not exist.
        json.JSONDecodeError: If the manifest file is malformed.
    """
    manifest = ContractManifest.from_file(manifest_path)
    actual_id = blake3.blake3(live_contract_bytes).hexdigest()

    if actual_id != manifest.contract_id:
        raise FatalContractMismatch(
            service_name=manifest.service,
            version=manifest.version,
            expected_id=manifest.contract_id,
            actual_id=actual_id,
            manifest_path=manifest_path,
        )

    return manifest
