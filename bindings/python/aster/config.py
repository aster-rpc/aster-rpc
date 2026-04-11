"""
Aster configuration -- endpoint, trust, and storage.

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

    # Network
    ASTER_RELAY_MODE=default
    ASTER_SECRET_KEY=<base64>
    ASTER_BIND_ADDR=0.0.0.0:9000
    ASTER_ENABLE_MONITORING=false
    ASTER_ENABLE_HOOKS=false
    ASTER_HOOK_TIMEOUT_MS=5000

    # Logging / observability
    ASTER_LOG_FORMAT=json|text     # default: text
    ASTER_LOG_LEVEL=debug|info|warning|error  # default: info
    ASTER_LOG_MASK=true|false      # mask sensitive fields, default: true
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
        "local_discovery": False,
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
        enable_local_discovery=data["local_discovery"],
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
        "local_discovery",
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
        "local_discovery",
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
# AsterConfig -- unified configuration for AsterServer
# ============================================================================


from dataclasses import dataclass, field as dc_field
import json as _json
import logging

_config_logger = logging.getLogger(__name__)

# Sensitive fields whose values are masked in print_config output.
_MASKED_FIELDS = frozenset({"secret_key", "enrollment_credential_file"})


@dataclass
class AsterConfig:
    """Unified configuration for :class:`AsterServer`.

    Combines trust (root public key, admission policy), storage (memory vs
    persistent), and networking (relay, bind address, etc.) into one object.

    **Trust model (Aster-trust-spec.md §1.1):** The root *private* key is
    offline -- it never touches a running node. Nodes receive only the root
    *public* key (to verify credentials) and optionally an enrollment
    credential (a pre-signed token for mesh join). The founding node of a
    mesh needs no enrollment credential; it bootstraps the accepted-producer
    set with just its own EndpointId.

    Three ways to get an ``AsterConfig``:

    1. **Auto from env** (default when ``AsterServer`` gets no config)::

           config = AsterConfig.from_env()

    2. **From a TOML file** (with env overrides)::

           config = AsterConfig.from_file("aster.toml")

    3. **Inline** (testing, scripts)::

           config = AsterConfig(root_pubkey=pub)
    """

    # ── Trust ────────────────────────────────────────────────────────────

    root_pubkey: bytes | None = None
    """32-byte ed25519 root public key (the deployment trust anchor).
    Highest priority -- overrides ``root_pubkey_file`` when set."""

    root_pubkey_file: str | None = None
    """Path to a file containing the root public key.  Accepts either a
    plain hex string or a JSON object with a ``"public_key"`` field."""

    enrollment_credential_file: str | None = None
    """Path to a JSON enrollment credential (pre-signed by the offline root
    key).  Required when a node joins an existing producer mesh.  Not needed
    for the founding node or for dev/ephemeral mode."""

    allow_all_consumers: bool = False
    """Skip consumer admission gate. Default: gate consumers."""

    enrollment_credential_iid: str | None = None
    """Cloud Instance Identity Document token.  Required when the enrollment
    credential's policy includes ``aster.iid_*`` attributes (AWS/GCP/Azure).
    Set via ``ASTER_ENROLLMENT_CREDENTIAL_IID``."""

    allow_all_consumers: bool = False
    """Skip consumer admission gate. Default: gate consumers."""

    allow_all_producers: bool = True
    """Skip producer admission gate. Default: allow all producers."""

    # ── Connect (consumer-side) ──────────────────────────────────────────

    endpoint_addr: str | None = None
    """Producer's endpoint address (base64 NodeAddr or EndpointId hex string).
    The consumer dials this to reach RPC + admission + blobs/docs/gossip.
    Set via ``ASTER_ENDPOINT_ADDR``."""

    # ── Storage ──────────────────────────────────────────────────────────

    storage_path: str | None = None
    """If set, use FsStore at this path; otherwise in-memory."""

    # ── Network (forwarded to EndpointConfig) ────────────────────────────

    secret_key: bytes | None = None
    """32-byte node identity key. Determines the stable ``EndpointId``.
    If unset, a fresh identity is generated each run (fine for dev)."""

    relay_mode: str | None = None
    bind_addr: str | None = None
    enable_monitoring: bool = False
    enable_hooks: bool = False
    hook_timeout_ms: int = 5000
    clear_ip_transports: bool = False
    clear_relay_transports: bool = False
    portmapper_config: str | None = None
    proxy_url: str | None = None
    proxy_from_env: bool = False
    local_discovery: bool = False
    """Enable mDNS local network discovery. Nodes on the same LAN can find
    each other without relay servers. Default off.
    Env: ``ASTER_LOCAL_DISCOVERY``. TOML: ``[network] local_discovery``."""

    # ── Logging / observability ─────────────────────────────────────────

    log_format: str = "text"
    """Log output format: ``"json"`` for structured (k8s/ELK) or ``"text"`` for dev.
    Env: ``ASTER_LOG_FORMAT``. TOML: ``[logging] format``."""

    log_level: str = "info"
    """Log level: ``"debug"``, ``"info"``, ``"warning"``, ``"error"``.
    Env: ``ASTER_LOG_LEVEL``. TOML: ``[logging] level``."""

    log_mask: bool = True
    """Mask sensitive fields in logs (keys, credentials, endpoint IDs).
    Env: ``ASTER_LOG_MASK``. TOML: ``[logging] mask``."""

    # ── Identity file ────────────────────────────────────────────────────

    identity_file: str | None = None
    """Path to ``.aster-identity`` TOML file.  When set, the node key and
    enrollment credentials are loaded from this file.
    Env: ``ASTER_IDENTITY_FILE``.  Default: looks for ``.aster-identity``
    in the current working directory."""

    # ── Internal (not user-facing) ───────────────────────────────────────

    _sources: dict = dc_field(default_factory=dict, repr=False)
    """Provenance tracker: maps field name → source string
    (e.g. ``"ASTER_ROOT_PUBKEY_FILE"``, ``"aster.toml [trust]"``, ``"default"``)."""

    _ephemeral_privkey: bytes | None = dc_field(default=None, repr=False)
    """Transient private key generated in dev mode.  Used only by
    ``simple_consumer`` to auto-mint a credential.  Never persisted,
    never configurable.  ``None`` in production."""

    # ── Resolve ──────────────────────────────────────────────────────────

    def resolve_root_pubkey(self) -> bytes | None:
        """Return the root public key, resolving from config sources.

        Resolution order:

        1. Inline ``root_pubkey`` (highest priority).
        2. ``root_pubkey_file`` (hex string or JSON with ``public_key``).
        3. Generate an ephemeral keypair (dev mode only -- logged).

        The root *private* key never appears here. In dev mode, a transient
        private key is stored on ``_ephemeral_privkey`` so the companion
        ``simple_consumer`` example can auto-mint a credential; it is never
        persisted or configurable.
        """
        if self.root_pubkey is not None:
            return self.root_pubkey

        if self.root_pubkey_file:
            pub = _load_pubkey_from_file(self.root_pubkey_file)
            if pub is not None:
                self.root_pubkey = pub
                return pub
            _config_logger.warning(
                "Root pubkey file %s not found or invalid", self.root_pubkey_file
            )

        # Dev mode: generate ephemeral keypair if admission is needed.
        if not self.allow_all_consumers or not self.allow_all_producers:
            from .trust.signing import generate_root_keypair

            priv, pub = generate_root_keypair()
            self._ephemeral_privkey = priv  # transient, never persisted
            self.root_pubkey = pub
            self._sources["root_pubkey"] = "ephemeral (dev mode)"
            _config_logger.info(
                "Generated ephemeral root key "
                "(set ASTER_ROOT_PUBKEY_FILE for production)"
            )
            return pub

        return None

    def load_identity(self, peer_name: str | None = None,
                      role: str | None = None) -> tuple[bytes | None, dict | None]:
        """Load a peer entry from the ``.aster-identity`` file.

        Args:
            peer_name: Select peer by ``name``.  If None, auto-selects by
                ``role`` (first match).
            role: Fallback selector when ``peer_name`` is None.
                ``"producer"`` for AsterServer, ``"consumer"`` for AsterClient.

        Returns:
            ``(secret_key_bytes, peer_dict)`` or ``(None, None)`` when no
            identity file is found.
        """
        import base64 as _b64

        path = self._resolve_identity_path()
        if path is None:
            return None, None

        if sys.version_info >= (3, 11):
            import tomllib as _tl
        else:
            import tomli as _tl  # type: ignore[no-redef]

        with open(path, "rb") as f:
            data = _tl.load(f)

        node = data.get("node", {})
        secret_key_b64 = node.get("secret_key")
        secret_key = _b64.b64decode(secret_key_b64) if secret_key_b64 else None

        # Find the peer entry
        peers = data.get("peers", [])
        peer = None
        if peer_name is not None:
            for p in peers:
                if p.get("name") == peer_name:
                    peer = p
                    break
        elif role is not None:
            for p in peers:
                if p.get("role") == role:
                    peer = p
                    break
        elif peers:
            peer = peers[0]  # single peer → auto-select

        return secret_key, peer

    def _resolve_identity_path(self) -> str | None:
        """Find the .aster-identity file."""
        if self.identity_file:
            expanded = os.path.expanduser(self.identity_file)
            return expanded if os.path.exists(expanded) else None
        # Default: look in cwd
        default = os.path.join(os.getcwd(), ".aster-identity")
        return default if os.path.exists(default) else None

    def load_identity_from_path(
        self,
        path: str,
        peer_name: str | None = None,
        role: str | None = None,
    ) -> tuple[bytes | None, dict | None]:
        """Load secret_key + peer entry from a specific TOML identity file.

        Same return shape as :meth:`load_identity` but reads from an
        explicit path instead of consulting ``self.identity_file``. Used
        by ``AsterClient`` to fold the secret key out of an
        ``enrollment_credential_file`` when no separate ``identity=`` was
        provided -- both options should do the same thing for the same
        TOML file produced by ``aster enroll node``.
        """
        import base64 as _b64

        if sys.version_info >= (3, 11):
            import tomllib as _tl
        else:
            import tomli as _tl  # type: ignore[no-redef]

        with open(path, "rb") as f:
            data = _tl.load(f)

        node = data.get("node", {})
        secret_key_b64 = node.get("secret_key")
        secret_key = _b64.b64decode(secret_key_b64) if secret_key_b64 else None

        peers = data.get("peers", [])
        peer = None
        if peer_name is not None:
            for p in peers:
                if p.get("name") == peer_name:
                    peer = p
                    break
        elif role is not None:
            for p in peers:
                if p.get("role") == role:
                    peer = p
                    break
        elif peers:
            peer = peers[0]

        return secret_key, peer

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
            self.local_discovery,
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
            enable_local_discovery=self.local_discovery,
        )

    # ── print_config ─────────────────────────────────────────────────────

    def print_config(self, *, json: bool = False) -> str:
        """Render the resolved configuration with provenance and masking.

        Sensitive fields (``secret_key``, ``enrollment_credential_file``)
        are masked.  ``root_pubkey`` is public and shown in full.

        Args:
            json: If True, return a JSON string; otherwise a human-readable table.

        Returns:
            The rendered config string (also printed to stdout).
        """
        sections = {
            "trust": [
                ("root_pubkey", self._fmt_bytes(self.root_pubkey, mask=False)),
                ("root_pubkey_file", self.root_pubkey_file or "<not set>"),
                ("enrollment_credential_file", self._fmt_masked(self.enrollment_credential_file)),
                ("enrollment_credential_iid", self._fmt_masked(self.enrollment_credential_iid)),
                ("allow_all_consumers", self.allow_all_consumers),
                ("allow_all_producers", self.allow_all_producers),
            ],
            "connect": [
                ("endpoint_addr", self.endpoint_addr or "<not set>"),
            ],
            "network": [
                ("secret_key", self._fmt_bytes(self.secret_key, mask=True)),
                ("relay_mode", self.relay_mode or "<default>"),
                ("bind_addr", self.bind_addr or "<any>"),
                ("enable_monitoring", self.enable_monitoring),
                ("enable_hooks", self.enable_hooks),
            ],
            "storage": [
                ("path", self.storage_path or "<in-memory>"),
            ],
        }

        if json:
            out: dict = {}
            for section, fields in sections.items():
                out[section] = {}
                for name, value in fields:
                    source = self._sources.get(name, "default")
                    out[section][name] = {"value": value, "source": source}
            text = _json.dumps(out, indent=2, default=str)
        else:
            lines = []
            for section, fields in sections.items():
                lines.append(f"  [{section}]")
                for name, value in fields:
                    source = self._sources.get(name, "default")
                    lines.append(f"    {name:<28s}: {value!s:<36s} ({source})")
            text = "\n".join(lines)

        print(text)
        return text

    @staticmethod
    def _fmt_bytes(val: bytes | None, *, mask: bool) -> str:
        if val is None:
            return "<not set>"
        h = val.hex()
        if mask:
            return f"****...{h[-8:]}" if len(h) > 8 else "****"
        return h

    @staticmethod
    def _fmt_masked(val: str | None) -> str:
        if val is None:
            return "<not set>"
        return f"****...{val[-12:]}" if len(val) > 12 else "****"

    # ── Factory methods ──────────────────────────────────────────────────

    @classmethod
    def from_env(cls) -> "AsterConfig":
        """Build config from ``ASTER_*`` environment variables only."""
        return cls._load(toml_data=None, toml_path=None)

    @classmethod
    def from_file(cls, path: str | Path) -> "AsterConfig":
        """Build config from a TOML file, with env-var overrides."""
        with Path(path).open("rb") as fh:
            raw = tomllib.load(fh)
        return cls._load(toml_data=raw, toml_path=str(path))

    @classmethod
    def _load(cls, toml_data: dict | None, toml_path: str | None) -> "AsterConfig":
        kwargs: dict = {}
        sources: dict[str, str] = {}
        env = os.environ
        toml_label = toml_path or "aster.toml"

        def _set(field: str, value, source: str) -> None:
            kwargs[field] = value
            sources[field] = source

        # ── Trust (TOML [trust] section) ─────────────────────────────────
        trust = (toml_data or {}).get("trust", {})
        if "root_pubkey" in trust:
            raw = trust["root_pubkey"]
            _set("root_pubkey", bytes.fromhex(raw), f"{toml_label} [trust]")
        if "root_pubkey_file" in trust:
            _set("root_pubkey_file", str(trust["root_pubkey_file"]), f"{toml_label} [trust]")
        if "enrollment_credential" in trust:
            _set("enrollment_credential_file", str(trust["enrollment_credential"]), f"{toml_label} [trust]")
        if "enrollment_credential_iid" in trust:
            _set("enrollment_credential_iid", str(trust["enrollment_credential_iid"]), f"{toml_label} [trust]")
        if "allow_all_consumers" in trust:
            _set("allow_all_consumers", bool(trust["allow_all_consumers"]), f"{toml_label} [trust]")
        if "allow_all_producers" in trust:
            _set("allow_all_producers", bool(trust["allow_all_producers"]), f"{toml_label} [trust]")

        # ── Connect (TOML [connect] section) ─────────────────────────────
        connect = (toml_data or {}).get("connect", {})
        if "endpoint_addr" in connect:
            _set("endpoint_addr", str(connect["endpoint_addr"]), f"{toml_label} [connect]")

        # ── Storage (TOML [storage] section) ─────────────────────────────
        storage = (toml_data or {}).get("storage", {})
        if "path" in storage:
            _set("storage_path", str(storage["path"]), f"{toml_label} [storage]")

        # ── Network (TOML [network] section) ─────────────────────────────
        network = (toml_data or {}).get("network", {})
        _NET_FIELDS = ("relay_mode", "bind_addr", "portmapper_config", "proxy_url")
        for f in _NET_FIELDS:
            if f in network:
                _set(f, str(network[f]) if network[f] is not None else None, f"{toml_label} [network]")
        _NET_BOOLS = (
            "enable_monitoring", "enable_hooks",
            "clear_ip_transports", "clear_relay_transports", "proxy_from_env",
            "local_discovery",
        )
        for f in _NET_BOOLS:
            if f in network:
                _set(f, bool(network[f]), f"{toml_label} [network]")
        if "hook_timeout_ms" in network:
            _set("hook_timeout_ms", int(network["hook_timeout_ms"]), f"{toml_label} [network]")
        if "secret_key" in network and network["secret_key"] is not None:
            _set("secret_key", base64.b64decode(network["secret_key"]), f"{toml_label} [network]")

        # ── Logging (TOML [logging] section) ─────────────────────────────
        log_sec = (toml_data or {}).get("logging", {})
        if "format" in log_sec:
            _set("log_format", str(log_sec["format"]).lower(), f"{toml_label} [logging]")
        if "level" in log_sec:
            _set("log_level", str(log_sec["level"]).lower(), f"{toml_label} [logging]")
        if "mask" in log_sec:
            _set("log_mask", bool(log_sec["mask"]), f"{toml_label} [logging]")

        # ── Env overrides (always win) ───────────────────────────────────
        if (v := env.get("ASTER_ROOT_PUBKEY")) is not None:
            _set("root_pubkey", bytes.fromhex(v.strip()), "ASTER_ROOT_PUBKEY")
        if (v := env.get("ASTER_ROOT_PUBKEY_FILE")) is not None:
            _set("root_pubkey_file", v.strip(), "ASTER_ROOT_PUBKEY_FILE")
        if (v := env.get("ASTER_ENROLLMENT_CREDENTIAL")) is not None:
            _set("enrollment_credential_file", v.strip(), "ASTER_ENROLLMENT_CREDENTIAL")
        if (v := env.get("ASTER_ENROLLMENT_CREDENTIAL_IID")) is not None:
            _set("enrollment_credential_iid", v.strip(), "ASTER_ENROLLMENT_CREDENTIAL_IID")
        if (v := env.get("ASTER_ENDPOINT_ADDR")) is not None:
            _set("endpoint_addr", v.strip(), "ASTER_ENDPOINT_ADDR")
        if (v := env.get("ASTER_IDENTITY_FILE")) is not None:
            _set("identity_file", v.strip(), "ASTER_IDENTITY_FILE")
        if (v := env.get("ASTER_ALLOW_ALL_CONSUMERS")) is not None:
            _set("allow_all_consumers", _parse_bool(v, "ASTER_ALLOW_ALL_CONSUMERS"), "ASTER_ALLOW_ALL_CONSUMERS")
        if (v := env.get("ASTER_ALLOW_ALL_PRODUCERS")) is not None:
            _set("allow_all_producers", _parse_bool(v, "ASTER_ALLOW_ALL_PRODUCERS"), "ASTER_ALLOW_ALL_PRODUCERS")
        if (v := env.get("ASTER_STORAGE_PATH")) is not None:
            _set("storage_path", v.strip() or None, "ASTER_STORAGE_PATH")
        if (v := env.get("ASTER_SECRET_KEY")) is not None:
            _set("secret_key", base64.b64decode(v.strip()) if v.strip() else None, "ASTER_SECRET_KEY")
        if (v := env.get("ASTER_RELAY_MODE")) is not None:
            _set("relay_mode", v.strip() or None, "ASTER_RELAY_MODE")
        if (v := env.get("ASTER_BIND_ADDR")) is not None:
            _set("bind_addr", v.strip() or None, "ASTER_BIND_ADDR")
        for f in _NET_BOOLS:
            var = f"ASTER_{f.upper()}"
            if (v := env.get(var)) is not None:
                _set(f, _parse_bool(v, var), var)
        if (v := env.get("ASTER_HOOK_TIMEOUT_MS")) is not None:
            _set("hook_timeout_ms", int(v), "ASTER_HOOK_TIMEOUT_MS")
        for f in ("portmapper_config", "proxy_url"):
            var = f"ASTER_{f.upper()}"
            if (v := env.get(var)) is not None:
                _set(f, v.strip() or None, var)

        # Logging env overrides
        if (v := env.get("ASTER_LOG_FORMAT")) is not None:
            _set("log_format", v.strip().lower(), "ASTER_LOG_FORMAT")
        if (v := env.get("ASTER_LOG_LEVEL")) is not None:
            _set("log_level", v.strip().lower(), "ASTER_LOG_LEVEL")
        if (v := env.get("ASTER_LOG_MASK")) is not None:
            _set("log_mask", _parse_bool(v, "ASTER_LOG_MASK"), "ASTER_LOG_MASK")

        obj = cls(**{k: v for k, v in kwargs.items() if k != "_sources"})
        obj._sources = sources
        return obj


def _load_pubkey_from_file(path: str) -> bytes | None:
    """Load a root public key from a file.

    Accepts either:
    - A plain hex string (64 chars = 32 bytes).
    - A JSON object with a ``"public_key"`` hex field.
    """
    expanded = os.path.expanduser(path)
    if not os.path.exists(expanded):
        return None
    with open(expanded) as f:
        content = f.read().strip()
    # Try JSON first.
    if content.startswith("{"):
        try:
            d = _json.loads(content)
            return bytes.fromhex(d["public_key"])
        except (KeyError, ValueError, _json.JSONDecodeError):
            pass
    # Try plain hex.
    try:
        raw = bytes.fromhex(content)
        if len(raw) == 32:
            return raw
    except ValueError:
        pass
    _config_logger.warning("Could not parse root pubkey from %s", path)
    return None
