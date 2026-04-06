"""
aster_cli.contract — Offline ``aster contract gen`` command-line interface.

Usage::

    aster contract gen --service my_module:MyServiceClass --out .aster/manifest.json

The CLI:
1. Imports the service class from the specified module
2. Resolves the type graph from the service's method signatures
3. Computes all canonical hashes
4. Writes manifest.json to the specified output path

No network connection or credentials required.
"""

from __future__ import annotations

import argparse
import importlib
import os
import sys
import time


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
    from aster.contract.manifest import ContractManifest
    from aster.decorators import _SERVICE_INFO_ATTR

    cls = _import_service_class(args.service)

    # Get ServiceInfo from the decorated class
    service_info = getattr(cls, _SERVICE_INFO_ATTR, None)
    if service_info is None:
        print(
            f"Error: Class {cls.__name__} is not decorated with @service.",
            file=sys.stderr,
        )
        return 1

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

    # Build manifest
    manifest = ContractManifest(
        service=service_info.name,
        version=service_info.version,
        contract_id=contract_id,
        canonical_encoding="fory-xlang/0.15",
        type_count=len(type_defs),
        type_hashes=type_hashes_hex,
        method_count=len(contract.methods),
        serialization_modes=ser_modes,
        scoped=scoped_str,
        deprecated=False,
        semver=getattr(args, "semver", None),
        vcs_revision=None,
        vcs_tag=None,
        vcs_url=None,
        changelog=None,
        published_by="",
        published_at_epoch_ms=int(time.time() * 1000),
    )

    # Create output directory if needed
    out_path = args.out
    out_dir = os.path.dirname(out_path)
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)

    # Write manifest
    manifest.save(out_path)

    print(f"Contract manifest written to: {out_path}")
    print(f"  Service:     {service_info.name} v{service_info.version}")
    print(f"  Contract ID: {contract_id}")
    print(f"  Types:       {len(type_defs)}")
    print(f"  Methods:     {len(contract.methods)}")

    return 0


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
        metavar="MODULE:CLASS",
        help="Service class to generate the manifest for, e.g. my_module:MyService",
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

    if args.command == "contract":
        if args.contract_command == "gen":
            sys.exit(_gen_command(args))
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
