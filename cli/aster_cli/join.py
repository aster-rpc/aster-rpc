"""Identity-oriented CLI commands for Aster."""

from __future__ import annotations

import argparse
import asyncio
import importlib
import json
import os
import re
from datetime import datetime, timezone
from types import SimpleNamespace
from typing import Any

from aster_cli.aster_service import (
    build_signed_envelope,
    generate_nonce,
    now_epoch_seconds,
    open_aster_service,
)
from aster_cli.handle_validation import validate_handle
from aster_cli.profile import (
    get_active_profile,
    get_aster_service_config,
    update_active_profile,
)

_EMAIL_RE = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")
_DEMO_VERIFICATION_CODE = "123456"


def now_iso() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def get_local_identity_state() -> dict[str, Any]:
    profile_name, profile, config = get_active_profile()
    root_pubkey = profile.get("root_pubkey", "")
    handle = profile.get("handle", "")
    handle_status = profile.get("handle_status", "unregistered") or "unregistered"
    display = display_handle_for_profile(profile)
    aster_service = get_aster_service_config(config)
    air_gapped = not aster_service.get("enabled", True)
    return {
        "profile_name": profile_name,
        "profile": profile,
        "root_pubkey": root_pubkey,
        "handle": handle,
        "handle_status": handle_status,
        "email": profile.get("email", ""),
        "display_handle": display,
        "air_gapped": air_gapped,
    }


def display_handle_for_profile(profile: dict[str, Any]) -> str:
    handle = str(profile.get("handle", "")).strip()
    status = str(profile.get("handle_status", "unregistered") or "unregistered")
    root_pubkey = str(profile.get("root_pubkey", "")).strip()
    if handle and status in {"pending", "verified"}:
        return f"@{handle}"
    if root_pubkey:
        return f"@{root_pubkey[:12]}"
    return "@local"


def ensure_root_key_exists() -> bool:
    _profile_name, profile, _config = get_active_profile()
    if profile.get("root_pubkey"):
        return False
    from aster_cli.keygen import _keygen_root

    args = SimpleNamespace(
        out=os.path.expanduser("~/.aster/root.key"),
        profile=None,
    )
    rc = _keygen_root(args)
    if rc != 0:
        raise SystemExit(rc)
    return True


def _prompt(prompt: str, default: str | None = None) -> str:
    suffix = f" [{default}]" if default else ""
    value = input(f"{prompt}{suffix}: ").strip()
    return value or (default or "")


def cmd_status(args: argparse.Namespace) -> int:
    state = get_local_identity_state()
    remote_error = None
    if (
        not getattr(args, "local_only", False)
        and state["root_pubkey"]
        and not state["air_gapped"]
    ):
        try:
            remote = asyncio.run(_fetch_remote_status(args))
        except Exception as exc:
            remote = None
            remote_error = str(exc)
        if remote is not None:
            state["remote"] = remote
            state["handle"] = remote.get("handle", state["handle"])
            state["handle_status"] = remote.get("status", state["handle_status"])
            state["display_handle"] = f"@{state['handle']}" if state["handle"] else state["display_handle"]
            profile_updates = {
                "handle": state["handle"],
                "handle_status": state["handle_status"],
                "handle_claimed_at": remote.get("registered_at", state["profile"].get("handle_claimed_at", "")),
            }
            if remote.get("display_name") is not None:
                profile_updates["display_name"] = remote.get("display_name")
            if remote.get("bio") is not None:
                profile_updates["bio"] = remote.get("bio")
            if remote.get("url") is not None:
                profile_updates["url"] = remote.get("url")
            update_active_profile(**profile_updates)

    if getattr(args, "raw_json", False):
        payload = {
            "profile": state["profile_name"],
            "identity": state["display_handle"],
            "handle": state["handle"],
            "handle_status": state["handle_status"],
            "root_pubkey": state["root_pubkey"],
            "email": state["email"],
            "air_gapped": state["air_gapped"],
        }
        if "remote" in state:
            payload["remote"] = state["remote"]
        if remote_error:
            payload["remote_error"] = remote_error
        print(json.dumps(payload, indent=2))
        return 0

    print(f"Profile: {state['profile_name']}")
    print(f"Identity: {state['display_handle']}")
    print(f"Handle status: {state['handle_status']}")
    print(f"Root pubkey: {state['root_pubkey'] or '<not configured>'}")
    if state["handle"]:
        print(f"Handle: @{state['handle']}")
    if state["email"]:
        print(f"Email: {state['email']}")
    print(f"@aster service: {'disabled (air-gapped)' if state['air_gapped'] else 'enabled'}")
    claimed_at = state["profile"].get("handle_claimed_at", "")
    if claimed_at:
        print(f"Claimed at: {claimed_at}")
    if "remote" in state:
        print("Remote: reachable")
    elif remote_error:
        print(f"Remote: unavailable ({remote_error})")
    return 0


def cmd_join(args: argparse.Namespace) -> int:
    created = ensure_root_key_exists()
    if created:
        print("Created your Aster identity.")

    handle = args.handle or _prompt("Choose a handle")
    ok, reason = validate_handle(handle)
    if not ok:
        print(f"Error: invalid handle: {reason}")
        return 1

    email = args.email or _prompt("Email address")
    if not _EMAIL_RE.match(email):
        print("Error: invalid email address.")
        return 1

    announcements = bool(args.announcements)

    if getattr(args, "demo", False):
        _name, profile = update_active_profile(
            handle=handle,
            handle_status="pending",
            handle_claimed_at=now_iso(),
            email=email,
            announcements=announcements,
        )
        print(f"Reserved @{handle} in local preview mode.")
        print(f"Verification code: {_DEMO_VERIFICATION_CODE}")
        print("Run `aster verify 123456 --demo` to finish the preview flow.")
        return 0

    try:
        return asyncio.run(_join_remote(args, handle=handle, email=email, announcements=announcements))
    except Exception as exc:
        print(f"Error: {exc}")
        return 1


async def _join_remote(
    args: argparse.Namespace,
    *,
    handle: str,
    email: str,
    announcements: bool,
) -> int:
    runtime = await open_aster_service(getattr(args, "aster", None))
    try:
        profile_client = await runtime.profile_client()
        types_mod = importlib.import_module(
            profile_client.__module__.replace(".services.", ".types.")
        )
        availability = await profile_client.check_availability(
            types_mod.CheckAvailabilityRequest(handle=handle)
        )
        if not getattr(availability, "available", False):
            reason = getattr(availability, "reason", "unavailable")
            print(f"Error: @{handle} is not available: {reason}")
            return 1

        payload = {
            "action": "join",
            "handle": handle,
            "email": email,
            "announcements": announcements,
            "timestamp": now_epoch_seconds(),
            "nonce": generate_nonce(),
        }
        envelope = build_signed_envelope(
            payload,
            root_key_file=getattr(args, "root_key", None),
        )
        await profile_client.join(runtime.signed_request(envelope))
    finally:
        await runtime.close()

    update_active_profile(
        handle=handle,
        handle_status="pending",
        handle_claimed_at=now_iso(),
        email=email,
        announcements=announcements,
    )
    print(f"Reserved @{handle}.")
    print("Verification code sent if delivery is configured on this @aster service.")
    print("Run `aster verify <code>` to finish registration.")
    return 0


def cmd_verify(args: argparse.Namespace) -> int:
    _profile_name, profile, _config = get_active_profile()
    if args.resend:
        if profile.get("handle_status") != "pending":
            print("No pending verification to resend.")
            return 1
        if not getattr(args, "demo", False):
            try:
                return asyncio.run(_resend_remote(args, profile=profile))
            except Exception as exc:
                print(f"Error: {exc}")
                return 1
        print(f"Verification code: {_DEMO_VERIFICATION_CODE}")
        print("Local preview code re-issued.")
        return 0

    code = args.code or _prompt("Verification code")
    if profile.get("handle_status") != "pending":
        print("No pending verification.")
        return 1
    if getattr(args, "demo", False) and code != _DEMO_VERIFICATION_CODE:
        print("Invalid code for local preview. Use 123456.")
        return 1

    if not getattr(args, "demo", False):
        try:
            return asyncio.run(_verify_remote(args, profile=profile, code=code))
        except Exception as exc:
            print(f"Error: {exc}")
            return 1

    handle = profile.get("handle", "")
    update_active_profile(handle_status="verified")
    print(f"Handle @{handle} verified in local preview mode.")
    return 0


async def _resend_remote(args: argparse.Namespace, *, profile: dict[str, Any]) -> int:
    runtime = await open_aster_service(getattr(args, "aster", None))
    try:
        profile_client = await runtime.profile_client()
        payload = {
            "action": "resend_verification",
            "handle": profile.get("handle", ""),
            "timestamp": now_epoch_seconds(),
            "nonce": generate_nonce(),
        }
        envelope = build_signed_envelope(
            payload,
            root_key_file=getattr(args, "root_key", None),
        )
        await profile_client.resend_verification(runtime.signed_request(envelope))
    finally:
        await runtime.close()

    print("Verification code re-sent.")
    return 0


async def _verify_remote(args: argparse.Namespace, *, profile: dict[str, Any], code: str) -> int:
    runtime = await open_aster_service(getattr(args, "aster", None))
    handle = profile.get("handle", "")
    try:
        profile_client = await runtime.profile_client()
        payload = {
            "action": "verify",
            "handle": handle,
            "code": code,
            "timestamp": now_epoch_seconds(),
            "nonce": generate_nonce(),
        }
        envelope = build_signed_envelope(
            payload,
            root_key_file=getattr(args, "root_key", None),
        )
        await profile_client.verify(runtime.signed_request(envelope))
    finally:
        await runtime.close()

    update_active_profile(handle_status="verified")
    print(f"Handle @{handle} verified.")
    return 0


async def _fetch_remote_status(args: argparse.Namespace) -> dict[str, Any] | None:
    runtime = await open_aster_service(getattr(args, "aster", None))
    try:
        profile_client = await runtime.profile_client()
        payload = {
            "action": "handle_status",
            "timestamp": now_epoch_seconds(),
            "nonce": generate_nonce(),
        }
        envelope = build_signed_envelope(
            payload,
            root_key_file=getattr(args, "root_key", None),
        )
        result = await profile_client.handle_status(runtime.signed_request(envelope))
    finally:
        await runtime.close()

    return {
        "handle": getattr(result, "handle", ""),
        "status": getattr(result, "status", ""),
        "email_masked": getattr(result, "email_masked", ""),
        "display_name": getattr(result, "display_name", None),
        "bio": getattr(result, "bio", None),
        "url": getattr(result, "url", None),
        "registered_at": getattr(result, "registered_at", ""),
        "services_published": getattr(result, "services_published", 0),
        "recovery_codes_remaining": getattr(result, "recovery_codes_remaining", 0),
    }


def register_join_subparser(subparsers: argparse._SubParsersAction) -> None:
    join_parser = subparsers.add_parser("join", help="Claim an Aster handle")
    join_parser.add_argument("--handle", default=None, help="Handle to claim")
    join_parser.add_argument("--email", default=None, help="Verification email")
    join_parser.add_argument("--aster", default=None, help="Override @aster service address")
    join_parser.add_argument("--root-key", default=None, help="Path to root key JSON backup")
    join_parser.add_argument(
        "--announcements",
        action="store_true",
        help="Opt into product announcements",
    )
    join_parser.add_argument(
        "--demo",
        action="store_true",
        help="Run the local preview flow without calling @aster",
    )

    verify_parser = subparsers.add_parser("verify", help="Verify a claimed handle")
    verify_parser.add_argument("code", nargs="?", default=None, help="Verification code")
    verify_parser.add_argument("--aster", default=None, help="Override @aster service address")
    verify_parser.add_argument("--root-key", default=None, help="Path to root key JSON backup")
    verify_parser.add_argument("--resend", action="store_true", help="Resend the code")
    verify_parser.add_argument(
        "--demo",
        action="store_true",
        help="Run the local preview flow without calling @aster",
    )

    status_parser = subparsers.add_parser("status", help="Show local Aster identity state")
    status_parser.add_argument("--aster", default=None, help="Override @aster service address")
    status_parser.add_argument("--root-key", default=None, help="Path to root key JSON backup")
    status_parser.add_argument("--local", action="store_true", dest="local_only", help="Skip remote handle_status")
    status_parser.add_argument("--json", action="store_true", dest="raw_json", help=argparse.SUPPRESS)

    whoami_parser = subparsers.add_parser("whoami", help="Alias for `aster status`")
    whoami_parser.add_argument("--aster", default=None, help="Override @aster service address")
    whoami_parser.add_argument("--root-key", default=None, help="Path to root key JSON backup")
    whoami_parser.add_argument("--local", action="store_true", dest="local_only", help="Skip remote handle_status")
    whoami_parser.add_argument("--json", action="store_true", dest="raw_json", help=argparse.SUPPRESS)


def run_join_command(args: argparse.Namespace) -> int:
    if args.command in {"status", "whoami"}:
        return cmd_status(args)
    if args.command == "join":
        return cmd_join(args)
    if args.command == "verify":
        return cmd_verify(args)
    return 1
