"""
Endpoint configuration loader.

Loads EndpointConfig from a TOML file and/or ``ASTER_*`` environment variables.
Environment variables always override file values.

TOML example (``aster.toml``)::

    alpns = ["myproto/1", "myproto/2"]
    relay_mode = "default"          # "default" | "disabled" | "staging" | "custom"
    bind_addr = "0.0.0.0:9000"
    portmapper_config = "disabled"  # "enabled" | "disabled"
    proxy_url = "http://proxy.corp:8080"
    # secret_key = "<base64-encoded 32 bytes>"

Environment variables (``ASTER_<FIELD_UPPER_CASE>``)::

    ASTER_ALPNS=myproto/1,myproto/2
    ASTER_RELAY_MODE=default
    ASTER_SECRET_KEY=<base64-encoded 32 bytes>
    ASTER_BIND_ADDR=0.0.0.0:9000
    ASTER_CLEAR_IP_TRANSPORTS=true
    ASTER_CLEAR_RELAY_TRANSPORTS=false
    ASTER_PORTMAPPER_CONFIG=disabled
    ASTER_PROXY_URL=http://proxy:8080
    ASTER_PROXY_FROM_ENV=true
    ASTER_ENABLE_MONITORING=false
    ASTER_ENABLE_HOOKS=false
    ASTER_HOOK_TIMEOUT_MS=5000
"""

from __future__ import annotations

import base64
import os
import sys
from pathlib import Path
from typing import Optional, Union

if sys.version_info >= (3, 11):
    import tomllib
else:
    try:
        import tomli as tomllib  # type: ignore[no-redef]
    except ImportError as _tomli_err:
        raise ImportError(
            "Install tomli to read TOML config files on Python < 3.11: "
            "pip install tomli"
        ) from _tomli_err

from ._aster_python import EndpointConfig

_BOOL_TRUE = {"1", "true", "yes", "on"}
_BOOL_FALSE = {"0", "false", "no", "off"}


def _parse_bool(value: str, var: str) -> bool:
    v = value.strip().lower()
    if v in _BOOL_TRUE:
        return True
    if v in _BOOL_FALSE:
        return False
    raise ValueError(
        f"{var}: expected true/false/1/0, got {value!r}"
    )


def _parse_alpns(value: str) -> list:
    """Comma-separated protocol strings → list[bytes]."""
    return [p.strip().encode() for p in value.split(",") if p.strip()]


def load_endpoint_config(
    path: Optional[Union[str, Path]] = None,
) -> EndpointConfig:
    """Load an :class:`EndpointConfig` from a TOML file and/or environment.

    Resolution order (later wins):

    1. Built-in defaults (empty ``alpns``, everything off).
    2. Config file values, if *path* is provided.
    3. ``ASTER_*`` environment variables.

    Args:
        path: Optional path to a ``.toml`` config file.  If *None*, only
              environment variables are applied.

    Returns:
        A fully-constructed :class:`EndpointConfig`.

    Raises:
        FileNotFoundError: If *path* is given but the file does not exist.
        ValueError: If any field value is invalid.
        ``tomllib.TOMLDecodeError``: If the TOML file is malformed.
    """
    data: dict = {
        "alpns": [],
        "relay_mode": None,
        "secret_key": None,
        "enable_monitoring": False,
        "enable_hooks": False,
        "hook_timeout_ms": 5000,
        "bind_addr": None,
        "clear_ip_transports": False,
        "clear_relay_transports": False,
        "portmapper_config": None,
        "proxy_url": None,
        "proxy_from_env": False,
    }

    if path is not None:
        with Path(path).open("rb") as fh:
            raw = tomllib.load(fh)
        _merge_toml(data, raw, source=str(path))

    _apply_env(data)

    return EndpointConfig(
        alpns=data["alpns"],
        relay_mode=data["relay_mode"],
        secret_key=data["secret_key"],
        enable_monitoring=data["enable_monitoring"],
        enable_hooks=data["enable_hooks"],
        hook_timeout_ms=data["hook_timeout_ms"],
        bind_addr=data["bind_addr"],
        clear_ip_transports=data["clear_ip_transports"],
        clear_relay_transports=data["clear_relay_transports"],
        portmapper_config=data["portmapper_config"],
        proxy_url=data["proxy_url"],
        proxy_from_env=data["proxy_from_env"],
    )


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------

def _merge_toml(data: dict, raw: dict, source: str) -> None:
    """Merge parsed TOML values into *data*, coercing types as needed."""

    def _err(field: str, msg: str) -> ValueError:
        return ValueError(f"{source}: field '{field}': {msg}")

    if "alpns" in raw:
        vals = raw["alpns"]
        if not isinstance(vals, list):
            raise _err("alpns", "must be a list of strings")
        data["alpns"] = [
            v.encode() if isinstance(v, str) else bytes(v)
            for v in vals
        ]

    if "relay_mode" in raw:
        data["relay_mode"] = str(raw["relay_mode"]) if raw["relay_mode"] is not None else None

    if "secret_key" in raw:
        raw_key = raw["secret_key"]
        if raw_key is None:
            data["secret_key"] = None
        else:
            try:
                data["secret_key"] = base64.b64decode(raw_key)
            except Exception as exc:
                raise _err("secret_key", f"must be base64-encoded: {exc}") from exc

    _BOOL_FIELDS = (
        "enable_monitoring",
        "enable_hooks",
        "clear_ip_transports",
        "clear_relay_transports",
        "proxy_from_env",
    )
    for field in _BOOL_FIELDS:
        if field in raw:
            v = raw[field]
            if not isinstance(v, bool):
                raise _err(field, "must be a boolean (true or false)")
            data[field] = v

    if "hook_timeout_ms" in raw:
        v = raw["hook_timeout_ms"]
        if not isinstance(v, int) or v < 0:
            raise _err("hook_timeout_ms", "must be a non-negative integer")
        data["hook_timeout_ms"] = v

    for field in ("bind_addr", "portmapper_config", "proxy_url"):
        if field in raw:
            v = raw[field]
            data[field] = None if v is None else str(v)


def _apply_env(data: dict) -> None:
    """Override *data* with ``ASTER_*`` environment variables."""
    env = os.environ

    if (v := env.get("ASTER_ALPNS")) is not None:
        data["alpns"] = _parse_alpns(v)

    if (v := env.get("ASTER_RELAY_MODE")) is not None:
        data["relay_mode"] = v.strip() or None

    if (v := env.get("ASTER_SECRET_KEY")) is not None:
        stripped = v.strip()
        if not stripped:
            data["secret_key"] = None
        else:
            try:
                data["secret_key"] = base64.b64decode(stripped)
            except Exception as exc:
                raise ValueError(
                    f"ASTER_SECRET_KEY: must be a base64-encoded 32-byte key: {exc}"
                ) from exc

    _BOOL_FIELDS = (
        "enable_monitoring",
        "enable_hooks",
        "clear_ip_transports",
        "clear_relay_transports",
        "proxy_from_env",
    )
    for field in _BOOL_FIELDS:
        var = f"ASTER_{field.upper()}"
        if (v := env.get(var)) is not None:
            data[field] = _parse_bool(v, var)

    if (v := env.get("ASTER_HOOK_TIMEOUT_MS")) is not None:
        try:
            data["hook_timeout_ms"] = int(v)
        except ValueError as exc:
            raise ValueError(f"ASTER_HOOK_TIMEOUT_MS: must be an integer: {exc}") from exc

    for field in ("bind_addr", "portmapper_config", "proxy_url"):
        var = f"ASTER_{field.upper()}"
        if (v := env.get(var)) is not None:
            data[field] = v.strip() or None
