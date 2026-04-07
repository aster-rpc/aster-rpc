"""
aster.trust.gossip — Producer mesh gossip: signing, verification, and dispatch.

Spec reference: Aster-trust-spec.md §2.3, §2.6.  Plan: ASTER_PLAN.md §14.2–14.6.

Canonical signing bytes (normative — §2.6, ASTER_PLAN.md §14.2):

    u8(type) || payload || sender.encode('utf-8') || u64_be(epoch_ms)

Do NOT reorder ``epoch_ms`` before ``sender+payload`` — the spec fixes this
byte order and any deviation breaks cross-implementation verification.

Topic derivation (§2.3):

    blake3(root_pubkey + b"aster-producer-mesh" + salt).digest()[:32]
"""

from __future__ import annotations

import logging
import struct
import time
from typing import Any, Callable

from .mesh import (
    ClockDriftConfig,
    ContractPublishedPayload,
    LeaseUpdatePayload,
    MeshState,
    ProducerMessage,
    ProducerMessageType,
)
from .rcan import validate_rcan

logger = logging.getLogger(__name__)


# ── Topic derivation ──────────────────────────────────────────────────────────


def derive_gossip_topic(root_pubkey: bytes, salt: bytes) -> bytes:
    """Derive the 32-byte gossip topic for the producer mesh.

    ``blake3(root_pubkey + b"aster-producer-mesh" + salt).digest()``

    The salt keeps the topic private to admitted producers.
    """
    import blake3  # type: ignore[import]

    data = root_pubkey + b"aster-producer-mesh" + salt
    return blake3.blake3(data).digest()


# ── Canonical signing bytes ───────────────────────────────────────────────────


def producer_message_signing_bytes(
    msg_type: int,
    payload: bytes,
    sender: str,
    epoch_ms: int,
) -> bytes:
    """Return the canonical bytes that are signed / verified.

    Normative byte order (§2.6):
        u8(type) || payload || sender.encode('utf-8') || u64_be(epoch_ms)
    """
    return b"".join(
        [
            struct.pack("B", msg_type),
            payload,
            sender.encode("utf-8"),
            struct.pack(">Q", epoch_ms),
        ]
    )


# ── Sign / verify ─────────────────────────────────────────────────────────────


def sign_producer_message(
    msg_type: int,
    payload: bytes,
    sender: str,
    epoch_ms: int,
    signing_key_raw: bytes,
) -> ProducerMessage:
    """Create and sign a ProducerMessage.

    Args:
        msg_type:        ProducerMessageType integer value.
        payload:         Serialized per-type payload bytes.
        sender:          This node's endpoint_id (hex string).
        epoch_ms:        Wall-clock timestamp in milliseconds.
        signing_key_raw: 32-byte raw ed25519 private key seed.

    Returns:
        A fully signed ``ProducerMessage``.
    """
    from .signing import load_private_key

    to_sign = producer_message_signing_bytes(msg_type, payload, sender, epoch_ms)
    privkey = load_private_key(signing_key_raw)
    signature = privkey.sign(to_sign)
    return ProducerMessage(
        type=msg_type,
        payload=payload,
        sender=sender,
        epoch_ms=epoch_ms,
        signature=signature,
    )


def verify_producer_message(msg: ProducerMessage, peer_pubkey_raw: bytes) -> bool:
    """Verify a ProducerMessage's signature against the sender's public key.

    Returns True on success, False on any verification failure.
    """
    from cryptography.exceptions import InvalidSignature

    from .signing import load_public_key

    try:
        to_verify = producer_message_signing_bytes(
            msg.type, msg.payload, msg.sender, msg.epoch_ms
        )
        pubkey = load_public_key(peer_pubkey_raw)
        pubkey.verify(msg.signature, to_verify)
        return True
    except (InvalidSignature, Exception):  # noqa: BLE001
        return False


# ── Gossip handler ────────────────────────────────────────────────────────────


async def handle_producer_message(
    msg: ProducerMessage,
    state: MeshState,
    config: ClockDriftConfig,
    peer_pubkeys: dict[str, bytes],
    registry_callback: Callable[[str, object], None] | None = None,
    drift_detector: "ClockDriftDetector | None" = None,  # noqa: F821
    on_self_departure: Callable[[], None] | None = None,
) -> None:
    """Process one inbound ProducerMessage.

    Normative processing order (§2.6, §2.10):
    1. Replay-window check.
    2. Sender membership check.
    3. Signature verification.
    4. Track clock offset / run drift detection.
    5. Dispatch by message type.

    Args:
        msg:               Incoming message.
        state:             Mutable MeshState shared by the node.
        config:            Clock drift / replay-window configuration.
        peer_pubkeys:      Maps endpoint_id → raw 32-byte ed25519 public key.
        registry_callback: Optional callback(event_type, payload) for Phase 10
                           integration (ContractPublished / LeaseUpdate).
        drift_detector:    Optional ClockDriftDetector instance.
        on_self_departure: Optional zero-arg callback invoked when self-departure
                           is triggered.
    """
    now_ms = int(time.time() * 1000)

    # ── 1. Replay-window check ────────────────────────────────────────────────
    delta = abs(now_ms - msg.epoch_ms)
    if delta > config.replay_window_ms:
        logger.debug(
            "gossip: dropping message from %s (outside replay window: delta=%dms)",
            msg.sender,
            delta,
        )
        return

    # ── 2. Sender membership check ────────────────────────────────────────────
    if msg.sender not in state.accepted_producers:
        logger.warning(
            "gossip: SECURITY ALERT — message from non-accepted sender %s; "
            "possible salt leak or deauthorized node still subscribed",
            msg.sender,
        )
        return

    # ── 3. Signature verification ─────────────────────────────────────────────
    peer_pubkey = peer_pubkeys.get(msg.sender)
    if peer_pubkey is None:
        # Cannot verify — no public key on file for this sender.
        logger.warning(
            "gossip: SECURITY ALERT — no public key for accepted sender %s; "
            "dropping message",
            msg.sender,
        )
        return

    if not verify_producer_message(msg, peer_pubkey):
        logger.warning(
            "gossip: SECURITY ALERT — invalid signature from accepted sender %s",
            msg.sender,
        )
        return

    # ── 4. Clock offset tracking + drift detection ────────────────────────────
    offset = now_ms - msg.epoch_ms
    state.peer_offsets[msg.sender] = offset

    if drift_detector is not None:
        drift_detector.track_offset(msg.sender, msg.epoch_ms)

        # Peer drift isolation / recovery
        if drift_detector.peer_in_drift(msg.sender):
            if msg.sender not in state.drift_isolated:
                logger.warning(
                    "gossip: peer %s clock drift exceeds tolerance (offset=%dms); "
                    "isolating",
                    msg.sender,
                    offset,
                )
                state.drift_isolated.add(msg.sender)
        else:
            # Fresh acceptable message — recover from isolation if present
            if msg.sender in state.drift_isolated:
                logger.info(
                    "gossip: peer %s recovered from drift isolation",
                    msg.sender,
                )
                state.drift_isolated.discard(msg.sender)

        # Self-drift check (self-offset ≈ 0 if our clock is accurate)
        self_offset_estimate = 0  # We just sent, so our offset is near zero
        if drift_detector.self_in_drift(self_offset_estimate) and not state.mesh_dead:
            logger.error(
                "gossip: self-departure triggered — local clock deviates from "
                "mesh median; broadcasting Depart and suppressing further sends"
            )
            state.mesh_dead = True
            if on_self_departure is not None:
                on_self_departure()
            return

    # ── 5. Message dispatch ───────────────────────────────────────────────────
    msg_type = msg.type
    if msg_type == ProducerMessageType.INTRODUCE:
        _handle_introduce(msg, state)
    elif msg_type == ProducerMessageType.DEPART:
        _handle_depart(msg, state, drift_detector)
    elif msg_type == ProducerMessageType.CONTRACT_PUBLISHED:
        _handle_contract_published(msg, state, registry_callback)
    elif msg_type == ProducerMessageType.LEASE_UPDATE:
        _handle_lease_update(msg, state, registry_callback)
    else:
        logger.debug("gossip: unknown message type %d from %s; dropping", msg_type, msg.sender)


# ── Type-specific dispatch helpers ────────────────────────────────────────────


def _handle_introduce(msg: ProducerMessage, state: MeshState) -> None:
    ok, reason = validate_rcan(msg.payload)
    if not ok:
        logger.debug(
            "gossip: Introduce from %s has invalid rcan: %s", msg.sender, reason
        )
        return
    state.accepted_producers.add(msg.sender)
    logger.info("gossip: Introduce accepted — %s joined the mesh", msg.sender)


def _handle_depart(
    msg: ProducerMessage,
    state: MeshState,
    drift_detector: "ClockDriftDetector | None",  # noqa: F821
) -> None:
    state.accepted_producers.discard(msg.sender)
    state.drift_isolated.discard(msg.sender)
    state.peer_offsets.pop(msg.sender, None)
    if drift_detector is not None:
        drift_detector.remove_peer(msg.sender)
    logger.info("gossip: Depart — %s left the mesh", msg.sender)


def _handle_contract_published(
    msg: ProducerMessage,
    state: MeshState,
    registry_callback: Callable[[str, object], None] | None,
) -> None:
    if msg.sender in state.drift_isolated:
        logger.debug(
            "gossip: ContractPublished from drift-isolated peer %s; skipping",
            msg.sender,
        )
        return
    if registry_callback is not None:
        payload = ContractPublishedPayload.__new__(ContractPublishedPayload)
        # Parse payload bytes as simple JSON to avoid heavy Fory dependency here.
        import json

        try:
            from aster.limits import MAX_GOSSIP_PAYLOAD_SIZE
            raw = msg.payload
            if len(raw) > MAX_GOSSIP_PAYLOAD_SIZE:
                logger.warning("gossip: payload too large (%d bytes), dropping", len(raw))
                return
            d = json.loads(raw.decode("utf-8"))
            payload.service_name = d["service_name"]
            payload.version = int(d["version"])
            payload.contract_collection_hash = d["contract_collection_hash"]
            registry_callback("contract_published", payload)
        except Exception as exc:  # noqa: BLE001
            logger.debug("gossip: malformed ContractPublished payload: %s", exc)


def _handle_lease_update(
    msg: ProducerMessage,
    state: MeshState,
    registry_callback: Callable[[str, object], None] | None,
) -> None:
    if msg.sender in state.drift_isolated:
        logger.debug(
            "gossip: LeaseUpdate from drift-isolated peer %s; skipping",
            msg.sender,
        )
        return
    if registry_callback is not None:
        import json

        try:
            from aster.limits import MAX_GOSSIP_PAYLOAD_SIZE
            if len(msg.payload) > MAX_GOSSIP_PAYLOAD_SIZE:
                logger.warning("gossip: LeaseUpdate payload too large (%d bytes)", len(msg.payload))
                return
            d = json.loads(msg.payload.decode("utf-8"))
            payload = LeaseUpdatePayload(
                service_name=d["service_name"],
                version=int(d["version"]),
                contract_id=d["contract_id"],
                health_status=d["health_status"],
                addressing_info=d.get("addressing_info", {}),
            )
            registry_callback("lease_update", payload)
        except Exception as exc:  # noqa: BLE001
            logger.debug("gossip: malformed LeaseUpdate payload: %s", exc)


# ── Lease heartbeat ──────────────────────────────────────────────────────────


async def run_lease_heartbeat(
    gossip_topic_handle: object,
    sender: str,
    signing_key_raw: bytes,
    service_name: str,
    version: int,
    contract_id: str,
    health_getter: "Callable[[], str]",
    heartbeat_interval_ms: int = 900_000,
    addressing_info: "dict[str, str] | None" = None,
) -> None:
    """Broadcast a signed LeaseUpdate message every ``heartbeat_interval_ms``.

    This coroutine runs until cancelled.  Callers should wrap it with
    ``asyncio.create_task`` and cancel the task on shutdown.

    Args:
        gossip_topic_handle: A ``GossipTopicHandle`` with a
            ``broadcast(data: bytes) -> Coroutine`` method.
        sender:              This node's endpoint_id (hex string).
        signing_key_raw:     32-byte raw ed25519 private key seed.
        service_name:        Service name to include in the LeaseUpdate payload.
        version:             Service version.
        contract_id:         Contract ID (64-char hex).
        health_getter:       Zero-arg callable returning current health status
                             string.  Called fresh on each heartbeat so state
                             transitions are reflected immediately.
        heartbeat_interval_ms: Broadcast cadence in milliseconds (default 15 min).
        addressing_info:     Optional addressing metadata forwarded in the payload.
    """
    import asyncio

    interval_s = heartbeat_interval_ms / 1000.0
    try:
        while True:
            await asyncio.sleep(interval_s)
            epoch_ms = int(time.time() * 1000)
            payload = encode_lease_update_payload(
                service_name=service_name,
                version=version,
                contract_id=contract_id,
                health_status=health_getter(),
                addressing_info=addressing_info,
            )
            msg = sign_producer_message(
                msg_type=ProducerMessageType.LEASE_UPDATE,
                payload=payload,
                sender=sender,
                epoch_ms=epoch_ms,
                signing_key_raw=signing_key_raw,
            )
            # Serialize the signed envelope as JSON for the gossip wire.
            import json as _json
            wire = _json.dumps(
                {
                    "type": msg.type,
                    "payload": msg.payload.hex(),
                    "sender": msg.sender,
                    "epoch_ms": msg.epoch_ms,
                    "signature": msg.signature.hex(),
                },
                separators=(",", ":"),
            ).encode("utf-8")
            try:
                await gossip_topic_handle.broadcast(wire)
                logger.debug(
                    "heartbeat: LeaseUpdate broadcast for %s v%d (epoch=%d)",
                    service_name,
                    version,
                    epoch_ms,
                )
            except Exception as exc:  # noqa: BLE001
                logger.warning("heartbeat: broadcast failed: %s", exc)
    except asyncio.CancelledError:
        pass


def start_lease_heartbeat(
    gossip_topic_handle: object,
    sender: str,
    signing_key_raw: bytes,
    service_name: str,
    version: int,
    contract_id: str,
    health_getter: "Callable[[], str]",
    heartbeat_interval_ms: int = 900_000,
    addressing_info: "dict[str, str] | None" = None,
) -> "Any":
    """Spawn ``run_lease_heartbeat`` as an asyncio background task.

    Returns the ``asyncio.Task`` so the caller can cancel it on shutdown.
    """
    import asyncio

    return asyncio.create_task(
        run_lease_heartbeat(
            gossip_topic_handle=gossip_topic_handle,
            sender=sender,
            signing_key_raw=signing_key_raw,
            service_name=service_name,
            version=version,
            contract_id=contract_id,
            health_getter=health_getter,
            heartbeat_interval_ms=heartbeat_interval_ms,
            addressing_info=addressing_info,
        ),
        name=f"lease-heartbeat-{service_name}-v{version}",
    )


# ── Payload serializers (JSON-based for Phase 12) ─────────────────────────────


def encode_introduce_payload(rcan: bytes) -> bytes:
    """Encode IntroducePayload.  The rcan grant is stored as raw bytes."""
    # Use rcan bytes directly as payload — opaque pass-through.
    return rcan


def encode_depart_payload(reason: str = "") -> bytes:
    """Encode DepartPayload as UTF-8 JSON."""
    import json

    return json.dumps({"reason": reason}, separators=(",", ":")).encode("utf-8")


def encode_contract_published_payload(
    service_name: str, version: int, contract_collection_hash: str
) -> bytes:
    """Encode ContractPublishedPayload as UTF-8 JSON."""
    import json

    return json.dumps(
        {
            "service_name": service_name,
            "version": version,
            "contract_collection_hash": contract_collection_hash,
        },
        separators=(",", ":"),
    ).encode("utf-8")


def encode_lease_update_payload(
    service_name: str,
    version: int,
    contract_id: str,
    health_status: str,
    addressing_info: dict[str, str] | None = None,
) -> bytes:
    """Encode LeaseUpdatePayload as UTF-8 JSON."""
    import json

    return json.dumps(
        {
            "service_name": service_name,
            "version": version,
            "contract_id": contract_id,
            "health_status": health_status,
            "addressing_info": addressing_info or {},
        },
        separators=(",", ":"),
    ).encode("utf-8")
