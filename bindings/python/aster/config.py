"""
Aster configuration — endpoint, trust, and storage.

Three layers, each overriding the previous:

1. Built-in defaults (ephemeral key, in-memory store, all gates open).
2. TOML config file (``aster.toml``).
3. ``ASTER_*`` environment variables.

TOML example (``aster.toml``)::

    [trust]
    root_key_file = "~/.aster/root.key"    # JSON with private_key + public_key hex
    allow_all_consumers = false
    allow_all_producers = true

    [storage]
    path = "/var/lib/aster"                # omit for in-memory

    [network]
    relay_mode = "default"
    bind_addr = "0.0.0.0:9000"
    portmapper_config = "disabled"
    # secret_key = "<base64-encoded 32 bytes>"

Environment variables::

    # Trust
    ASTER_ROOT_KEY_FILE=~/.aster/root.key
    ASTER_ALLOW_ALL_CONSUMERS=false
    ASTER_ALLOW_ALL_PRODUCERS=true

    # Storage
    ASTER_STORAGE_PATH=/var/lib/aster

    # Network (same as before)
    ASTER_RELAY_MODE=default
    ASTER_SECRET_KEY=<base64>
    ASTER_BIND_ADDR=0.0.0.0:9000
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

from ._aster import EndpointConfig

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


# ============================================================================
# AsterConfig — unified configuration for AsterServer
# ============================================================================


from dataclasses import dataclass, field as dc_field
import json
import logging

_config_logger = logging.getLogger(__name__)


@dataclass
class AsterConfig:
    """Unified configuration for :class:`AsterServer`.

    Combines trust (root key, admission policy), storage (memory vs
    persistent), and networking (relay, bind address, etc.) into one
    object.

    Three ways to get one:

    1. **Auto from env** (the default when ``AsterServer`` gets no config)::

           config = AsterConfig.from_env()

    2. **From a TOML file** (with env overrides)::

           config = AsterConfig.from_file("aster.toml")

    3. **Inline** (testing, scripts)::

           config = AsterConfig(root_pubkey=pub, root_privkey=priv)
    """

    # ── Trust ────────────────────────────────────────────────────────────
    root_key_file: str | None = None
    """Path to a JSON file with ``private_key`` and ``public_key`` hex fields.
    Loaded lazily by :meth:`resolve_root_key`."""

    root_pubkey: bytes | None = None
    """32-byte ed25519 public key (overrides root_key_file if set)."""

    root_privkey: bytes | None = None
    """32-byte ed25519 private key seed (overrides root_key_file if set)."""

    allow_all_consumers: bool = False
    """Skip consumer admission gate. Default: gate consumers."""

    allow_all_producers: bool = True
    """Skip producer admission gate. Default: allow all producers."""

    # ── Storage ──────────────────────────────────────────────────────────
    storage_path: str | None = None
    """If set, use FsStore at this path; otherwise in-memory."""

    # ── Network (forwarded to EndpointConfig) ────────────────────────────
    relay_mode: str | None = None
    secret_key: bytes | None = None
    bind_addr: str | None = None
    enable_monitoring: bool = False
    enable_hooks: bool = False
    hook_timeout_ms: int = 5000
    clear_ip_transports: bool = False
    clear_relay_transports: bool = False
    portmapper_config: str | None = None
    proxy_url: str | None = None
    proxy_from_env: bool = False

    # ── Lifecycle ────────────────────────────────────────────────────────

    def resolve_root_key(self) -> tuple[bytes | None, bytes | None]:
        """Return ``(privkey, pubkey)``, loading from file if needed.

        Resolution order:
        1. Inline ``root_pubkey`` / ``root_privkey`` (highest priority).
        2. ``root_key_file`` JSON (``{"private_key": "<hex>", "public_key": "<hex>"}``).
        3. Generate an ephemeral keypair (with a log message).
        """
        priv = self.root_privkey
        pub = self.root_pubkey

        if pub is not None:
            return priv, pub

        if self.root_key_file:
            path = os.path.expanduser(self.root_key_file)
            if os.path.exists(path):
                with open(path) as f:
                    kd = json.load(f)
                priv = bytes.fromhex(kd["private_key"])
                pub = bytes.fromhex(kd["public_key"])
                _config_logger.info("Loaded root key from %s", path)
                return priv, pub
            _config_logger.warning("Root key file %s not found", path)

        # No key configured and admission is needed — generate ephemeral.
        if not self.allow_all_consumers or not self.allow_all_producers:
            from .trust.signing import generate_root_keypair
            priv, pub = generate_root_keypair()
            _config_logger.info(
                "Generated ephemeral root key "
                "(set ASTER_ROOT_KEY_FILE to persist)"
            )
            return priv, pub

        # Both gates off — no key needed.
        return None, None

    def to_endpoint_config(self) -> EndpointConfig | None:
        """Build an :class:`EndpointConfig` from the network fields.

        Returns ``None`` when all network fields are at their defaults
        (so ``IrohNode.memory_with_alpns`` can use the fast
        ``Endpoint::bind(presets::N0)`` path).
        """
        has_custom = any([
            self.relay_mode, self.secret_key, self.bind_addr,
            self.enable_monitoring, self.enable_hooks,
            self.clear_ip_transports, self.clear_relay_transports,
            self.portmapper_config, self.proxy_url, self.proxy_from_env,
            self.hook_timeout_ms != 5000,
        ])
        if not has_custom:
            return None
        return EndpointConfig(
            alpns=[],  # Router sets ALPNs
            relay_mode=self.relay_mode,
            secret_key=self.secret_key,
            enable_monitoring=self.enable_monitoring,
            enable_hooks=self.enable_hooks,
            hook_timeout_ms=self.hook_timeout_ms,
            bind_addr=self.bind_addr,
            clear_ip_transports=self.clear_ip_transports,
            clear_relay_transports=self.clear_relay_transports,
            portmapper_config=self.portmapper_config,
            proxy_url=self.proxy_url,
            proxy_from_env=self.proxy_from_env,
        )

    # ── Factory methods ──────────────────────────────────────────────────

    @classmethod
    def from_env(cls) -> "AsterConfig":
        """Build config from ``ASTER_*`` environment variables only."""
        return cls._load(toml_data=None)

    @classmethod
    def from_file(cls, path: str | Path) -> "AsterConfig":
        """Build config from a TOML file, with env-var overrides."""
        with Path(path).open("rb") as fh:
            raw = tomllib.load(fh)
        return cls._load(toml_data=raw)

    @classmethod
    def _load(cls, toml_data: dict | None) -> "AsterConfig":
        kwargs: dict = {}
        env = os.environ

        # ── Trust (TOML [trust] section) ─────────────────────────────────
        trust = (toml_data or {}).get("trust", {})
        if "root_key_file" in trust:
            kwargs["root_key_file"] = str(trust["root_key_file"])
        if "allow_all_consumers" in trust:
            kwargs["allow_all_consumers"] = bool(trust["allow_all_consumers"])
        if "allow_all_producers" in trust:
            kwargs["allow_all_producers"] = bool(trust["allow_all_producers"])

        # ── Storage (TOML [storage] section) ─────────────────────────────
        storage = (toml_data or {}).get("storage", {})
        if "path" in storage:
            kwargs["storage_path"] = str(storage["path"])

        # ── Network (TOML [network] section) ─────────────────────────────
        network = (toml_data or {}).get("network", {})
        _NET_FIELDS = (
            "relay_mode", "bind_addr", "portmapper_config", "proxy_url",
        )
        for f in _NET_FIELDS:
            if f in network:
                kwargs[f] = str(network[f]) if network[f] is not None else None
        _NET_BOOLS = (
            "enable_monitoring", "enable_hooks",
            "clear_ip_transports", "clear_relay_transports", "proxy_from_env",
        )
        for f in _NET_BOOLS:
            if f in network:
                kwargs[f] = bool(network[f])
        if "hook_timeout_ms" in network:
            kwargs["hook_timeout_ms"] = int(network["hook_timeout_ms"])
        if "secret_key" in network and network["secret_key"] is not None:
            kwargs["secret_key"] = base64.b64decode(network["secret_key"])

        # ── Env overrides (always win) ───────────────────────────────────
        if (v := env.get("ASTER_ROOT_KEY_FILE")) is not None:
            kwargs["root_key_file"] = v.strip()
        if (v := env.get("ASTER_ALLOW_ALL_CONSUMERS")) is not None:
            kwargs["allow_all_consumers"] = _parse_bool(v, "ASTER_ALLOW_ALL_CONSUMERS")
        if (v := env.get("ASTER_ALLOW_ALL_PRODUCERS")) is not None:
            kwargs["allow_all_producers"] = _parse_bool(v, "ASTER_ALLOW_ALL_PRODUCERS")
        if (v := env.get("ASTER_STORAGE_PATH")) is not None:
            kwargs["storage_path"] = v.strip() or None

        # Network env vars (reuse existing names)
        if (v := env.get("ASTER_RELAY_MODE")) is not None:
            kwargs["relay_mode"] = v.strip() or None
        if (v := env.get("ASTER_SECRET_KEY")) is not None:
            kwargs["secret_key"] = base64.b64decode(v.strip()) if v.strip() else None
        if (v := env.get("ASTER_BIND_ADDR")) is not None:
            kwargs["bind_addr"] = v.strip() or None
        for f in _NET_BOOLS:
            var = f"ASTER_{f.upper()}"
            if (v := env.get(var)) is not None:
                kwargs[f] = _parse_bool(v, var)
        if (v := env.get("ASTER_HOOK_TIMEOUT_MS")) is not None:
            kwargs["hook_timeout_ms"] = int(v)
        for f in ("portmapper_config", "proxy_url"):
            var = f"ASTER_{f.upper()}"
            if (v := env.get(var)) is not None:
                kwargs[f] = v.strip() or None

        return cls(**kwargs)
