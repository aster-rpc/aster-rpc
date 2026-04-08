"""
aster.trust.consumer — Consumer admission handler and wire types.

Spec reference: Aster-trust-spec.md §3.2, §3.2.2.

Two actors:

  Server (producer node)
    ``handle_consumer_admission_rpc`` — verifies credential, admits peer,
    returns ``ConsumerAdmissionResponse`` with services + registry_ticket.

  Client (consumer node)
    ``ConsumerAdmissionRequest`` — the wire message to send.
    ``ConsumerAdmissionResponse.from_json`` — parse the server's reply.

Wire format: newline-delimited JSON over a QUIC bidi-stream on
``aster.consumer_admission`` ALPN.  The client sends one JSON line; the
server responds with one JSON line and closes the stream.
"""

from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import dataclass, field
from typing import Callable, TYPE_CHECKING

from .admission import admit
from .credentials import ConsumerEnrollmentCredential

if TYPE_CHECKING:
    from .hooks import MeshEndpointHook
    from .nonces import NonceStoreProtocol
    from ..registry.models import ServiceSummary

logger = logging.getLogger(__name__)


# ── Wire types ────────────────────────────────────────────────────────────────


@dataclass
class ConsumerAdmissionRequest:
    """Wire message sent by a consumer over ``aster.consumer_admission``.

    ``credential_json`` is a JSON object with these keys:
      credential_type, root_pubkey (hex), expires_at (int), attributes (dict),
      endpoint_id (str | null), nonce (hex | null), signature (hex).
    ``iid_token`` is optional; empty string if not provided.
    """

    credential_json: str
    iid_token: str = ""

    def to_json(self) -> str:
        return json.dumps(
            {"credential_json": self.credential_json, "iid_token": self.iid_token},
            separators=(",", ":"),
        )

    @classmethod
    def from_json(cls, raw: str | bytes) -> "ConsumerAdmissionRequest":
        d = json.loads(raw)
        return cls(
            credential_json=d["credential_json"],
            iid_token=d.get("iid_token") or "",
        )


@dataclass
class ConsumerAdmissionResponse:
    """Response returned by the server after consumer admission (§3.2.2).

    ``admitted=True`` on success.  ``reason`` is always empty in the wire
    response (oracle protection); it is only set for local diagnostics.
    """

    admitted: bool
    attributes: dict[str, str] = field(default_factory=dict)
    services: list["ServiceSummary"] = field(default_factory=list)
    registry_ticket: str = ""
    root_pubkey: str = ""        # hex-encoded 32-byte ed25519 public key
    gossip_topic: str = ""       # hex-encoded 32-byte topic — only for root node
    reason: str = ""             # MUST be empty in wire response (§3.2.2)

    def to_json(self) -> str:
        from ..registry.models import ServiceSummary as _SS  # avoid circular at module level

        d = {
            "admitted": self.admitted,
            "attributes": self.attributes,
            "services": [
                s.to_json_dict() if isinstance(s, _SS) else s
                for s in self.services
            ],
            "registry_ticket": self.registry_ticket,
            "root_pubkey": self.root_pubkey,
            "reason": "",   # never leak reason on wire
        }
        if self.gossip_topic:
            d["gossip_topic"] = self.gossip_topic
        return json.dumps(d, separators=(",", ":"))

    @classmethod
    def from_json(cls, raw: str | bytes) -> "ConsumerAdmissionResponse":
        from ..registry.models import ServiceSummary as _SS

        from ..limits import MAX_SERVICES_IN_ADMISSION, MAX_CHANNELS_PER_SERVICE

        d = json.loads(raw)
        raw_services = d.get("services") or []
        if len(raw_services) > MAX_SERVICES_IN_ADMISSION:
            raw_services = raw_services[:MAX_SERVICES_IN_ADMISSION]
        services = [
            _SS.from_json_dict(s) if isinstance(s, dict) else s
            for s in raw_services
        ]
        return cls(
            admitted=bool(d["admitted"]),
            attributes=d.get("attributes") or {},
            services=services,
            registry_ticket=d.get("registry_ticket") or "",
            root_pubkey=d.get("root_pubkey") or "",
            gossip_topic=d.get("gossip_topic") or "",
            reason="",
        )


# ── Credential serialisation helpers ─────────────────────────────────────────


def consumer_cred_to_json(cred: ConsumerEnrollmentCredential) -> str:
    """Serialise a ConsumerEnrollmentCredential to the wire JSON format."""
    return json.dumps(
        {
            "credential_type": cred.credential_type,
            "root_pubkey": cred.root_pubkey.hex(),
            "expires_at": cred.expires_at,
            "attributes": cred.attributes,
            "endpoint_id": cred.endpoint_id,
            "nonce": cred.nonce.hex() if cred.nonce is not None else None,
            "signature": cred.signature.hex(),
        },
        separators=(",", ":"),
    )


def consumer_cred_from_json(s: str | bytes) -> ConsumerEnrollmentCredential:
    """Deserialise a ConsumerEnrollmentCredential from the wire JSON format."""
    from ..limits import validate_hex_field

    d = json.loads(s)

    # Validate hex field lengths before parsing
    validate_hex_field("root_pubkey", d.get("root_pubkey", ""))
    nonce_hex = d.get("nonce") or ""
    if nonce_hex:
        validate_hex_field("nonce", nonce_hex)
    sig_hex = d.get("signature") or ""
    if sig_hex:
        validate_hex_field("signature", sig_hex)
    eid = d.get("endpoint_id") or ""
    if eid:
        validate_hex_field("endpoint_id", eid)

    return ConsumerEnrollmentCredential(
        credential_type=d["credential_type"],
        root_pubkey=bytes.fromhex(d["root_pubkey"]),
        expires_at=int(d["expires_at"]),
        attributes=d.get("attributes") or {},
        endpoint_id=d.get("endpoint_id"),
        nonce=bytes.fromhex(nonce_hex) if nonce_hex else None,
        signature=bytes.fromhex(sig_hex) if sig_hex else b"",
    )


# ── Server-side handler ───────────────────────────────────────────────────────


async def handle_consumer_admission_rpc(
    request_json: str,
    root_pubkey: bytes,
    hook: "MeshEndpointHook",
    peer_node_id: str,
    nonce_store: "NonceStoreProtocol | None" = None,
    services: "list[ServiceSummary] | None" = None,
    registry_ticket: str = "",
    allow_unenrolled: bool = False,
    gossip_topic_id: bytes | None = None,
) -> ConsumerAdmissionResponse:
    """Server-side handler for the ``aster.consumer_admission`` ALPN.

    Args:
        request_json:     JSON-serialised ``ConsumerAdmissionRequest``.
        root_pubkey:      The server's root public key (32 bytes) used to
                          verify the credential's signature.
        hook:             ``MeshEndpointHook`` — ``add_peer`` is called on
                          successful admission.
        peer_node_id:     QUIC peer identity from the connection handshake.
        nonce_store:      Required when accepting OTT credentials.
        services:         List of ``ServiceSummary`` to include in the response.
                          Pass ``None`` or ``[]`` when no services are published.
        registry_ticket:  Read-only iroh-docs share ticket for the registry doc;
                          empty string if this node does not operate a registry.
        allow_unenrolled: When True, auto-admit peers that present an empty
                          credential (dev mode / ``allow_all_consumers=True``).

    Returns:
        ``ConsumerAdmissionResponse`` — always returned, never raises.
        On failure ``admitted=False`` and ``reason=""`` (no oracle leak).
    """
    _denied = ConsumerAdmissionResponse(
        admitted=False,
        root_pubkey=root_pubkey.hex(),
    )

    try:
        req = ConsumerAdmissionRequest.from_json(request_json)
    except Exception as exc:  # noqa: BLE001
        logger.warning("consumer admission: malformed request from %s: %s", peer_node_id, exc)
        return _denied

    from aster.health import get_admission_metrics
    _adm = get_admission_metrics()

    # Include gossip topic only when the connecting peer IS the root node
    # (its endpoint_id == root_pubkey hex). This lets the operator's shell
    # observe the producer mesh without exposing the topic to other consumers.
    _topic_for_peer = ""
    if gossip_topic_id and peer_node_id == root_pubkey.hex():
        _topic_for_peer = gossip_topic_id.hex()
        logger.debug("consumer admission: root node detected — including gossip topic")

    # Dev mode / open gate: empty credential → auto-admit.
    if not req.credential_json and allow_unenrolled:
        if hook is not None:
            hook.add_peer(peer_node_id)
        _adm.record_consumer_admit()
        _role = "root" if _topic_for_peer else "open gate"
        logger.info("consumer admission: auto-admitted %s (%s)", peer_node_id, _role)
        return ConsumerAdmissionResponse(
            admitted=True,
            attributes={},
            services=list(services or []),
            registry_ticket=registry_ticket,
            root_pubkey=root_pubkey.hex(),
            gossip_topic=_topic_for_peer,
        )

    try:
        cred = consumer_cred_from_json(req.credential_json)
    except Exception as exc:  # noqa: BLE001
        logger.warning("consumer admission: malformed credential from %s: %s", peer_node_id, exc)
        return _denied

    # Verify the credential's root_pubkey matches the server's trusted key.
    # Without this check any party could mint their own root key and self-admit.
    if cred.root_pubkey != root_pubkey:
        logger.warning(
            "consumer admission: untrusted root key from %s (got %s, expected %s)",
            peer_node_id,
            cred.root_pubkey.hex()[:12],
            root_pubkey.hex()[:12],
        )
        return _denied

    result = await admit(
        cred,
        peer_node_id,
        nonce_store=nonce_store,
        iid_token=req.iid_token or None,
    )

    if not result.admitted:
        _adm.record_consumer_deny()
        logger.info("consumer admission: denied %s", peer_node_id)
        return _denied

    if hook is not None:
        hook.add_peer(peer_node_id)
    _adm.record_consumer_admit()
    logger.info("consumer admission: admitted %s", peer_node_id)

    return ConsumerAdmissionResponse(
        admitted=True,
        attributes=result.attributes or {},
        services=list(services or []),
        registry_ticket=registry_ticket,
        root_pubkey=root_pubkey.hex(),
        gossip_topic=_topic_for_peer,
    )


# ── Listener helper ───────────────────────────────────────────────────────────


async def serve_consumer_admission(
    endpoint: object,
    root_pubkey: bytes,
    hook: "MeshEndpointHook",
    nonce_store: "NonceStoreProtocol | None" = None,
    services_getter: "Callable[[], list[ServiceSummary]] | None" = None,
    registry_ticket_getter: "Callable[[], str] | None" = None,
) -> None:
    """Accept and process connections on ``aster.consumer_admission`` until cancelled.

    Runs as a background task alongside the main server.  Each connection is
    handled in its own ``asyncio.Task`` so one slow consumer cannot block others.

    Args:
        endpoint:               A ``NetClient`` (from ``create_endpoint_with_config``)
                                bound to the ``aster.consumer_admission`` ALPN.
        root_pubkey:            The server's root public key (32 bytes).
        hook:                   ``MeshEndpointHook`` allowlist manager.
        nonce_store:            Required for OTT credentials.
        services_getter:        Callable returning current ``list[ServiceSummary]``.
                                Called fresh for every admission.
        registry_ticket_getter: Callable returning the current doc share ticket.
                                Called fresh for every admission.
    """
    try:
        while True:
            conn = await endpoint.accept()
            asyncio.create_task(
                handle_consumer_admission_connection(
                    conn,
                    root_pubkey=root_pubkey,
                    hook=hook,
                    nonce_store=nonce_store,
                    services_getter=services_getter,
                    registry_ticket_getter=registry_ticket_getter,
                )
            )
    except asyncio.CancelledError:
        pass
    except Exception as exc:  # noqa: BLE001
        logger.error("serve_consumer_admission: unexpected error: %s", exc)


async def handle_consumer_admission_connection(
    conn: object,
    root_pubkey: bytes,
    hook: "MeshEndpointHook",
    nonce_store: "NonceStoreProtocol | None" = None,
    services_getter: "Callable[[], list[ServiceSummary]] | None" = None,
    registry_ticket_getter: "Callable[[], str] | None" = None,
    allow_unenrolled: bool = False,
    gossip_topic_getter: "Callable[[], bytes | None] | None" = None,
) -> None:
    """Handle one consumer admission connection: read request, write response."""
    peer_node_id = conn.remote_id()
    try:
        send, recv = await conn.accept_bi()
        raw = await recv.read_to_end(64 * 1024)
        if not raw:
            logger.warning("consumer admission: empty request from %s", peer_node_id)
            return

        services = services_getter() if services_getter is not None else []
        ticket = registry_ticket_getter() if registry_ticket_getter is not None else ""
        topic = gossip_topic_getter() if gossip_topic_getter is not None else None

        response = await handle_consumer_admission_rpc(
            raw.decode(),
            root_pubkey=root_pubkey,
            allow_unenrolled=allow_unenrolled,
            hook=hook,
            peer_node_id=peer_node_id,
            nonce_store=nonce_store,
            services=services,
            registry_ticket=ticket,
            gossip_topic_id=topic,
        )

        await send.write_all(response.to_json().encode())
        await send.finish()
        # Don't conn.close() — let QUIC drain the streams naturally.
        # Calling close() sends CONNECTION_CLOSE which kills in-flight
        # data before the consumer can read_to_end().
    except Exception as exc:  # noqa: BLE001
        logger.warning("consumer admission: error handling %s: %s", peer_node_id, exc)
