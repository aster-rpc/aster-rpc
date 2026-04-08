"""Identity-oriented CLI commands for Aster."""

from __future__ import annotations

import argparse
import os
import re
from datetime import datetime, timezone
from types import SimpleNamespace
from typing import Any

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


def _require_demo(args: argparse.Namespace, action: str) -> int:
    print(
        f"{action} is wired as a local preview only right now.\n"
        f"Use `--demo` to exercise the CLI flow before the @aster service client lands.",
    )
    return 2


def cmd_status(args: argparse.Namespace) -> int:
    state = get_local_identity_state()
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
    return 0


def cmd_join(args: argparse.Namespace) -> int:
    created = ensure_root_key_exists()
    if created:
        print("Created your Aster identity.")

    if not getattr(args, "demo", False):
        return _require_demo(args, "aster join")

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


def cmd_verify(args: argparse.Namespace) -> int:
    _profile_name, profile, _config = get_active_profile()
    if not getattr(args, "demo", False):
        return _require_demo(args, "aster verify")

    if args.resend:
        if profile.get("handle_status") != "pending":
            print("No pending verification to resend.")
            return 1
        print(f"Verification code: {_DEMO_VERIFICATION_CODE}")
        print("Local preview code re-issued.")
        return 0

    code = args.code or _prompt("Verification code")
    if profile.get("handle_status") != "pending":
        print("No pending verification.")
        return 1
    if code != _DEMO_VERIFICATION_CODE:
        print("Invalid code for local preview. Use 123456.")
        return 1

    handle = profile.get("handle", "")
    update_active_profile(handle_status="verified")
    print(f"Handle @{handle} verified in local preview mode.")
    return 0


def register_join_subparser(subparsers: argparse._SubParsersAction) -> None:
    join_parser = subparsers.add_parser("join", help="Claim an Aster handle")
    join_parser.add_argument("--handle", default=None, help="Handle to claim")
    join_parser.add_argument("--email", default=None, help="Verification email")
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
    verify_parser.add_argument("--resend", action="store_true", help="Resend the code")
    verify_parser.add_argument(
        "--demo",
        action="store_true",
        help="Run the local preview flow without calling @aster",
    )

    status_parser = subparsers.add_parser("status", help="Show local Aster identity state")
    status_parser.add_argument("--json", action="store_true", dest="raw_json", help=argparse.SUPPRESS)

    whoami_parser = subparsers.add_parser("whoami", help="Alias for `aster status`")
    whoami_parser.add_argument("--json", action="store_true", dest="raw_json", help=argparse.SUPPRESS)


def run_join_command(args: argparse.Namespace) -> int:
    if args.command in {"status", "whoami"}:
        return cmd_status(args)
    if args.command == "join":
        return cmd_join(args)
    if args.command == "verify":
        return cmd_verify(args)
    return 1
