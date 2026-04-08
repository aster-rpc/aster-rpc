"""
aster_cli.contract — Offline ``aster contract`` command-line interface.

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

    # Compute type hashes (already done in resolve_with_cycles — re-compute for manifest)
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

# Fixed key order for deterministic output — methods and types sorted
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


def main() -> None:
    """Entry point for the ``aster`` CLI."""
    parser = argparse.ArgumentParser(
        prog="aster",
        description="Aster RPC framework command-line tools.",
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

    # ``aster shell`` subcommand
    from aster_cli.shell import register_shell_subparser
    register_shell_subparser(subparsers)

    # ``aster blob``, ``aster service`` — CLI equivalents of shell commands
    from aster_cli.shell.plugin import register_cli_subcommands
    register_cli_subcommands(subparsers)

    # ``aster init`` subcommand
    from aster_cli.init import register_init_subparser
    register_init_subparser(subparsers)

    # ``aster mcp`` subcommand
    from aster_cli.mcp.server import register_mcp_subparser
    register_mcp_subparser(subparsers)

    # ``aster authorize`` — sign a producer enrollment credential (legacy)
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
        else:
            contract_parser.print_help()
            sys.exit(1)
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
    elif args.command in {"publish", "unpublish"}:
        from aster_cli.publish import run_publish_command
        sys.exit(run_publish_command(args))
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
