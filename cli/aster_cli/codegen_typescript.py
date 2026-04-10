"""
aster_cli.codegen_typescript -- Generate typed TypeScript client libraries from
Aster manifests.

Mirrors :mod:`aster_cli.codegen` (Python) but emits TypeScript classes that
consume the ``@aster-rpc/aster`` runtime. The generated code has no decorator
runtime requirements -- type classes are plain JS classes with field defaults
and a ``Partial`` constructor, and service clients are explicit method classes
that delegate to ``transport.unary`` / ``transport.serverStream`` /
``transport.clientStream`` / ``transport.bidiStream``.

The dual-schema handling matches the Python generator: the manifest's v1
schema (``kind`` / ``item_ref`` / ``ref_name``) is preferred when present,
and the legacy ``type`` string is used as a fallback for manifests produced
by bindings that haven't migrated to v1 (e.g. the current TS binding).
"""

from __future__ import annotations

import os
import re
from datetime import datetime, timezone
from typing import Any

from aster_cli.codegen import (
    _TypeRecord,
    _to_snake_case,
    classify_types,
    collect_types,
)


# ── Naming ───────────────────────────────────────────────────────────────────


def _to_kebab_case(name: str) -> str:
    """Convert CamelCase or snake_case to kebab-case for filenames."""
    return _to_snake_case(name).replace("_", "-")


# ── Type mapping ─────────────────────────────────────────────────────────────

# Maps both legacy (``type`` string) and v1 (``kind``) names to TS primitives.
_TS_PRIMITIVE: dict[str, str] = {
    "str": "string",
    "string": "string",
    "int": "number",
    "int32": "number",
    "int64": "number",
    "float": "number",
    "float32": "number",
    "float64": "number",
    "double": "number",
    "bool": "boolean",
    "boolean": "boolean",
    "bytes": "Uint8Array",
}


_TS_DEFAULT: dict[str, str] = {
    "string": '""',
    "number": "0",
    "boolean": "false",
    "Uint8Array": "new Uint8Array()",
}


def _ts_primitive_default(ts_type: str) -> str | None:
    """Return the default literal for a TS primitive, or None if not primitive."""
    return _TS_DEFAULT.get(ts_type)


def _ts_type_from_field(
    f: dict[str, Any],
    known_types: dict[str, str],
) -> tuple[str, str]:
    """Derive (TS type annotation, default expression) from a manifest field.

    Handles both the v1 ``kind``-based schema and the legacy ``type`` string
    schema. ``known_types`` maps display names to themselves -- entries are
    treated as TS class references (and assumed to live in the same emitted
    namespace).
    """
    kind = f.get("kind")

    # ── v1 schema (Python-binding manifests) ─────────────────────────────
    if kind is not None:
        nullable = bool(f.get("nullable", False))
        ts_type, default = _v1_type(f, kind, known_types)
        if nullable:
            ts_type = f"{ts_type} | null"
            # Prefer the field's explicit default, otherwise null for nullables
            if f.get("default_kind") == "none":
                default = "null"
        return ts_type, default

    # ── Legacy schema (TS-binding manifests) ─────────────────────────────
    return _legacy_type(f, known_types)


def _v1_type(
    f: dict[str, Any],
    kind: str,
    known_types: dict[str, str],
) -> tuple[str, str]:
    """Resolve a v1 schema field to (ts_type, default)."""
    ts_type: str
    default: str

    if kind in _TS_PRIMITIVE:
        ts_type = _TS_PRIMITIVE[kind]
        default = _v1_default(f, ts_type, ref_default=None)
    elif kind == "list":
        item_kind = f.get("item_kind", "string")
        if item_kind == "ref":
            inner_name = f.get("item_ref", "unknown")
            inner = known_types.get(inner_name, "unknown")
        else:
            inner = _TS_PRIMITIVE.get(item_kind, "unknown")
        ts_type = f"{inner}[]"
        default = "[]"
    elif kind == "map":
        key_kind = f.get("key_kind", "string")
        # JS object keys are always strings; we coerce other key kinds to string
        key_ts = "string" if key_kind != "string" and key_kind in _TS_PRIMITIVE else "string"
        val_kind = f.get("value_kind", "string")
        if val_kind == "ref":
            val_name = f.get("value_ref", "unknown")
            val_ts = known_types.get(val_name, "unknown")
        else:
            val_ts = _TS_PRIMITIVE.get(val_kind, "unknown")
        ts_type = f"Record<{key_ts}, {val_ts}>"
        default = "{}"
    elif kind == "ref":
        ref_name = f.get("ref_name", "unknown")
        ts_type = known_types.get(ref_name, "unknown")
        # Refs to unknown classes can't be cheaply default-constructed; use null
        default = "null as any"
    elif kind == "enum":
        # Aster enums travel as strings on the wire
        ts_type = "string"
        default = '""'
    else:
        ts_type = "unknown"
        default = "undefined as any"

    # Honour an explicit default if present
    explicit = _v1_default(f, ts_type, ref_default=default)
    return ts_type, explicit


def _v1_default(
    f: dict[str, Any],
    ts_type: str,
    ref_default: str | None,
) -> str:
    """Resolve the default expression for a v1 field."""
    dk = f.get("default_kind")
    if dk == "value":
        dv = f.get("default_value")
        return _literal(dv)
    if dk == "empty_list":
        return "[]"
    if dk == "empty_map":
        return "{}"
    if dk == "null":
        return "null as any"
    # ``none`` or missing -> fall back to type-derived default
    if ref_default is not None:
        return ref_default
    return _ts_primitive_default(ts_type) or "undefined as any"


def _legacy_type(
    f: dict[str, Any],
    known_types: dict[str, str],
) -> tuple[str, str]:
    """Resolve a legacy ``type`` string field to (ts_type, default).

    The TS-binding manifest emitter uses this schema today: it has only
    a coarse type label (str/int/float/bool/list/dict) and no element
    metadata, so list/dict element types are emitted as ``unknown``.
    """
    raw = str(f.get("type", "str")).lower()

    # Pre-existing default value if any
    explicit_default = f.get("default")

    if raw in _TS_PRIMITIVE:
        ts = _TS_PRIMITIVE[raw]
        if explicit_default is not None:
            return ts, _literal(explicit_default)
        return ts, _ts_primitive_default(ts) or '""'

    # list[X] from Python-style legacy
    m = re.match(r"list\[(.+)\]$", raw)
    if m:
        inner_raw = m.group(1)
        inner = _TS_PRIMITIVE.get(inner_raw, known_types.get(inner_raw, "unknown"))
        return f"{inner}[]", "[]"

    # bare ``list`` from TS-binding manifests
    if raw == "list":
        return "unknown[]", "[]"

    # dict[K, V]
    m = re.match(r"dict\[(.+),\s*(.+)\]$", raw)
    if m:
        v_raw = m.group(2)
        v_ts = _TS_PRIMITIVE.get(v_raw, known_types.get(v_raw, "unknown"))
        return f"Record<string, {v_ts}>", "{}"

    if raw == "dict":
        return "Record<string, unknown>", "{}"

    # Optional[X] -> X | null
    m = re.match(r"optional\[(.+)\]$", raw)
    if m:
        inner_ts, _ = _legacy_type({"type": m.group(1)}, known_types)
        return f"{inner_ts} | null", "null as any"
    if raw == "optional":
        return "unknown | null", "null as any"

    # Reference to a generated class
    raw_orig = str(f.get("type", "str"))
    if raw_orig in known_types:
        return raw_orig, "null as any"

    # Unknown -> bail to unknown
    return "unknown", "undefined as any"


def _literal(value: Any) -> str:
    """Render a manifest default value as a TS literal expression."""
    if value is None:
        return "null as any"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, str):
        # Use JSON string encoding for safety
        import json as _json
        return _json.dumps(value)
    if isinstance(value, list):
        return "[]"
    if isinstance(value, dict):
        return "{}"
    return "undefined as any"


# ── Code generation ──────────────────────────────────────────────────────────


_GEN_HEADER = (
    "/**\n"
    " * Auto-generated by: aster contract gen-client {source} --lang typescript\n"
    " * Contract ID: {contract_id}\n"
    " * Generated at: {ts}\n"
    " * DO NOT EDIT -- regenerate with: aster contract gen-client {source} --lang typescript\n"
    " */\n"
)


def _header(source: str, contract_id: str) -> str:
    return _GEN_HEADER.format(
        source=source or "<source>",
        contract_id=contract_id or "<unknown>",
        ts=datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    )


def _gen_type_class(rec: _TypeRecord, known_types: dict[str, str]) -> str:
    """Emit a TS class for a type record.

    The class:
    - declares each field with its inferred TS type
    - assigns a default value so ``new T()`` always produces a valid instance
    - takes a ``Partial<T>`` in the constructor and copies the keys via Object.assign,
      matching the pattern used by hand-written Aster TS code
    """
    lines: list[str] = []
    lines.append(f"export class {rec.display_name} {{")
    if not rec.fields:
        lines.append(f"  constructor(_init?: Partial<{rec.display_name}>) {{}}")
        lines.append("}")
        return "\n".join(lines)

    for f in rec.fields:
        fname = f["name"]
        ts_type, default = _ts_type_from_field(f, known_types)
        lines.append(f"  {fname}: {ts_type} = {default};")
    lines.append(
        f"  constructor(init?: Partial<{rec.display_name}>) {{ "
        f"if (init) Object.assign(this, init); }}"
    )
    lines.append("}")
    return "\n".join(lines)


def _ts_request_type(method: dict[str, Any], known_types: dict[str, str]) -> str:
    name = method.get("request_type", "")
    return known_types.get(name, "unknown")


def _ts_response_type(method: dict[str, Any], known_types: dict[str, str]) -> str:
    name = method.get("response_type", "")
    return known_types.get(name, "unknown")


def _gen_service_client(
    svc_name: str,
    manifest: dict[str, Any],
    known_types: dict[str, str],
) -> str:
    """Emit a TS service client class.

    Session-scoped services and shared services need different runtime
    plumbing. Shared services can use ``transport.unary`` et al. directly
    because every call gets a fresh stream. Session services must speak the
    session protocol (StreamHeader with method="" then CALL frames over a
    single bidi stream) -- that machinery already lives in
    ``SessionProxyClient``, which the wrapper exposes via ``client.proxy()``.
    So for session services we generate a delegating shim that wraps the
    proxy and bolts typed signatures onto it.
    """
    contract_id = manifest.get("contract_id", "")
    version = manifest.get("version", 1)
    cls = f"{svc_name}Client"
    # Sort methods by name so the output is stable across producers --
    # the Python publisher sorts methods at canonicalisation time, but
    # the TS publisher emits them in source order. Sorting here means
    # both producers yield byte-identical client code (modulo the
    # manifest-richness gaps documented in the README).
    methods = sorted(manifest.get("methods", []), key=lambda m: m.get("name", ""))
    is_session = manifest.get("scoped") in ("session", "stream")

    if is_session:
        return _gen_session_client(svc_name, version, contract_id, methods, known_types, cls)
    return _gen_shared_client(svc_name, version, contract_id, methods, known_types, cls)


def _gen_shared_client(
    svc_name: str,
    version: int,
    contract_id: str,
    methods: list[dict[str, Any]],
    known_types: dict[str, str],
    cls: str,
) -> str:
    lines: list[str] = []
    lines.append(f"/** Typed client for {svc_name} v{version}. */")
    lines.append(f"export class {cls} {{")
    lines.append(f'  static readonly serviceName = "{svc_name}";')
    lines.append(f"  static readonly serviceVersion = {version};")
    lines.append(f'  static readonly contractId = "{contract_id}";')
    lines.append("")
    lines.append("  private constructor(private readonly transport: AsterTransport) {}")
    lines.append("")
    lines.append("  /**")
    lines.append(f"   * Resolve {svc_name} on a connected AsterClient and return a")
    lines.append("   * typed client wired to the existing RPC transport.")
    lines.append("   */")
    lines.append(
        f"  static async fromConnection(client: AsterClientWrapper): Promise<{cls}> {{"
    )
    lines.append(
        f'    const summary = client.services.find(s => s.name === "{svc_name}");'
    )
    lines.append("    if (!summary) {")
    lines.append(
        f'      throw new Error("{svc_name} not offered by this peer '
        f'(did you connect to the right address?)");'
    )
    lines.append("    }")
    lines.append(
        "    // Reuse the wrapper's already-connected RPC transport."
    )
    lines.append(
        "    const transport: AsterTransport | undefined ="
    )
    lines.append(
        '      (client as unknown as { transport?: AsterTransport }).transport;'
    )
    lines.append("    if (!transport) {")
    lines.append(
        '      throw new Error("AsterClient has no transport; call await client.connect() first");'
    )
    lines.append("    }")
    lines.append(f"    return new {cls}(transport);")
    lines.append("  }")
    lines.append("")
    lines.append("  /** Close the underlying transport. */")
    lines.append("  async close(): Promise<void> {")
    lines.append("    await this.transport.close();")
    lines.append("  }")

    for method in methods:
        mname = method["name"]
        pattern = method.get("pattern", "unary")
        req_ts = _ts_request_type(method, known_types)
        resp_ts = _ts_response_type(method, known_types)
        lines.append("")

        if pattern == "unary":
            lines.append(f"  async {mname}(request: {req_ts}): Promise<{resp_ts}> {{")
            lines.append(
                f"    return (await this.transport.unary("
                f'"{svc_name}", "{mname}", request)) as {resp_ts};'
            )
            lines.append("  }")

        elif pattern == "server_stream":
            lines.append(
                f"  async *{mname}(request: {req_ts}): AsyncGenerator<{resp_ts}> {{"
            )
            lines.append(
                f"    for await (const item of this.transport.serverStream("
                f'"{svc_name}", "{mname}", request)) {{'
            )
            lines.append(f"      yield item as {resp_ts};")
            lines.append("    }")
            lines.append("  }")

        elif pattern == "client_stream":
            lines.append(
                f"  async {mname}(requests: AsyncIterable<{req_ts}>): Promise<{resp_ts}> {{"
            )
            lines.append(
                f"    return (await this.transport.clientStream("
                f'"{svc_name}", "{mname}", requests)) as {resp_ts};'
            )
            lines.append("  }")

        elif pattern == "bidi_stream":
            lines.append(f"  {mname}(): BidiChannel {{")
            lines.append(
                f'    return this.transport.bidiStream("{svc_name}", "{mname}");'
            )
            lines.append("  }")

        else:
            lines.append(f"  // unknown pattern: {pattern}")

    lines.append("}")
    return "\n".join(lines)


def _gen_session_client(
    svc_name: str,
    version: int,
    contract_id: str,
    methods: list[dict[str, Any]],
    known_types: dict[str, str],
    cls: str,
) -> str:
    """Emit a typed shim around AsterClientWrapper.proxy() for session services.

    AsterClientWrapper.proxy() returns a SessionProxyClient when the service
    is session-scoped. SessionProxyClient already opens the persistent bidi
    stream, sends the session header, and routes calls via the session
    protocol -- but its method signatures are dynamically typed because it
    uses a JS Proxy with name interception. We just bolt typed wrappers
    onto it here.
    """
    lines: list[str] = []
    lines.append(f"/** Typed client for session-scoped {svc_name} v{version}. */")
    lines.append(f"export class {cls} {{")
    lines.append(f'  static readonly serviceName = "{svc_name}";')
    lines.append(f"  static readonly serviceVersion = {version};")
    lines.append(f'  static readonly contractId = "{contract_id}";')
    lines.append("")
    lines.append(
        "  // SessionProxyClient is dynamically typed (JS Proxy with method-name"
    )
    lines.append(
        "  // interception); we wrap it with typed signatures below."
    )
    lines.append("  // eslint-disable-next-line @typescript-eslint/no-explicit-any")
    lines.append("  private constructor(private readonly inner: any) {}")
    lines.append("")
    lines.append("  /**")
    lines.append(f"   * Open a session-scoped {svc_name} client. The first method")
    lines.append("   * call opens the underlying bidi stream; subsequent calls reuse")
    lines.append(f"   * it so per-{svc_name} state survives across calls.")
    lines.append("   */")
    lines.append(
        f"  static async fromConnection(client: AsterClientWrapper): Promise<{cls}> {{"
    )
    lines.append(
        f'    const summary = client.services.find(s => s.name === "{svc_name}");'
    )
    lines.append("    if (!summary) {")
    lines.append(
        f'      throw new Error("{svc_name} not offered by this peer '
        f'(did you connect to the right address?)");'
    )
    lines.append("    }")
    lines.append(f'    const inner = client.proxy("{svc_name}");')
    lines.append(f"    return new {cls}(inner);")
    lines.append("  }")
    lines.append("")
    lines.append("  /** Close this session, releasing the underlying bidi stream. */")
    lines.append("  async close(): Promise<void> {")
    lines.append("    await this.inner.close();")
    lines.append("  }")

    for method in methods:
        mname = method["name"]
        pattern = method.get("pattern", "unary")
        req_ts = _ts_request_type(method, known_types)
        resp_ts = _ts_response_type(method, known_types)
        lines.append("")

        if pattern == "unary":
            lines.append(f"  async {mname}(request: {req_ts}): Promise<{resp_ts}> {{")
            lines.append(f"    return (await this.inner.{mname}(request)) as {resp_ts};")
            lines.append("  }")
        else:
            # SessionProxyClient on the TS side currently throws UNIMPLEMENTED
            # for stream/bidi patterns; emit the typed signature anyway with a
            # clear runtime error so the codegen output is still useful.
            lines.append(
                f"  // session-scoped {pattern} is not yet supported by SessionProxyClient"
            )
            lines.append(f"  async {mname}(_request: {req_ts}): Promise<{resp_ts}> {{")
            lines.append(
                f'    throw new Error("{svc_name}.{mname}: session-scoped {pattern}'
                f' methods are not yet supported by the TypeScript runtime");'
            )
            lines.append("  }")

    lines.append("}")
    return "\n".join(lines)


# ── Public entry point ───────────────────────────────────────────────────────


def generate_typescript_clients(
    manifests: dict[str, dict[str, Any]],
    out_dir: str,
    namespace: str,
    source: str = "",
) -> list[str]:
    """Generate TypeScript client files from manifests.

    Layout (mirrors the Python output)::

        out_dir/<namespace>/
            types/
                <SharedType>.ts
                <service>_v<version>.ts
            services/
                <service>_v<version>.ts

    Files are written via :func:`_write` (creates parent dirs as needed).
    Returns the list of generated paths.
    """
    generated: list[str] = []

    all_types = collect_types(manifests)
    service_types, shared_types = classify_types(all_types)

    known_types: dict[str, str] = {
        rec.display_name: rec.display_name for rec in all_types.values()
    }

    ns_dir = os.path.join(out_dir, _to_snake_case(namespace))
    types_dir = os.path.join(ns_dir, "types")
    services_dir = os.path.join(ns_dir, "services")
    os.makedirs(types_dir, exist_ok=True)
    os.makedirs(services_dir, exist_ok=True)

    first_manifest = next(iter(manifests.values()), {})
    contract_id = first_manifest.get("contract_id", "")

    # ── Shared types: one file per type ──────────────────────────────────
    for rec in sorted(shared_types, key=lambda r: r.display_name):
        fname = _to_kebab_case(rec.display_name) + ".ts"
        fpath = os.path.join(types_dir, fname)
        body = _header(source, contract_id) + "\n"
        # Inline imports for any shared refs that this type touches
        body += _gen_type_class(rec, known_types) + "\n"
        _write(fpath, body)
        generated.append(fpath)

    # ── Service-scoped type files ────────────────────────────────────────
    for svc_name, manifest in sorted(manifests.items()):
        svc_recs = service_types.get(svc_name, [])
        if not svc_recs:
            continue
        version = manifest.get("version", 1)
        fname = f"{_to_kebab_case(svc_name)}-v{version}.ts"
        fpath = os.path.join(types_dir, fname)
        svc_contract_id = manifest.get("contract_id", contract_id)

        body = _header(source, svc_contract_id) + "\n"
        # Pull in shared types that this service's types reference. Use the
        # `.js` extension on relative imports so the output works under
        # strict Node16/NodeNext ESM resolution as well as Bun's loose mode.
        shared_imports = sorted(
            _shared_imports(svc_recs, shared_types, known_types),
            key=lambda t: t[0],
        )
        for imp_name, imp_file in shared_imports:
            body += f'import {{ {imp_name} }} from "./{imp_file}.js";\n'
        if shared_imports:
            body += "\n"
        for rec in sorted(svc_recs, key=lambda r: r.display_name):
            body += _gen_type_class(rec, known_types) + "\n\n"
        _write(fpath, body.rstrip() + "\n")
        generated.append(fpath)

    # ── Service client files ─────────────────────────────────────────────
    for svc_name, manifest in sorted(manifests.items()):
        version = manifest.get("version", 1)
        fname = f"{_to_kebab_case(svc_name)}-v{version}.ts"
        fpath = os.path.join(services_dir, fname)
        svc_contract_id = manifest.get("contract_id", contract_id)

        body = _header(source, svc_contract_id) + "\n"
        # AsterClientWrapper is always needed (it's the fromConnection arg).
        # The remaining imports depend on the service shape: session-scoped
        # services delegate to client.proxy() and don't touch the transport
        # type at all, while shared services need AsterTransport (and
        # BidiChannel if any method is bidi-streaming). Combine value and
        # type imports into a single statement using the inline `type`
        # modifier (TS 5.0+) so the generated code stays compact.
        is_session = manifest.get("scoped") in ("session", "stream")
        import_parts: list[str] = ["AsterClientWrapper"]
        if not is_session:
            import_parts.append("type AsterTransport")
            method_patterns = {
                m.get("pattern", "unary") for m in manifest.get("methods", [])
            }
            if "bidi_stream" in method_patterns:
                import_parts.append("type BidiChannel")
        body += f'import {{ {", ".join(import_parts)} }} from "@aster-rpc/aster";\n'

        # Build the set of type names this service references
        type_imports = _service_type_imports(
            svc_name, manifest, all_types, shared_types
        )
        # Group imports by source file
        by_file: dict[str, list[str]] = {}
        for name, file_stem in type_imports:
            by_file.setdefault(file_stem, []).append(name)
        for file_stem in sorted(by_file):
            names = sorted(set(by_file[file_stem]))
            joined = ", ".join(names)
            body += f'import {{ {joined} }} from "../types/{file_stem}.js";\n'
        body += "\n"
        body += _gen_service_client(svc_name, manifest, known_types) + "\n"
        _write(fpath, body)
        generated.append(fpath)

    # ── index.ts barrel ──────────────────────────────────────────────────
    index_lines: list[str] = [_header(source, contract_id), ""]
    for svc_name, manifest in sorted(manifests.items()):
        version = manifest.get("version", 1)
        stem = f"{_to_kebab_case(svc_name)}-v{version}"
        index_lines.append(f'export * from "./services/{stem}.js";')
    for rec in sorted(shared_types, key=lambda r: r.display_name):
        stem = _to_kebab_case(rec.display_name)
        index_lines.append(f'export * from "./types/{stem}.js";')
    for svc_name, manifest in sorted(manifests.items()):
        if service_types.get(svc_name):
            version = manifest.get("version", 1)
            stem = f"{_to_kebab_case(svc_name)}-v{version}"
            index_lines.append(f'export * from "./types/{stem}.js";')
    index_path = os.path.join(ns_dir, "index.ts")
    _write(index_path, "\n".join(index_lines) + "\n")
    generated.append(index_path)

    return generated


def _shared_imports(
    svc_recs: list[_TypeRecord],
    shared_types: list[_TypeRecord],
    known_types: dict[str, str],
) -> list[tuple[str, str]]:
    """Find shared types that this service's own types reference."""
    shared_names = {r.display_name for r in shared_types}
    out: list[tuple[str, str]] = []
    seen: set[str] = set()
    for rec in svc_recs:
        for f in rec.fields:
            for cand in _candidate_refs(f):
                if cand in shared_names and cand not in seen:
                    out.append((cand, _to_kebab_case(cand)))
                    seen.add(cand)
    return out


def _service_type_imports(
    svc_name: str,
    manifest: dict[str, Any],
    all_types: dict[str, _TypeRecord],
    shared_types: list[_TypeRecord],
) -> list[tuple[str, str]]:
    """Resolve every type the service client needs to import.

    Returns ``(class_name, file_stem_without_ext)`` tuples. The file stem
    is relative to ``types/``.
    """
    version = manifest.get("version", 1)
    svc_stem = f"{_to_kebab_case(svc_name)}-v{version}"
    shared_names = {r.display_name for r in shared_types}
    known = {r.display_name for r in all_types.values()}

    out: list[tuple[str, str]] = []
    seen: set[str] = set()

    def _add(name: str) -> None:
        if not name or name in seen or name in ("None", "Any", "unknown"):
            return
        if name not in known:
            return
        seen.add(name)
        if name in shared_names:
            out.append((name, _to_kebab_case(name)))
        else:
            out.append((name, svc_stem))

    for method in manifest.get("methods", []):
        for key in ("request_type", "response_type"):
            _add(method.get(key, ""))
        for fkey in ("fields", "response_fields"):
            for f in method.get(fkey, []):
                for cand in _candidate_refs(f):
                    _add(cand)
    return out


def _candidate_refs(field: dict[str, Any]) -> list[str]:
    """Pull every plausible class-reference name out of a manifest field."""
    cands: list[str] = []
    # v1 schema refs
    if field.get("kind") == "ref":
        cands.append(field.get("ref_name", ""))
    if field.get("item_kind") == "ref":
        cands.append(field.get("item_ref", ""))
    if field.get("value_kind") == "ref":
        cands.append(field.get("value_ref", ""))
    # legacy element_type
    cands.append(field.get("element_type", ""))
    # legacy bare type may itself be a class
    raw = field.get("type", "")
    if isinstance(raw, str):
        m = re.match(r"list\[(.+)\]$", raw)
        if m:
            cands.append(m.group(1))
        elif raw and raw[0:1].isupper():
            cands.append(raw)
    return [c for c in cands if c]


def _write(path: str, content: str) -> None:
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
    """Format a TS-flavoured usage snippet to print after generation."""
    ns_snake = _to_snake_case(namespace)
    lines = [f"\nGenerated TypeScript clients -> {out_dir}/{ns_snake}/\n"]
    lines.append("Usage:\n")
    lines.append('  import { AsterClientWrapper } from "@aster-rpc/aster";')

    svc_name = next(iter(manifests), "MyService")
    version = manifests.get(svc_name, {}).get("version", 1)
    svc_stem = f"{_to_kebab_case(svc_name)}-v{version}"

    lines.append(
        f'  import {{ {svc_name}Client }} from '
        f'"./{ns_snake}/services/{svc_stem}.js";'
    )

    example_method = None
    example_req_type = None
    for m in manifests.get(svc_name, {}).get("methods", []):
        if m.get("pattern", "unary") == "unary":
            example_method = m["name"]
            example_req_type = m.get("request_type", "")
            break
    if example_req_type:
        lines.append(
            f'  import {{ {example_req_type} }} from '
            f'"./{ns_snake}/types/{svc_stem}.js";'
        )

    lines.append("")
    if address:
        lines.append(f'  const client = new AsterClientWrapper({{ address: "{address}" }});')
    else:
        lines.append('  const client = new AsterClientWrapper({ address: "aster1..." });')
    lines.append("  await client.connect();")
    lines.append(f"  const svc = await {svc_name}Client.fromConnection(client);")

    if example_method and example_req_type:
        lines.append(
            f"  const result = await svc.{example_method}(new {example_req_type}({{}}));"
        )
        lines.append("  console.log(result);")

    return "\n".join(lines)
