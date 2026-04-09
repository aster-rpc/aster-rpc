"""Helpers for talking to the Day 0 @aster service from the CLI."""

from __future__ import annotations

import contextlib
import importlib
import json
import os
import secrets
import sys
import tempfile
import time
import asyncio
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from aster.trust.signing import load_private_key
from aster_cli.codegen import generate_python_clients
from aster_cli.credentials import get_root_privkey
from aster_cli.profile import get_active_profile, get_aster_service_config


def now_epoch_seconds() -> int:
    return int(time.time())


def generate_nonce() -> str:
    return secrets.token_hex(16)


def canonical_payload_json(payload: dict[str, Any]) -> str:
    """Serialize payloads in a stable form before signing."""
    return json.dumps(payload, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def load_root_private_key_hex(
    *,
    root_key_file: str | None = None,
    profile_name: str | None = None,
) -> tuple[str, str]:
    """Return (profile_name, private_key_hex) for the active profile."""
    active_name, _profile, _config = get_active_profile()
    name = profile_name or active_name

    priv_hex = get_root_privkey(name)
    if priv_hex:
        return name, priv_hex

    key_path = Path(os.path.expanduser(root_key_file or "~/.aster/root.key"))
    if key_path.exists():
        data = json.loads(key_path.read_text())
        priv_hex = str(data.get("private_key", "")).strip()
        if priv_hex:
            return name, priv_hex

    raise RuntimeError(
        f"no root private key found for profile '{name}'. "
        "Run `aster keygen root` first, or pass --root-key."
    )


def resolve_aster_service_address(explicit: str | None = None) -> str:
    """Resolve the @aster service address from CLI arg, env, or config."""
    if explicit:
        return explicit

    env_addr = os.environ.get("ASTER_SERVICE_ADDR", "").strip()
    if env_addr:
        return env_addr

    _name, _profile, config = get_active_profile()
    service_cfg = get_aster_service_config(config)
    if not service_cfg.get("enabled", True):
        raise RuntimeError(
            "@aster service access is disabled in config. "
            "Pass --aster to override, or re-enable `[aster_service].enabled`."
        )

    addr = str(service_cfg.get("node_id", "")).strip()
    if addr:
        return addr

    raise RuntimeError(
        "no @aster service address configured. "
        "Set `[aster_service].node_id`, export `ASTER_SERVICE_ADDR`, or pass --aster."
    )


def parse_duration_seconds(value: str | int | None, *, default: int) -> int:
    """Parse a simple duration like 300, 5m, 1h, or 1d."""
    if value is None:
        return default
    if isinstance(value, int):
        return value

    raw = str(value).strip().lower()
    if not raw:
        return default
    if raw.isdigit():
        return int(raw)

    unit = raw[-1]
    number = raw[:-1]
    if not number.isdigit():
        raise ValueError(f"invalid duration: {value!r}")
    count = int(number)
    if unit == "s":
        return count
    if unit == "m":
        return count * 60
    if unit == "h":
        return count * 3600
    if unit == "d":
        return count * 86400
    raise ValueError(f"invalid duration: {value!r}")


def load_local_endpoint_id(identity_path: str | None = None) -> str | None:
    """Load the current node endpoint_id from .aster-identity if present."""
    path = Path(identity_path or ".aster-identity")
    if not path.exists():
        return None

    from aster_cli.identity import load_identity

    data = load_identity(path)
    node = data.get("node", {})
    endpoint_id = str(node.get("endpoint_id", "")).strip()
    return endpoint_id or None


@dataclass
class SignedEnvelope:
    payload: dict[str, Any]
    payload_json: str
    signer_pubkey: str
    signature: str


def build_signed_envelope(
    payload: dict[str, Any],
    *,
    root_key_file: str | None = None,
    profile_name: str | None = None,
    signer_pubkey: str | None = None,
) -> SignedEnvelope:
    """Sign a payload for the Day 0 @aster SignedRequest wrapper."""
    active_name, profile, _config = get_active_profile()
    name, priv_hex = load_root_private_key_hex(
        root_key_file=root_key_file,
        profile_name=profile_name or active_name,
    )
    pub_hex = signer_pubkey or str(profile.get("root_pubkey", "")).strip()
    if not pub_hex:
        raise RuntimeError(
            f"profile '{name}' has no root public key configured. "
            "Run `aster keygen root` first."
        )

    payload_json = canonical_payload_json(payload)
    signature = load_private_key(bytes.fromhex(priv_hex)).sign(payload_json.encode("utf-8")).hex()
    return SignedEnvelope(
        payload=payload,
        payload_json=payload_json,
        signer_pubkey=pub_hex,
        signature=signature,
    )


class AsterServiceRuntime:
    """Runtime-generated typed clients for the current @aster node."""

    def __init__(self, address: str):
        self.address = address
        self._peer: PeerConnection | None = None
        self._package_name: str | None = None
        self._temp_dir: str | None = None
        self._loaded: bool = False
        self._types_signed_request: type[Any] | None = None
        self._profile_client_cls: type[Any] | None = None
        self._publication_client_cls: type[Any] | None = None
        self._access_client_cls: type[Any] | None = None
        self._clients: dict[str, Any] = {}

    async def connect(self) -> None:
        from aster_cli.shell.app import PeerConnection

        peer = PeerConnection(self.address)
        await peer.connect()
        manifests: dict[str, dict[str, Any]] = {}
        last_error: Exception | None = None
        for _attempt in range(4):
            try:
                await peer._fetch_manifests()
            except Exception as exc:
                last_error = exc
            manifests = peer.get_manifests()
            if manifests:
                break
            await asyncio.sleep(0.5)
        if not manifests:
            if last_error is not None:
                raise RuntimeError(f"no service manifests available from @aster ({last_error})")
            raise RuntimeError("no service manifests available from @aster")

        temp_dir = tempfile.mkdtemp(prefix="aster-cli-runtime-")
        package_name = f"aster_cli_runtime_{os.getpid()}"
        generate_python_clients(
            manifests,
            out_dir=temp_dir,
            namespace=package_name,
            source=self.address,
        )

        sys.path.insert(0, temp_dir)
        try:
            signed_mod = importlib.import_module(
                f"{package_name}.types.signed_request"
            )
            profile_mod = importlib.import_module(
                f"{package_name}.services.profile_service_v1"
            )
            publication_mod = importlib.import_module(
                f"{package_name}.services.publication_service_v1"
            )
            access_mod = importlib.import_module(
                f"{package_name}.services.access_service_v1"
            )
        except Exception:
            with contextlib.suppress(ValueError):
                sys.path.remove(temp_dir)
            raise

        self._peer = peer
        self._temp_dir = temp_dir
        self._package_name = package_name
        self._types_signed_request = signed_mod.SignedRequest
        self._profile_client_cls = profile_mod.ProfileServiceClient
        self._publication_client_cls = publication_mod.PublicationServiceClient
        self._access_client_cls = access_mod.AccessServiceClient
        self._loaded = True

    async def close(self) -> None:
        if self._peer and getattr(self._peer, "_aster_client", None) is not None:
            with contextlib.suppress(Exception):
                await self._peer._aster_client.close()
        self._clients.clear()
        if self._temp_dir:
            with contextlib.suppress(ValueError):
                sys.path.remove(self._temp_dir)

    async def __aenter__(self) -> AsterServiceRuntime:
        await self.connect()
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        await self.close()

    def signed_request(self, envelope: SignedEnvelope) -> Any:
        if not self._loaded or self._types_signed_request is None:
            raise RuntimeError("runtime clients not loaded")
        return self._types_signed_request(
            payload_json=envelope.payload_json,
            signer_pubkey=envelope.signer_pubkey,
            signature=envelope.signature,
        )

    async def profile_client(self) -> Any:
        if "profile" not in self._clients:
            if self._profile_client_cls is None or self._peer is None:
                raise RuntimeError("runtime clients not loaded")
            self._clients["profile"] = await self._profile_client_cls.from_connection(
                self._peer._aster_client
            )
        return self._clients["profile"]

    async def publication_client(self) -> Any:
        if "publication" not in self._clients:
            if self._publication_client_cls is None or self._peer is None:
                raise RuntimeError("runtime clients not loaded")
            self._clients["publication"] = await self._publication_client_cls.from_connection(
                self._peer._aster_client
            )
        return self._clients["publication"]

    async def access_client(self) -> Any:
        if "access" not in self._clients:
            if self._access_client_cls is None or self._peer is None:
                raise RuntimeError("runtime clients not loaded")
            self._clients["access"] = await self._access_client_cls.from_connection(
                self._peer._aster_client
            )
        return self._clients["access"]


async def open_aster_service(explicit_address: str | None = None) -> AsterServiceRuntime:
    runtime = AsterServiceRuntime(resolve_aster_service_address(explicit_address))
    await runtime.connect()
    return runtime
