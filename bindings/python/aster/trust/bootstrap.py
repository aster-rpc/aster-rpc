"""
aster.trust.bootstrap -- Producer mesh bootstrap (founding node + join).

Spec reference: Aster-trust-spec.md §2.1, §2.5.  Plan: ASTER_PLAN.md §14.5.

Provides helpers for:

handle_admission_rpc()
    Server-side handler for ``aster.producer_admission`` ALPN -- verifies a
    joining producer's credential and updates the mesh state.

serve_producer_admission()
    Accept loop that dispatches incoming admission connections.

make_ephemeral_mesh_state()
    Build an in-memory founding MeshState for single-node / test scenarios.

Environment variables:
    ASTER_ENROLLMENT        Path to JSON-serialized EnrollmentCredential.
    ASTER_ROOT_KEY          Path to 32-byte raw ed25519 private key file.
    ASTER_MESH_STATE_DIR    Override for the state directory (default ~/.aster).
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import secrets
import time

from .mesh import AdmissionRequest, AdmissionResponse, ClockDriftConfig, MeshState
from .gossip import derive_gossip_topic

logger = logging.getLogger(__name__)

_DEFAULT_STATE_DIR = os.path.expanduser("~/.aster")


def _state_dir() -> str:
    return os.environ.get("ASTER_MESH_STATE_DIR", _DEFAULT_STATE_DIR)


def _state_path(name: str) -> str:
    return os.path.join(_state_dir(), name)


def _load_enrollment_credential(path: str | None = None):
    """Load an EnrollmentCredential from JSON file.

    Path resolved from ``path`` → ``ASTER_ENROLLMENT`` env var.
    """
    from .credentials import EnrollmentCredential

    env_path = path or os.environ.get("ASTER_ENROLLMENT")
    if not env_path:
        raise RuntimeError(
            "Set ASTER_ENROLLMENT to the path of your enrollment credential JSON file"
        )
    with open(env_path) as fh:
        d = json.load(fh)
    cred = EnrollmentCredential(
        endpoint_id=d["endpoint_id"],
        root_pubkey=bytes.fromhex(d["root_pubkey"]),
        expires_at=int(d["expires_at"]),
        attributes=d.get("attributes", {}),
        signature=bytes.fromhex(d.get("signature", "")),
    )
    return cred


def _load_or_generate_producer_key() -> bytes:
    """Load the producer signing key from ``~/.aster/producer.key``, or generate one.

    The key is persisted so that the same signing key survives restarts.
    Returns the 32-byte raw private key seed.
    """
    key_path = _state_path("producer.key")
    os.makedirs(_state_dir(), exist_ok=True)
    if os.path.exists(key_path):
        with open(key_path, "rb") as fh:
            raw = fh.read()
        if len(raw) != 32:
            raise ValueError(
                f"Producer key at {key_path} is {len(raw)} bytes; expected 32"
            )
        logger.info("bootstrap: loaded producer key from %s", key_path)
        return raw
    # Generate a fresh key.
    from .signing import generate_root_keypair

    priv_raw, _ = generate_root_keypair()
    fd = os.open(key_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(fd, "wb") as fh:
            fh.write(priv_raw)
    except Exception:
        os.close(fd)
        raise
    logger.info("bootstrap: generated new producer key → %s", key_path)
    return priv_raw


def _load_or_generate_salt() -> bytes:
    """Load the mesh salt from ``~/.aster/mesh_salt``, or generate a fresh one.

    Returns 32 random bytes.
    """
    salt_path = _state_path("mesh_salt")
    os.makedirs(_state_dir(), exist_ok=True)
    if os.path.exists(salt_path):
        with open(salt_path, "rb") as fh:
            salt = fh.read()
        if len(salt) != 32:
            raise ValueError(f"mesh_salt at {salt_path} is {len(salt)} bytes; expected 32")
        logger.info("bootstrap: loaded existing mesh salt")
        return salt
    salt = secrets.token_bytes(32)
    with open(salt_path, "wb") as fh:
        fh.write(salt)
    logger.info("bootstrap: generated new mesh salt → %s", salt_path)
    return salt


def _save_mesh_state(state: MeshState) -> None:
    """Persist MeshState to ``~/.aster/mesh_state.json``."""
    path = _state_path("mesh_state.json")
    os.makedirs(_state_dir(), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w") as fh:
        json.dump(state.to_json_dict(), fh, indent=2)
    os.replace(tmp, path)
    logger.debug("bootstrap: mesh_state saved → %s", path)


async def handle_admission_rpc(
    request_json: str,
    own_state: MeshState,
    own_root_pubkey: bytes,
    config: ClockDriftConfig | None = None,
) -> AdmissionResponse:
    """Server-side handler for aster.producer_admission ALPN.

    Reads an AdmissionRequest, runs Phase 11 admission checks, responds.

    Args:
        request_json:    JSON-serialized AdmissionRequest.
        own_state:       The founding/accepting node's MeshState.
        own_root_pubkey: The root public key used to verify the credential.
        config:          ClockDriftConfig (for future use).

    Returns:
        AdmissionResponse (accepted or rejected with reason).
    """
    from .admission import check_offline
    from .credentials import EnrollmentCredential
    from .nonces import InMemoryNonceStore

    try:
        req_dict = json.loads(request_json)
        cred = EnrollmentCredential(
            endpoint_id=req_dict["endpoint_id"],
            root_pubkey=bytes.fromhex(req_dict["root_pubkey"]),
            expires_at=int(req_dict["expires_at"]),
            attributes=req_dict.get("attributes", {}),
            signature=bytes.fromhex(req_dict.get("signature", "")),
        )
    except Exception as exc:  # noqa: BLE001
        logger.warning("admission: malformed AdmissionRequest: %s", exc)
        return AdmissionResponse(accepted=False, reason="malformed request")

    # Verify the credential's root_pubkey matches the mesh's trusted key.
    if cred.root_pubkey != own_root_pubkey:
        logger.warning(
            "admission: untrusted root key from %s (got %s)",
            cred.endpoint_id,
            cred.root_pubkey.hex()[:12],
        )
        return AdmissionResponse(accepted=False, reason="untrusted root key")

    # Run offline admission checks (signature, expiry, endpoint_id match).
    result = await check_offline(cred, cred.endpoint_id, InMemoryNonceStore())
    if not result.admitted:
        logger.info("admission: rejected %s -- %s", cred.endpoint_id, result.reason)
        return AdmissionResponse(accepted=False, reason=result.reason or "admission check failed")

    # Accept: add to mesh state and return salt + current accepted producers.
    own_state.accepted_producers.add(cred.endpoint_id)
    _save_mesh_state(own_state)

    logger.info("admission: accepted %s into mesh", cred.endpoint_id)
    return AdmissionResponse(
        accepted=True,
        salt=own_state.salt,
        accepted_producers=sorted(own_state.accepted_producers),
        reason="",
    )


# ── Server-side serve loop ────────────────────────────────────────────────────


async def serve_producer_admission(
    endpoint: object,
    *,
    own_root_pubkey: bytes,
    own_state: MeshState,
    config: ClockDriftConfig | None = None,
    persist_state: bool = True,
) -> None:
    """Accept and process connections on ``aster.producer_admission`` until cancelled.

    Runs as a
    background task alongside the main server; each connection is handled in
    its own :class:`asyncio.Task` so one slow peer cannot block others.

    Wire format (newline-free JSON over a bidi-stream):

      request  : ``{"credential_json": "<EnrollmentCredential JSON>", "iid_token": "..."}``
      response : ``{"accepted": bool, "salt": "<hex>", "accepted_producers": [...], "reason": ""}``

    Each accepted producer is added to ``own_state.accepted_producers`` (via
    :func:`handle_admission_rpc`). If ``persist_state`` is True (the default),
    the updated state is written to ``~/.aster/mesh_state.json``; set False
    for ephemeral/in-memory meshes (tests, simple examples).

    Args:
        endpoint:        A ``NetClient`` bound to ``aster.producer_admission``.
        own_root_pubkey: The 32-byte root public key this node trusts.
        own_state:       This node's :class:`MeshState`; mutated on accept.
        config:          Optional :class:`ClockDriftConfig` (future: drift checks).
        persist_state:   If True (default), call ``_save_mesh_state`` on accept.
    """
    try:
        while True:
            conn = await endpoint.accept()
            asyncio.create_task(
                handle_producer_admission_connection(
                    conn,
                    own_root_pubkey=own_root_pubkey,
                    own_state=own_state,
                    config=config,
                    persist_state=persist_state,
                )
            )
    except asyncio.CancelledError:
        pass
    except Exception as exc:  # noqa: BLE001
        logger.error("serve_producer_admission: unexpected error: %s", exc)


async def handle_producer_admission_connection(
    conn: object,
    *,
    own_root_pubkey: bytes,
    own_state: MeshState,
    config: ClockDriftConfig | None = None,
    persist_state: bool = True,
) -> None:
    """Handle one producer admission connection: read request, write response."""
    peer_node_id = conn.remote_id()
    try:
        send, recv = await conn.accept_bi()
        raw = await recv.read_to_end(64 * 1024)
        if not raw:
            logger.warning("producer admission: empty request from %s", peer_node_id)
            return

        # Parse the AdmissionRequest wrapper and extract credential_json.
        try:
            wrapper = json.loads(raw)
            cred_json = wrapper.get("credential_json") or ""
        except (ValueError, AttributeError):
            # Back-compat: accept raw credential JSON as well.
            cred_json = raw.decode("utf-8", errors="replace")

        # Temporarily skip persistence if requested; handle_admission_rpc always
        # calls _save_mesh_state on accept, so we swap the module-level
        # function in-process when persist_state is False.
        if persist_state:
            response = await handle_admission_rpc(
                cred_json, own_state, own_root_pubkey, config
            )
        else:
            _original = globals()["_save_mesh_state"]
            globals()["_save_mesh_state"] = lambda _state: None
            try:
                response = await handle_admission_rpc(
                    cred_json, own_state, own_root_pubkey, config
                )
            finally:
                globals()["_save_mesh_state"] = _original

        payload = {
            "accepted": response.accepted,
            "salt": response.salt.hex(),
            "accepted_producers": list(response.accepted_producers),
            "reason": "",  # oracle protection -- never leak on wire
        }
        await send.write_all(json.dumps(payload, separators=(",", ":")).encode())
        await send.finish()
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "producer admission: error handling connection from %s: %s",
            peer_node_id,
            exc,
        )


def make_ephemeral_mesh_state(root_pubkey: bytes) -> MeshState:
    """Build an in-memory founding :class:`MeshState` for a standalone producer.

    Useful for ``AsterServer(allow_all_producers=False)`` when the caller
    doesn't need persistent mesh state (e.g. single-node demos, tests).
    Generates a fresh random salt and an empty accepted-producer set.
    """
    salt = secrets.token_bytes(32)
    now_ms = int(time.time() * 1000)
    return MeshState(
        accepted_producers=set(),
        salt=salt,
        topic_id=derive_gossip_topic(root_pubkey, salt),
        peer_offsets={},
        drift_isolated=set(),
        last_heartbeat_epoch_ms=now_ms,
        mesh_joined_at_epoch_ms=now_ms,
    )
