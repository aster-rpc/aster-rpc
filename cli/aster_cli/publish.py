"""Publish-oriented CLI commands for the Day 0 @aster service."""

from __future__ import annotations

import argparse
import asyncio
import importlib
import json
from pathlib import Path
from typing import Any

import blake3
from aster.contract.manifest import ContractManifest

from aster_cli.aster_service import (
    build_signed_envelope,
    canonical_payload_json,
    generate_nonce,
    load_local_endpoint_id,
    now_epoch_seconds,
    open_aster_service,
    parse_duration_seconds,
)
from aster_cli.identity import load_identity, save_identity
from aster_cli.profile import get_active_profile, get_published_services, set_published_services


def _manifest_path(path: str | None = None) -> Path:
    return Path(path or ".aster/manifest.json")


def _identity_path(path: str | None = None) -> Path:
    return Path(path or ".aster-identity")


def store_producer_token(
    service_name: str,
    token: str,
    *,
    contract_id: str,
    identity_file: str | None = None,
) -> Path:
    path = _identity_path(identity_file)
    if path.exists():
        data = load_identity(path)
    else:
        data = {"node": {}, "peers": []}
    published = data.setdefault("published_services", {})
    published[service_name] = {
        "producer_token": token.strip(),
        "contract_id": contract_id,
        "service_name": service_name,
    }
    save_identity(path, data)
    return path


def load_producer_token(service_name: str, *, identity_file: str | None = None) -> str | None:
    path = _identity_path(identity_file)
    if not path.exists():
        return None
    data = load_identity(path)
    entry = data.get("published_services", {}).get(service_name, {})
    token = str(entry.get("producer_token", "")).strip()
    return token or None


def remove_producer_token(service_name: str, *, identity_file: str | None = None) -> bool:
    path = _identity_path(identity_file)
    if not path.exists():
        return False
    data = load_identity(path)
    published = data.get("published_services", {})
    if service_name not in published:
        return False
    del published[service_name]
    save_identity(path, data)
    return True


def _load_manifest_file(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    data = json.loads(path.read_text())
    return data if isinstance(data, list) else [data]


def _write_manifest_file(path: Path, manifests: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload: Any = manifests[0] if len(manifests) == 1 else manifests
    path.write_text(json.dumps(payload, indent=2) + "\n")


def _merge_manifest(path: Path, manifest: Any) -> dict[str, Any]:
    manifests = _load_manifest_file(path)
    incoming = json.loads(manifest.to_json())
    key = (incoming["service"], incoming["version"])
    merged = [m for m in manifests if (m.get("service"), m.get("version")) != key]
    merged.append(incoming)
    merged.sort(key=lambda item: (item.get("service", ""), item.get("version", 0)))
    _write_manifest_file(path, merged)
    return incoming


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


def _lookup_manifest(manifest_path: Path, service_name: str) -> dict[str, Any]:
    for manifest in _load_manifest_file(manifest_path):
        if manifest.get("service") == service_name:
            return manifest
    raise RuntimeError(f"manifest for {service_name!r} not found in {manifest_path}")


def _roles_from_args(args: argparse.Namespace) -> list[str]:
    if args.role:
        return sorted(set(args.role))
    return ["consumer"]


def _canonical_manifest_json(manifest: dict[str, Any]) -> str:
    return ContractManifest.from_json(json.dumps(manifest)).to_json(indent=None)


def _directory_contract_id(manifest: dict[str, Any]) -> tuple[str, str]:
    manifest_json = _canonical_manifest_json(manifest)
    return manifest_json, blake3.blake3(manifest_json.encode("utf-8")).hexdigest()


def _build_publish_payload(
    *,
    handle: str,
    service_name: str,
    manifest: dict[str, Any],
    args: argparse.Namespace,
    endpoint_id: str,
) -> dict[str, Any]:
    endpoint_ttl = parse_duration_seconds(args.endpoint_ttl, default=300)
    token_ttl = parse_duration_seconds(args.token_ttl, default=300)
    now = now_epoch_seconds()
    delegation_mode = "closed" if args.closed else "open"
    manifest_json, contract_id = _directory_contract_id(manifest)

    payload = {
        "action": "publish",
        "handle": handle,
        "service_name": service_name,
        "contract_id": contract_id,
        "manifest_json": manifest_json,
        "description": args.description,
        "status": args.status,
        "endpoints": [
            {
                "node_id": endpoint_id,
                "relay": args.relay or "",
                "registered_at": now,
                "ttl": endpoint_ttl,
            }
        ],
        "delegation": {
            "authority": "consumer",
            "mode": delegation_mode,
            "token_ttl": token_ttl,
            "rate_limit": args.rate_limit,
            "roles": _roles_from_args(args),
        },
        "timestamp": now,
        "nonce": generate_nonce(),
    }
    return payload


def cmd_publish(args: argparse.Namespace) -> int:
    _profile_name, profile, _config = get_active_profile()
    if not profile.get("root_pubkey"):
        print("Error: no root key configured. Run `aster keygen root` first.")
        return 1

    handle = str(profile.get("handle", "")).strip()
    if profile.get("handle_status") != "verified" or not handle:
        print("Error: publish requires a verified handle. Finish `aster join` / `aster verify` first.")
        return 1

    manifest_path = _manifest_path(args.manifest)
    spec, service_name = _resolve_service_spec(args.target, manifest_path)

    manifest_dict: dict[str, Any]
    if spec:
        from aster_cli.contract import _gen_single_service

        manifest = _gen_single_service(spec, argparse.Namespace(semver=args.semver))
        if manifest is None:
            return 1
        manifest_dict = _merge_manifest(manifest_path, manifest)
        service_name = manifest.service
        print(f"Updated {manifest_path} with {service_name} v{manifest.version}.")
    else:
        manifest_dict = _lookup_manifest(manifest_path, service_name)

    if getattr(args, "demo", False):
        published = get_published_services(profile)
        if service_name not in published:
            published.append(service_name)
            set_published_services(published)
        print(f"Marked {service_name} as published in local preview state.")
        print("Remote publish skipped because --demo was requested.")
        return 0

    if not str(args.description).strip():
        print("Error: publish requires --description for the Day 0 service directory.")
        return 1

    endpoint_id = args.endpoint_id or load_local_endpoint_id(args.identity_file)
    if not endpoint_id:
        print(
            "Error: could not determine endpoint_id from .aster-identity. "
            "Pass --endpoint-id or --identity-file."
        )
        return 1

    try:
        return asyncio.run(
            _publish_remote(
                args,
                handle=handle,
                service_name=service_name,
                manifest_dict=manifest_dict,
                endpoint_id=endpoint_id,
            )
        )
    except Exception as exc:
        print(f"Error: {exc}")
        return 1


async def _publish_remote(
    args: argparse.Namespace,
    *,
    handle: str,
    service_name: str,
    manifest_dict: dict[str, Any],
    endpoint_id: str,
) -> int:
    runtime = await open_aster_service(getattr(args, "aster", None))
    result = None
    try:
        publication_client = await runtime.publication_client()
        payload = _build_publish_payload(
            handle=handle,
            service_name=service_name,
            manifest=manifest_dict,
            args=args,
            endpoint_id=endpoint_id,
        )
        envelope = build_signed_envelope(
            payload,
            root_key_file=getattr(args, "root_key", None),
        )
        result = await publication_client.publish(runtime.signed_request(envelope))
        if getattr(args, "private", False) or getattr(args, "public", False):
            visibility = "private" if getattr(args, "private", False) else "public"
            visibility_payload = {
                "action": "set_visibility",
                "handle": handle,
                "service_name": service_name,
                "visibility": visibility,
                "timestamp": now_epoch_seconds(),
                "nonce": generate_nonce(),
            }
            visibility_envelope = build_signed_envelope(
                visibility_payload,
                root_key_file=getattr(args, "root_key", None),
            )
            await publication_client.set_visibility(runtime.signed_request(visibility_envelope))
    finally:
        await runtime.close()

    producer_token = str(getattr(result, "producer_token", "") or "").strip()
    if not producer_token:
        raise RuntimeError("publish succeeded but @aster did not return a producer token")

    token_path = store_producer_token(
        service_name,
        producer_token,
        contract_id=str(getattr(result, "contract_id", "") or payload["contract_id"]),
        identity_file=getattr(args, "identity_file", None),
    )

    _profile_name, profile, _config = get_active_profile()
    published = get_published_services(profile)
    if service_name not in published:
        published.append(service_name)
        set_published_services(published)
    print(f"Published @{handle}/{service_name}.")
    print(f"Stored producer token in {token_path}.")
    if result is not None and getattr(result, "first_publish", False):
        recovery_codes = getattr(result, "recovery_codes", None) or []
        if recovery_codes:
            print("Recovery codes:")
            for code in recovery_codes:
                print(f"  {code}")
        else:
            print("First publish complete.")
    return 0


def cmd_unpublish(args: argparse.Namespace) -> int:
    _profile_name, profile, _config = get_active_profile()
    handle = str(profile.get("handle", "")).strip()
    if not handle:
        print("Error: no handle configured.")
        return 1

    if getattr(args, "demo", False):
        published = get_published_services(profile)
        if args.service not in published:
            print(f"{args.service} is not marked as published locally.")
            return 1
        published.remove(args.service)
        set_published_services(published)
        remove_producer_token(args.service, identity_file=getattr(args, "identity_file", None))
        print(f"Removed local published marker for {args.service}.")
        return 0

    try:
        return asyncio.run(_unpublish_remote(args, handle=handle))
    except Exception as exc:
        print(f"Error: {exc}")
        return 1


async def _unpublish_remote(args: argparse.Namespace, *, handle: str) -> int:
    runtime = await open_aster_service(getattr(args, "aster", None))
    try:
        publication_client = await runtime.publication_client()
        payload = {
            "action": "unpublish",
            "handle": handle,
            "service_name": args.service,
            "timestamp": now_epoch_seconds(),
            "nonce": generate_nonce(),
        }
        envelope = build_signed_envelope(
            payload,
            root_key_file=getattr(args, "root_key", None),
        )
        await publication_client.unpublish(runtime.signed_request(envelope))
    finally:
        await runtime.close()

    _profile_name, profile, _config = get_active_profile()
    published = get_published_services(profile)
    if args.service in published:
        published.remove(args.service)
        set_published_services(published)
    removed_token = remove_producer_token(args.service, identity_file=getattr(args, "identity_file", None))
    print(f"Unpublished @{handle}/{args.service}.")
    if removed_token:
        print("Removed stored producer token.")
    return 0


def cmd_set_visibility(args: argparse.Namespace) -> int:
    _profile_name, profile, _config = get_active_profile()
    handle = str(profile.get("handle", "")).strip()
    if not handle:
        print("Error: no handle configured.")
        return 1

    try:
        return asyncio.run(_set_visibility_remote(args, handle=handle))
    except Exception as exc:
        print(f"Error: {exc}")
        return 1


async def _set_visibility_remote(args: argparse.Namespace, *, handle: str) -> int:
    runtime = await open_aster_service(getattr(args, "aster", None))
    try:
        publication_client = await runtime.publication_client()
        payload = {
            "action": "set_visibility",
            "handle": handle,
            "service_name": args.service,
            "visibility": args.visibility,
            "timestamp": now_epoch_seconds(),
            "nonce": generate_nonce(),
        }
        envelope = build_signed_envelope(
            payload,
            root_key_file=getattr(args, "root_key", None),
        )
        await publication_client.set_visibility(runtime.signed_request(envelope))
    finally:
        await runtime.close()

    print(f"Set @{handle}/{args.service} visibility to {args.visibility}.")
    return 0


def cmd_update_service(args: argparse.Namespace) -> int:
    _profile_name, profile, _config = get_active_profile()
    handle = str(profile.get("handle", "")).strip()
    if not handle:
        print("Error: no handle configured.")
        return 1
    if args.description is None and args.status is None and args.replacement is None:
        print("Error: update-service requires at least one of --description, --status, or --replacement.")
        return 1

    try:
        return asyncio.run(_update_service_remote(args, handle=handle))
    except Exception as exc:
        print(f"Error: {exc}")
        return 1


async def _update_service_remote(args: argparse.Namespace, *, handle: str) -> int:
    runtime = await open_aster_service(getattr(args, "aster", None))
    try:
        publication_client = await runtime.publication_client()
        payload = {
            "action": "update_service",
            "handle": handle,
            "service_name": args.service,
            "description": args.description,
            "status": args.status,
            "replacement": args.replacement,
            "timestamp": now_epoch_seconds(),
            "nonce": generate_nonce(),
        }
        envelope = build_signed_envelope(
            payload,
            root_key_file=getattr(args, "root_key", None),
        )
        await publication_client.update_service(runtime.signed_request(envelope))
    finally:
        await runtime.close()

    print(f"Updated @{handle}/{args.service}.")
    return 0


def cmd_discover(args: argparse.Namespace) -> int:
    try:
        return asyncio.run(_discover_remote(args))
    except Exception as exc:
        print(f"Error: {exc}")
        return 1


async def _discover_remote(args: argparse.Namespace) -> int:
    runtime = await open_aster_service(getattr(args, "aster", None))
    try:
        publication_client = await runtime.publication_client()
        types_mod = importlib.import_module(
            publication_client.__module__.replace(".services.", ".types.")
        )
        request = types_mod.DiscoverRequest(
            query=args.query,
            limit=args.limit,
            offset=args.offset,
        )
        result = await publication_client.discover(request)
    finally:
        await runtime.close()

    payload = {
        "total": getattr(result, "total", 0),
        "services": [
            {
                "handle": getattr(item, "handle", ""),
                "service_name": getattr(item, "service_name", ""),
                "version": getattr(item, "version", 0),
                "contract_id": getattr(item, "contract_id", ""),
                "description": getattr(item, "description", ""),
                "status": getattr(item, "status", ""),
                "method_count": getattr(item, "method_count", 0),
                "endpoint_count": getattr(item, "endpoint_count", 0),
                "visibility": getattr(item, "visibility", ""),
                "delegation_mode": getattr(item, "delegation_mode", ""),
                "published_at": getattr(item, "published_at", ""),
            }
            for item in getattr(result, "services", [])
        ],
    }

    if getattr(args, "raw_json", False):
        print(json.dumps(payload, indent=2))
        return 0

    print(f"Found {payload['total']} service(s).")
    for item in payload["services"]:
        print(
            f"  @{item['handle']}/{item['service_name']} "
            f"[v{item['version']}] "
            f"{item['status'] or 'unknown'} "
            f"{item['visibility'] or 'public'} "
            f"{item['delegation_mode'] or 'open'}"
        )
        if item["description"]:
            print(f"    {item['description']}")
    return 0


def register_publish_subparser(subparsers: argparse._SubParsersAction) -> None:
    publish_parser = subparsers.add_parser("publish", help="Publish a service to @aster")
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
    publish_parser.add_argument("--aster", default=None, help="Override @aster service address")
    publish_parser.add_argument("--root-key", default=None, help="Path to root key JSON backup")
    publish_parser.add_argument("--identity-file", default=None, help="Path to .aster-identity")
    publish_parser.add_argument("--endpoint-id", default=None, help="Override local endpoint_id")
    publish_parser.add_argument("--relay", default="", help="Relay URL for the published endpoint")
    publish_parser.add_argument("--endpoint-ttl", default="5m", help="Endpoint TTL (default: 5m)")
    publish_parser.add_argument("--description", required=False, default="", help="Human-facing service description")
    publish_parser.add_argument(
        "--status",
        choices=["experimental", "stable", "deprecated"],
        default="experimental",
        help="Lifecycle status (default: experimental)",
    )
    visibility = publish_parser.add_mutually_exclusive_group()
    visibility.add_argument("--public", action="store_true", help="Publish as publicly discoverable")
    visibility.add_argument("--private", action="store_true", help="Publish as private but resolvable")
    delegation = publish_parser.add_mutually_exclusive_group()
    delegation.add_argument("--open", action="store_true", help="Allow open delegated enrollment (default)")
    delegation.add_argument("--closed", action="store_true", help="Require explicit access grants")
    publish_parser.add_argument("--token-ttl", default="5m", help="Delegated token TTL (default: 5m)")
    publish_parser.add_argument("--rate-limit", default=None, help='Delegated issuance rate limit like "1/60m"')
    publish_parser.add_argument("--role", action="append", default=[], help="Delegated role (repeatable)")
    publish_parser.add_argument("--demo", action="store_true", help="Run the local preview flow only")

    unpublish_parser = subparsers.add_parser("unpublish", help="Unpublish a service from @aster")
    unpublish_parser.add_argument("service", help="Service name")
    unpublish_parser.add_argument("--aster", default=None, help="Override @aster service address")
    unpublish_parser.add_argument("--root-key", default=None, help="Path to root key JSON backup")
    unpublish_parser.add_argument("--identity-file", default=None, help="Path to .aster-identity")
    unpublish_parser.add_argument("--demo", action="store_true", help="Run the local preview flow only")

    visibility_parser = subparsers.add_parser("visibility", help="Change service visibility on @aster")
    visibility_parser.add_argument("service", help="Service name")
    visibility_parser.add_argument("visibility", choices=["public", "private"], help="Target visibility")
    visibility_parser.add_argument("--aster", default=None, help="Override @aster service address")
    visibility_parser.add_argument("--root-key", default=None, help="Path to root key JSON backup")

    update_service_parser = subparsers.add_parser("update-service", help="Update published service metadata")
    update_service_parser.add_argument("service", help="Service name")
    update_service_parser.add_argument("--description", default=None, help="New description or empty string")
    update_service_parser.add_argument(
        "--status",
        choices=["experimental", "stable", "deprecated"],
        default=None,
        help="Updated lifecycle status",
    )
    update_service_parser.add_argument("--replacement", default=None, help="Replacement @handle/Service pointer")
    update_service_parser.add_argument("--aster", default=None, help="Override @aster service address")
    update_service_parser.add_argument("--root-key", default=None, help="Path to root key JSON backup")

    discover_parser = subparsers.add_parser("discover", help="Search published services on @aster")
    discover_parser.add_argument("query", nargs="?", default="", help="Search query or @handle")
    discover_parser.add_argument("--aster", default=None, help="Override @aster service address")
    discover_parser.add_argument("--limit", type=int, default=20, help="Max results (default: 20)")
    discover_parser.add_argument("--offset", type=int, default=0, help="Pagination offset")
    discover_parser.add_argument("--json", action="store_true", dest="raw_json", help="Output raw JSON")


def run_publish_command(args: argparse.Namespace) -> int:
    if args.command == "publish":
        return cmd_publish(args)
    if args.command == "unpublish":
        return cmd_unpublish(args)
    if args.command == "visibility":
        return cmd_set_visibility(args)
    if args.command == "update-service":
        return cmd_update_service(args)
    if args.command == "discover":
        return cmd_discover(args)
    return 1
