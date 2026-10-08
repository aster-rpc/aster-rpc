"""Explicit durable audit submission through the shared Rust outbox.

Awaiting submit commits locally; delivery is at least once. Cancellation can
leave a committed event without a returned ID, so resubmission can duplicate it.
Close preserves pending records and does not require a reachable receiver.
"""
from __future__ import annotations

from dataclasses import asdict, dataclass, field
import json
from os import PathLike
from typing import Any


class AuditOutboxError(Exception):
    """Native failure, classified by the stable ``kind`` code."""

    def __init__(self, kind: str, message: str):
        super().__init__(message)
        self.kind = kind


@dataclass(frozen=True)
class AuditDestination:
    endpoint: str
    receiver_identity: str | None = None
    authorization: str | None = field(default=None, repr=False)


@dataclass(frozen=True)
class AuditOutboxConfig:
    directory: str | PathLike[str]
    destination: AuditDestination
    max_retained_bytes: int = 256 * 1024 * 1024
    max_event_bytes: int = 64 * 1024


@dataclass(frozen=True)
class AuditEvent:
    timestamp_ms: int
    kind: str
    payload: dict[str, Any]


@dataclass(frozen=True)
class AuditDeliveryError:
    kind: str
    http_status: int | None
    sequence: int
    immediate_flush_failure: bool
    message: str


@dataclass(frozen=True)
class AuditOutboxStatus:
    outbox_id: str
    storage_recovered: bool
    committed_sequence: int
    pending_records: int
    pending_bytes: int
    dead_letter_records: int
    dead_letter_bytes: int
    disposition_receipts: int
    disposition_bytes: int
    retained_bytes: int
    max_retained_bytes: int
    queued_records: int
    queued_bytes: int
    oldest_age_ms: int | None
    accepting: bool
    ready: bool
    delivery_state: str
    last_delivery_error: AuditDeliveryError | None
    storage_worker_alive: bool
    delivery_worker_alive: bool
    maintenance: bool
    maintenance_duration_ms: int | None
    closed: bool
    durable_commits: int
    last_group_size: int


@dataclass(frozen=True)
class AuditDispositionReceipt:
    event_id: str
    action: str
    timestamp_ms: int
    reason: str
    operator_reference: str
    destination_fingerprint: str
    rejection: AuditDeliveryError | None


@dataclass(frozen=True)
class AuditQuarantineExport:
    event_id: str
    envelope_base64: str
    pending: bool
    receipts: tuple[AuditDispositionReceipt, ...]


def _json(value: Any) -> str:
    try:
        encoded = json.dumps(value, allow_nan=False, ensure_ascii=False, separators=(",", ":"))
        encoded.encode("utf-8")  # Reject lone surrogates before native String extraction.
        return encoded
    except (TypeError, ValueError, OverflowError, UnicodeError) as e:
        raise AuditOutboxError("invalid_event", "audit arguments must be JSON serializable") from e


def _delivery(value: dict | None) -> AuditDeliveryError | None:
    return AuditDeliveryError(**value) if value is not None else None


class AuditOutbox:
    """One directory, one native worker pair, explicit async lifecycle."""

    def __init__(self, native: Any):
        self._native = native

    @classmethod
    async def open(cls, config: AuditOutboxConfig) -> AuditOutbox:
        from ._aster import NativeAuditOutbox
        value = asdict(config)
        value["directory"] = str(config.directory)
        return cls(await NativeAuditOutbox.open(_json(value)))

    def clone(self) -> AuditOutbox:
        """Share workers, directory lock and capacity with this handle."""
        return AuditOutbox(self._native.clone_handle())

    async def submit(self, event: AuditEvent) -> str:
        """Return the full stable event ID after durable local acceptance."""
        return await self._native.submit(_json(asdict(event)))

    async def status(self) -> AuditOutboxStatus:
        value = json.loads(await self._native.status())
        value["last_delivery_error"] = _delivery(value["last_delivery_error"])
        return AuditOutboxStatus(**value)

    async def flush(self, timeout: float) -> None:
        """Wait for the committed prefix; timeout is finite seconds >= 0."""
        await self._native.flush(timeout)

    async def close(self) -> None:
        await self._native.close()

    async def reconfigure(self, destination: AuditDestination) -> None:
        await self._native.reconfigure(_json(asdict(destination)))

    async def quarantine(self, event_id: str, reason: str, operator_reference: str) -> None:
        await self._native.quarantine(event_id, reason, operator_reference)

    async def export(self, event_id: str) -> AuditQuarantineExport:
        value = json.loads(await self._native.export(event_id))
        receipts = []
        for receipt in value["receipts"]:
            receipt["rejection"] = _delivery(receipt["rejection"])
            receipts.append(AuditDispositionReceipt(**receipt))
        value["receipts"] = tuple(receipts)
        return AuditQuarantineExport(**value)

    async def replay(self, event_id: str) -> None:
        await self._native.replay(event_id)

    async def __aenter__(self) -> AuditOutbox:
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        await self.close()
