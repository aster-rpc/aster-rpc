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
from typing import Any

ASTER_DIR = Path(os.path.expanduser("~/.aster"))
CONFIG_PATH = ASTER_DIR / "config.toml"

DEFAULT_ASTER_SERVICE = {
    "enabled": True,
    "node_id": "",
    "relay": "",
    "offline_banner": True,
}
DEFAULT_PROFILE_FIELDS = {
    "handle": "",
    "handle_status": "unregistered",
    "handle_claimed_at": "",
    "email": "",
    "signer": "local",
    "published_services": "",
}


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
        return _normalize_config({"active_profile": "default", "profiles": {}})
    with CONFIG_PATH.open("rb") as f:
        return _normalize_config(tomllib.load(f))


def _normalize_profile(profile: dict[str, Any]) -> dict[str, Any]:
    normalized = dict(profile)
    for key, value in DEFAULT_PROFILE_FIELDS.items():
        normalized.setdefault(key, value)
    return normalized


def _normalize_config(data: dict[str, Any]) -> dict[str, Any]:
    normalized = dict(data)
    normalized.setdefault("active_profile", "default")
    profiles = normalized.setdefault("profiles", {})
    for name, profile in list(profiles.items()):
        profiles[name] = _normalize_profile(profile if isinstance(profile, dict) else {})
    aster_service = normalized.setdefault("aster_service", {})
    for key, value in DEFAULT_ASTER_SERVICE.items():
        aster_service.setdefault(key, value)
    return normalized


def _toml_value(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, list):
        items = ", ".join(_toml_value(item) for item in value)
        return f"[{items}]"
    escaped = str(value).replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def _save_config(data: dict) -> None:
    """Write config to ~/.aster/config.toml with 0600 permissions."""
    ASTER_DIR.mkdir(parents=True, exist_ok=True)
    data = _normalize_config(data)

    lines = []
    lines.append(f'active_profile = "{data.get("active_profile", "default")}"')
    lines.append("")

    aster_service = data.get("aster_service", {})
    if aster_service:
        lines.append("[aster_service]")
        for key, value in aster_service.items():
            lines.append(f"{key} = {_toml_value(value)}")
        lines.append("")

    for name, profile in data.get("profiles", {}).items():
        lines.append(f"[profiles.{name}]")
        for key, value in profile.items():
            lines.append(f"{key} = {_toml_value(value)}")
        lines.append("")

    content = "\n".join(lines)
    tmp = CONFIG_PATH.with_suffix(".tmp")
    tmp.write_text(content)
    os.chmod(tmp, 0o600)
    tmp.replace(CONFIG_PATH)


def _active_profile(config: dict) -> str:
    return config.get("active_profile", "default")


def get_active_profile_name(config: dict | None = None) -> str:
    config = _load_config() if config is None else _normalize_config(config)
    return _active_profile(config)


def get_active_profile(config: dict | None = None) -> tuple[str, dict[str, Any], dict[str, Any]]:
    config = _load_config() if config is None else _normalize_config(config)
    name = _active_profile(config)
    profiles = config.setdefault("profiles", {})
    profile = profiles.setdefault(name, {})
    profiles[name] = _normalize_profile(profile)
    return name, profiles[name], config


def update_active_profile(**updates: Any) -> tuple[str, dict[str, Any]]:
    name, profile, config = get_active_profile()
    for key, value in updates.items():
        profile[key] = value
    config["profiles"][name] = profile
    _save_config(config)
    return name, profile


def update_aster_service(**updates: Any) -> dict[str, Any]:
    config = _load_config()
    aster_service = config.setdefault("aster_service", {})
    for key, value in updates.items():
        aster_service[key] = value
    _save_config(config)
    return aster_service


def get_aster_service_config(config: dict | None = None) -> dict[str, Any]:
    config = _load_config() if config is None else _normalize_config(config)
    return dict(config.get("aster_service", {}))


def get_published_services(profile: dict[str, Any] | None = None) -> list[str]:
    if profile is None:
        _name, profile, _config = get_active_profile()
    raw = str(profile.get("published_services", "")).strip()
    if not raw:
        return []
    return [item for item in (part.strip() for part in raw.split(",")) if item]


def set_published_services(services: list[str]) -> tuple[str, dict[str, Any]]:
    services = sorted(set(services))
    return update_active_profile(published_services=",".join(services))


def print_active_profile_hint() -> None:
    """Print the active profile name when multiple profiles exist."""
    config = _load_config()
    profiles = config.get("profiles", {})
    if len(profiles) > 1:
        active = _active_profile(config)
        print(f"[profile: {active}]", file=sys.stderr)


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
