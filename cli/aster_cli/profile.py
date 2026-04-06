"""
aster_cli.profile — Operator profile management.

Profiles are stored in ``~/.aster/config.toml`` and represent deployment
meshes (dev, staging, prod). Each profile holds the root public key;
the corresponding private key is in the OS keyring.

Commands:
    aster profile list
    aster profile create <name>
    aster profile use <name>
    aster profile show [name]
    aster profile delete <name>
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

ASTER_DIR = Path(os.path.expanduser("~/.aster"))
CONFIG_PATH = ASTER_DIR / "config.toml"


def _load_config() -> dict:
    """Load ~/.aster/config.toml, returning empty structure if missing."""
    if sys.version_info >= (3, 11):
        import tomllib
    else:
        try:
            import tomli as tomllib  # type: ignore[no-redef]
        except ImportError:
            print("Error: install tomli for Python < 3.11: pip install tomli", file=sys.stderr)
            sys.exit(1)

    if not CONFIG_PATH.exists():
        return {"active_profile": "default", "profiles": {}}
    with CONFIG_PATH.open("rb") as f:
        return tomllib.load(f)


def _save_config(data: dict) -> None:
    """Write config to ~/.aster/config.toml with 0600 permissions."""
    ASTER_DIR.mkdir(parents=True, exist_ok=True)

    lines = []
    lines.append(f'active_profile = "{data.get("active_profile", "default")}"')
    lines.append("")

    for name, profile in data.get("profiles", {}).items():
        lines.append(f"[profiles.{name}]")
        for key, value in profile.items():
            if isinstance(value, str):
                lines.append(f'{key} = "{value}"')
            elif isinstance(value, bool):
                lines.append(f"{key} = {'true' if value else 'false'}")
            elif isinstance(value, (int, float)):
                lines.append(f"{key} = {value}")
        lines.append("")

    content = "\n".join(lines)
    tmp = CONFIG_PATH.with_suffix(".tmp")
    tmp.write_text(content)
    os.chmod(tmp, 0o600)
    tmp.replace(CONFIG_PATH)


def _active_profile(config: dict) -> str:
    return config.get("active_profile", "default")


# ── Commands ─────────────────────────────────────────────────────────────


def cmd_list(args) -> int:
    config = _load_config()
    active = _active_profile(config)
    profiles = config.get("profiles", {})

    if not profiles:
        print("No profiles configured. Run: aster profile create <name>")
        return 0

    for name in sorted(profiles):
        marker = " *" if name == active else ""
        pubkey = profiles[name].get("root_pubkey", "<no root key>")
        print(f"  {name}{marker}  root_pubkey={pubkey[:16]}...")
    return 0


def cmd_create(args) -> int:
    name = args.name
    config = _load_config()
    profiles = config.setdefault("profiles", {})

    if name in profiles:
        print(f"Error: profile '{name}' already exists.", file=sys.stderr)
        return 1

    profiles[name] = {}
    if not config.get("active_profile") or len(profiles) == 1:
        config["active_profile"] = name

    _save_config(config)
    print(f"Created profile '{name}'.")
    if config["active_profile"] == name:
        print(f"  (set as active profile)")
    return 0


def cmd_use(args) -> int:
    name = args.name
    config = _load_config()
    profiles = config.get("profiles", {})

    if name not in profiles:
        print(f"Error: profile '{name}' does not exist.", file=sys.stderr)
        print(f"  Available: {', '.join(sorted(profiles)) or '(none)'}")
        return 1

    config["active_profile"] = name
    _save_config(config)
    print(f"Active profile set to '{name}'.")
    return 0


def cmd_show(args) -> int:
    config = _load_config()
    name = args.name or _active_profile(config)
    profiles = config.get("profiles", {})

    if name not in profiles:
        print(f"Error: profile '{name}' does not exist.", file=sys.stderr)
        return 1

    profile = profiles[name]
    is_active = " (active)" if name == _active_profile(config) else ""
    print(f"Profile: {name}{is_active}")
    print(f"  root_pubkey: {profile.get('root_pubkey', '<not set>')}")

    from aster_cli.credentials import has_keyring, get_root_privkey
    if has_keyring():
        has_key = get_root_privkey(name) is not None
        print(f"  root_privkey: {'****... (in keyring)' if has_key else '<not set>'}")
    else:
        print(f"  root_privkey: <keyring not available>")

    for key, value in profile.items():
        if key == "root_pubkey":
            continue
        print(f"  {key}: {value}")
    return 0


def cmd_delete(args) -> int:
    name = args.name
    config = _load_config()
    profiles = config.get("profiles", {})

    if name not in profiles:
        print(f"Error: profile '{name}' does not exist.", file=sys.stderr)
        return 1

    if not args.yes:
        confirm = input(f"Delete profile '{name}'? [y/N] ").strip().lower()
        if confirm not in ("y", "yes"):
            print("Cancelled.")
            return 0

    del profiles[name]

    from aster_cli.credentials import delete_root_privkey
    delete_root_privkey(name)

    if config.get("active_profile") == name:
        config["active_profile"] = next(iter(sorted(profiles)), "default")

    _save_config(config)
    print(f"Deleted profile '{name}'.")
    return 0


# ── Argparse registration ────────────────────────────────────────────────


def register_profile_subparser(subparsers) -> None:
    profile_parser = subparsers.add_parser("profile", help="Manage operator profiles")
    profile_sub = profile_parser.add_subparsers(dest="profile_command")

    profile_sub.add_parser("list", help="List profiles")

    create_p = profile_sub.add_parser("create", help="Create a new profile")
    create_p.add_argument("name", help="Profile name")

    use_p = profile_sub.add_parser("use", help="Set the active profile")
    use_p.add_argument("name", help="Profile name")

    show_p = profile_sub.add_parser("show", help="Show profile details")
    show_p.add_argument("name", nargs="?", default=None, help="Profile name (default: active)")

    del_p = profile_sub.add_parser("delete", help="Delete a profile")
    del_p.add_argument("name", help="Profile name")
    del_p.add_argument("--yes", "-y", action="store_true", help="Skip confirmation")


def run_profile_command(args) -> int:
    cmd = args.profile_command
    if cmd == "list":
        return cmd_list(args)
    elif cmd == "create":
        return cmd_create(args)
    elif cmd == "use":
        return cmd_use(args)
    elif cmd == "show":
        return cmd_show(args)
    elif cmd == "delete":
        return cmd_delete(args)
    else:
        print("Usage: aster profile {list|create|use|show|delete}", file=sys.stderr)
        return 1
