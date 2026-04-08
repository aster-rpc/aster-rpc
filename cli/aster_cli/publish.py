"""Local publish-preview commands for the Aster CLI."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
from typing import Any

from aster_cli.profile import get_active_profile, get_published_services, set_published_services


def _manifest_path(path: str | None = None) -> Path:
    return Path(path or ".aster/manifest.json")


def _load_manifest_file(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    data = json.loads(path.read_text())
    return data if isinstance(data, list) else [data]


def _write_manifest_file(path: Path, manifests: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload: Any = manifests[0] if len(manifests) == 1 else manifests
    path.write_text(json.dumps(payload, indent=2) + "\n")


def _merge_manifest(path: Path, manifest: Any) -> None:
    manifests = _load_manifest_file(path)
    incoming = json.loads(manifest.to_json())
    key = (incoming["service"], incoming["version"])
    merged = [m for m in manifests if (m.get("service"), m.get("version")) != key]
    merged.append(incoming)
    merged.sort(key=lambda item: (item.get("service", ""), item.get("version", 0)))
    _write_manifest_file(path, merged)


def _resolve_service_spec(target: str, manifest_path: Path) -> tuple[str | None, str]:
    if ":" in target:
        return target, target.rsplit(":", 1)[-1]

    manifests = _load_manifest_file(manifest_path)
    for manifest in manifests:
        if manifest.get("service") == target:
            return None, target
    raise SystemExit(
        f"Could not resolve service {target!r}. Pass MODULE:CLASS or generate {manifest_path} first."
    )


def cmd_publish(args: argparse.Namespace) -> int:
    _profile_name, profile, _config = get_active_profile()
    if not profile.get("root_pubkey"):
        print("Error: no root key configured. Run `aster keygen root` first.")
        return 1

    manifest_path = _manifest_path(args.manifest)
    spec, service_name = _resolve_service_spec(args.target, manifest_path)

    if spec:
        from aster_cli.contract import _gen_single_service

        manifest = _gen_single_service(spec, argparse.Namespace(semver=args.semver))
        if manifest is None:
            return 1
        _merge_manifest(manifest_path, manifest)
        service_name = manifest.service
        print(f"Updated {manifest_path} with {service_name} v{manifest.version}.")

    published = get_published_services(profile)
    if service_name not in published:
        published.append(service_name)
        set_published_services(published)

    print(f"Marked {service_name} as published in local preview state.")
    print("Remote publish to @aster is not wired yet; this is enough for shell/demo UX work.")
    return 0


def cmd_unpublish(args: argparse.Namespace) -> int:
    _profile_name, profile, _config = get_active_profile()
    published = get_published_services(profile)
    if args.service not in published:
        print(f"{args.service} is not marked as published locally.")
        return 1
    published.remove(args.service)
    set_published_services(published)
    print(f"Removed local published marker for {args.service}.")
    return 0


def register_publish_subparser(subparsers: argparse._SubParsersAction) -> None:
    publish_parser = subparsers.add_parser("publish", help="Publish a service (local preview)")
    publish_parser.add_argument("target", help="MODULE:CLASS or service name already in .aster/manifest.json")
    publish_parser.add_argument(
        "--manifest",
        default=".aster/manifest.json",
        help="Manifest path to update/read (default: .aster/manifest.json)",
    )
    publish_parser.add_argument(
        "--semver",
        default=None,
        help="Optional semantic version to embed when generating a manifest",
    )

    unpublish_parser = subparsers.add_parser("unpublish", help="Unpublish a service (local preview)")
    unpublish_parser.add_argument("service", help="Service name")


def run_publish_command(args: argparse.Namespace) -> int:
    if args.command == "publish":
        return cmd_publish(args)
    if args.command == "unpublish":
        return cmd_unpublish(args)
    return 1
