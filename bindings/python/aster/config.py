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
    enrollment_credential = "server.cred"   # `aster enroll node --out`

    [enrollment]                            # see EnrollmentConfig
    audience = "mission-control"
    fleet_namespace = "<64 hex>"            # `aster enroll namespace create`
    state_path = "/var/lib/aster/enrollment.json"

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

Enrollment: a configuration that sets an ``[enrollment]`` audience, fleet
namespace or issuer delegation, or a credential (``enrollment_credential_file``
/ ``enrollment_credential_chain``), switches Enrollment on for
:class:`aster.AsterServer` and :class:`aster.AsterClient`; ``[enrollment]
enabled`` forces it on or off. Once on, every value is validated before the
node starts and any problem raises ``ValueError`` naming the setting: nothing
falls back to an open node. Without it a node is open (Gate 0 admits every
peer).

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
_MASKED_FIELDS = frozenset(
    {"secret_key", "enrollment_credential_file", "enrollment_credential_chain"}
)

# Env var spellings that are not simply ``ASTER_<FIELD>``.
_ENV_OVERRIDES = {
    "enrollment_credential_file": "ASTER_ENROLLMENT_CREDENTIAL",
    "enrollment": "ASTER_ENROLLMENT_*",
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
        "root_pubkey", "root_pubkey_file", "next_root_commitment", "root_seq",
        "root_rotations", "enrollment_credential_file", "enrollment_credential_chain",
    ):
        return "[trust]"
    if name == "enrollment":
        return "[enrollment]"
    if name == "endpoint_addr":
        return "[connect]"
    if name in ("storage_path", "gc_interval_ms", "blob_zstd_level"):
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
class EnrollmentLimits:
    """``[enrollment.limits]``: overrides of the Enrollment defaults (``None``
    keeps the default). Env ``ASTER_ENROLLMENT_<FIELD>`` upper-cased."""

    handshake_timeout_ms: int | None = None
    step_timeout_ms: int | None = None
    policy_timeout_ms: int | None = None
    reconciliation_interval_ms: int | None = None
    max_admitted_peers: int | None = None
    sync_timeout_ms: int | None = None
    """Bound on enrolling with and synchronizing from a replica (default 15000)."""


@dataclass(frozen=True)
class RoleMappingConfig:
    """``[enrollment.roles]``: how a peer's verified claims become the roles
    ``requires=`` checks.

    Roles = the comma-separated value of the string claim attribute
    ``attribute`` (default ``aster.role``; ``""`` disables it), plus every
    verified scope when ``scopes_as_roles``, plus the roles ``scopes`` and
    ``tags`` map each verified scope or tag to. With ``include_attributes``
    the other claim attributes reach handlers too (never ``aster.role``,
    which only the mapping assigns). Evaluated per call, so revocation and
    renewal apply to the next call.
    """

    attribute: str = "aster.role"
    """Claim attribute listing roles (``ASTER_ENROLLMENT_ROLE_ATTRIBUTE``)."""
    scopes_as_roles: bool = False
    """Each verified scope is also a role (``ASTER_ENROLLMENT_SCOPES_AS_ROLES``)."""
    scopes: Mapping[str, tuple[str, ...]] = dc_field(default_factory=dict)
    """Scope -> roles (``[enrollment.roles.scopes]``)."""
    tags: Mapping[str, tuple[str, ...]] = dc_field(default_factory=dict)
    """Tag -> roles (``[enrollment.roles.tags]``)."""
    include_attributes: bool = True
    """Pass the other claim attributes through (``ASTER_ENROLLMENT_INCLUDE_ATTRIBUTES``)."""


@dataclass(frozen=True)
class EnrollmentConfig:
    """The ``[enrollment]`` section (Rust ``aster::EnrollmentConfig``).

    The trusted root and this node's credential live in ``[trust]``
    (:attr:`AsterConfig.root_pubkey` / ``root_pubkey_file``,
    ``enrollment_credential_file`` / ``enrollment_credential_chain``, and the
    rotation pin). Field names, TOML keys and ``ASTER_ENROLLMENT_*``
    variables match Rust exactly.
    """

    enabled: bool | None = None
    """Force on/off; ``None`` enables when an audience, fleet namespace,
    issuer delegation or credential is set (``ASTER_ENROLLMENT_ENABLED``)."""
    audience: str | None = None
    """Fleet audience. Required (``ASTER_ENROLLMENT_AUDIENCE``)."""
    fleet_namespace: str | None = None
    """Fleet policy namespace, 64 hex. Required (``ASTER_ENROLLMENT_FLEET_NAMESPACE``)."""
    state_path: str | None = None
    """Durable denials/watermarks; default ``<storage_path>/enrollment.json``.
    A server needs one of the two (``ASTER_ENROLLMENT_STATE_PATH``)."""
    replicas: tuple[str, ...] = ()
    """``aster1...`` tickets or node ids of policy replicas (``ASTER_ENROLLMENT_REPLICAS``)."""
    policy: Any = None
    """Admission policy tree (``PolicyConfig``): ``"tolerate_stale_policy"``
    (default), ``{"all_of": [{"scope": "ops"}, "tolerate_stale_policy"]}``, ...
    (``ASTER_ENROLLMENT_POLICY`` as JSON)."""
    require_synchronized_clock: bool = False
    """Refuse to start on an unsynchronized clock (Linux)."""
    activation_leeway_secs: int | None = None
    """``iat``/``nbf`` leeway, 0-300 (default 60)."""
    limits: EnrollmentLimits = dc_field(default_factory=EnrollmentLimits)
    roles: RoleMappingConfig = dc_field(default_factory=RoleMappingConfig)
    issuer_delegation_file: str | None = None
    """Server only: run a delegated issuer serving renewals
    (``[enrollment.issuer] delegation_file``)."""
    issuer_lifetime_secs: int | None = None
    """Lifetime of issued credentials (``[enrollment.issuer] lifetime``; default 24 h)."""
    renew_from: str | None = None
    """Issuer to renew this node's credential from (ticket or node id)."""
    renew: bool | None = None
    """``False`` disables renewal; ``None`` renews when an issuer is known."""
    synchronize: bool | None = None
    """Client: synchronize the fleet namespace the server offers (default on)."""

    def __post_init__(self) -> None:
        limits = self.limits
        if isinstance(limits, Mapping):
            limits = EnrollmentLimits(**limits)
        roles = self.roles
        if isinstance(roles, Mapping):
            roles = RoleMappingConfig(**roles)
        object.__setattr__(self, "limits", limits)
        object.__setattr__(self, "roles", roles)
        if isinstance(self.replicas, str):
            object.__setattr__(self, "replicas", (self.replicas,))
        else:
            object.__setattr__(self, "replicas", tuple(self.replicas or ()))
        if self.state_path is not None:
            object.__setattr__(self, "state_path", os.fspath(self.state_path))
        if self.issuer_delegation_file is not None:
            object.__setattr__(
                self, "issuer_delegation_file", os.fspath(self.issuer_delegation_file)
            )

    def _to_native(self) -> dict[str, Any]:
        out = {f.name: getattr(self, f.name) for f in dataclasses.fields(self)}
        out["replicas"] = list(self.replicas)
        out["policy"] = None if self.policy is None else _json.dumps(self.policy)
        out["limits"] = dataclasses.asdict(self.limits)
        out["roles"] = {
            "attribute": self.roles.attribute,
            "scopes_as_roles": self.roles.scopes_as_roles,
            "scopes": {k: _role_list(v) for k, v in dict(self.roles.scopes).items()},
            "tags": {k: _role_list(v) for k, v in dict(self.roles.tags).items()},
            "include_attributes": self.roles.include_attributes,
        }
        return out

    @classmethod
    def _from_native(cls, d: Mapping[str, Any]) -> "EnrollmentConfig":
        d = dict(d)
        d["replicas"] = tuple(d.get("replicas") or ())
        policy = d.get("policy")
        d["policy"] = None if policy is None else _json.loads(policy)
        d["limits"] = EnrollmentLimits(**d["limits"])
        roles = dict(d["roles"])
        roles["scopes"] = {k: tuple(v) for k, v in roles["scopes"].items()}
        roles["tags"] = {k: tuple(v) for k, v in roles["tags"].items()}
        d["roles"] = RoleMappingConfig(**roles)
        return cls(**d)


def _role_list(value: Any) -> list[str]:
    if isinstance(value, str):
        return [r.strip() for r in value.split(",") if r.strip()]
    return [str(r).strip() for r in value if str(r).strip()]


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
    next_root_commitment: str | None = None
    """Root rotation pin, 64 hex (``ASTER_NEXT_ROOT_COMMITMENT``). Without it
    every rotation statement is refused."""
    root_seq: int = 0
    """Rotations applied to reach the root (``ASTER_ROOT_SEQ``)."""
    root_rotations: tuple[str, ...] = ()
    """Applied rotation statements, oldest first (``ASTER_ROOT_ROTATIONS``,
    comma-separated); requires ``next_root_commitment``."""
    enrollment_credential_file: str | None = None
    """This node's enrollment credential chain file (``aster enroll node
    --out``); TOML ``[trust] enrollment_credential``, env
    ``ASTER_ENROLLMENT_CREDENTIAL``. Setting it enables enrollment."""
    enrollment_credential_chain: str | None = dc_field(default=None, repr=False)
    """The credential chain inline; wins over the file
    (``ASTER_ENROLLMENT_CREDENTIAL_CHAIN``)."""

    # -- Connect -------------------------------------------------------------
    endpoint_addr: str | None = None
    """Producer address a consumer dials (``aster1...`` ticket / NodeAddr / id)."""

    # -- Storage -------------------------------------------------------------
    storage_path: str | None = None
    """Persistent node directory; ``None`` = in-memory."""
    gc_interval_ms: int | None = None
    """Blob GC interval in ms; ``None`` / ``0`` = no GC."""
    blob_zstd_level: int | None = 3
    """zstd level for blob transport (``ASTER_BLOB_ZSTD_LEVEL``, TOML
    ``[storage] blob_zstd_level``). ``0``, ``None`` or ``"off"`` turns it off:
    the node serves and fetches blobs on the plain ALPN only. Levels above 19
    are clamped; the resolved value is ``None`` when off."""

    # -- Identity ------------------------------------------------------------
    secret_key: bytes | None = dc_field(default=None, repr=False)
    """32-byte node key (accepts ``SecretKey`` / 64-hex too). Fixes the node id."""
    secret_key_file: str | None = None
    """64-hex secret key file, loaded or created at start."""

    # -- Network -------------------------------------------------------------
    relay_mode: str = "default"
    relay_urls: tuple[str, ...] = ()
    bind_addr: str | None = None
    reuse_last_port: bool = True
    """On a persistent node, re-bind the UDP ports it last bound when they are
    still free (``ASTER_REUSE_LAST_PORT``, TOML ``[network] reuse_last_port``).
    Applies only when ``bind_addr`` leaves the port to the OS; the ports are
    recorded in ``bind-ports.json`` in ``storage_path``."""
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

    # -- Enrollment ([enrollment]) ----------------------------------------------
    enrollment: EnrollmentConfig = dc_field(default_factory=EnrollmentConfig)
    """The ``[enrollment]`` section; also accepts a dict of its fields."""

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
        if isinstance(raw["root_rotations"], str):
            raw["root_rotations"] = [raw["root_rotations"]]
        raw["root_rotations"] = list(raw["root_rotations"] or ())
        enrollment = raw["enrollment"]
        if enrollment is None:
            enrollment = EnrollmentConfig()
        elif isinstance(enrollment, Mapping):
            enrollment = EnrollmentConfig(**enrollment)
        elif not isinstance(enrollment, EnrollmentConfig):
            raise TypeError(
                f"enrollment: expected EnrollmentConfig or dict, got {type(enrollment).__name__}"
            )
        raw["enrollment"] = enrollment._to_native()
        native = NativeAsterConfig(**raw)
        self._adopt(native)

    def _adopt(self, native: NativeAsterConfig) -> None:
        """Set every field to the canonical value of the Rust config."""
        for name, value in native.to_dict().items():
            if name in ("relay_urls", "alpns", "root_rotations"):
                value = tuple(value)
            elif name == "enrollment":
                value = EnrollmentConfig._from_native(value)
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
    def is_enrollment_enabled(self) -> bool:
        """Whether this configuration switches Enrollment on (Rust
        ``AsterConfig::is_enrollment_enabled``): ``enrollment.enabled`` when
        set, else whether an audience, fleet namespace, issuer delegation or
        credential is configured. The root key alone does not."""
        return self._native().is_enrollment_enabled()

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
        ``enrollment_credential_chain``) are masked. Returns the rendered text
        (also printed to stdout).
        """
        def _or(value: Any, default: str) -> Any:
            return value if value is not None else default

        sections = {
            "trust": [
                ("root_pubkey", self._fmt_bytes(self.root_pubkey, mask=False)),
                ("root_pubkey_file", self.root_pubkey_file or "<not set>"),
                ("next_root_commitment", self.next_root_commitment or "<not set>"),
                ("root_seq", self.root_seq),
                ("enrollment_credential_file", self._fmt_masked(self.enrollment_credential_file)),
                ("enrollment_credential_chain", self._fmt_masked(self.enrollment_credential_chain)),
            ],
            "enrollment": [
                ("enabled", self.is_enrollment_enabled),
                ("audience", self.enrollment.audience or "<not set>"),
                ("fleet_namespace", self.enrollment.fleet_namespace or "<not set>"),
                ("state_path", self.enrollment.state_path or "<default>"),
                ("replicas", ", ".join(self.enrollment.replicas) or "<none>"),
                ("issuer_delegation_file", self.enrollment.issuer_delegation_file or "<not set>"),
                ("renew_from", self.enrollment.renew_from or "<not set>"),
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
                ("reuse_last_port", self.reuse_last_port),
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
                ("blob_zstd_level", _or(self.blob_zstd_level, "<off>")),
            ],
        }
        source_key = {"path": "storage_path"}
        enrollment_keys = {
            "enabled", "audience", "fleet_namespace", "state_path", "replicas",
            "issuer_delegation_file", "renew_from",
        }

        def _source(name: str) -> str:
            if name in enrollment_keys:
                return self._sources.get("enrollment", "default")
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


__all__ = ["AsterConfig", "EnrollmentConfig", "EnrollmentLimits", "RoleMappingConfig"]
