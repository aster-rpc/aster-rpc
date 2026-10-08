"""Native parity/lifecycle smoke tests; the Rust suite owns the fault matrix."""
import asyncio
import base64
import hashlib
import importlib.util
import json
from pathlib import Path

import pytest

from aster import AuditDestination, AuditEvent, AuditOutbox, AuditOutboxConfig, AuditOutboxError

_spec = importlib.util.spec_from_file_location(
    "audit_receiver", Path(__file__).parents[1] / "audit_outbox" / "receiver.py"
)
_receiver_module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_receiver_module)


@pytest.fixture
def receiver(tmp_path):
    fixture = _receiver_module.Receiver(tmp_path / "receiver.sqlite").start()
    yield fixture
    fixture.close()


def config(tmp_path, receiver, **kwargs):
    return AuditOutboxConfig(tmp_path / "outbox", AuditDestination(receiver.endpoint), **kwargs)


async def test_submit_reopen_flush_exact_event_and_typed_error(tmp_path, receiver):
    receiver.response_status = 503
    cfg = config(tmp_path, receiver, max_event_bytes=1024)
    outbox = await AuditOutbox.open(cfg)
    try:
        event = AuditEvent((1 << 64) - 1, "operation.intent", {"amount": 123, "nested": [True, None]})
        event_id = await outbox.submit(event)
        with pytest.raises(AuditOutboxError) as caught:
            await outbox.submit(AuditEvent(1, "too_big", {"data": "x" * 2048}))
        assert caught.value.kind == "event_too_large"
        state = await outbox.status()
        assert state.pending_records == 1
        assert state.committed_sequence == 1
        assert state.storage_worker_alive
        assert isinstance(state.storage_recovered, bool)
    finally:
        await outbox.close()
    receiver.response_status = None
    receiver.drop_next_ack = True
    async with await AuditOutbox.open(cfg) as reopened:
        await reopened.flush(10)
        assert (await reopened.status()).pending_records == 0
    raw = receiver.events()[event_id]
    assert hashlib.sha256(raw).hexdigest() == event_id.rsplit(":", 1)[1]
    envelope = json.loads(raw)
    assert envelope["timestamp_ms"] == event.timestamp_ms
    assert envelope["payload"] == event.payload
    assert len(receiver.events()) == 1
    assert sum(event_id in batch for batch in receiver.attempts) >= 2


async def test_clone_cancel_flush_and_close_preserve_records(tmp_path, receiver):
    receiver.response_status = 503
    cfg = config(tmp_path, receiver)
    outbox = await AuditOutbox.open(cfg)
    clone = outbox.clone()
    try:
        event_id = await clone.submit(AuditEvent(1, "intent", {"key": "value"}))
        assert (await outbox.status()).pending_records == 1
        await outbox.reconfigure(AuditDestination(receiver.endpoint, authorization="Bearer rotated"))
        assert "Bearer rotated" not in repr(await outbox.status())
        pending_flush = asyncio.create_task(clone.flush(10))
        await asyncio.sleep(0.01)
        pending_flush.cancel()
        with pytest.raises(asyncio.CancelledError):
            await pending_flush
        with pytest.raises(AuditOutboxError) as caught:
            await outbox.flush(0.01)
        assert caught.value.kind == "timeout"
        assert (await outbox.status()).pending_records == 1
    finally:
        await clone.close()
        await outbox.close()
    with pytest.raises(AuditOutboxError) as caught:
        await outbox.submit(AuditEvent(2, "closed", {}))
    assert caught.value.kind == "closed"
    receiver.response_status = None
    async with await AuditOutbox.open(cfg) as reopened:
        await reopened.flush(10)
    assert event_id in receiver.events()


async def test_quarantine_export_replay_keeps_opaque_envelope(tmp_path, receiver):
    receiver.response_status = 422
    cfg = config(tmp_path, receiver)
    async with await AuditOutbox.open(cfg) as outbox:
        event_id = await outbox.submit(AuditEvent(1, "reject", {"evidence": [1, 2]}))
        with pytest.raises(AuditOutboxError) as caught:
            await outbox.flush(10)
        assert caught.value.kind == "delivery_stalled"
        await outbox.quarantine(event_id, "receiver repair", "ticket-123")
        exported = await outbox.export(event_id)
        raw = base64.b64decode(exported.envelope_base64, validate=True)
        assert hashlib.sha256(raw).hexdigest() == event_id.rsplit(":", 1)[1]
        assert exported.event_id == event_id
        assert exported.receipts[0].operator_reference == "ticket-123"
        assert (await outbox.status()).dead_letter_records == 1
        with pytest.raises(AuditOutboxError) as caught:
            await outbox.flush(1)
        assert caught.value.kind == "disposition"
    receiver.response_status = None
    async with await AuditOutbox.open(cfg) as reopened:
        assert (await reopened.export(event_id)).envelope_base64 == exported.envelope_base64
        await reopened.replay(event_id)
        async def delivered():
            while (await reopened.status()).pending_records:
                await asyncio.sleep(0.05)
        await asyncio.wait_for(delivered(), 10)
        with pytest.raises(AuditOutboxError) as caught:
            await reopened.flush(1)
        assert caught.value.kind == "disposition"
    assert receiver.events()[event_id] == raw


async def test_cancel_open_releases_directory_and_invalid_duration(tmp_path, receiver):
    cfg = config(tmp_path, receiver)
    opening = asyncio.create_task(AuditOutbox.open(cfg))
    await asyncio.sleep(0)
    opening.cancel()
    try:
        late_handle = await opening
    except asyncio.CancelledError:
        pass
    else:
        await late_handle.close()
    # Cancellation may race native open. Cleanup must eventually release the lock.
    for attempt in range(100):
        try:
            outbox = await AuditOutbox.open(cfg)
            break
        except AuditOutboxError as error:
            if error.kind != "storage" or attempt == 99:
                raise
            await asyncio.sleep(0.01)
    async with outbox:
        for duration in (-1.0, float("nan"), float("inf")):
            with pytest.raises(AuditOutboxError) as caught:
                await outbox.flush(duration)
            assert caught.value.kind == "invalid_event"
        with pytest.raises(AuditOutboxError) as caught:
            await outbox.submit(AuditEvent(1, "invalid", {"value": float("nan")}))
        assert caught.value.kind == "invalid_event"
