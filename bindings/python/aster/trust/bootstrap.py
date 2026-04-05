"""
aster.trust.bootstrap — Producer mesh bootstrap (founding node + join).

Spec reference: Aster-trust-spec.md §2.1, §2.5.  Plan: ASTER_PLAN.md §14.5.

Two startup modes:

start_founding_node()
    The first producer in a new mesh.  Generates a random 32-byte salt, derives
    the gossip topic, initializes MeshState, and prints a bootstrap ticket for
    subsequent nodes to present.

join_mesh()
    A subsequent producer.  Dials the bootstrap peer over the
    ``aster.producer_admission`` ALPN, presents its credential, receives salt
    and accepted_producers, then subscribes to the gossip topic.

Both modes persist state to ``~/.aster/`` for crash recovery.

Environment variables:
    ASTER_ENROLLMENT        Path to JSON-serialized EnrollmentCredential.
    ASTER_ROOT_KEY          Path to 32-byte raw ed25519 private key file.
    ASTER_BOOTSTRAP_TICKET  NodeAddr ticket string for subsequent node join.
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


def _load_mesh_state() -> MeshState | None:
    """Load MeshState from ``~/.aster/mesh_state.json``, or return None."""
    path = _state_path("mesh_state.json")
    if not os.path.exists(path):
        return None
    try:
        with open(path) as fh:
            d = json.load(fh)
        return MeshState.from_json_dict(d)
    except Exception as exc:  # noqa: BLE001
        logger.warning("bootstrap: failed to load mesh_state.json: %s", exc)
        return None


def start_founding_node(
    enrollment_path: str | None = None,
    config: ClockDriftConfig | None = None,
    force_new_salt: bool = False,
) -> MeshState:
    """Start the founding node of a new producer mesh.

    Steps (§2.1):
    1. Load own enrollment credential; verify offline.
    2. Load or generate producer signing key.
    3. Load or generate 32-byte salt.
    4. Derive topic_id = blake3(root_pubkey + b"aster-producer-mesh" + salt).
    5. Initialize MeshState with {self} as the only accepted producer.
    6. Persist state.
    7. Print bootstrap ticket to stdout.

    Returns:
        The initialized MeshState.
    """
    from .admission import check_offline
    from .nonces import InMemoryNonceStore

    # 1. Load and verify credential.
    cred = _load_enrollment_credential(enrollment_path)
    # check_offline is async; use asyncio.run for the synchronous bootstrap path.
    import asyncio as _asyncio

    _result = _asyncio.run(check_offline(cred, cred.endpoint_id, InMemoryNonceStore()))
    if not _result.admitted:
        raise RuntimeError(f"Founding node credential invalid: {_result.reason}")

    # 2. Producer signing key (load/generate; used by caller for signing gossip messages).
    _load_or_generate_producer_key()

    # 3. Salt.
    if force_new_salt:
        salt = secrets.token_bytes(32)
        salt_path = _state_path("mesh_salt")
        os.makedirs(_state_dir(), exist_ok=True)
        with open(salt_path, "wb") as fh:
            fh.write(salt)
    else:
        salt = _load_or_generate_salt()

    # 4. Topic derivation.
    topic_id = derive_gossip_topic(cred.root_pubkey, salt)

    # 5. MeshState.
    now_ms = int(time.time() * 1000)
    state = MeshState(
        accepted_producers={cred.endpoint_id},
        salt=salt,
        topic_id=topic_id,
        peer_offsets={},
        drift_isolated=set(),
        last_heartbeat_epoch_ms=now_ms,
        mesh_joined_at_epoch_ms=now_ms,
    )

    # 6. Persist.
    _save_mesh_state(state)

    # 7. Print bootstrap ticket.
    ticket = cred.endpoint_id
    print("Aster producer mesh started.")
    print(f"  endpoint_id : {cred.endpoint_id}")
    print(f"  topic_id    : {topic_id.hex()}")
    print(f"  salt        : {salt.hex()}")
    print(f"  Bootstrap ticket: {ticket}")
    print("Pass this ticket via ASTER_BOOTSTRAP_TICKET to subsequent nodes.")

    return state


def join_mesh(
    enrollment_path: str | None = None,
    bootstrap_ticket: str | None = None,
    config: ClockDriftConfig | None = None,
) -> MeshState | None:
    """Join an existing producer mesh.

    Steps (§2.5):
    1. Load own credential + bootstrap ticket.
    2. Build an AdmissionRequest.
    3. Return a MeshState configured with the bootstrap response.
       (The actual QUIC dial is performed by the caller using the iroh transport;
        this function handles the credential packaging + state setup.)

    In the full runtime flow the caller would:
      - Use iroh NetClient to open a bidi stream with ALPN aster.producer_admission.
      - Send the AdmissionRequest JSON.
      - Receive the AdmissionResponse JSON.
      - Call ``apply_admission_response()`` to finalize MeshState.

    For tests / CLI use, this function returns the AdmissionRequest that the
    caller should send, plus a commit function.

    Returns:
        AdmissionRequest to be sent to the bootstrap peer.
    """
    ticket = bootstrap_ticket or os.environ.get("ASTER_BOOTSTRAP_TICKET")
    if not ticket:
        raise RuntimeError(
            "Set ASTER_BOOTSTRAP_TICKET to the bootstrap endpoint_id ticket"
        )

    cred = _load_enrollment_credential(enrollment_path)

    # Serialize credential to JSON for the request.
    cred_json = json.dumps(
        {
            "endpoint_id": cred.endpoint_id,
            "root_pubkey": cred.root_pubkey.hex(),
            "expires_at": cred.expires_at,
            "attributes": cred.attributes,
            "signature": cred.signature.hex(),
        },
        separators=(",", ":"),
    )

    req = AdmissionRequest(credential_json=cred_json)
    logger.info(
        "bootstrap: prepared AdmissionRequest for bootstrap peer %s", ticket
    )
    return req


def apply_admission_response(
    response: AdmissionResponse,
    own_endpoint_id: str,
) -> MeshState:
    """Finalize MeshState after receiving a successful AdmissionResponse.

    Args:
        response:         The AdmissionResponse from the bootstrap peer.
        own_endpoint_id:  This node's endpoint ID.

    Returns:
        Initialized and persisted MeshState ready for gossip subscription.

    Raises:
        RuntimeError: if ``response.accepted`` is False.
    """
    if not response.accepted:
        raise RuntimeError(
            f"Admission refused: {response.reason or '(no reason provided)'}"
        )

    # Load root pubkey from own credential.
    cred = _load_enrollment_credential()
    topic_id = derive_gossip_topic(cred.root_pubkey, response.salt)

    now_ms = int(time.time() * 1000)
    accepted = set(response.accepted_producers) | {own_endpoint_id}
    state = MeshState(
        accepted_producers=accepted,
        salt=response.salt,
        topic_id=topic_id,
        peer_offsets={},
        drift_isolated=set(),
        last_heartbeat_epoch_ms=now_ms,
        mesh_joined_at_epoch_ms=now_ms,
    )

    # Persist salt + state.
    salt_path = _state_path("mesh_salt")
    os.makedirs(_state_dir(), exist_ok=True)
    with open(salt_path, "wb") as fh:
        fh.write(response.salt)
    _save_mesh_state(state)

    logger.info(
        "bootstrap: joined mesh with %d accepted producers, topic %s",
        len(accepted),
        topic_id.hex(),
    )
    return state


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
        logger.info("admission: rejected %s — %s", cred.endpoint_id, result.reason)
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


# ── Server-side serve loop (symmetric with serve_consumer_admission) ─────────


async def serve_producer_admission(
    endpoint: object,
    *,
    own_root_pubkey: bytes,
    own_state: MeshState,
    config: ClockDriftConfig | None = None,
    persist_state: bool = True,
) -> None:
    """Accept and process connections on ``aster.producer_admission`` until cancelled.

    Mirrors :func:`aster.trust.consumer.serve_consumer_admission`. Runs as a
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
            "reason": "",  # oracle protection — never leak on wire
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
