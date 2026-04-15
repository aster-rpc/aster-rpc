"""
aster_cli.mcp.schema -- Convert Aster manifest schemas to MCP/JSON Schema.

Maps FieldSchema (from contract manifests) to JSON Schema properties,
and method descriptors to MCP Tool definitions. Pure functions, no I/O.
"""

from __future__ import annotations

import re
from typing import Any


# ── Type mapping ──────────────────────────────────────────────────────────────

# Aster type name → JSON Schema type
_PRIMITIVE_MAP: dict[str, dict[str, Any]] = {
    "str": {"type": "string"},
    "string": {"type": "string"},
    "int": {"type": "integer"},
    "int32": {"type": "integer"},
    "int64": {"type": "integer"},
    "float": {"type": "number"},
    "float32": {"type": "number"},
    "float64": {"type": "number"},
    "double": {"type": "number"},
    "bool": {"type": "boolean"},
    "boolean": {"type": "boolean"},
    "bytes": {"type": "string", "contentEncoding": "base64"},
}

# Regex for generic container types
_LIST_RE = re.compile(r"^[Ll]ist\[(.+)\]$")
_DICT_RE = re.compile(r"^[Dd]ict\[str,\s*(.+)\]$")
_OPTIONAL_RE = re.compile(r"^[Oo]ptional\[(.+)\]$")


def aster_type_to_json_schema(type_name: str) -> dict[str, Any]:
    """Convert an Aster type name to a JSON Schema type definition.

    Handles primitives, list[X], dict[str, X], Optional[X], and falls back
    to ``{"type": "object"}`` for unknown/complex types.

    Args:
        type_name: Aster type name (e.g., "str", "list[int]", "MyDataclass").

    Returns:
        JSON Schema dict (e.g., {"type": "string"}).
    """
    if not type_name:
        return {"type": "string"}

    # Check primitives
    lower = type_name.lower()
    if lower in _PRIMITIVE_MAP:
        return dict(_PRIMITIVE_MAP[lower])

    # Check Optional[X]
    m = _OPTIONAL_RE.match(type_name)
    if m:
        inner = aster_type_to_json_schema(m.group(1))
        return {**inner, "nullable": True}

    # Check list[X]
    m = _LIST_RE.match(type_name)
    if m:
        inner = aster_type_to_json_schema(m.group(1))
        return {"type": "array", "items": inner}

    # Check dict[str, X]
    m = _DICT_RE.match(type_name)
    if m:
        inner = aster_type_to_json_schema(m.group(1))
        return {"type": "object", "additionalProperties": inner}

    # Unknown / complex type → object
    return {"type": "object", "description": f"Aster type: {type_name}"}


# ── Field → JSON Schema property ─────────────────────────────────────────────


def field_to_json_schema(field: dict[str, Any]) -> dict[str, Any]:
    """Convert an Aster FieldSchema dict to a JSON Schema property.

    Propagates:
        - ``description`` into the schema's ``description`` key.
        - ``tags`` into an ``x-aster-tags`` extension keyword and appended
          as ``[tag1, tag2]`` suffix to ``description`` (so LLMs see it
          even if they ignore extensions).

    Args:
        field: Dict with keys: name, type, required, default, description, tags.

    Returns:
        JSON Schema property dict.
    """
    schema = aster_type_to_json_schema(field.get("type", "str"))

    description = field.get("description") or ""
    tags = list(field.get("tags", []) or [])

    if tags:
        suffix = f"[{', '.join(tags)}]"
        description = f"{description} {suffix}".strip()

    if description:
        schema["description"] = description

    if tags:
        schema["x-aster-tags"] = tags

    if field.get("default") is not None:
        schema["default"] = field["default"]

    return schema


# ── Method → MCP Tool ─────────────────────────────────────────────────────────


def method_to_tool_definition(
    service_name: str,
    method: dict[str, Any],
) -> dict[str, Any]:
    """Convert an Aster method descriptor to an MCP tool definition dict.

    Args:
        service_name: The service name (e.g., "HelloService").
        method: Method dict from ContractManifest.methods.

    Returns:
        Dict compatible with mcp.types.Tool construction.
    """
    method_name = method.get("name", "unknown")
    pattern = method.get("pattern", "unary")
    req_type = method.get("request_type", "")
    resp_type = method.get("response_type", "")

    # Build inputSchema from fields
    fields = method.get("fields", [])
    properties: dict[str, Any] = {}
    required: list[str] = []

    for f in fields:
        properties[f["name"]] = field_to_json_schema(f)
        if f.get("required", True):
            required.append(f["name"])

    # Add meta-parameters for streaming patterns
    if pattern == "server_stream":
        properties["aster_max_items"] = {
            "type": "integer",
            "description": "Maximum number of stream items to collect (default: 100)",
            "default": 100,
        }
        properties["aster_timeout"] = {
            "type": "number",
            "description": "Timeout in seconds for stream collection (default: 30)",
            "default": 30.0,
        }
    elif pattern == "client_stream":
        properties["aster_items"] = {
            "type": "array",
            "description": "List of items to send as a client stream",
            "items": {"type": "object"},
        }
        required.append("aster_items")

    input_schema: dict[str, Any] = {
        "type": "object",
        "properties": properties,
    }
    if required:
        input_schema["required"] = required

    # Build description. For Mode 2 (inline) methods show the signature
    # the producer wrote rather than the opaque synthesized request class
    # name, so LLM agents generate sensible tool calls.
    sig_parts = []
    if method.get("request_style") == "inline":
        inline = method.get("inline_params", []) or []
        if inline:
            pretty = ", ".join(
                f"{p.get('name', 'arg')}: {p.get('kind') or p.get('type') or '?'}"
                for p in inline
            )
            sig_parts.append(f"Request: ({pretty})")
        else:
            sig_parts.append("Request: ()")
    elif req_type:
        sig_parts.append(f"Request: {req_type}")
    if resp_type:
        sig_parts.append(f"Response: {resp_type}")
    sig = ". ".join(sig_parts) + "." if sig_parts else ""

    timeout_note = ""
    if method.get("timeout"):
        timeout_note = f" Timeout: {method['timeout']}s."

    synthetic = (
        f"{pattern} RPC method on {service_name}. {sig}{timeout_note}"
    ).strip()

    # Prefer author-supplied description. Still append the synthetic
    # signature + timeout note in parentheses so LLMs get both the semantic
    # intent (author) and the concrete shape (synthetic).
    author_desc = (method.get("description") or "").strip()
    if author_desc:
        if sig or timeout_note:
            description = f"{author_desc} ({synthetic})"
        else:
            description = author_desc
    else:
        description = synthetic

    # Append tags as a bracketed suffix and as an x-aster-tags extension.
    # Also flag deprecated methods in the description prefix.
    tags = list(method.get("tags", []) or [])
    if method.get("deprecated"):
        description = f"[DEPRECATED] {description}"
    if tags:
        description = f"{description} [tags: {', '.join(tags)}]"

    result: dict[str, Any] = {
        "name": f"{service_name}.{method_name}",
        "description": description,
        "inputSchema": input_schema,
    }
    if tags:
        result["x-aster-tags"] = tags
    if method.get("deprecated"):
        result["x-aster-deprecated"] = True

    return result


def service_to_tool_definitions(service: dict[str, Any]) -> list[dict[str, Any]]:
    """Convert all methods in a service to MCP tool definitions.

    Args:
        service: Service dict from PeerConnection.list_services(), which
                 includes a "methods" key with method descriptors.

    Returns:
        List of tool definition dicts.
    """
    service_name = service.get("name", "UnknownService")
    methods = service.get("methods", [])
    tools = []

    for method in methods:
        pattern = method.get("pattern", "unary")
        # Skip bidi_stream in Phase 1
        if pattern == "bidi_stream":
            continue
        tools.append(method_to_tool_definition(service_name, method))

    return tools
