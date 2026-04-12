"""
aster_cli.contract -- Offline ``aster contract`` command-line interface.

Subcommands::

    aster contract gen    --service my_module:MyServiceClass
    aster contract export [--manifest PATH] [-o PATH]
    aster contract import <file.aster.json>
    aster contract verify <file.aster.json> [--manifest PATH]

No network connection or credentials required.
"""

from __future__ import annotations

import argparse
import importlib
import json as _json
import os
import subprocess
import sys
import time
from pathlib import Path


def _git_revision() -> str | None:
    """Return the current HEAD commit hash, or None if not in a git repo."""
    try:
        result = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode == 0:
            return result.stdout.strip()
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    return None


def _git_tag() -> str | None:
    """Return the exact tag on HEAD, or None if untagged / not a git repo."""
    try:
        result = subprocess.run(
            ["git", "describe", "--tags", "--exact-match"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode == 0:
            return result.stdout.strip()
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    return None


def _git_remote_url() -> str | None:
    """Return the 'origin' remote URL, or None."""
    try:
        result = subprocess.run(
            ["git", "remote", "get-url", "origin"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode == 0:
            return result.stdout.strip()
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    return None


def _import_service_class(spec: str) -> type:
    """Import a class from a ``module:ClassName`` spec string.

    Args:
        spec: String of the form "module.path:ClassName" or
              "module.path:ClassName" where module can be a dotted path.

    Returns:
        The class object.

    Raises:
        SystemExit: If the module or class cannot be found.
    """
    if ":" not in spec:
        print(
            f"Error: --service must be of the form module:ClassName, got {spec!r}",
            file=sys.stderr,
        )
        sys.exit(1)

    module_path, class_name = spec.rsplit(":", 1)

    try:
        module = importlib.import_module(module_path)
    except ImportError as e:
        print(f"Error: Cannot import module {module_path!r}: {e}", file=sys.stderr)
        sys.exit(1)

    cls = getattr(module, class_name, None)
    if cls is None:
        print(
            f"Error: Module {module_path!r} has no attribute {class_name!r}",
            file=sys.stderr,
        )
        sys.exit(1)

    return cls


def _gen_command(args: argparse.Namespace) -> int:
    """Execute the ``gen`` subcommand.

    Args:
        args: Parsed arguments with ``service``, ``out``, and optional ``semver``.

    Returns:
        Exit code (0 for success).
    """
    from aster.contract.identity import (
        ServiceContract,
        build_type_graph,
        canonical_xlang_bytes,
        compute_contract_id,
        compute_type_hash,
        resolve_with_cycles,
    )
    from aster.contract.manifest import ContractManifest, extract_method_descriptors
    from aster.decorators import _SERVICE_INFO_ATTR

    # Support multiple --service args for multi-service manifests
    service_specs = args.service  # list due to action="append"
    all_manifests = []

    for spec in service_specs:
        manifest = _gen_single_service(spec, args)
        if manifest is None:
            return 1
        all_manifests.append(manifest)

    # Write manifest(s)
    out_path = args.out
    out_dir = os.path.dirname(out_path)
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)

    if len(all_manifests) == 1:
        all_manifests[0].save(out_path)
    else:
        # Multi-service: write as JSON array
        with open(out_path, "w", encoding="utf-8") as f:
            f.write(_json.dumps([_json.loads(m.to_json()) for m in all_manifests], indent=2))

    print(f"Contract manifest written to: {out_path}")
    for m in all_manifests:
        print(f"  {m.service} v{m.version}  contract_id={m.contract_id[:16]}...  methods={m.method_count}")

    return 0


def _gen_single_service(spec: str, args: argparse.Namespace) -> "ContractManifest | None":
    """Generate a manifest for a single service class."""
    from aster.contract.identity import (
        ServiceContract,
        build_type_graph,
        canonical_xlang_bytes,
        compute_contract_id,
        compute_type_hash,
        resolve_with_cycles,
    )
    from aster.contract.manifest import ContractManifest, extract_method_descriptors
    from aster.decorators import _SERVICE_INFO_ATTR

    cls = _import_service_class(spec)

    service_info = getattr(cls, _SERVICE_INFO_ATTR, None)
    if service_info is None:
        print(
            f"Error: Class {cls.__name__} is not decorated with @service.",
            file=sys.stderr,
        )
        return None

    # Collect all root types from method signatures
    root_types: list[type] = []
    for method_info in service_info.methods.values():
        if method_info.request_type is not None:
            root_types.append(method_info.request_type)
        if method_info.response_type is not None:
            root_types.append(method_info.response_type)

    # Build type graph and resolve with cycle-breaking
    type_graph = build_type_graph(root_types)
    type_defs = resolve_with_cycles(type_graph)

    # Compute type hashes (already done in resolve_with_cycles -- re-compute for manifest)
    type_hashes: dict[str, bytes] = {}
    for fqn, td in type_defs.items():
        td_bytes = canonical_xlang_bytes(td)
        type_hashes[fqn] = compute_type_hash(td_bytes)

    # Build ServiceContract
    contract = ServiceContract.from_service_info(service_info, type_hashes)

    # Compute canonical bytes and contract_id
    contract_bytes = canonical_xlang_bytes(contract)
    contract_id = compute_contract_id(contract_bytes)

    # Map scoped string
    scoped_str = getattr(service_info, "scoped", "shared")

    # Build type_hashes list (hex, sorted)
    type_hashes_hex = sorted(h.hex() for h in type_hashes.values())

    # Serialization modes
    ser_modes: list[str] = []
    for mode in service_info.serialization_modes:
        if hasattr(mode, "value"):
            ser_modes.append(mode.value)
        else:
            ser_modes.append(str(mode))
    if not ser_modes:
        ser_modes = ["xlang"]

    # Capture VCS info
    vcs_revision = _git_revision()
    vcs_tag = _git_tag()
    vcs_url = _git_remote_url()

    # Extract method descriptors with field definitions
    methods = extract_method_descriptors(service_info)

    # Determine producer_language per spec 11.3.2.3:
    # required when "native" in serialization_modes, empty string otherwise.
    producer_lang = "python" if "native" in ser_modes else ""

    # Build manifest
    manifest = ContractManifest(
        service=service_info.name,
        version=service_info.version,
        contract_id=contract_id,
        canonical_encoding="fory-xlang/0.15",
        type_count=len(type_defs),
        type_hashes=type_hashes_hex,
        method_count=len(contract.methods),
        methods=methods,
        serialization_modes=ser_modes,
        producer_language=producer_lang,
        scoped=scoped_str,
        deprecated=False,
        semver=getattr(args, "semver", None),
        vcs_revision=vcs_revision,
        vcs_tag=vcs_tag,
        vcs_url=vcs_url,
        changelog=None,
        published_by="",
        published_at_epoch_ms=int(time.time() * 1000),
    )

    return manifest


# ── Export / Import / Verify ─────────────────────────────────────────────────

_ASTER_CONTRACT_VERSION = "1"
_CONTRACTS_DIR = Path(os.path.expanduser("~/.aster/contracts"))

# Fixed key order for deterministic output -- methods and types sorted
# alphabetically, all dict keys in a stable order so `diff` is meaningful.
_METHOD_KEY_ORDER = [
    "name", "pattern", "request_type", "response_type",
    "request_wire_tag", "response_wire_tag",
    "timeout", "idempotent", "fields", "response_fields",
]

_FIELD_KEY_ORDER = ["name", "type", "required", "default"]


def _ordered_method(m: dict) -> dict:
    """Return method dict with keys in deterministic order."""
    out: dict = {}
    for k in _METHOD_KEY_ORDER:
        if k in m:
            v = m[k]
            if k in ("fields", "response_fields") and isinstance(v, list):
                v = [_ordered_field(f) for f in sorted(v, key=lambda f: f.get("name", ""))]
            out[k] = v
    # Any extra keys not in the fixed list, sorted
    for k in sorted(m.keys()):
        if k not in out:
            out[k] = m[k]
    return out


def _ordered_field(f: dict) -> dict:
    """Return field dict with keys in deterministic order."""
    out: dict = {}
    for k in _FIELD_KEY_ORDER:
        if k in f:
            out[k] = f[k]
    for k in sorted(f.keys()):
        if k not in out:
            out[k] = f[k]
    return out


def _manifest_to_export(manifest_data: dict) -> dict:
    """Convert a manifest dict to the deterministic .aster.json export format."""
    methods = manifest_data.get("methods", [])
    ordered_methods = [_ordered_method(m) for m in sorted(methods, key=lambda m: m.get("name", ""))]

    return {
        "aster_contract": _ASTER_CONTRACT_VERSION,
        "service": manifest_data["service"],
        "version": manifest_data.get("version", 1),
        "contract_id": f"blake3:{manifest_data['contract_id']}",
        "canonical_encoding": manifest_data.get("canonical_encoding", "fory-xlang/0.15"),
        "scoped": manifest_data.get("scoped", "shared"),
        "serialization_modes": sorted(manifest_data.get("serialization_modes", ["xlang"])),
        "methods": ordered_methods,
        "type_hashes": sorted(manifest_data.get("type_hashes", [])),
        "type_count": manifest_data.get("type_count", 0),
        "method_count": manifest_data.get("method_count", len(ordered_methods)),
        "semver": manifest_data.get("semver"),
        "deprecated": manifest_data.get("deprecated", False),
    }


def _export_to_json(export: dict) -> str:
    """Serialize export dict to deterministic JSON."""
    return _json.dumps(export, indent=2, sort_keys=False, ensure_ascii=False) + "\n"


def _load_manifests(manifest_path: str) -> list[dict]:
    """Load one or more manifests from a JSON file (single object or array)."""
    with open(manifest_path, encoding="utf-8") as f:
        data = _json.load(f)
    if isinstance(data, list):
        return data
    return [data]


def _export_filename(service_name: str) -> str:
    """Generate the export filename for a service."""
    return f"{service_name}.aster.json"


def _export_command(args: argparse.Namespace) -> int:
    """Execute the ``export`` subcommand."""
    manifest_path = args.manifest

    if not os.path.exists(manifest_path):
        print(f"Error: Manifest not found at {manifest_path}", file=sys.stderr)
        print("  Run `aster contract gen` first, or pass --manifest <path>.", file=sys.stderr)
        return 1

    manifests = _load_manifests(manifest_path)
    out_dir = args.out

    for mdata in manifests:
        export = _manifest_to_export(mdata)
        service_name = export["service"]
        filename = _export_filename(service_name)

        if out_dir:
            os.makedirs(out_dir, exist_ok=True)
            out_path = os.path.join(out_dir, filename)
        else:
            out_path = filename

        with open(out_path, "w", encoding="utf-8") as f:
            f.write(_export_to_json(export))

        print(f"Exported: {out_path}")
        print(f"  {service_name} v{export['version']}  contract_id={export['contract_id'][:22]}...")

    return 0


def _import_command(args: argparse.Namespace) -> int:
    """Execute the ``import`` subcommand."""
    file_path = args.file

    if not os.path.exists(file_path):
        print(f"Error: File not found: {file_path}", file=sys.stderr)
        return 1

    with open(file_path, encoding="utf-8") as f:
        data = _json.load(f)

    # Validate it's an aster export file
    if "aster_contract" not in data:
        print("Error: Not a valid .aster.json file (missing aster_contract field).", file=sys.stderr)
        return 1

    fmt_version = data["aster_contract"]
    if fmt_version != _ASTER_CONTRACT_VERSION:
        print(f"Warning: Export format version {fmt_version} (expected {_ASTER_CONTRACT_VERSION})", file=sys.stderr)

    service = data.get("service", "")
    version = data.get("version", 1)
    contract_id = data.get("contract_id", "")

    if not service:
        print("Error: Missing 'service' field.", file=sys.stderr)
        return 1

    # Strip blake3: prefix for storage
    raw_id = contract_id.removeprefix("blake3:")

    # Store in ~/.aster/contracts/<service>/<contract_id>.aster.json
    store_dir = _CONTRACTS_DIR / service
    store_dir.mkdir(parents=True, exist_ok=True)
    store_path = store_dir / f"{raw_id[:16]}.aster.json"

    with open(store_path, "w", encoding="utf-8") as f:
        f.write(_export_to_json(data))

    print(f"Imported: {service} v{version}")
    print(f"  contract_id={contract_id}")
    print(f"  Stored: {store_path}")

    return 0


def _verify_command(args: argparse.Namespace) -> int:
    """Execute the ``verify`` subcommand.

    Compares a .aster.json export against the local manifest to check
    whether the contract identity matches.
    """
    file_path = args.file

    if not os.path.exists(file_path):
        print(f"Error: File not found: {file_path}", file=sys.stderr)
        return 1

    with open(file_path, encoding="utf-8") as f:
        export_data = _json.load(f)

    if "aster_contract" not in export_data:
        print("Error: Not a valid .aster.json file.", file=sys.stderr)
        return 1

    export_id = export_data.get("contract_id", "").removeprefix("blake3:")
    export_service = export_data.get("service", "")

    # If --manifest given, compare against that. Otherwise use .aster/manifest.json.
    manifest_path = args.manifest

    if not os.path.exists(manifest_path):
        print(f"Error: Manifest not found at {manifest_path}", file=sys.stderr)
        print("  Run `aster contract gen` first, or pass --manifest <path>.", file=sys.stderr)
        return 1

    manifests = _load_manifests(manifest_path)

    # Find matching service
    match = None
    for m in manifests:
        if m.get("service") == export_service:
            match = m
            break

    if match is None:
        print(f"Error: Service {export_service!r} not found in {manifest_path}", file=sys.stderr)
        return 1

    live_id = match["contract_id"]

    if live_id == export_id:
        print(f"OK: {export_service} contract matches.")
        print(f"  contract_id=blake3:{live_id[:16]}...")
        return 0
    else:
        print(f"MISMATCH: {export_service} contract has changed.", file=sys.stderr)
        print(f"  Export:  blake3:{export_id}", file=sys.stderr)
        print(f"  Local:   blake3:{live_id}", file=sys.stderr)
        return 1


def _call_command(args: argparse.Namespace) -> int:
    """Execute ``aster call <address> Service.Method '{json}'``."""
    import asyncio

    address = args.address
    method_spec = args.method
    payload_json = args.payload

    # Parse Service.Method
    if "." not in method_spec:
        print(f"Error: method must be Service.Method (got '{method_spec}')", file=sys.stderr)
        return 1
    service_name, method_name = method_spec.rsplit(".", 1)

    # Parse JSON payload
    try:
        payload = _json.loads(payload_json)
    except _json.JSONDecodeError as e:
        print(f"Error: invalid JSON payload: {e}", file=sys.stderr)
        return 1

    rcan_path = getattr(args, "rcan", None)

    async def _run() -> int:
        import os as _os
        from aster.runtime import AsterClient
        from aster.config import AsterConfig

        config = AsterConfig.from_env()
        config.storage_path = None

        if rcan_path:
            # Look for the .aster-identity file alongside the credential
            # file. `aster enroll node` creates them as a pair in the same
            # directory. If the env didn't already point at one and the
            # CWD doesn't have one, fall back to the credential's dir.
            cred_dir = _os.path.dirname(_os.path.abspath(rcan_path))
            paired = _os.path.join(cred_dir, ".aster-identity")
            if not config.identity_file or not _os.path.exists(config.identity_file):
                if _os.path.exists(paired):
                    config.identity_file = paired
        else:
            # No credential -- skip identity file lookup so we don't
            # accidentally use stale CWD .aster-identity in dev mode
            config.identity_file = "/dev/null/.aster-identity"

        client = AsterClient(
            config=config,
            address=address,
            enrollment_credential_file=rcan_path,
        )
        try:
            await client.connect()

            # Detect session-scoped services and use session proxy
            try:
                proxy = client.proxy(service_name)
                method = getattr(proxy, method_name)
                result = await method(payload)
            except TypeError as te:
                if "session-scoped" in str(te):
                    # Auto-open a session for the one-shot call
                    session = await client.session(service_name)
                    try:
                        result = await session.call(method_name, payload)
                    finally:
                        await session.close()
                else:
                    raise

            print(_json.dumps(result, indent=2, default=str))
            return 0
        except Exception as exc:
            from aster.runtime import AdmissionDeniedError
            if isinstance(exc, AdmissionDeniedError):
                print(f"Error: {exc}", file=sys.stderr)
                return 1
            msg = str(exc)
            if "PERMISSION_DENIED" in msg:
                print(f"Error: permission denied for {service_name}.{method_name}.\n"
                      f"  Check your credential has the required role.",
                      file=sys.stderr)
            elif "UNAVAILABLE" in msg:
                print(f"Error: service unavailable -- connection may have dropped.",
                      file=sys.stderr)
            elif "DEADLINE_EXCEEDED" in msg:
                print(f"Error: request timed out.", file=sys.stderr)
            else:
                print(f"Error: {exc}", file=sys.stderr)
            return 1
        finally:
            await client.close()

    return asyncio.run(_run())


def _gen_client_command(args: argparse.Namespace) -> int:
    """Execute ``aster contract gen-client``."""
    import asyncio
    from aster_cli.codegen import generate_python_clients
    from aster_cli.codegen import format_usage_snippet as format_python_snippet
    from aster_cli.codegen_typescript import (
        generate_typescript_clients,
        format_usage_snippet as format_typescript_snippet,
    )

    source = args.source
    lang = args.lang
    out = args.out

    if lang not in ("python", "typescript"):
        print(f"Error: unsupported --lang '{lang}' (expected: python, typescript)", file=sys.stderr)
        return 1

    # Determine source type and load manifests
    from aster.runtime import AdmissionDeniedError
    if os.path.isfile(source):
        # Source is a local .aster.json export file
        manifests = _load_manifests_for_codegen(source)
        namespace = args.package or Path(source).stem.replace(".aster", "").replace(".", "_") or "local"
    elif source.startswith("@") and "/" in source:
        try:
            manifests = asyncio.run(
                _fetch_manifests_from_directory_ref(source, getattr(args, "aster", None))
            )
        except AdmissionDeniedError as exc:
            print(f"Error: {exc}", file=sys.stderr)
            return 1
        namespace = args.package or _namespace_from_directory_ref(source)
    elif source.startswith("aster1"):
        # Source is a live node ticket -- connect, fetch manifests, disconnect
        try:
            manifests = asyncio.run(_fetch_manifests_from_node(source, getattr(args, "rcan", None)))
        except AdmissionDeniedError as exc:
            print(f"Error: {exc}", file=sys.stderr)
            return 1
        namespace = args.package or source[6:14]  # first 8 chars after "aster1"
    else:
        print(f"Error: unrecognised source '{source}'", file=sys.stderr)
        print("  Use a .aster.json file path, an aster1... ticket, or @handle/Service", file=sys.stderr)
        return 1

    if not manifests:
        print("Error: no manifests found", file=sys.stderr)
        return 1

    # Sanitize namespace
    import re
    namespace = re.sub(r"[^a-zA-Z0-9_]", "_", namespace).strip("_") or "aster_client"

    if lang == "typescript":
        generated = generate_typescript_clients(manifests, out, namespace, source)
        snippet = format_typescript_snippet(
            out, namespace, manifests, source if source.startswith("aster1") else ""
        )
    else:
        generated = generate_python_clients(manifests, out, namespace, source)
        snippet = format_python_snippet(
            out, namespace, manifests, source if source.startswith("aster1") else ""
        )

    print(f"Generated {len(generated)} files")
    for f in generated:
        print(f"  {f}")
    print(snippet)

    return 0


def _load_manifests_for_codegen(filepath: str) -> dict[str, dict]:
    """Load manifests from a .aster.json export or manifest.json file."""
    with open(filepath, "r", encoding="utf-8") as f:
        data = _json.load(f)

    # Handle both single manifest and array of manifests
    if isinstance(data, list):
        items = data
    elif isinstance(data, dict) and "service" in data:
        items = [data]
    elif isinstance(data, dict) and "methods" in data:
        items = [data]
    else:
        print(f"Warning: unrecognised manifest format in {filepath}", file=sys.stderr)
        return {}

    manifests = {}
    for item in items:
        # .aster.json exports have a "contract" wrapper
        if "contract" in item:
            manifest = item["contract"]
        else:
            manifest = item
        svc_name = manifest.get("service", "UnknownService")
        manifests[svc_name] = manifest

    return manifests


def _namespace_from_directory_ref(source: str) -> str:
    """Derive a stable package namespace from ``@handle/Service``."""
    handle, service = source.split("/", 1)
    return f"{handle.lstrip('@')}_{service}"


async def _fetch_manifests_from_node(ticket: str, rcan_path: str | None = None) -> dict[str, dict]:
    """Connect to a live node, fetch all manifests, and return them."""
    from aster.runtime import AsterClient, _coerce_node_addr
    from aster.config import AsterConfig
    from aster import docs_client, blobs_client
    from aster.registry.keys import contract_key
    import asyncio
    import os as _os

    config = AsterConfig.from_env()
    config.storage_path = None
    if rcan_path:
        cred_dir = _os.path.dirname(_os.path.abspath(rcan_path))
        paired = _os.path.join(cred_dir, ".aster-identity")
        if not config.identity_file or not _os.path.exists(config.identity_file):
            if _os.path.exists(paired):
                config.identity_file = paired
    else:
        config.identity_file = "/dev/null/.aster-identity"

    client = AsterClient(
        config=config,
        address=ticket,
        enrollment_credential_file=rcan_path,
    )
    await client.connect()

    bc = blobs_client(client._node)
    dc = docs_client(client._node)
    ns = client.registry_namespace
    addr = _coerce_node_addr(client._endpoint_addr_in)
    peer_id = addr.endpoint_id

    doc, rx = await dc.join_and_subscribe_namespace(ns, peer_id)

    # Wait for sync
    for _ in range(25):
        try:
            event = await asyncio.wait_for(rx.recv(), timeout=2.0)
            if hasattr(event, "kind") and event.kind == "sync_finished":
                await asyncio.sleep(0.3)
                break
        except asyncio.TimeoutError:
            entries = await doc.query_key_prefix(b"contracts/")
            if entries:
                break

    manifests: dict[str, dict] = {}
    for svc in client._services:
        try:
            key = contract_key(svc.contract_id)
            entries = await doc.query_key_exact(key)
            if not entries:
                continue
            content = await doc.read_entry_content(entries[0].content_hash)
            artifact = _json.loads(content)
            ch = artifact.get("collection_hash", "")
            if not ch:
                continue
            files = await bc.download_collection_hash(ch, peer_id)
            for name, data in files:
                if name == "manifest.json":
                    manifests[svc.name] = _json.loads(data)
                    break
        except Exception as exc:
            print(f"Warning: failed to fetch manifest for {svc.name}: {exc}", file=sys.stderr)

    return manifests


async def _fetch_manifests_from_directory_ref(
    source: str,
    explicit_aster: str | None = None,
) -> dict[str, dict]:
    """Fetch a manifest from the live @aster directory by ``@handle/Service``."""
    from aster_cli.aster_service import open_aster_service

    if "/" not in source or not source.startswith("@"):
        raise ValueError(f"invalid directory ref: {source!r}")

    handle, service_name = source[1:].split("/", 1)
    runtime = await open_aster_service(explicit_aster)
    try:
        publication_client = await runtime.publication_client()
        types_mod = importlib.import_module(
            publication_client.__module__.replace(".services.", ".types.")
        )
        result = await publication_client.get_manifest(
            types_mod.GetManifestRequest(handle=handle, service_name=service_name)
        )
    finally:
        await runtime.close()

    manifest_json = getattr(result, "manifest_json", "") or ""
    if not manifest_json:
        return {}
    manifest = _json.loads(manifest_json)
    return {manifest.get("service", service_name): manifest}


def _preview_command(args: argparse.Namespace) -> int:
    """Execute ``aster contract preview`` per spec 11.4.5.

    Renders a human-friendly dump of the service contract's wire-type mapping
    so devs can see what consumers will see -- field IDs (NFC-name-sorted),
    wire types, source types, defaults, and mode info.
    """
    import json

    if getattr(args, "service", None):
        return _preview_from_source(args)
    elif getattr(args, "manifest", None):
        return _preview_from_manifest(args.manifest)
    else:
        # Default: try .aster/manifest.json
        manifest_path = ".aster/manifest.json"
        if os.path.exists(manifest_path):
            return _preview_from_manifest(manifest_path)
        print(
            "Error: No --service or --manifest specified and no .aster/manifest.json found.",
            file=sys.stderr,
        )
        print("  Run `aster contract preview --service module:Class` or `--manifest path`.", file=sys.stderr)
        return 1


def _preview_from_source(args: argparse.Namespace) -> int:
    """Preview by walking Python source classes."""
    from aster.contract.identity import (
        ServiceContract,
        build_type_graph,
        canonical_xlang_bytes,
        compute_contract_id,
        compute_type_hash,
        resolve_with_cycles,
    )
    from aster.contract.manifest import extract_method_descriptors
    from aster.decorators import _SERVICE_INFO_ATTR

    for spec in args.service:
        cls = _import_service_class(spec)
        service_info = getattr(cls, _SERVICE_INFO_ATTR, None)
        if service_info is None:
            print(f"Error: {cls.__name__} is not @service decorated.", file=sys.stderr)
            return 1

        root_types: list[type] = []
        for mi in service_info.methods.values():
            if mi.request_type is not None:
                root_types.append(mi.request_type)
            if mi.response_type is not None:
                root_types.append(mi.response_type)

        type_graph = build_type_graph(root_types)
        type_defs = resolve_with_cycles(type_graph)
        type_hashes: dict[str, bytes] = {}
        for fqn, td in type_defs.items():
            td_bytes = canonical_xlang_bytes(td)
            type_hashes[fqn] = compute_type_hash(td_bytes)

        contract = ServiceContract.from_service_info(service_info, type_hashes)
        contract_bytes = canonical_xlang_bytes(contract)
        contract_id = compute_contract_id(contract_bytes)

        ser_modes = []
        for m in service_info.serialization_modes:
            if hasattr(m, "name"):
                ser_modes.append(m.name.lower())
            elif hasattr(m, "value") and isinstance(m.value, str):
                ser_modes.append(m.value)
            else:
                ser_modes.append(str(m))
        if not ser_modes:
            ser_modes = ["xlang"]

        _print_preview(
            service_name=service_info.name,
            version=service_info.version,
            contract_id=contract_id,
            producer="python",
            modes=ser_modes,
            methods=extract_method_descriptors(service_info),
        )
    return 0


def _preview_from_manifest(path: str) -> int:
    """Preview from a manifest.json file."""
    import json
    try:
        with open(path) as f:
            data = json.load(f)
    except Exception as e:
        print(f"Error reading {path}: {e}", file=sys.stderr)
        return 1

    manifests = data if isinstance(data, list) else [data]
    for m in manifests:
        _print_preview(
            service_name=m.get("service", "?"),
            version=m.get("version", 0),
            contract_id=m.get("contract_id", "?"),
            producer=m.get("producer_language", ""),
            modes=m.get("serialization_modes", ["xlang"]),
            methods=m.get("methods", []),
        )
    return 0


def _print_preview(
    service_name: str,
    version: int,
    contract_id: str,
    producer: str,
    modes: list[str],
    methods: list[dict],
) -> None:
    """Print the human-friendly preview per spec 11.4.5."""
    print(f"service {service_name}@{version}")
    print(f"contract_id     : {contract_id}")
    if producer:
        print(f"producer        : {producer}")
    print(f"modes           : {', '.join(modes)}")
    print()

    # Collect all unique message types from methods
    seen_types: set[str] = set()
    for m in methods:
        for field_list_key in ("fields", "response_fields"):
            fields = m.get(field_list_key, [])
            if not fields:
                continue
            type_name = m.get("request_type" if field_list_key == "fields" else "response_type", "?")
            if type_name in seen_types:
                continue
            seen_types.add(type_name)
            # Sort fields by name for NFC-name-sorted display (matches canonical field_id order)
            sorted_fields = sorted(fields, key=lambda f: f.get("name", ""))
            print(f"message {type_name} {{")
            for i, f in enumerate(sorted_fields):
                fid = i + 1
                fname = f.get("name", "?")
                ftype = f.get("type", "?")
                req = f.get("required", True)
                default = f.get("default")
                line = f"  #{fid:<3} {fname:<20}: {ftype}"
                if not req and default is not None:
                    line += f"  = {default!r}"
                elif not req:
                    line += "  (optional)"
                print(line)
            print("}")
            print()

    for m in methods:
        pattern = m.get("pattern", "unary")
        req_type = m.get("request_type", "?")
        resp_type = m.get("response_type", "?")
        timeout = m.get("timeout")
        print(f"rpc {m.get('name', '?')} ({pattern})")
        print(f"  request   : {req_type}")
        print(f"  response  : {resp_type}")
        if timeout:
            print(f"  timeout   : {timeout}s")
        if m.get("idempotent"):
            print(f"  idempotent: true")
        print()


def _resolve_aster_version() -> str:
    """Find the installed aster-cli + aster-rpc versions for `--version`.

    Reads from importlib.metadata so the version string reflects the
    actually-installed package, not whatever's hard-coded in source.
    """
    parts: list[str] = []
    try:
        from importlib.metadata import version as _pkg_version, PackageNotFoundError
    except ImportError:  # pragma: no cover -- Python <3.8 not supported anyway
        return "unknown"
    try:
        parts.append(f"aster-cli {_pkg_version('aster-cli')}")
    except PackageNotFoundError:
        pass
    try:
        parts.append(f"aster-rpc {_pkg_version('aster-rpc')}")
    except PackageNotFoundError:
        pass
    return " / ".join(parts) if parts else "unknown"


def main() -> None:
    """Entry point for the ``aster`` CLI."""
    parser = argparse.ArgumentParser(
        prog="aster",
        description="Aster RPC framework command-line tools.",
    )
    parser.add_argument(
        "--version",
        action="version",
        version=f"%(prog)s {_resolve_aster_version()}",
    )
    subparsers = parser.add_subparsers(dest="command", help="Subcommand")

    # ``aster contract`` subcommand group
    contract_parser = subparsers.add_parser("contract", help="Contract identity commands")
    contract_subparsers = contract_parser.add_subparsers(
        dest="contract_command", help="Contract subcommand"
    )

    # ``aster contract gen``
    gen_parser = contract_subparsers.add_parser(
        "gen",
        help="Generate a contract manifest from a service class.",
    )
    gen_parser.add_argument(
        "--service",
        required=True,
        action="append",
        metavar="MODULE:CLASS",
        help="Service class to generate the manifest for (repeatable for multi-service)",
    )
    gen_parser.add_argument(
        "--out",
        default=".aster/manifest.json",
        metavar="PATH",
        help="Output path for the manifest JSON file (default: .aster/manifest.json)",
    )
    gen_parser.add_argument(
        "--semver",
        default=None,
        metavar="VERSION",
        help="Optional semantic version string to embed in the manifest",
    )

    # ``aster contract gen-client``
    gen_client_parser = contract_subparsers.add_parser(
        "gen-client",
        help="Generate a typed client library from a manifest or live node.",
    )
    gen_client_parser.add_argument(
        "source",
        metavar="SOURCE",
        help=(
            "Manifest source: path to .aster.json file, "
            "aster1... ticket to a live node, or @handle/Service"
        ),
    )
    gen_client_parser.add_argument(
        "--out",
        required=True,
        metavar="DIR",
        help="Output directory for generated client files",
    )
    gen_client_parser.add_argument(
        "--package",
        default=None,
        metavar="NAME",
        help="Package/module name (default: derived from source)",
    )
    gen_client_parser.add_argument(
        "--lang",
        required=True,
        choices=["python", "typescript"],
        metavar="LANG",
        help="Target language: python | typescript (required)",
    )
    gen_client_parser.add_argument(
        "--aster",
        default=None,
        metavar="ADDR",
        help="Override @aster service address for @handle/Service sources",
    )
    gen_client_parser.add_argument(
        "--rcan",
        default=None,
        metavar="PATH",
        help="Path to enrollment credential (.cred file) for trusted-mode connections",
    )

    # ``aster contract export``
    export_parser = contract_subparsers.add_parser(
        "export",
        help="Export contract(s) to portable .aster.json files.",
    )
    export_parser.add_argument(
        "--manifest",
        default=".aster/manifest.json",
        metavar="PATH",
        help="Source manifest (default: .aster/manifest.json)",
    )
    export_parser.add_argument(
        "-o", "--out",
        default=None,
        metavar="DIR",
        help="Output directory (default: current directory)",
    )

    # ``aster contract import``
    import_parser = contract_subparsers.add_parser(
        "import",
        help="Import a .aster.json file into the local contract store.",
    )
    import_parser.add_argument(
        "file",
        metavar="FILE",
        help="Path to .aster.json file",
    )

    # ``aster contract verify``
    verify_parser = contract_subparsers.add_parser(
        "verify",
        help="Verify a .aster.json matches the local manifest.",
    )
    verify_parser.add_argument(
        "file",
        metavar="FILE",
        help="Path to .aster.json file to verify",
    )
    verify_parser.add_argument(
        "--manifest",
        default=".aster/manifest.json",
        metavar="PATH",
        help="Local manifest to compare against (default: .aster/manifest.json)",
    )

    # ``aster contract preview``
    preview_parser = contract_subparsers.add_parser(
        "preview",
        help="Human-readable dump of a contract's wire-type mapping.",
    )
    preview_group = preview_parser.add_mutually_exclusive_group()
    preview_group.add_argument(
        "--service",
        action="append",
        metavar="MODULE:CLASS",
        help="Service class to preview (reads from source tree; requires Python toolchain)",
    )
    preview_group.add_argument(
        "--manifest",
        default=None,
        metavar="PATH",
        help="Path to manifest.json to preview (toolchain-agnostic)",
    )

    # ``aster trust`` subcommand group (Phase 11)
    from aster_cli.trust import register_trust_subparser, run_trust_command
    trust_parser = register_trust_subparser(subparsers)

    # ``aster keygen`` subcommand group
    from aster_cli.keygen import register_keygen_subparser, run_keygen_command
    register_keygen_subparser(subparsers)

    # ``aster profile`` subcommand group
    from aster_cli.profile import register_profile_subparser
    register_profile_subparser(subparsers)

    # ``aster enroll`` subcommand group
    from aster_cli.enroll import register_enroll_subparser
    register_enroll_subparser(subparsers)

    # Identity / registration commands
    from aster_cli.join import register_join_subparser
    register_join_subparser(subparsers)

    # Publish preview commands
    from aster_cli.publish import register_publish_subparser
    register_publish_subparser(subparsers)

    # Access-control commands
    from aster_cli.access import register_access_subparser
    register_access_subparser(subparsers)

    # ``aster shell`` subcommand
    from aster_cli.shell import register_shell_subparser
    register_shell_subparser(subparsers)

    # ``aster blob`` / ``aster service`` -- intentionally NOT registered at
    # the top-level CLI for now. The shell plugin system registers these as
    # argparse subcommands, but they have no execution path in main() yet
    # (there is no ``elif args.command == "service"`` branch), so calling
    # them prints the root help and exits 1 -- a worse UX than the command
    # simply not existing. They remain available inside `aster shell`
    # as `blob ls`, `service describe`, etc. Re-enable when the dispatcher
    # is wired up.
    #
    # from aster_cli.shell.plugin import register_cli_subcommands
    # register_cli_subcommands(subparsers)

    # ``aster call`` -- one-shot RPC invocation
    call_parser = subparsers.add_parser(
        "call",
        help="Invoke an RPC method on a remote service.",
    )
    call_parser.add_argument(
        "address",
        metavar="ADDRESS",
        help="Server address (aster1... ticket)",
    )
    call_parser.add_argument(
        "method",
        metavar="SERVICE.METHOD",
        help="Service and method (e.g., MissionControl.getStatus)",
    )
    call_parser.add_argument(
        "payload",
        nargs="?",
        default="{}",
        metavar="JSON",
        help='JSON payload (default: {})',
    )
    call_parser.add_argument(
        "--json", dest="json_output", action="store_true",
        help="Output raw JSON (default)",
    )
    call_parser.add_argument(
        "--rcan", default=None, metavar="PATH",
        help="Path to credential file (JSON or .aster-identity TOML)",
    )

    # ``aster init`` subcommand
    from aster_cli.init import register_init_subparser
    register_init_subparser(subparsers)

    # ``aster mcp`` subcommand
    from aster_cli.mcp.server import register_mcp_subparser
    register_mcp_subparser(subparsers)

    # ``aster authorize`` -- sign a producer enrollment credential (legacy)
    auth_parser = subparsers.add_parser(
        "authorize",
        help="Sign a producer enrollment credential (offline)",
    )
    auth_parser.add_argument(
        "--root-key", required=True, metavar="PATH",
        help="Path to root key JSON (from 'aster keygen root')",
    )
    auth_parser.add_argument(
        "--producer-id", required=True, metavar="NODE_ID",
        help="Producer NodeId to bind to the credential",
    )
    auth_parser.add_argument(
        "--attributes", default=None, metavar="JSON",
        help='Optional JSON attributes, e.g. \'{"aster.role":"producer"}\'',
    )
    auth_parser.add_argument(
        "--expires", default=None, metavar="ISO8601",
        help="Expiry datetime in ISO 8601 (default: +30 days)",
    )
    auth_parser.add_argument(
        "--out", default="enrollment.token", metavar="PATH",
        help="Output path for the signed credential JSON (default: enrollment.token)",
    )

    args = parser.parse_args()

    # Show active profile hint when multiple profiles exist
    if args.command and args.command != "profile":
        from aster_cli.profile import print_active_profile_hint
        print_active_profile_hint()

    if args.command == "contract":
        if args.contract_command == "gen":
            sys.exit(_gen_command(args))
        elif args.contract_command == "export":
            sys.exit(_export_command(args))
        elif args.contract_command == "import":
            sys.exit(_import_command(args))
        elif args.contract_command == "verify":
            sys.exit(_verify_command(args))
        elif args.contract_command == "gen-client":
            sys.exit(_gen_client_command(args))
        elif args.contract_command == "preview":
            sys.exit(_preview_command(args))
        else:
            contract_parser.print_help()
            sys.exit(1)
    elif args.command == "call":
        sys.exit(_call_command(args))
    elif args.command == "trust":
        sys.exit(run_trust_command(args))
    elif args.command == "keygen":
        sys.exit(run_keygen_command(args))
    elif args.command == "profile":
        from aster_cli.profile import run_profile_command
        sys.exit(run_profile_command(args))
    elif args.command == "enroll":
        from aster_cli.enroll import run_enroll_command
        sys.exit(run_enroll_command(args))
    elif args.command in {"join", "verify", "status", "whoami"}:
        from aster_cli.join import run_join_command
        sys.exit(run_join_command(args))
    elif args.command in {"publish", "unpublish", "discover", "visibility", "update-service"}:
        from aster_cli.publish import run_publish_command
        sys.exit(run_publish_command(args))
    elif args.command == "access":
        from aster_cli.access import run_access_command
        sys.exit(run_access_command(args))
    elif args.command == "shell":
        from aster_cli.shell import run_shell_command
        sys.exit(run_shell_command(args))
    elif args.command == "init":
        from aster_cli.init import run_init_command
        sys.exit(run_init_command(args))
    elif args.command == "mcp":
        from aster_cli.mcp.server import run_mcp_command
        sys.exit(run_mcp_command(args))
    elif args.command == "authorize":
        # Map to trust sign --type producer (legacy)
        args.endpoint_id = args.producer_id
        args.type = "producer"
        args.trust_command = "sign"
        sys.exit(run_trust_command(args))
    else:
        parser.print_help()
        sys.exit(1)


if __name__ == "__main__":
    main()
