"""
aster.trust.mesh — Producer mesh data model.

Spec reference: Aster-trust-spec.md §2.1, §2.6.  Plan: ASTER_PLAN.md §14.2.

Data classes for the producer gossip channel.  All Fory-tagged types are used
for on-wire serialization inside ``ProducerMessage.payload``.

Codec note: the outer ``ProducerMessage`` envelope is also Fory-serialized when
sent over gossip.  ``payload`` holds a nested Fory-serialized per-type struct.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from enum import IntEnum
from typing import Any


class ProducerMessageType(IntEnum):
    """Message type discriminator (§2.6 — normative; types 0-127 reserved)."""

    INTRODUCE = 1
    DEPART = 2
    CONTRACT_PUBLISHED = 3
    LEASE_UPDATE = 4


@dataclass
class ProducerMessage:
    """Signed producer gossip envelope (§2.6).

    ``signature`` covers ``type || payload || sender.encode() || u64_be(epoch_ms)``.
    Fields:
        type:       ProducerMessageType discriminator.
        payload:    Fory-encoded per-type payload bytes.
        sender:     Endpoint ID (hex string) of the originating producer.
        epoch_ms:   Wall-clock at send time (milliseconds since Unix epoch).
        signature:  64-byte ed25519 signature; empty before signing.
    """

    type: int                   # ProducerMessageType value
    payload: bytes
    sender: str                 # endpoint_id hex
    epoch_ms: int
    signature: bytes = b""      # 64 bytes after sign_producer_message()


@dataclass
class IntroducePayload:
    """Introduce payload (type=1).

    ``rcan`` is an opaque serialized rcan grant conveying the Producer
    capability.  The format is TBD upstream (§14.12 open question); Phase 12
    ships with opaque bytes.
    """

    rcan: bytes                 # opaque — rcan grant bytes


@dataclass
class DepartPayload:
    """Depart payload (type=2).

    Spec §2.6 says Depart has an empty payload, but the Aster Python
    implementation carries an optional human-readable ``reason`` for operator
    visibility.  Empty string = no reason.
    """

    reason: str = ""            # human-readable; empty = no reason


@dataclass
class ContractPublishedPayload:
    """ContractPublished payload (type=3)."""

    service_name: str
    version: int
    contract_collection_hash: str   # hex — HashSeq root of the published bundle


@dataclass
class LeaseUpdatePayload:
    """LeaseUpdate payload (type=4)."""

    service_name: str
    version: int
    contract_id: str
    health_status: str
    addressing_info: dict[str, str] = field(default_factory=dict)


@dataclass
class MeshState:
    """Runtime state of a producer mesh participant.

    Persisted to ``~/.aster/mesh_state.json`` between restarts.
    """

    accepted_producers: set[str] = field(default_factory=set)
    """Admitted endpoint_ids (including self)."""

    salt: bytes = b""
    """32-byte random salt used to derive the gossip topic."""

    topic_id: bytes = b""
    """32-byte gossip topic ID."""

    peer_offsets: dict[str, int] = field(default_factory=dict)
    """Maps endpoint_id → clock-offset in ms (now_ms - msg.epoch_ms)."""

    drift_isolated: set[str] = field(default_factory=set)
    """Peers whose clock drift exceeded the tolerance threshold."""

    last_heartbeat_epoch_ms: int = 0
    """Wall-clock ms of the most recent LeaseUpdate heartbeat broadcast."""

    mesh_joined_at_epoch_ms: int = 0
    """Wall-clock ms when this node joined the mesh (grace period anchor)."""

    mesh_dead: bool = False
    """True after self-departure; suppresses further gossip sends."""

    def to_json_dict(self) -> dict[str, Any]:
        """Serialize to a JSON-compatible dict for persistence."""
        return {
            "accepted_producers": sorted(self.accepted_producers),
            "salt": self.salt.hex(),
            "topic_id": self.topic_id.hex(),
            "peer_offsets": dict(self.peer_offsets),
            "drift_isolated": sorted(self.drift_isolated),
            "last_heartbeat_epoch_ms": self.last_heartbeat_epoch_ms,
            "mesh_joined_at_epoch_ms": self.mesh_joined_at_epoch_ms,
            "mesh_dead": self.mesh_dead,
        }

    @classmethod
    def from_json_dict(cls, d: dict[str, Any]) -> "MeshState":
        """Deserialize from a JSON-compatible dict."""
        return cls(
            accepted_producers=set(d.get("accepted_producers", [])),
            salt=bytes.fromhex(d.get("salt", "")),
            topic_id=bytes.fromhex(d.get("topic_id", "")),
            peer_offsets=dict(d.get("peer_offsets", {})),
            drift_isolated=set(d.get("drift_isolated", [])),
            last_heartbeat_epoch_ms=int(d.get("last_heartbeat_epoch_ms", 0)),
            mesh_joined_at_epoch_ms=int(d.get("mesh_joined_at_epoch_ms", 0)),
            mesh_dead=bool(d.get("mesh_dead", False)),
        )


@dataclass
class ClockDriftConfig:
    """Tunable knobs for replay-window + drift detection.

    Environment variable overrides:
      ``ASTER_CLOCK_DRIFT_TOLERANCE_MS``  → ``drift_tolerance_ms``
      ``ASTER_REPLAY_WINDOW_MS``          → ``replay_window_ms``
      ``ASTER_GRACE_PERIOD_MS``           → ``grace_period_ms``
    """

    replay_window_ms: int = 30_000      # ±30s (§2.6)
    drift_tolerance_ms: int = 5_000     # ±5s (§2.10)
    lease_heartbeat_ms: int = 900_000   # 15 min (SHOULD)
    grace_period_ms: int = 60_000       # 60s post-join
    min_peers_for_median: int = 3       # need 3+ peers for mesh median

    def __post_init__(self) -> None:
        # Apply environment variable overrides after field initialization.
        if _env := os.environ.get("ASTER_CLOCK_DRIFT_TOLERANCE_MS"):
            self.drift_tolerance_ms = int(_env)
        if _env := os.environ.get("ASTER_REPLAY_WINDOW_MS"):
            self.replay_window_ms = int(_env)
        if _env := os.environ.get("ASTER_GRACE_PERIOD_MS"):
            self.grace_period_ms = int(_env)


@dataclass
class AdmissionRequest:
    """Sent by a joining node to the bootstrap peer over aster.producer_admission."""

    credential_json: str    # JSON-serialized EnrollmentCredential
    iid_token: str = ""     # Optional pre-fetched IID token (empty = not provided)


@dataclass
class AdmissionResponse:
    """Response from the bootstrap peer's admission handler."""

    accepted: bool
    salt: bytes = b""
    accepted_producers: list[str] = field(default_factory=list)
    reason: str = ""    # logging only — not exposed to peer in production
