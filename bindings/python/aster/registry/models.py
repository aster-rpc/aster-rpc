"""
aster.registry.models -- Data classes for the Aster service registry.

Spec references:
- ArtifactRef:    Aster-SPEC.md §11.2.1
- EndpointLease:  Aster-SPEC.md §11.6
- GossipEvent:    Aster-SPEC.md §11.7
- HealthStatus:   Aster-SPEC.md §11.6
"""

from __future__ import annotations

import json
import time
from dataclasses import asdict, dataclass, field
from enum import IntEnum


class HealthStatus(str):
    """Health state of a registered endpoint (§11.6)."""

    STARTING = "starting"    # Not yet ready
    READY = "ready"          # Accepting calls normally
    DEGRADED = "degraded"    # Accepting but with reduced capacity
    DRAINING = "draining"    # Graceful shutdown in progress

    _VALID = {"starting", "ready", "degraded", "draining"}

    @classmethod
    def validate(cls, value: str) -> str:
        if value not in cls._VALID:
            raise ValueError(f"Invalid HealthStatus: {value!r}")
        return value


# Convenience constants matching spec names
STARTING = HealthStatus.STARTING
READY = HealthStatus.READY
DEGRADED = HealthStatus.DEGRADED
DRAINING = HealthStatus.DRAINING


class GossipEventType(IntEnum):
    """All 6 normative gossip event types (§11.7)."""

    CONTRACT_PUBLISHED = 0
    CHANNEL_UPDATED = 1
    ENDPOINT_LEASE_UPSERTED = 2
    ENDPOINT_DOWN = 3
    ACL_CHANGED = 4
    COMPATIBILITY_PUBLISHED = 5


@dataclass
class ServiceSummary:
    """Compact service descriptor returned in ConsumerAdmissionResponse (§3.2.2).

    Provides enough information for a consumer to select a service and fetch
    its contract without joining the registry doc.
    """

    name: str
    version: int
    contract_id: str                    # BLAKE3 hex digest
    channels: dict[str, str]            # channel name → contract_id
    # Dispatch scope: "shared" (default) or "session".
    pattern: str = "shared"
    # Serialization modes the server supports for this service (e.g.
    # ["xlang"], ["json"], or both). Empty/missing means the consumer should
    # assume the project default (XLANG). The TypeScript binding publishes
    # ["json"] only because Fory JS is not yet XLANG-compliant.
    serialization_modes: list[str] = field(default_factory=list)

    def to_json_dict(self) -> dict:
        return {
            "name": self.name,
            "version": self.version,
            "contract_id": self.contract_id,
            "channels": self.channels,
            "pattern": self.pattern,
            "serialization_modes": list(self.serialization_modes),
        }

    @classmethod
    def from_json_dict(cls, d: dict) -> "ServiceSummary":
        return cls(
            name=d["name"],
            version=int(d["version"]),
            contract_id=d["contract_id"],
            channels=d.get("channels") or {},
            pattern=d.get("pattern") or "shared",
            serialization_modes=list(d.get("serialization_modes") or []),
        )


@dataclass
class ArtifactRef:
    """Docs pointer to an immutable Iroh collection (§11.2.1).

    Stored at ``contracts/{contract_id}`` in the registry doc.

    ``collection_format`` is ``"raw"`` for single-blob (Phase 10 default) or
    ``"index"`` for multi-file collections built by ``publication.upload_collection``.
    Old records without this field default to ``"raw"`` on deserialization.
    """

    contract_id: str                    # hex -- BLAKE3 of ServiceContract
    collection_hash: str                # hex -- Iroh blob hash of collection root
    provider_endpoint_id: str | None = None   # NodeId serving blobs ALPN
    relay_url: str | None = None
    ticket: str | None = None           # Optional bearer BlobTicket
    published_by: str = ""              # AuthorId who published
    published_at_epoch_ms: int = 0
    collection_format: str = "raw"      # "raw" or "index" (multi-file)

    def to_json(self) -> str:
        return json.dumps(asdict(self), separators=(",", ":"))

    @staticmethod
    def from_json(s: str | bytes) -> "ArtifactRef":
        data = json.loads(s)
        # collection_format was added after initial Phase 10; default "raw" for old records.
        data.setdefault("collection_format", "raw")
        return ArtifactRef(**data)


@dataclass
class EndpointLease:
    """Renewable advertisement for a live endpoint (§11.6).

    Stored at ``services/{name}/contracts/{cid}/endpoints/{eid}``.
    """

    endpoint_id: str                    # NodeId hex
    contract_id: str                    # Contract being served
    service: str                        # service_name
    version: int                        # int32 version number
    lease_expires_epoch_ms: int         # Absolute expiry timestamp
    lease_seq: int                      # Monotonic per (service, contract_id, endpoint_id)
    alpn: str                           # ALPN string, e.g. "aster/1"
    serialization_modes: list[str]      # Modes this endpoint supports
    feature_flags: list[str]
    relay_url: str | None
    direct_addrs: list[str]             # "ip:port" strings
    load: float | None                  # 0.0--1.0
    language_runtime: str | None        # "python/3.13", "rust/1.80", etc.
    aster_version: str                  # e.g. "0.8.0"
    policy_realm: str | None
    health_status: str                  # One of HealthStatus constants
    tags: list[str]
    updated_at_epoch_ms: int            # Wall-clock at last write

    def to_json(self) -> str:
        return json.dumps(asdict(self), separators=(",", ":"))

    @staticmethod
    def from_json(s: str | bytes) -> "EndpointLease":
        data = json.loads(s)
        return EndpointLease(**data)

    def is_fresh(self, lease_duration_s: int = 45) -> bool:
        """Return True if this lease has not expired."""
        now_ms = int(time.time() * 1000)
        return (now_ms - self.updated_at_epoch_ms) <= lease_duration_s * 1000

    def is_routable(self) -> bool:
        """Return True if health is READY or DEGRADED (not STARTING/DRAINING)."""
        return self.health_status in (READY, DEGRADED)


@dataclass
class GossipEvent:
    """Flat change notification broadcast over gossip (§11.7)."""

    type: GossipEventType               # One of 6 normative event types
    service: str | None = None
    version: int | None = None
    channel: str | None = None
    contract_id: str | None = None
    endpoint_id: str | None = None
    key_prefix: str | None = None       # For ACL_CHANGED
    timestamp_ms: int = 0

    def to_json(self) -> str:
        d = {
            "type": int(self.type),
            "service": self.service,
            "version": self.version,
            "channel": self.channel,
            "contract_id": self.contract_id,
            "endpoint_id": self.endpoint_id,
            "key_prefix": self.key_prefix,
            "timestamp_ms": self.timestamp_ms,
        }
        return json.dumps(d, separators=(",", ":"))

    @staticmethod
    def from_json(s: str | bytes) -> "GossipEvent":
        d = json.loads(s)
        return GossipEvent(
            type=GossipEventType(d["type"]),
            service=d.get("service"),
            version=d.get("version"),
            channel=d.get("channel"),
            contract_id=d.get("contract_id"),
            endpoint_id=d.get("endpoint_id"),
            key_prefix=d.get("key_prefix"),
            timestamp_ms=d.get("timestamp_ms", 0),
        )
