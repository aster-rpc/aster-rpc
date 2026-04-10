"""
aster_cli.codegen -- Generate typed client libraries from Aster manifests.

Implements the algorithm described in docs/_internal/aster-client-generation.md.
Currently supports Python output only.
"""

from __future__ import annotations

import os
import re
import textwrap
from datetime import datetime, timezone
from typing import Any


# ── Type mapping ─────────────────────────────────────────────────────────────

_PY_TYPE_MAP: dict[str, str] = {
    "str": "str",
    "string": "str",
    "int": "int",
    "int32": "int",
    "int64": "int",
    "float": "float",
    "float32": "float",
    "float64": "float",
    "double": "float",
    "bool": "bool",
    "boolean": "bool",
    "bytes": "bytes",
    "optional": "Optional[str]",
}

_PY_DEFAULTS: dict[str, str] = {
    "str": '""',
    "int": "0",
    "float": "0.0",
    "bool": "False",
    "bytes": 'b""',
}

_OPTIONAL_FIELD_HINTS: dict[str, str] = {
    "bio": "Optional[str]",
    "display_name": "Optional[str]",
    "rate_limit": "Optional[str]",
    "recovery_codes": "Optional[list[str]]",
    "replacement": "Optional[str]",
    "scope_node_id": "Optional[str]",
    "url": "Optional[str]",
}

_KNOWN_WIRE_TYPES: dict[str, tuple[str, list[dict[str, Any]]]] = {
    "DelegationStatement": (
        "aster/DelegationStatement",
        [
            {"name": "authority", "type": "str", "required": False, "default": "consumer"},
            {"name": "mode", "type": "str", "required": False, "default": "open"},
            {"name": "token_ttl", "type": "int", "required": False, "default": 300},
            {"name": "rate_limit", "type": "Optional", "required": False, "default": None},
            {"name": "roles", "type": "list[str]", "required": False, "default": None},
        ],
    ),
    "SigningKeyAttestation": (
        "aster/SigningKeyAttestation",
        [
            {"name": "signing_pubkey", "type": "str", "required": False, "default": ""},
            {"name": "key_id", "type": "str", "required": False, "default": ""},
            {"name": "valid_from", "type": "int", "required": False, "default": 0},
            {"name": "valid_until", "type": "int", "required": False, "default": 0},
            {"name": "root_signature", "type": "str", "required": False, "default": ""},
        ],
    ),
}


def _to_snake_case(name: str) -> str:
    """Convert CamelCase to snake_case."""
    s1 = re.sub(r"(.)([A-Z][a-z]+)", r"\1_\2", name)
    return re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", s1).lower()


def _py_type_str(type_name: str, known_types: dict[str, str]) -> str:
    """Convert a manifest type name to a Python type annotation string.

    Args:
        type_name: The type name from the manifest (e.g., "str", "list[DiscoverEntry]").
        known_types: Map of display_name -> Python class name for generated types.
    """
    # Handle PEP 604 unions from manifest (e.g., "str | None") -- convert
    # to Optional[X] for pyfory compatibility
    if " | None" in type_name:
        inner = type_name.replace(" | None", "").strip()
        inner_py = _py_type_str(inner, known_types)
        return f"Optional[{inner_py}]"

    lower = type_name.lower()
    if lower in _PY_TYPE_MAP:
        return _PY_TYPE_MAP[lower]

    # list[X]
    m = re.match(r"list\[(.+)\]", type_name, re.IGNORECASE)
    if m:
        inner = m.group(1)
        inner_py = known_types.get(inner, _PY_TYPE_MAP.get(inner.lower(), inner))
        return f"list[{inner_py}]"

    # dict[K, V]
    m = re.match(r"dict\[(.+),\s*(.+)\]", type_name, re.IGNORECASE)
    if m:
        k = _PY_TYPE_MAP.get(m.group(1).lower(), m.group(1))
        v = known_types.get(m.group(2), _PY_TYPE_MAP.get(m.group(2).lower(), m.group(2)))
        return f"dict[{k}, {v}]"

    # Optional[X] -- use typing.Optional, not PEP 604 union (pyfory compat)
    m = re.match(r"optional\[(.+)\]", type_name, re.IGNORECASE)
    if m:
        inner = _py_type_str(m.group(1), known_types)
        return f"Optional[{inner}]"

    # Known generated type
    if type_name in known_types:
        return known_types[type_name]

    # Unknown complex type -- use Any to avoid NameErrors
    if "." in type_name or type_name[0:1].isupper():
        return "Any"

    return type_name


def _py_default_str(type_name: str, default: Any) -> str | None:
    """Return a Python default value string, or None if no default."""
    if default is not None:
        if isinstance(default, str):
            return repr(default)
        if isinstance(default, bool):
            return "True" if default else "False"
        return str(default)

    lower = type_name.lower()
    if lower in _PY_DEFAULTS:
        return _PY_DEFAULTS[lower]

    if lower.startswith("list"):
        return "dataclasses.field(default_factory=list)"
    if lower.startswith("dict"):
        return "dataclasses.field(default_factory=dict)"
    if lower.startswith("optional") or "| none" in lower:
        return "None"

    return None


# ── V1 schema-aware helpers ──────────────────────────────────────────────────

_KIND_TO_PY: dict[str, str] = {
    "string": "str",
    "int": "int",
    "float": "float",
    "bool": "bool",
    "bytes": "bytes",
}


def _py_type_from_field(f: dict[str, Any], known_types: dict[str, str]) -> str:
    """Derive Python type annotation from a v1 schema field dict.

    Falls back to the legacy _py_type_str for unversioned fields.
    """
    kind = f.get("kind")
    if kind is None:
        return _field_py_type(f, known_types)

    nullable = f.get("nullable", False)
    base: str

    if kind in _KIND_TO_PY:
        base = _KIND_TO_PY[kind]
    elif kind == "list":
        item_kind = f.get("item_kind", "string")
        if item_kind == "ref":
            item_name = f.get("item_ref", "Any")
            item_py = known_types.get(item_name, item_name)
        elif item_kind in _KIND_TO_PY:
            item_py = _KIND_TO_PY[item_kind]
        else:
            item_py = "Any"
        base = f"list[{item_py}]"
    elif kind == "map":
        key_py = _KIND_TO_PY.get(f.get("key_kind", "string"), "str")
        val_kind = f.get("value_kind", "string")
        if val_kind == "ref":
            val_py = known_types.get(f.get("value_ref", "Any"), f.get("value_ref", "Any"))
        elif val_kind in _KIND_TO_PY:
            val_py = _KIND_TO_PY[val_kind]
        else:
            val_py = "Any"
        base = f"dict[{key_py}, {val_py}]"
    elif kind == "ref":
        ref_name = f.get("ref_name", "Any")
        base = known_types.get(ref_name, ref_name)
    elif kind == "enum":
        base = "str"
    else:
        base = "Any"

    if nullable:
        return f"Optional[{base}]"
    return base


def _py_default_from_field(f: dict[str, Any]) -> str | None:
    """Derive Python default expression from a v1 schema field dict.

    Falls back to legacy _py_default_str for unversioned fields.
    """
    dk = f.get("default_kind")
    if dk is None:
        return _py_default_str(f.get("type", "str"), f.get("default"))

    if dk == "value":
        dv = f.get("default_value")
        if isinstance(dv, str):
            return repr(dv)
        if isinstance(dv, bool):
            return "True" if dv else "False"
        if dv is not None:
            return str(dv)
        return "None"
    if dk == "empty_list":
        return "dataclasses.field(default_factory=list)"
    if dk == "empty_map":
        return "dataclasses.field(default_factory=dict)"
    if dk == "null":
        return "None"
    if dk == "none":
        return None
    return None


def _field_py_type(field: dict[str, Any], known_types: dict[str, str]) -> str:
    raw_type = field.get("type", "str")
    if str(raw_type).lower() == "optional":
        hinted = _OPTIONAL_FIELD_HINTS.get(field.get("name", ""))
        if hinted:
            return hinted
    elem_type_name = field.get("element_type", "")
    if elem_type_name and str(raw_type).lower().startswith("list"):
        elem_cls = known_types.get(elem_type_name, elem_type_name)
        return f"list[{elem_cls}]"
    return _py_type_str(raw_type, known_types)


# ── Type collection ──────────────────────────────────────────────────────────


class _TypeRecord:
    """Collected type info from manifests."""

    def __init__(self, wire_tag: str, display_name: str, fields: list[dict[str, Any]]):
        self.wire_tag = wire_tag
        self.display_name = display_name
        self.fields = fields
        self.services: set[str] = set()  # services that reference this type
        self.is_request_response = False  # direct method param, not just nested


def collect_types(
    manifests: dict[str, dict[str, Any]],
) -> dict[str, _TypeRecord]:
    """Walk all manifests and collect every referenced type by wire_tag.

    Args:
        manifests: Map of service_name -> manifest dict.

    Returns:
        Map of wire_tag -> _TypeRecord.
    """
    types: dict[str, _TypeRecord] = {}

    def _ensure_type(wire_tag: str, display_name: str, fields: list[dict], service: str) -> None:
        if not wire_tag and not display_name:
            return
        # Use wire_tag as key if available, otherwise display_name
        key = wire_tag or display_name
        if key not in types:
            types[key] = _TypeRecord(wire_tag, display_name, fields)
        types[key].services.add(service)

    def _collect_element_types(fields: list[dict], service: str) -> None:
        for f in fields:
            # V1 schema: item_wire_tag / item_ref
            elem_tag = f.get("item_wire_tag", "") or f.get("element_wire_tag", "")
            elem_name = f.get("item_ref", "") or f.get("element_type", "")
            elem_fields = f.get("element_fields", [])
            if elem_tag:
                _ensure_type(elem_tag, elem_name, elem_fields, service)
            # V1 schema: ref fields
            if f.get("kind") == "ref":
                ref_tag = f.get("wire_tag", "")
                ref_name = f.get("ref_name", "")
                if ref_tag:
                    _ensure_type(ref_tag, ref_name, [], service)
            field_type = f.get("type", "")
            if isinstance(field_type, str) and field_type in _KNOWN_WIRE_TYPES:
                wire_tag, nested_fields = _KNOWN_WIRE_TYPES[field_type]
                _ensure_type(wire_tag, field_type, nested_fields, service)

    for svc_name, manifest in manifests.items():
        for method in manifest.get("methods", []):
            # Request type
            req_tag = method.get("request_wire_tag", "")
            req_name = method.get("request_type", "")
            req_fields = method.get("fields", [])
            if req_tag or req_name:
                _ensure_type(req_tag, req_name, req_fields, svc_name)
                key = req_tag or req_name
                types[key].is_request_response = True
                _collect_element_types(req_fields, svc_name)

            # Response type
            resp_tag = method.get("response_wire_tag", "")
            resp_name = method.get("response_type", "")
            resp_fields = method.get("response_fields", [])
            if resp_tag or resp_name:
                _ensure_type(resp_tag, resp_name, resp_fields, svc_name)
                key = resp_tag or resp_name
                types[key].is_request_response = True
                _collect_element_types(resp_fields, svc_name)

    return types


def classify_types(
    types: dict[str, _TypeRecord],
) -> tuple[dict[str, list[_TypeRecord]], list[_TypeRecord]]:
    """Classify types as service-scoped or shared.

    Returns:
        (service_types, shared_types) where:
        - service_types: map of service_name -> list of types scoped to that service
        - shared_types: list of types used across multiple services
    """
    service_types: dict[str, list[_TypeRecord]] = {}
    shared_types: list[_TypeRecord] = []

    for rec in types.values():
        if len(rec.services) == 1:
            svc = next(iter(rec.services))
            service_types.setdefault(svc, []).append(rec)
        else:
            shared_types.append(rec)

    return service_types, shared_types


# ── Python code generation ───────────────────────────────────────────────────


def _gen_header(source: str, contract_id: str) -> str:
    ts = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    return (
        f'"""\nAuto-generated by: aster contract gen {source}\n'
        f"Contract ID: {contract_id}\n"
        f"Generated at: {ts}\n"
        f'DO NOT EDIT -- regenerate with: aster contract gen {source}\n"""\n'
    )


def _gen_type_class(rec: _TypeRecord, known_types: dict[str, str]) -> str:
    """Generate a Python dataclass for a type record."""
    lines = []
    if rec.wire_tag:
        lines.append(f'@wire_type("{rec.wire_tag}")')
    lines.append("@dataclasses.dataclass")
    lines.append(f"class {rec.display_name}:")

    if not rec.fields:
        lines.append("    pass")
        return "\n".join(lines)

    for f in rec.fields:
        fname = f["name"]
        ftype_str = _py_type_from_field(f, known_types)
        default = _py_default_from_field(f)

        if default is not None:
            lines.append(f"    {fname}: {ftype_str} = {default}")
        else:
            lines.append(f"    {fname}: {ftype_str} = None")

    return "\n".join(lines)


def _gen_service_client(
    svc_name: str,
    manifest: dict[str, Any],
    known_types: dict[str, str],
    all_types: dict[str, _TypeRecord] | None = None,
) -> str:
    """Generate a Python ServiceClient subclass."""
    contract_id = manifest.get("contract_id", "")
    version = manifest.get("version", 1)
    cls_name = f"{svc_name}Client"

    methods = manifest.get("methods", [])

    lines = []
    lines.append(f"class {cls_name}(ServiceClient):")
    lines.append(f'    """Typed client for {svc_name} v{version}.')
    lines.append(f"")
    lines.append(f"    Usage::")
    lines.append(f"")
    lines.append(f"        client = AsterClient(address='aster1...')")
    lines.append(f"        await client.connect()")
    lines.append(f"        svc = await {cls_name}.from_connection(client)")
    lines.append(f'    """')
    lines.append("")
    lines.append(f'    _service_name = "{svc_name}"')
    lines.append(f"    _service_version = {version}")
    lines.append(f'    _contract_id = "{contract_id}"')

    # Collect all type classes referenced by this service's methods
    # (for Fory codec registration in from_connection).
    # Includes direct request/response types AND nested element types.
    type_refs: list[str] = []
    # Only include types that have wire_tags (Fory XLANG requires them)
    tagged_types = {rec.display_name for rec in (all_types or {}).values() if rec.wire_tag}
    def _add_ref(name: str) -> None:
        cls_ref = known_types.get(name, name)
        if cls_ref and cls_ref not in ("None", "Any", "") and cls_ref not in type_refs and cls_ref in tagged_types:
            type_refs.append(cls_ref)
    for method in methods:
        for key in ("request_type", "response_type"):
            _add_ref(method.get(key, ""))
        for field_list_key in ("fields", "response_fields"):
            for f in method.get(field_list_key, []):
                elem_type = f.get("element_type", "")
                if elem_type:
                    _add_ref(elem_type)
                field_type = f.get("type", "")
                if isinstance(field_type, str) and field_type in known_types:
                    _add_ref(field_type)
    lines.append(f"    _wire_types: list[type] = [{', '.join(type_refs)}]")
    lines.append("")

    # Generate MethodInfo class variables for each method
    for method in methods:
        mname = method["name"]
        pattern = method.get("pattern", "unary")
        req_cls = known_types.get(method.get("request_type", ""), "None")
        resp_cls = known_types.get(method.get("response_type", ""), "None")
        timeout_val = method.get("timeout")
        idempotent = method.get("idempotent", False)
        lines.append(f"    _mi_{mname} = MethodInfo(")
        lines.append(f'        name="{mname}",')
        lines.append(f'        pattern="{pattern}",')
        lines.append(f"        request_type={req_cls},")
        lines.append(f"        response_type={resp_cls},")
        lines.append(f"        timeout={timeout_val},")
        lines.append(f"        idempotent={idempotent},")
        lines.append(f"    )")

    # Generate from_connection classmethod
    lines.append("")
    lines.append(f"    @classmethod")
    lines.append(f"    async def from_connection(cls, aster_client: AsterClient) -> {cls_name}:")
    lines.append(f'        """Create a {cls_name} from a connected AsterClient."""')
    lines.append(f"        from aster.codec import ForyCodec")
    lines.append(f"        from aster.service import ServiceInfo")
    lines.append(f"        from aster.transport.iroh import IrohTransport")
    lines.append(f"        summary = None")
    lines.append(f"        for s in aster_client._services:")
    lines.append(f'            if s.name == "{svc_name}":')
    lines.append(f"                summary = s")
    lines.append(f"                break")
    lines.append(f"        if summary is None:")
    lines.append(f'            raise RuntimeError("{svc_name} not found on this connection")')
    lines.append(f'        rpc_addr = summary.channels.get("rpc", "")')
    lines.append(f"        if not rpc_addr:")
    lines.append(f'            raise RuntimeError("{svc_name} has no rpc channel")')
    lines.append(f"        conn = await aster_client._rpc_conn_for(rpc_addr)")
    lines.append(f"        modes = list(getattr(summary, 'serialization_modes', None) or [])")
    lines.append(f"        if modes and 'xlang' not in modes and 'json' in modes:")
    lines.append(f"            from aster.json_codec import JsonProxyCodec")
    lines.append(f"            codec = JsonProxyCodec()")
    lines.append(f"        else:")
    lines.append(f"            codec = ForyCodec(types=cls._wire_types)")
    lines.append(f"        transport = IrohTransport(conn, codec=codec)")

    # Build methods dict from the class-level MethodInfo objects
    mi_names = [m["name"] for m in methods]
    mi_dict = ", ".join(f'"{n}": cls._mi_{n}' for n in mi_names)
    lines.append(f"        info = ServiceInfo(")
    lines.append(f'            name="{svc_name}",')
    lines.append(f"            version={version},")
    lines.append(f"            methods={{{mi_dict}}},")
    lines.append(f"        )")
    lines.append(f"        return cls(transport, info, codec)")

    # Generate method stubs
    for method in methods:
        mname = method["name"]
        pattern = method.get("pattern", "unary")
        req_cls = known_types.get(method.get("request_type", ""), "Any")
        resp_cls = known_types.get(method.get("response_type", ""), "Any")

        lines.append("")

        if pattern == "unary":
            lines.append(f"    async def {mname}(")
            lines.append(f"        self, request: {req_cls}, *, timeout: float | None = None")
            lines.append(f"    ) -> {resp_cls}:")
            lines.append(f"        return await self._call_unary(")
            lines.append(f"            method_info=self._mi_{mname},")
            lines.append(f"            request=request,")
            lines.append(f"            timeout=timeout,")
            lines.append(f"        )")

        elif pattern == "server_stream":
            lines.append(f"    def {mname}(")
            lines.append(f"        self, request: {req_cls}, *, timeout: float | None = None")
            lines.append(f"    ) -> AsyncIterator[{resp_cls}]:")
            lines.append(f"        return self._call_server_stream(")
            lines.append(f"            method_info=self._mi_{mname},")
            lines.append(f"            request=request,")
            lines.append(f"            timeout=timeout,")
            lines.append(f"        )")

        elif pattern == "client_stream":
            lines.append(f"    async def {mname}(")
            lines.append(f"        self, requests: AsyncIterator[{req_cls}], *, timeout: float | None = None")
            lines.append(f"    ) -> {resp_cls}:")
            lines.append(f"        return await self._call_client_stream(")
            lines.append(f"            method_info=self._mi_{mname},")
            lines.append(f"            requests=requests,")
            lines.append(f"            timeout=timeout,")
            lines.append(f"        )")

        elif pattern == "bidi_stream":
            lines.append(f"    def {mname}(")
            lines.append(f"        self, *, timeout: float | None = None")
            lines.append(f"    ) -> BidiChannel:")
            lines.append(f"        return self._call_bidi_stream(")
            lines.append(f"            method_info=self._mi_{mname},")
            lines.append(f"            timeout=timeout,")
            lines.append(f"        )")

    return "\n".join(lines)


# ── Main generation entry point ──────────────────────────────────────────────


def generate_python_clients(
    manifests: dict[str, dict[str, Any]],
    out_dir: str,
    namespace: str,
    source: str = "",
) -> list[str]:
    """Generate Python client files from manifests.

    Args:
        manifests: Map of service_name -> manifest dict (from ContractManifest).
        out_dir: Root output directory.
        namespace: Handle or endpoint_id prefix for the package namespace.
        source: Source description for the header comment.

    Returns:
        List of generated file paths.
    """
    generated: list[str] = []

    # Step 2-3: Collect and classify types
    all_types = collect_types(manifests)
    service_types, shared_types = classify_types(all_types)

    # Build known_types map: display_name -> display_name (identity for now)
    known_types: dict[str, str] = {}
    for rec in all_types.values():
        known_types[rec.display_name] = rec.display_name

    # Create directory structure
    ns_dir = os.path.join(out_dir, _to_snake_case(namespace))
    types_dir = os.path.join(ns_dir, "types")
    services_dir = os.path.join(ns_dir, "services")
    os.makedirs(types_dir, exist_ok=True)
    os.makedirs(services_dir, exist_ok=True)

    # Contract ID for header (use first manifest's)
    first_manifest = next(iter(manifests.values()), {})
    contract_id = first_manifest.get("contract_id", "")

    # Common imports for type files
    _type_imports = "\nimport dataclasses\nfrom typing import Any, Optional\n\nfrom aster.codec import wire_type\n"

    # Step 4a: Generate shared type files
    for rec in sorted(shared_types, key=lambda r: r.display_name):
        fname = _to_snake_case(rec.display_name) + ".py"
        fpath = os.path.join(types_dir, fname)
        content = _gen_header(source, contract_id)
        content += _type_imports + "\n\n"
        content += _gen_type_class(rec, known_types) + "\n"
        _write_file(fpath, content)
        generated.append(fpath)

    # Step 4b: Generate service-scoped type files
    for svc_name, manifest in sorted(manifests.items()):
        svc_contract_id = manifest.get("contract_id", contract_id)
        fname = _to_snake_case(svc_name) + "_v" + str(manifest.get("version", 1)) + ".py"
        fpath = os.path.join(types_dir, fname)

        svc_recs = service_types.get(svc_name, [])
        if not svc_recs:
            continue

        content = _gen_header(source, svc_contract_id)
        content += _type_imports

        # Import shared types referenced by this service's types
        shared_imports = _collect_shared_imports(svc_recs, shared_types, known_types)
        for imp_name, imp_file in sorted(shared_imports):
            content += f"from .{imp_file} import {imp_name}\n"

        for rec in sorted(svc_recs, key=lambda r: r.display_name):
            content += "\n\n" + _gen_type_class(rec, known_types)

        content += "\n"
        _write_file(fpath, content)
        generated.append(fpath)

    # Step 5: Generate service client files
    for svc_name, manifest in sorted(manifests.items()):
        svc_contract_id = manifest.get("contract_id", contract_id)
        version = manifest.get("version", 1)
        fname = _to_snake_case(svc_name) + "_v" + str(version) + ".py"
        fpath = os.path.join(services_dir, fname)

        content = _gen_header(source, svc_contract_id)
        content += "\nfrom __future__ import annotations\n\n"
        content += "from collections.abc import AsyncIterator\n"
        content += "from typing import TYPE_CHECKING\n\n"
        content += "from aster.client import ServiceClient\n"
        content += "from aster.service import MethodInfo\n"
        content += "from aster.transport.base import BidiChannel\n\n"
        content += "if TYPE_CHECKING:\n"
        content += "    from aster.runtime import AsterClient\n"

        # Import all types used by this service's methods
        type_imports = _collect_service_type_imports(svc_name, manifest, all_types, shared_types)
        for imp_name, imp_module in sorted(type_imports):
            content += f"from ..types.{imp_module} import {imp_name}\n"

        content += "\n\n"
        content += _gen_service_client(svc_name, manifest, known_types, all_types)
        content += "\n"

        _write_file(fpath, content)
        generated.append(fpath)

    # Step 6: Generate __init__.py files
    _write_init(ns_dir, manifests, namespace, generated)
    _write_init(types_dir, {}, "", [])
    _write_init(services_dir, {}, "", [])

    return generated


def _collect_shared_imports(
    svc_recs: list[_TypeRecord],
    shared_types: list[_TypeRecord],
    known_types: dict[str, str],
) -> list[tuple[str, str]]:
    """Find shared types that this service's types reference."""
    shared_names = {r.display_name for r in shared_types}
    shared_tags = {r.wire_tag: r for r in shared_types}
    imports: list[tuple[str, str]] = []  # (class_name, module_name)
    seen: set[str] = set()

    for rec in svc_recs:
        for f in rec.fields:
            # Check element types
            elem_tag = f.get("element_wire_tag", "")
            if elem_tag and elem_tag in shared_tags:
                name = shared_tags[elem_tag].display_name
                if name not in seen:
                    imports.append((name, _to_snake_case(name)))
                    seen.add(name)
            # Check field type names
            type_name = f.get("type", "")
            if type_name in shared_names and type_name not in seen:
                imports.append((type_name, _to_snake_case(type_name)))
                seen.add(type_name)
            m = re.match(r"list\[(.+)\]", type_name, re.IGNORECASE)
            if m:
                inner = m.group(1)
                if inner in shared_names and inner not in seen:
                    imports.append((inner, _to_snake_case(inner)))
                    seen.add(inner)

    return imports


def _collect_service_type_imports(
    svc_name: str,
    manifest: dict[str, Any],
    all_types: dict[str, _TypeRecord],
    shared_types: list[_TypeRecord],
) -> list[tuple[str, str]]:
    """Collect all type imports needed by a service client file."""
    imports: list[tuple[str, str]] = []  # (class_name, module_name)
    seen: set[str] = set()
    version = manifest.get("version", 1)
    svc_types_module = _to_snake_case(svc_name) + f"_v{version}"
    shared_names = {r.display_name: r for r in shared_types}
    known_display_names = {r.display_name for r in all_types.values()}

    def _add_import(display_name: str) -> None:
        if not display_name or display_name in seen or display_name in ("None", "Any"):
            return
        seen.add(display_name)
        if display_name in shared_names:
            imports.append((display_name, _to_snake_case(display_name)))
        else:
            imports.append((display_name, svc_types_module))

    for method in manifest.get("methods", []):
        # Direct request/response types
        for name_key in ("request_type", "response_type"):
            _add_import(method.get(name_key, ""))
        # Element types from list fields
        for field_list_key in ("fields", "response_fields"):
            for f in method.get(field_list_key, []):
                _add_import(f.get("element_type", ""))
                field_type = f.get("type", "")
                if isinstance(field_type, str):
                    cleaned = field_type.removeprefix("Optional[").removesuffix("]")
                    if cleaned in known_display_names:
                        _add_import(cleaned)

    return imports


def _write_init(dir_path: str, manifests: dict, namespace: str, generated: list[str]) -> None:
    """Write an __init__.py file."""
    fpath = os.path.join(dir_path, "__init__.py")
    _write_file(fpath, "")


def _write_file(path: str, content: str) -> None:
    """Write content to a file, creating parent dirs."""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(content)


# ── Usage snippet ────────────────────────────────────────────────────────────


def format_usage_snippet(
    out_dir: str,
    namespace: str,
    manifests: dict[str, dict[str, Any]],
    address: str = "",
) -> str:
    """Format a usage snippet to print after generation."""
    ns_snake = _to_snake_case(namespace)
    lines = [f"\nGenerated clients -> {out_dir}/{ns_snake}/\n"]
    lines.append("Usage:\n")
    lines.append("  from aster import AsterClient")

    # Pick first service as example
    svc_name = next(iter(manifests), "MyService")
    version = manifests.get(svc_name, {}).get("version", 1)
    svc_snake = _to_snake_case(svc_name)

    lines.append(
        f"  from {ns_snake}.services.{svc_snake}_v{version} import {svc_name}Client"
    )

    # Find a unary method for the example
    example_method = None
    example_req = None
    for m in manifests.get(svc_name, {}).get("methods", []):
        if m.get("pattern", "unary") == "unary":
            example_method = m["name"]
            example_req = m.get("request_type", "Request")
            break

    if example_req:
        lines.append(
            f"  from {ns_snake}.types.{svc_snake}_v{version} import {example_req}"
        )

    lines.append("")
    if address:
        lines.append(f'  client = AsterClient(address="{address}")')
    else:
        lines.append('  client = AsterClient(address="aster1...")')
    lines.append("  await client.connect()")
    lines.append(f"  svc = await {svc_name}Client.from_connection(client)")

    if example_method and example_req:
        lines.append(f"  result = await svc.{example_method}({example_req}())")

    return "\n".join(lines)
