"""
Aster configuration -- the Rust ``aster::AsterConfig``, as a frozen dataclass.

Every value is loaded, validated and lowered by the Rust ``aster`` crate; this
module only adds the Python surface (keyword construction, ``replace``,
``print_config``). Resolution order, each layer overriding the previous:

1. Built-in defaults (in-memory node, fresh identity, default relays).
2. TOML config file (``aster.toml``) -- :meth:`AsterConfig.from_file`.
3. ``ASTER_*`` environment variables -- :meth:`AsterConfig.from_env`
   (also applied on top of the file by ``from_file``).
4. Code: keyword arguments / :meth:`AsterConfig.replace`.

Fields are the Rust ``AsterConfig`` field names, which are also the TOML keys
(see ``docs/aster-rust-getstarted.md`` section 2.2 for the full builder / env /
TOML table). Python-specific spellings: ``gc_interval_ms`` (an int; ``None`` or
``0`` disables GC), and ``relay_mode`` (``"default"`` / ``"staging"`` /
``"disabled"`` / ``"custom"``) plus ``relay_urls`` (setting ``relay_urls``
selects ``"custom"``; a bare URL as ``relay_mode`` does too).

TOML example (``aster.toml``)::

    [trust]
    root_pubkey_file = "~/.aster/root.pub"
    allow_all_consumers = false

    [storage]
    path = "/var/lib/aster"          # omit for in-memory
    gc_interval_ms = 600000

    [network]
    secret_key_file = "/var/lib/aster/node.key"   # 64-hex key file, created if missing
    relay_mode = "default"
    bind_addr = "0.0.0.0:9000"
    portmapper_config = "disabled"

    [logging]
    format = "json"

    [identity]
    file = "~/.aster/.aster-identity"   # credential file (peers, enrollment)

Identity: the node key lives in a **secret key file** (``secret_key_file``):
32 bytes as 64 hex characters, shared with the Rust SDK and every binding. It
is loaded when present and created (mode 0600) from ``secret_key`` or a fresh
key when not; a malformed file, or one disagreeing with ``secret_key``, is a
``ValueError``. The ``.aster-identity`` TOML (``identity_file``) is a
*credential* file (peers, root key, enrollment), not a key file.
"""

from __future__ import annotations

import dataclasses
import json as _json
import logging
import os
import sys
from dataclasses import dataclass, field as dc_field
from pathlib import Path
from typing import Any, Mapping, Optional

from ._aster import NativeAsterConfig, PublicKey, SecretKey

_config_logger = logging.getLogger(__name__)

# Sensitive fields whose values are masked in print_config output.
_MASKED_FIELDS = frozenset({"secret_key", "enrollment_credential_file"})

# Env var spellings that are not simply ``ASTER_<FIELD>``.
_ENV_OVERRIDES = {
    "enrollment_credential_file": "ASTER_ENROLLMENT_CREDENTIAL",
    "storage_path": "ASTER_STORAGE_PATH",
    "log_format": "ASTER_LOG_FORMAT",
    "log_level": "ASTER_LOG_LEVEL",
    "log_mask": "ASTER_LOG_MASK",
    "identity_file": "ASTER_IDENTITY_FILE",
}


def _env_var(name: str) -> str | None:
    if name == "alpns":
        return None
    return _ENV_OVERRIDES.get(name, f"ASTER_{name.upper()}")


def _toml_section(name: str) -> str:
    if name in (
        "root_pubkey", "root_pubkey_file", "enrollment_credential_file",
        "enrollment_credential_iid", "allow_all_consumers", "allow_all_producers",
    ):
        return "[trust]"
    if name == "endpoint_addr":
        return "[connect]"
    if name in ("storage_path", "gc_interval_ms"):
        return "[storage]"
    if name.startswith("log_"):
        return "[logging]"
    if name == "identity_file":
        return "[identity]"
    if name.startswith("health_"):
        return "[health]"
    return "[network]"


def _to_bytes32(value: Any, what: str) -> bytes | None:
    if value is None:
        return None
    if isinstance(value, (SecretKey, PublicKey)):
        return value.to_bytes()
    if isinstance(value, str):
        if what == "secret_key":
            return SecretKey.from_hex(value).to_bytes()
        return PublicKey.from_hex(value).to_bytes()
    if isinstance(value, (bytes, bytearray, memoryview)):
        return bytes(value)
    raise TypeError(f"{what}: expected bytes, hex str, or {what} object, got {type(value).__name__}")


def _path_or_none(value: Any) -> str | None:
    if value is None:
        return None
    return os.fspath(value)


@dataclass(frozen=True)
class AsterConfig:
    """Node, trust, storage and logging configuration (frozen).

    Three ways to get one::

        config = AsterConfig.from_env()              # ASTER_* env
        config = AsterConfig.from_file("aster.toml") # file, then env overrides
        config = AsterConfig(storage_path="/var/lib/app", relay_mode="disabled")

    Derive a modified copy with :meth:`replace`. Pass it to
    :meth:`aster.Node.start`, :class:`aster.AsterServer` or
    :class:`aster.AsterClient`. Construction validates every value through
    the Rust ``aster::AsterConfig`` (``ValueError`` / ``TypeError`` on bad
    input).
    """

    # -- Trust ---------------------------------------------------------------
    root_pubkey: bytes | None = None
    """32-byte Ed25519 root public key (accepts ``PublicKey`` / hex too)."""
    root_pubkey_file: str | None = None
    """File holding the root public key (hex, or JSON ``{"public_key": ...}``)."""
    enrollment_credential_file: str | None = None
    """Enrollment credential presented during admission."""
    enrollment_credential_iid: str | None = None
    """Cloud instance identity document token (``ASTER_ENROLLMENT_CREDENTIAL_IID``)."""
    allow_all_consumers: bool = False
    """Skip the consumer admission gate."""
    allow_all_producers: bool = True
    """Schema parity with Rust; the Python binding only supports ``True``."""

    # -- Connect -------------------------------------------------------------
    endpoint_addr: str | None = None
    """Producer address a consumer dials (``aster1...`` ticket / NodeAddr / id)."""

    # -- Storage -------------------------------------------------------------
    storage_path: str | None = None
    """Persistent node directory; ``None`` = in-memory."""
    gc_interval_ms: int | None = None
    """Blob GC interval in ms; ``None`` / ``0`` = no GC."""

    # -- Identity ------------------------------------------------------------
    secret_key: bytes | None = dc_field(default=None, repr=False)
    """32-byte node key (accepts ``SecretKey`` / 64-hex too). Fixes the node id."""
    secret_key_file: str | None = None
    """64-hex secret key file, loaded or created at start."""

    # -- Network -------------------------------------------------------------
    relay_mode: str = "default"
    relay_urls: tuple[str, ...] = ()
    bind_addr: str | None = None
    local_discovery: bool = False
    """mDNS local discovery (``ASTER_LOCAL_DISCOVERY``)."""
    enable_monitoring: bool = False
    enable_hooks: bool = False
    hook_timeout_ms: int = 5000
    hook_failure_mode: str = "fail_open"
    clear_ip_transports: bool = False
    clear_relay_transports: bool = False
    portmapper_config: str | None = None
    proxy_url: str | None = None
    proxy_from_env: bool = False
    transport_max_concurrent_bidi_streams: int | None = None
    transport_max_concurrent_uni_streams: int | None = None
    transport_stream_receive_window: int | None = None
    transport_receive_window: int | None = None
    transport_send_window: int | None = None
    transport_max_idle_timeout_ms: int | None = None
    transport_keep_alive_interval_ms: int | None = None
    transport_initial_mtu: int | None = None
    transport_datagram_receive_buffer_size: int | None = None
    transport_datagram_send_buffer_size: int | None = None
    transport_send_fairness: bool | None = None
    transport_enable_segmentation_offload: bool | None = None

    # -- Logging -------------------------------------------------------------
    log_format: str = "text"
    log_level: str = "info"
    log_mask: bool = True

    # -- Ops endpoints (/healthz, /readyz, /metrics; Rust ``aster::ops``) ------
    health_port: int = 0
    """TCP port of the ops listener. ``0`` means disabled unless ``health_enabled``."""
    health_host: str = "127.0.0.1"
    """Bind host (localhost only by default; pod probes need ``0.0.0.0``)."""
    health_enabled: bool | None = None
    """Force the listener on or off; ``None`` enables it when ``health_port != 0``."""

    # -- Credentials ---------------------------------------------------------
    identity_file: str | None = None
    """``.aster-identity`` credential file (default: ``./.aster-identity``)."""

    # -- Extra inbound ALPNs (code only) --------------------------------------
    alpns: tuple[bytes, ...] = ()

    # -- Internal -------------------------------------------------------------
    _sources: Mapping[str, str] = dc_field(
        default_factory=dict, init=False, repr=False, compare=False
    )
    _native_cfg: Any = dc_field(default=None, init=False, repr=False, compare=False)

    def __post_init__(self) -> None:
        raw = {f: getattr(self, f) for f in _field_names()}
        raw["root_pubkey"] = _to_bytes32(raw["root_pubkey"], "root_pubkey")
        raw["secret_key"] = _to_bytes32(raw["secret_key"], "secret_key")
        for name in (
            "root_pubkey_file", "enrollment_credential_file", "storage_path",
            "secret_key_file", "identity_file",
        ):
            raw[name] = _path_or_none(raw[name])
        if isinstance(raw["relay_urls"], str):
            raw["relay_urls"] = [raw["relay_urls"]]
        raw["relay_urls"] = list(raw["relay_urls"] or ())
        if raw["relay_urls"] and (raw["relay_mode"] or "default") == "default":
            raw["relay_mode"] = None
        raw["alpns"] = [a.encode() if isinstance(a, str) else bytes(a) for a in (raw["alpns"] or ())]
        native = NativeAsterConfig(**raw)
        self._adopt(native)

    def _adopt(self, native: NativeAsterConfig) -> None:
        """Set every field to the canonical value of the Rust config."""
        for name, value in native.to_dict().items():
            if name in ("relay_urls", "alpns"):
                value = tuple(value)
            object.__setattr__(self, name, value)
        object.__setattr__(self, "_native_cfg", native)

    def _native(self) -> NativeAsterConfig:
        """The Rust ``aster::AsterConfig`` this dataclass mirrors."""
        return self._native_cfg

    @classmethod
    def _from_native(
        cls, native: NativeAsterConfig, sources: Mapping[str, str] | None = None
    ) -> "AsterConfig":
        obj = cls.__new__(cls)
        object.__setattr__(obj, "_sources", dict(sources or {}))
        obj._adopt(native)
        return obj

    # -- Loaders --------------------------------------------------------------

    @classmethod
    def from_env(cls) -> "AsterConfig":
        """Defaults overlaid with ``ASTER_*`` environment variables."""
        native = NativeAsterConfig.from_env()
        base = NativeAsterConfig().to_dict()
        sources = {
            name: _env_var(name) or "env"
            for name, value in native.to_dict().items()
            if value != base[name]
        }
        return cls._from_native(native, sources)

    @classmethod
    def from_file(cls, path: str | os.PathLike) -> "AsterConfig":
        """``aster.toml`` overlaid with ``ASTER_*`` environment variables.

        Raises ``FileNotFoundError`` for a missing file and ``ValueError`` for
        malformed TOML or invalid values.
        """
        path = os.fspath(path)
        native = NativeAsterConfig.from_file(path)
        base = NativeAsterConfig().to_dict()
        file_only = NativeAsterConfig.file_only(path).to_dict()
        sources: dict[str, str] = {}
        for name, value in native.to_dict().items():
            if value != file_only[name]:
                sources[name] = _env_var(name) or "env"
            elif value != base[name]:
                sources[name] = f"{path} {_toml_section(name)}"
        return cls._from_native(native, sources)

    def replace(self, **changes: Any) -> "AsterConfig":
        """A copy with ``changes`` applied (validated); provenance is kept."""
        new = dataclasses.replace(self, **changes)
        sources = dict(self._sources)
        for name in changes:
            sources[name] = "code"
        object.__setattr__(new, "_sources", sources)
        return new

    def to_dict(self) -> dict[str, Any]:
        """All fields as plain values."""
        return {name: getattr(self, name) for name in _field_names()}

    # -- Resolution helpers (all Rust) -------------------------------------------

    @property
    def is_persistent(self) -> bool:
        """Whether ``storage_path`` selects a persistent node."""
        return self.storage_path is not None

    def resolve_root_pubkey(self) -> bytes | None:
        """The root public key from ``root_pubkey`` and/or ``root_pubkey_file``.

        A missing file is ignored. A malformed file, or a file that disagrees
        with the inline key, raises ``ValueError``.
        """
        key = self._native().resolve_root_pubkey()
        return key.to_bytes() if key is not None else None

    def resolve_secret_key(self) -> Optional[SecretKey]:
        """The effective node key: ``secret_key_file`` (loaded, or created
        from ``secret_key`` / a fresh key) or ``secret_key``. ``None`` when
        neither is set (the node then gets a fresh identity). Writes the key
        file when it is missing -- the same step ``Node.start`` performs.
        """
        return self._native().resolve_secret_key_file()

    def resolve_identity_path(self) -> str | None:
        """``identity_file`` if it exists, else ``./.aster-identity`` if it
        exists, else ``None``."""
        return self._native().resolve_identity_path()

    def load_identity(
        self, peer_name: str | None = None, role: str | None = None
    ) -> tuple[bytes | None, dict | None]:
        """Load ``[node] secret_key`` and a ``[[peers]]`` entry from the
        ``.aster-identity`` credential file.

        Peer selection: ``peer_name`` wins; else the first peer with ``role``;
        else the first peer. Returns ``(None, None)`` when no identity file is
        configured or found in the working directory.
        """
        path = self.resolve_identity_path()
        if path is None:
            return None, None
        return self.load_identity_from_path(path, peer_name=peer_name, role=role)

    def load_identity_from_path(
        self,
        path: str | os.PathLike,
        peer_name: str | None = None,
        role: str | None = None,
    ) -> tuple[bytes | None, dict | None]:
        """Like :meth:`load_identity`, from an explicit path."""
        return _load_identity_file(os.fspath(path), peer_name, role)

    # -- print_config ------------------------------------------------------------

    def print_config(self, *, json: bool = False) -> str:
        """Render the resolved configuration with provenance and masking.

        Sensitive fields (``secret_key``, ``enrollment_credential_file``,
        ``enrollment_credential_iid``) are masked. Returns the rendered text
        (also printed to stdout).
        """
        def _or(value: Any, default: str) -> Any:
            return value if value is not None else default

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
                ("secret_key_file", self.secret_key_file or "<not set>"),
                ("relay_mode", self.relay_mode),
                ("relay_urls", ", ".join(self.relay_urls) or "<none>"),
                ("bind_addr", self.bind_addr or "<any>"),
                ("local_discovery", self.local_discovery),
                ("enable_monitoring", self.enable_monitoring),
                ("enable_hooks", self.enable_hooks),
                ("transport_receive_window", _or(self.transport_receive_window, "<default>")),
                ("transport_send_window", _or(self.transport_send_window, "<default>")),
                ("transport_initial_mtu", _or(self.transport_initial_mtu, "<default>")),
                (
                    "transport_enable_segmentation_offload",
                    _or(self.transport_enable_segmentation_offload, "<default>"),
                ),
            ],
            "storage": [
                ("path", self.storage_path or "<in-memory>"),
                ("gc_interval_ms", self.gc_interval_ms or "<off>"),
            ],
        }
        source_key = {"path": "storage_path"}

        def _source(name: str) -> str:
            return self._sources.get(source_key.get(name, name), "default")

        if json:
            out: dict = {}
            for section, fields in sections.items():
                out[section] = {
                    name: {"value": value, "source": _source(name)}
                    for name, value in fields
                }
            text = _json.dumps(out, indent=2, default=str)
        else:
            lines = []
            for section, fields in sections.items():
                lines.append(f"  [{section}]")
                for name, value in fields:
                    lines.append(f"    {name:<28s}: {value!s:<36s} ({_source(name)})")
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


def _field_names() -> tuple[str, ...]:
    return tuple(NativeAsterConfig.field_names())


def _load_identity_file(
    path: str, peer_name: str | None, role: str | None
) -> tuple[bytes | None, dict | None]:
    """``[node] secret_key`` (parsed by Rust: 64 hex or base64) plus the raw
    selected ``[[peers]]`` table (credential material, read as-is)."""
    if sys.version_info >= (3, 11):
        import tomllib as _tl
    else:  # pragma: no cover
        import tomli as _tl  # type: ignore[no-redef]

    key = NativeAsterConfig.identity_secret_key(path)
    with open(path, "rb") as f:
        data = _tl.load(f)

    peers = data.get("peers", [])
    peer = None
    if peer_name is not None:
        peer = next((p for p in peers if p.get("name") == peer_name), None)
    elif role is not None:
        peer = next((p for p in peers if p.get("role") == role), None)
    elif peers:
        peer = peers[0]
    return (key.to_bytes() if key is not None else None), peer


__all__ = ["AsterConfig"]
