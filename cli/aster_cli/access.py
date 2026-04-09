"""Access-control CLI commands for the Day 0 @aster service."""

from __future__ import annotations

import argparse
import asyncio
import json

from aster_cli.aster_service import (
    build_signed_envelope,
    generate_nonce,
    now_epoch_seconds,
    open_aster_service,
    parse_duration_seconds,
)
from aster_cli.profile import get_active_profile


def _require_verified_handle() -> str:
    _profile_name, profile, _config = get_active_profile()
    handle = str(profile.get("handle", "")).strip()
    if profile.get("handle_status") != "verified" or not handle:
        raise RuntimeError(
            "access commands require a verified handle. Finish `aster join` / `aster verify` first."
        )
    return handle


def _signed_payload(action: str, **fields: object) -> dict[str, object]:
    payload: dict[str, object] = {
        "action": action,
        **fields,
        "timestamp": now_epoch_seconds(),
        "nonce": generate_nonce(),
    }
    return payload


def cmd_access_grant(args: argparse.Namespace) -> int:
    try:
        return asyncio.run(_grant_remote(args, handle=_require_verified_handle()))
    except Exception as exc:
        print(f"Error: {exc}")
        return 1


async def _grant_remote(args: argparse.Namespace, *, handle: str) -> int:
    runtime = await open_aster_service(getattr(args, "aster", None))
    try:
        access_client = await runtime.access_client()
        payload: dict[str, object] = _signed_payload(
            "grant_access",
            handle=handle,
            service_name=args.service,
            consumer_handle=args.consumer,
            role=args.role,
            scope=args.scope,
            scope_node_id=args.scope_node_id,
        )
        envelope = build_signed_envelope(payload, root_key_file=getattr(args, "root_key", None))
        result = await access_client.grant_access(runtime.signed_request(envelope))
    finally:
        await runtime.close()

    print(
        f"Granted {getattr(result, 'role', args.role)} access to "
        f"@{getattr(result, 'consumer_handle', args.consumer)} for {args.service} "
        f"({getattr(result, 'scope', args.scope)})."
    )
    return 0


def cmd_access_revoke(args: argparse.Namespace) -> int:
    try:
        return asyncio.run(_revoke_remote(args, handle=_require_verified_handle()))
    except Exception as exc:
        print(f"Error: {exc}")
        return 1


async def _revoke_remote(args: argparse.Namespace, *, handle: str) -> int:
    runtime = await open_aster_service(getattr(args, "aster", None))
    try:
        access_client = await runtime.access_client()
        payload = _signed_payload(
            "revoke_access",
            handle=handle,
            service_name=args.service,
            consumer_handle=args.consumer,
        )
        envelope = build_signed_envelope(payload, root_key_file=getattr(args, "root_key", None))
        result = await access_client.revoke_access(runtime.signed_request(envelope))
    finally:
        await runtime.close()

    if not getattr(result, "revoked", False):
        print(f"No access grant found for @{args.consumer} on {args.service}.")
        return 1
    print(f"Revoked access for @{getattr(result, 'consumer_handle', args.consumer)} on {args.service}.")
    return 0


def cmd_access_list(args: argparse.Namespace) -> int:
    try:
        return asyncio.run(_list_remote(args, handle=_require_verified_handle()))
    except Exception as exc:
        print(f"Error: {exc}")
        return 1


async def _list_remote(args: argparse.Namespace, *, handle: str) -> int:
    runtime = await open_aster_service(getattr(args, "aster", None))
    try:
        access_client = await runtime.access_client()
        payload = _signed_payload(
            "list_access",
            handle=handle,
            service_name=args.service,
        )
        envelope = build_signed_envelope(payload, root_key_file=getattr(args, "root_key", None))
        result = await access_client.list_access(runtime.signed_request(envelope))
    finally:
        await runtime.close()

    grants = [
        {
            "consumer_handle": getattr(entry, "consumer_handle", ""),
            "role": getattr(entry, "role", ""),
            "scope": getattr(entry, "scope", ""),
            "granted_at": getattr(entry, "granted_at", ""),
        }
        for entry in getattr(result, "grants", [])
    ]

    if getattr(args, "raw_json", False):
        print(json.dumps({"service_name": args.service, "grants": grants}, indent=2))
        return 0

    print(f"{args.service}: {len(grants)} grant(s)")
    for grant in grants:
        print(
            f"  @{grant['consumer_handle']} "
            f"{grant['role'] or 'consumer'} "
            f"{grant['scope'] or 'handle'} "
            f"{grant['granted_at'] or ''}".rstrip()
        )
    return 0


def _delegation_roles(args: argparse.Namespace) -> list[str]:
    if getattr(args, "role", None):
        return sorted(set(args.role))
    return ["consumer"]


def cmd_access_delegation(args: argparse.Namespace) -> int:
    try:
        return asyncio.run(_delegation_remote(args, handle=_require_verified_handle()))
    except Exception as exc:
        print(f"Error: {exc}")
        return 1


async def _delegation_remote(args: argparse.Namespace, *, handle: str) -> int:
    runtime = await open_aster_service(getattr(args, "aster", None))
    try:
        access_client = await runtime.access_client()
        payload = _signed_payload(
            "update_delegation",
            handle=handle,
            service_name=args.service,
            delegation={
                "authority": "consumer",
                "mode": "closed" if args.closed else "open",
                "token_ttl": parse_duration_seconds(args.token_ttl, default=300),
                "rate_limit": args.rate_limit,
                "roles": _delegation_roles(args),
            },
        )
        envelope = build_signed_envelope(payload, root_key_file=getattr(args, "root_key", None))
        result = await access_client.update_delegation(runtime.signed_request(envelope))
    finally:
        await runtime.close()

    delegation = getattr(result, "delegation", None)
    mode = getattr(delegation, "mode", "closed" if args.closed else "open")
    print(f"Updated @{handle}/{args.service} delegation to {mode}.")
    return 0


def cmd_access_public_private(args: argparse.Namespace) -> int:
    from aster_cli.publish import cmd_set_visibility

    visibility = "public" if args.access_command == "public" else "private"
    return cmd_set_visibility(
        argparse.Namespace(
            command="visibility",
            service=args.service,
            visibility=visibility,
            aster=getattr(args, "aster", None),
            root_key=getattr(args, "root_key", None),
        )
    )


def register_access_subparser(subparsers: argparse._SubParsersAction) -> None:
    access_parser = subparsers.add_parser("access", help="Manage Day 0 @aster service access grants")
    access_subparsers = access_parser.add_subparsers(dest="access_command")

    grant_parser = access_subparsers.add_parser("grant", help="Grant a consumer access to a service")
    grant_parser.add_argument("service", help="Published service name")
    grant_parser.add_argument("consumer", help="Consumer handle to grant")
    grant_parser.add_argument("--role", default="consumer", help="Delegated role (default: consumer)")
    grant_parser.add_argument(
        "--scope",
        choices=["handle", "node"],
        default="handle",
        help="Grant scope (default: handle)",
    )
    grant_parser.add_argument("--scope-node-id", default=None, help="Required when --scope node")
    grant_parser.add_argument("--aster", default=None, help="Override @aster service address")
    grant_parser.add_argument("--root-key", default=None, help="Path to root key JSON backup")

    revoke_parser = access_subparsers.add_parser("revoke", help="Revoke a consumer's service access")
    revoke_parser.add_argument("service", help="Published service name")
    revoke_parser.add_argument("consumer", help="Consumer handle to revoke")
    revoke_parser.add_argument("--aster", default=None, help="Override @aster service address")
    revoke_parser.add_argument("--root-key", default=None, help="Path to root key JSON backup")

    list_parser = access_subparsers.add_parser("list", help="List access grants for a published service")
    list_parser.add_argument("service", help="Published service name")
    list_parser.add_argument("--aster", default=None, help="Override @aster service address")
    list_parser.add_argument("--root-key", default=None, help="Path to root key JSON backup")
    list_parser.add_argument("--json", action="store_true", dest="raw_json", help="Output raw JSON")

    delegation_parser = access_subparsers.add_parser("delegation", help="Update delegated access mode for a service")
    delegation_parser.add_argument("service", help="Published service name")
    delegation_mode = delegation_parser.add_mutually_exclusive_group()
    delegation_mode.add_argument("--open", action="store_true", help="Allow open enrollment")
    delegation_mode.add_argument("--closed", action="store_true", help="Require explicit grants")
    delegation_parser.add_argument("--token-ttl", default="5m", help="Delegated token TTL (default: 5m)")
    delegation_parser.add_argument("--rate-limit", default=None, help='Delegated issuance rate limit like "1/60m"')
    delegation_parser.add_argument("--role", action="append", default=[], help="Delegated role (repeatable)")
    delegation_parser.add_argument("--aster", default=None, help="Override @aster service address")
    delegation_parser.add_argument("--root-key", default=None, help="Path to root key JSON backup")

    public_parser = access_subparsers.add_parser("public", help="Make a published service discoverable")
    public_parser.add_argument("service", help="Published service name")
    public_parser.add_argument("--aster", default=None, help="Override @aster service address")
    public_parser.add_argument("--root-key", default=None, help="Path to root key JSON backup")

    private_parser = access_subparsers.add_parser("private", help="Hide a published service from discovery")
    private_parser.add_argument("service", help="Published service name")
    private_parser.add_argument("--aster", default=None, help="Override @aster service address")
    private_parser.add_argument("--root-key", default=None, help="Path to root key JSON backup")


def run_access_command(args: argparse.Namespace) -> int:
    if args.access_command == "grant":
        return cmd_access_grant(args)
    if args.access_command == "revoke":
        return cmd_access_revoke(args)
    if args.access_command == "list":
        return cmd_access_list(args)
    if args.access_command == "delegation":
        return cmd_access_delegation(args)
    if args.access_command in {"public", "private"}:
        return cmd_access_public_private(args)
    print("Usage: aster access [grant|revoke|list] ...")
    return 1
