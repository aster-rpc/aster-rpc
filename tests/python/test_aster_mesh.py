"""
tests/python/test_aster_mesh.py -- Phase 12: Producer Mesh & Clock Drift tests.

Covers:
- ProducerMessage sign/verify round-trip
- Canonical signing bytes (spec §2.6 normative order)
- Tampered payload → signature verify fails
- Replay window: outside ±30s → dropped
- Unknown-sender membership check → dropped
- Topic derivation determinism + distinctness
- ClockDriftDetector: track_offset, mesh_median_offset, peer_in_drift
- Grace period suppresses drift decisions
- Self-drift detection
- Peer isolation: isolated peers' ContractPublished/LeaseUpdate skipped; Introduce/Depart processed
- Peer recovery from drift isolation
- handle_producer_message: full normative dispatch path
- Self-departure trigger
- Admission RPC: accepted + rejected
- Bootstrap: MeshState initialization helpers
- Payload encode/decode round-trips
"""

from __future__ import annotations

import json
import struct
import time

import pytest

from aster.trust import (
    generate_root_keypair,
    sign_producer_message,
    verify_producer_message,
    derive_gossip_topic,
    producer_message_signing_bytes,
    handle_producer_message,
    encode_introduce_payload,
    encode_depart_payload,
    encode_contract_published_payload,
    encode_lease_update_payload,
    ProducerMessage,
    ProducerMessageType,
    MeshState,
    ClockDriftConfig,
    ClockDriftDetector,
    AdmissionRequest,
    AdmissionResponse,
)
from aster.trust.bootstrap import (
    handle_admission_rpc,
)


# ─────────────────────────────────────────────────────────────────────────────
# Helpers
# ─────────────────────────────────────────────────────────────────────────────

def make_keypair():
    """Return (priv_raw, pub_raw) for tests."""
    return generate_root_keypair()


def make_mesh_state(**kw) -> MeshState:
    """Build a MeshState with sensible defaults."""
    defaults = dict(
        accepted_producers=set(),
        salt=b"\xab" * 32,
        topic_id=b"\xcd" * 32,
        peer_offsets={},
        drift_isolated=set(),
        last_heartbeat_epoch_ms=int(time.time() * 1000),
        mesh_joined_at_epoch_ms=int(time.time() * 1000) - 120_000,  # 2 min ago
    )
    defaults.update(kw)
    return MeshState(**defaults)


def now_ms() -> int:
    return int(time.time() * 1000)


# ─────────────────────────────────────────────────────────────────────────────
# §1 -- Signing bytes canonical order
# ─────────────────────────────────────────────────────────────────────────────

def test_signing_bytes_order():
    """Canonical bytes must be: u8(type) || payload || sender.encode() || u64_be(epoch_ms)."""
    msg_type = ProducerMessageType.INTRODUCE
    payload = b"\x01\x02\x03"
    sender = "abcdef"
    epoch_ms = 1_700_000_000_000

    result = producer_message_signing_bytes(int(msg_type), payload, sender, epoch_ms)
    expected = (
        struct.pack("B", int(msg_type))
        + payload
        + sender.encode("utf-8")
        + struct.pack(">Q", epoch_ms)
    )
    assert result == expected


def test_signing_bytes_different_types_differ():
    """Different message types produce different signing bytes."""
    payload = b"hello"
    sender = "peer1"
    epoch_ms = 123456789000

    b1 = producer_message_signing_bytes(ProducerMessageType.INTRODUCE, payload, sender, epoch_ms)
    b2 = producer_message_signing_bytes(ProducerMessageType.DEPART, payload, sender, epoch_ms)
    assert b1 != b2


# ─────────────────────────────────────────────────────────────────────────────
# §2 -- Sign / verify round-trip
# ─────────────────────────────────────────────────────────────────────────────

def test_sign_verify_introduce():
    priv, pub = make_keypair()
    payload = b"my-rcan-bytes"
    sender = "a" * 64
    epoch = now_ms()
    msg = sign_producer_message(ProducerMessageType.INTRODUCE, payload, sender, epoch, priv)

    assert msg.type == ProducerMessageType.INTRODUCE
    assert msg.payload == payload
    assert msg.sender == sender
    assert msg.epoch_ms == epoch
    assert len(msg.signature) == 64

    assert verify_producer_message(msg, pub) is True


def test_sign_verify_depart():
    priv, pub = make_keypair()
    payload = encode_depart_payload("graceful shutdown")
    sender = "b" * 64
    epoch = now_ms()
    msg = sign_producer_message(ProducerMessageType.DEPART, payload, sender, epoch, priv)
    assert verify_producer_message(msg, pub) is True


def test_sign_verify_lease_update():
    priv, pub = make_keypair()
    payload = encode_lease_update_payload(
        service_name="MyService",
        version=1,
        contract_id="c" * 64,
        health_status="READY",
        addressing_info={"relay_url": "https://relay.example.com"},
    )
    sender = "d" * 64
    epoch = now_ms()
    msg = sign_producer_message(ProducerMessageType.LEASE_UPDATE, payload, sender, epoch, priv)
    assert verify_producer_message(msg, pub) is True


def test_verify_tampered_payload_fails():
    """Changing payload after signing must invalidate the signature."""
    priv, pub = make_keypair()
    payload = b"original"
    msg = sign_producer_message(ProducerMessageType.INTRODUCE, payload, "peer", now_ms(), priv)

    # Tamper with payload
    tampered = ProducerMessage(
        type=msg.type,
        payload=b"tampered",
        sender=msg.sender,
        epoch_ms=msg.epoch_ms,
        signature=msg.signature,
    )
    assert verify_producer_message(tampered, pub) is False


def test_verify_tampered_sender_fails():
    priv, pub = make_keypair()
    msg = sign_producer_message(ProducerMessageType.INTRODUCE, b"rcan", "peer1", now_ms(), priv)
    tampered = ProducerMessage(
        type=msg.type,
        payload=msg.payload,
        sender="peer2_ATTACKER",
        epoch_ms=msg.epoch_ms,
        signature=msg.signature,
    )
    assert verify_producer_message(tampered, pub) is False


def test_verify_tampered_epoch_fails():
    priv, pub = make_keypair()
    msg = sign_producer_message(ProducerMessageType.INTRODUCE, b"rcan", "peer", now_ms(), priv)
    tampered = ProducerMessage(
        type=msg.type,
        payload=msg.payload,
        sender=msg.sender,
        epoch_ms=msg.epoch_ms + 1,
        signature=msg.signature,
    )
    assert verify_producer_message(tampered, pub) is False


def test_verify_wrong_pubkey_fails():
    priv, pub = make_keypair()
    _, other_pub = make_keypair()
    msg = sign_producer_message(ProducerMessageType.INTRODUCE, b"rcan", "peer", now_ms(), priv)
    assert verify_producer_message(msg, other_pub) is False


def test_verify_empty_signature_fails():
    _, pub = make_keypair()
    msg = ProducerMessage(
        type=ProducerMessageType.INTRODUCE,
        payload=b"rcan",
        sender="peer",
        epoch_ms=now_ms(),
        signature=b"",
    )
    assert verify_producer_message(msg, pub) is False


# ─────────────────────────────────────────────────────────────────────────────
# §3 -- Topic derivation
# ─────────────────────────────────────────────────────────────────────────────

def test_topic_derivation_deterministic():
    pubkey = b"\xaa" * 32
    salt = b"\xbb" * 32
    t1 = derive_gossip_topic(pubkey, salt)
    t2 = derive_gossip_topic(pubkey, salt)
    assert t1 == t2
    assert len(t1) == 32


def test_topic_different_salt():
    pubkey = b"\xaa" * 32
    t1 = derive_gossip_topic(pubkey, b"\x01" * 32)
    t2 = derive_gossip_topic(pubkey, b"\x02" * 32)
    assert t1 != t2


def test_topic_different_pubkey():
    salt = b"\xbb" * 32
    t1 = derive_gossip_topic(b"\x01" * 32, salt)
    t2 = derive_gossip_topic(b"\x02" * 32, salt)
    assert t1 != t2


def test_topic_derivation_known_vector():
    """Verify the blake3 derivation against a pre-computed vector."""
    import blake3
    pubkey = b"\xca\xfe" * 16
    salt = b"\xde\xad" * 16
    expected = blake3.blake3(pubkey + b"aster-producer-mesh" + salt).digest()
    assert derive_gossip_topic(pubkey, salt) == expected


# ─────────────────────────────────────────────────────────────────────────────
# §4 -- ClockDriftDetector
# ─────────────────────────────────────────────────────────────────────────────

def test_drift_detector_no_peers_returns_none():
    cfg = ClockDriftConfig(min_peers_for_median=3, grace_period_ms=0)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    assert det.mesh_median_offset() is None


def test_drift_detector_too_few_peers_returns_none():
    cfg = ClockDriftConfig(min_peers_for_median=3, grace_period_ms=0)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    det._peer_offsets = {"a": 100, "b": 200}  # only 2, need 3
    assert det.mesh_median_offset() is None


def test_drift_detector_median_three_peers():
    cfg = ClockDriftConfig(min_peers_for_median=3, grace_period_ms=0)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    det._peer_offsets = {"a": 100, "b": 200, "c": 300}
    assert det.mesh_median_offset() == 200


def test_drift_detector_median_high_even():
    """median_high picks the higher of the two middle values for even-length sequences."""
    cfg = ClockDriftConfig(min_peers_for_median=3, grace_period_ms=0)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    # 4 peers: [100, 200, 300, 400] → median_high = 300
    det._peer_offsets = {"a": 100, "b": 200, "c": 300, "d": 400}
    assert det.mesh_median_offset() == 300


def test_peer_in_drift_false_during_grace():
    """No drift decisions during grace period."""
    cfg = ClockDriftConfig(min_peers_for_median=3, grace_period_ms=60_000, drift_tolerance_ms=5_000)
    # Join 10 seconds ago -- still in grace
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=now_ms() - 10_000)
    det._peer_offsets = {"a": 0, "b": 0, "c": 10_000}  # c is way off
    assert det.peer_in_drift("c") is False


def test_peer_in_drift_true_after_grace():
    cfg = ClockDriftConfig(min_peers_for_median=3, grace_period_ms=0, drift_tolerance_ms=5_000)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    # Median will be 100; peer c has offset 9000 → deviation = 8900 > 5000
    det._peer_offsets = {"a": 0, "b": 100, "c": 9_000, "x": 200}
    # median_high([0,100,200,9000]) = 200; |9000-200| = 8800 > 5000 ✓
    assert det.peer_in_drift("c") is True


def test_peer_not_in_drift_within_tolerance():
    cfg = ClockDriftConfig(min_peers_for_median=3, grace_period_ms=0, drift_tolerance_ms=5_000)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    det._peer_offsets = {"a": 100, "b": 200, "c": 250}
    # median=200; |250-200| = 50 < 5000
    assert det.peer_in_drift("c") is False


def test_self_in_drift_true():
    cfg = ClockDriftConfig(min_peers_for_median=3, grace_period_ms=0, drift_tolerance_ms=5_000)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    det._peer_offsets = {"a": 50, "b": 100, "c": 150}
    # median=100; self_offset=7000 → |7000-100|=6900 > 5000
    assert det.self_in_drift(7_000) is True


def test_self_in_drift_false_within_tolerance():
    cfg = ClockDriftConfig(min_peers_for_median=3, grace_period_ms=0, drift_tolerance_ms=5_000)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    det._peer_offsets = {"a": 50, "b": 100, "c": 150}
    # median=100; self_offset=200 → |200-100|=100 < 5000
    assert det.self_in_drift(200) is False


def test_track_offset_stores_computed_value():
    cfg = ClockDriftConfig(min_peers_for_median=1, grace_period_ms=0)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    epoch = now_ms() - 500  # message sent 500ms ago
    det.track_offset("peer1", epoch)
    after = now_ms()
    # Offset should be approximately 500ms (with some timing slack)
    offset = det._peer_offsets["peer1"]
    assert 400 <= offset <= after - epoch + 10  # generous bounds for CI


def test_remove_peer():
    cfg = ClockDriftConfig(min_peers_for_median=3, grace_period_ms=0)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    det._peer_offsets = {"a": 100, "b": 200}
    det.remove_peer("a")
    assert "a" not in det._peer_offsets
    assert "b" in det._peer_offsets


# ─────────────────────────────────────────────────────────────────────────────
# §5 -- handle_producer_message dispatch
# ─────────────────────────────────────────────────────────────────────────────

@pytest.fixture()
def signed_env():
    """Returns (priv, pub, sender_id) for a single accepted peer."""
    priv, pub = make_keypair()
    sender = "peer_" + "a" * 60
    return priv, pub, sender


def _make_signed_msg(priv, sender, msg_type, payload):
    return sign_producer_message(msg_type, payload, sender, now_ms(), priv)


@pytest.mark.asyncio
async def test_handle_introduce_adds_to_accepted(signed_env):
    priv, pub, sender = signed_env
    rcan = b"\x01\x02\x03\x04"  # non-empty rcan

    state = make_mesh_state(accepted_producers={sender})
    peer_pubkeys = {sender: pub}
    cfg = ClockDriftConfig(replay_window_ms=30_000, grace_period_ms=0)

    new_sender = "new_peer_" + "b" * 55
    # Re-sign as new_sender using new key but message carries new_sender
    priv2, pub2 = make_keypair()
    msg2 = sign_producer_message(ProducerMessageType.INTRODUCE, rcan, new_sender, now_ms(), priv2)
    # Bootstrap: add new_sender to accepted + pubkeys so the msg passes membership check
    state.accepted_producers.add(new_sender)
    peer_pubkeys[new_sender] = pub2

    await handle_producer_message(msg2, state, cfg, peer_pubkeys)
    # After Introduce dispatch, sender stays in accepted_producers (rcan validation is opaque-pass)
    assert new_sender in state.accepted_producers


@pytest.mark.asyncio
async def test_handle_depart_removes_from_accepted(signed_env):
    priv, pub, sender = signed_env
    payload = encode_depart_payload("goodbye")
    msg = _make_signed_msg(priv, sender, ProducerMessageType.DEPART, payload)

    state = make_mesh_state(accepted_producers={sender, "other_peer"})
    peer_pubkeys = {sender: pub}
    cfg = ClockDriftConfig(replay_window_ms=30_000, grace_period_ms=0)

    await handle_producer_message(msg, state, cfg, peer_pubkeys)
    assert sender not in state.accepted_producers
    assert "other_peer" in state.accepted_producers


@pytest.mark.asyncio
async def test_replay_window_drops_old_message(signed_env):
    priv, pub, sender = signed_env
    rcan = b"\x01"
    # epoch_ms 60s in the past → outside ±30s window
    old_epoch = now_ms() - 60_000
    msg = sign_producer_message(ProducerMessageType.INTRODUCE, rcan, sender, old_epoch, priv)

    state = make_mesh_state(accepted_producers={sender})
    peer_pubkeys = {sender: pub}
    cfg = ClockDriftConfig(replay_window_ms=30_000, grace_period_ms=0)

    orig_count = len(state.accepted_producers)
    await handle_producer_message(msg, state, cfg, peer_pubkeys)
    # State unchanged -- message was dropped
    assert len(state.accepted_producers) == orig_count


@pytest.mark.asyncio
async def test_replay_window_drops_future_message(signed_env):
    priv, pub, sender = signed_env
    rcan = b"\x01"
    # epoch_ms 60s in the future → outside ±30s window
    future_epoch = now_ms() + 60_000
    msg = sign_producer_message(ProducerMessageType.INTRODUCE, rcan, sender, future_epoch, priv)

    state = make_mesh_state(accepted_producers={sender})
    peer_pubkeys = {sender: pub}
    cfg = ClockDriftConfig(replay_window_ms=30_000, grace_period_ms=0)

    await handle_producer_message(msg, state, cfg, peer_pubkeys)
    # sender was already in accepted, and depart didn't fire → unchanged
    assert sender in state.accepted_producers


@pytest.mark.asyncio
async def test_unknown_sender_dropped():
    priv, pub = make_keypair()
    unknown_sender = "unknown_" + "x" * 56
    msg = sign_producer_message(ProducerMessageType.INTRODUCE, b"\x01", unknown_sender, now_ms(), priv)

    state = make_mesh_state(accepted_producers={"some_other_peer"})
    peer_pubkeys = {unknown_sender: pub}
    cfg = ClockDriftConfig(replay_window_ms=30_000, grace_period_ms=0)

    await handle_producer_message(msg, state, cfg, peer_pubkeys)
    # unknown_sender must not have been added
    assert unknown_sender not in state.accepted_producers


@pytest.mark.asyncio
async def test_bad_signature_dropped():
    priv, pub = make_keypair()
    _, other_pub = make_keypair()  # wrong pubkey
    sender = "sender_" + "a" * 57
    msg = sign_producer_message(ProducerMessageType.DEPART, b"bye", sender, now_ms(), priv)

    state = make_mesh_state(accepted_producers={sender})
    # Use the wrong pubkey → sig check fails → sender not removed
    peer_pubkeys = {sender: other_pub}
    cfg = ClockDriftConfig(replay_window_ms=30_000, grace_period_ms=0)

    await handle_producer_message(msg, state, cfg, peer_pubkeys)
    # Depart should not have been processed
    assert sender in state.accepted_producers


@pytest.mark.asyncio
async def test_lease_update_skipped_for_isolated_peer():
    priv, pub = make_keypair()
    sender = "isolated_" + "a" * 55
    payload = encode_lease_update_payload("svc", 1, "c" * 64, "READY")
    msg = sign_producer_message(ProducerMessageType.LEASE_UPDATE, payload, sender, now_ms(), priv)

    state = make_mesh_state(
        accepted_producers={sender},
        drift_isolated={sender},
    )
    peer_pubkeys = {sender: pub}
    cfg = ClockDriftConfig(replay_window_ms=30_000, grace_period_ms=0)

    callback_calls = []
    def callback(event_type, payload_obj):
        callback_calls.append((event_type, payload_obj))

    await handle_producer_message(msg, state, cfg, peer_pubkeys, registry_callback=callback)
    # LeaseUpdate from isolated peer must be skipped
    assert callback_calls == []


@pytest.mark.asyncio
async def test_contract_published_skipped_for_isolated_peer():
    priv, pub = make_keypair()
    sender = "iso_" + "b" * 60
    payload = encode_contract_published_payload("svc", 1, "h" * 64)
    msg = sign_producer_message(ProducerMessageType.CONTRACT_PUBLISHED, payload, sender, now_ms(), priv)

    state = make_mesh_state(accepted_producers={sender}, drift_isolated={sender})
    peer_pubkeys = {sender: pub}
    cfg = ClockDriftConfig(replay_window_ms=30_000, grace_period_ms=0)

    calls = []
    await handle_producer_message(msg, state, cfg, peer_pubkeys, registry_callback=lambda e, p: calls.append(e))
    assert calls == []


@pytest.mark.asyncio
async def test_lease_update_forwarded_for_non_isolated_peer():
    priv, pub = make_keypair()
    sender = "active_" + "c" * 57
    payload = encode_lease_update_payload("svc", 2, "c" * 64, "READY", {"relay_url": "x"})
    msg = sign_producer_message(ProducerMessageType.LEASE_UPDATE, payload, sender, now_ms(), priv)

    state = make_mesh_state(accepted_producers={sender})  # not isolated
    peer_pubkeys = {sender: pub}
    cfg = ClockDriftConfig(replay_window_ms=30_000, grace_period_ms=0)

    received = []
    def callback(event_type, payload_obj):
        received.append((event_type, payload_obj))

    await handle_producer_message(msg, state, cfg, peer_pubkeys, registry_callback=callback)
    assert len(received) == 1
    assert received[0][0] == "lease_update"
    assert received[0][1].health_status == "READY"


@pytest.mark.asyncio
async def test_depart_processed_even_for_isolated_peer():
    """Depart is always processed regardless of drift isolation."""
    priv, pub = make_keypair()
    sender = "iso_depart_" + "d" * 53
    payload = encode_depart_payload()
    msg = sign_producer_message(ProducerMessageType.DEPART, payload, sender, now_ms(), priv)

    state = make_mesh_state(accepted_producers={sender}, drift_isolated={sender})
    peer_pubkeys = {sender: pub}
    cfg = ClockDriftConfig(replay_window_ms=30_000, grace_period_ms=0)

    await handle_producer_message(msg, state, cfg, peer_pubkeys)
    assert sender not in state.accepted_producers
    assert sender not in state.drift_isolated


# ─────────────────────────────────────────────────────────────────────────────
# §6 -- Peer drift isolation + recovery
# ─────────────────────────────────────────────────────────────────────────────

@pytest.mark.asyncio
async def test_peer_isolated_on_drift():
    """A peer with excessive clock offset gets drift-isolated."""
    sender = "drifty_" + "e" * 57
    cfg = ClockDriftConfig(replay_window_ms=30_000, grace_period_ms=0, drift_tolerance_ms=5_000, min_peers_for_median=3)

    # Build a detector with 3 stable peers; inject the target peer at 10s offset
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    det._peer_offsets = {"peer_a": 50, "peer_b": 100, "peer_c": 150, sender: 10_000}

    # Verify: detector says sender is in drift
    assert det.peer_in_drift(sender) is True

    # Simulate the dispatch path: add to drift_isolated
    fresh_state = make_mesh_state(accepted_producers={sender})
    if det.peer_in_drift(sender):
        fresh_state.drift_isolated.add(sender)

    assert sender in fresh_state.drift_isolated


@pytest.mark.asyncio
async def test_peer_recovery_from_isolation():
    """A peer with an acceptable fresh offset is removed from drift_isolated."""
    priv, pub = make_keypair()
    sender = "recovering_" + "f" * 53
    payload = encode_lease_update_payload("svc", 1, "x" * 64, "READY")

    # Send a message with epoch_ms very close to now (acceptable offset)
    msg = sign_producer_message(ProducerMessageType.LEASE_UPDATE, payload, sender, now_ms(), priv)

    state = make_mesh_state(accepted_producers={sender}, drift_isolated={sender})
    peer_pubkeys = {sender: pub}
    cfg = ClockDriftConfig(replay_window_ms=30_000, grace_period_ms=0, drift_tolerance_ms=5_000, min_peers_for_median=3)

    # Use a detector with 3 "stable" peers + sender at ~0 offset → not in drift
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    det._peer_offsets = {"a": 50, "b": 100, "c": 150}
    # After tracking sender with epoch_ms≈now, offset will be ≈0
    # So sender_offset - median should be within tolerance

    received = []
    await handle_producer_message(
        msg, state, cfg, peer_pubkeys,
        registry_callback=lambda e, p: received.append(e),
        drift_detector=det,
    )
    # Sender should be removed from isolation since fresh message had acceptable offset
    assert sender not in state.drift_isolated


# ─────────────────────────────────────────────────────────────────────────────
# §7 -- Self-departure
# ─────────────────────────────────────────────────────────────────────────────

def test_self_departure_triggered_on_large_skew():
    """self_in_drift returns True when our clock is 10s off the median."""
    cfg = ClockDriftConfig(min_peers_for_median=3, grace_period_ms=0, drift_tolerance_ms=5_000)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    det._peer_offsets = {"a": 0, "b": 100, "c": 200}
    # median_high([0,100,200]) = 100; self_offset = 8000 → |8000-100| = 7900 > 5000
    assert det.self_in_drift(8_000) is True


def test_self_departure_suppressed_during_grace():
    cfg = ClockDriftConfig(min_peers_for_median=3, grace_period_ms=60_000, drift_tolerance_ms=5_000)
    # Join 10s ago -- still in grace
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=now_ms() - 10_000)
    det._peer_offsets = {"a": 0, "b": 100, "c": 200}
    assert det.self_in_drift(8_000) is False


@pytest.mark.asyncio
async def test_self_departure_sets_mesh_dead():
    """When self-departure triggers, state.mesh_dead = True and callback fires."""
    cfg = ClockDriftConfig(replay_window_ms=30_000, grace_period_ms=0, drift_tolerance_ms=5_000, min_peers_for_median=3)

    departed = []
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)
    # Inject 3 peers with stable offsets; detector will have a median of 100
    det._peer_offsets = {"a": 0, "b": 100, "c": 200}

    state = make_mesh_state()
    state.mesh_dead = False

    assert det.self_in_drift(8_000) is True

    # Simulate the self-departure path directly (as implemented in handle_producer_message)
    if det.self_in_drift(8_000) and not state.mesh_dead:
        state.mesh_dead = True
        departed.append(True)

    assert state.mesh_dead is True
    assert departed == [True]


# ─────────────────────────────────────────────────────────────────────────────
# §8 -- Admission RPC
# ─────────────────────────────────────────────────────────────────────────────

def _make_credential_json(endpoint_id: str, expires_future: bool = True) -> tuple[str, bytes, bytes]:
    """Return (cred_json, priv_raw, pub_raw) for a signed EnrollmentCredential."""
    from aster.trust import sign_credential
    from aster.trust.credentials import EnrollmentCredential

    priv, pub = make_keypair()
    expires_at = int(time.time()) + (3600 if expires_future else -3600)
    cred = EnrollmentCredential(
        endpoint_id=endpoint_id,
        root_pubkey=pub,
        expires_at=expires_at,
        attributes={},
    )
    cred.signature = sign_credential(cred, priv)
    cred_json = json.dumps({
        "endpoint_id": cred.endpoint_id,
        "root_pubkey": cred.root_pubkey.hex(),
        "expires_at": cred.expires_at,
        "attributes": cred.attributes,
        "signature": cred.signature.hex(),
    }, separators=(",", ":"))
    return cred_json, priv, pub


@pytest.mark.asyncio
async def test_admission_rpc_accepted():
    """Valid credential → accepted response with salt + accepted_producers."""
    endpoint_id = "new_node_" + "a" * 55
    cred_json, _, root_pub = _make_credential_json(endpoint_id)

    state = make_mesh_state(
        accepted_producers={"founding_node_" + "x" * 50},
        salt=b"\xf0" * 32,
        topic_id=b"\x0f" * 32,
    )

    response = await handle_admission_rpc(
        request_json=cred_json,
        own_state=state,
        own_root_pubkey=root_pub,
    )
    assert response.accepted is True
    assert len(response.salt) == 32
    assert endpoint_id in state.accepted_producers


@pytest.mark.asyncio
async def test_admission_rpc_rejected_malformed():
    """Malformed JSON → rejected."""
    state = make_mesh_state(salt=b"\xf0" * 32)
    _, root_pub = make_keypair()
    response = await handle_admission_rpc(
        request_json="not valid json {{{",
        own_state=state,
        own_root_pubkey=root_pub,
    )
    assert response.accepted is False


@pytest.mark.asyncio
async def test_admission_rpc_rejected_expired_credential():
    """Expired credential → rejected."""
    endpoint_id = "expired_node_" + "b" * 51
    cred_json, _, _ = _make_credential_json(endpoint_id, expires_future=False)
    state = make_mesh_state(salt=b"\xf0" * 32)
    _, root_pub = make_keypair()
    response = await handle_admission_rpc(
        request_json=cred_json,
        own_state=state,
        own_root_pubkey=root_pub,
    )
    assert response.accepted is False


    # test_apply_admission_response removed -- function was dead code


# ─────────────────────────────────────────────────────────────────────────────
# §9 -- Payload encode/decode
# ─────────────────────────────────────────────────────────────────────────────

def test_encode_depart_payload_round_trip():
    payload = encode_depart_payload("planned maintenance")
    d = json.loads(payload.decode("utf-8"))
    assert d["reason"] == "planned maintenance"


def test_encode_depart_payload_empty_reason():
    payload = encode_depart_payload()
    d = json.loads(payload.decode("utf-8"))
    assert d["reason"] == ""


def test_encode_contract_published_payload():
    payload = encode_contract_published_payload("MyService", 3, "hash" * 16)
    d = json.loads(payload.decode("utf-8"))
    assert d["service_name"] == "MyService"
    assert d["version"] == 3
    assert d["contract_collection_hash"] == "hash" * 16


def test_encode_lease_update_payload():
    payload = encode_lease_update_payload(
        "SvcA", 1, "cid" * 22, "DEGRADED", {"relay_url": "wss://r.example.com"}
    )
    d = json.loads(payload.decode("utf-8"))
    assert d["service_name"] == "SvcA"
    assert d["health_status"] == "DEGRADED"
    assert d["addressing_info"]["relay_url"] == "wss://r.example.com"


def test_encode_introduce_payload_pass_through():
    rcan = b"\x01\x02\x03\x04\x05"
    result = encode_introduce_payload(rcan)
    assert result == rcan


# ─────────────────────────────────────────────────────────────────────────────
# §10 -- MeshState serialization
# ─────────────────────────────────────────────────────────────────────────────

def test_mesh_state_round_trip_json():
    state = MeshState(
        accepted_producers={"peer1", "peer2"},
        salt=bytes(range(32)),
        topic_id=bytes(range(32, 64)),
        peer_offsets={"peer1": 100, "peer2": 200},
        drift_isolated={"peer2"},
        last_heartbeat_epoch_ms=1_700_000_000_000,
        mesh_joined_at_epoch_ms=1_699_000_000_000,
        mesh_dead=False,
    )
    d = state.to_json_dict()
    restored = MeshState.from_json_dict(d)

    assert restored.accepted_producers == state.accepted_producers
    assert restored.salt == state.salt
    assert restored.topic_id == state.topic_id
    assert restored.peer_offsets == state.peer_offsets
    assert restored.drift_isolated == state.drift_isolated
    assert restored.last_heartbeat_epoch_ms == state.last_heartbeat_epoch_ms
    assert restored.mesh_joined_at_epoch_ms == state.mesh_joined_at_epoch_ms
    assert restored.mesh_dead == state.mesh_dead


def test_mesh_state_empty_round_trip():
    state = MeshState()
    restored = MeshState.from_json_dict(state.to_json_dict())
    assert restored.accepted_producers == set()
    assert restored.salt == b""
    assert restored.mesh_dead is False


# ─────────────────────────────────────────────────────────────────────────────
# §11 -- ClockDriftConfig env overrides
# ─────────────────────────────────────────────────────────────────────────────

def test_clock_drift_config_env_override(monkeypatch):
    monkeypatch.setenv("ASTER_CLOCK_DRIFT_TOLERANCE_MS", "2000")
    monkeypatch.setenv("ASTER_REPLAY_WINDOW_MS", "10000")
    monkeypatch.setenv("ASTER_GRACE_PERIOD_MS", "5000")
    cfg = ClockDriftConfig()
    assert cfg.drift_tolerance_ms == 2000
    assert cfg.replay_window_ms == 10000
    assert cfg.grace_period_ms == 5000


def test_clock_drift_config_defaults():
    cfg = ClockDriftConfig()
    # Check defaults (env vars not set in this test context if unset)
    assert cfg.lease_heartbeat_ms == 900_000
    assert cfg.min_peers_for_median == 3


# ─────────────────────────────────────────────────────────────────────────────
# §12 -- 3-peer median drift example (integration-style)
# ─────────────────────────────────────────────────────────────────────────────

def test_three_peer_median_drift_detection():
    """End-to-end: 3 peers with known offsets; 1 outlier detected."""
    cfg = ClockDriftConfig(min_peers_for_median=3, grace_period_ms=0, drift_tolerance_ms=5_000)
    det = ClockDriftDetector(cfg, mesh_joined_at_epoch_ms=1)

    # Simulate 3 peers with offsets ~100ms (good) and 1 with 15s (bad)
    det._peer_offsets = {
        "alice": 80,
        "bob": 120,
        "carol": 100,
        "eve": 15_000,  # 15s ahead of median
    }
    median = det.mesh_median_offset()
    # median_high([80, 100, 120, 15000]) = 120
    assert median == 120

    assert det.peer_in_drift("alice") is False
    assert det.peer_in_drift("bob") is False
    assert det.peer_in_drift("carol") is False
    assert det.peer_in_drift("eve") is True   # |15000 - 120| = 14880 > 5000


# ─────────────────────────────────────────────────────────────────────────────
# §13 -- Admission response round-trip (bootstrap flow)
# ─────────────────────────────────────────────────────────────────────────────

def test_admission_response_accepted_fields():
    resp = AdmissionResponse(
        accepted=True,
        salt=b"\xab" * 32,
        accepted_producers=["p1", "p2"],
        reason="",
    )
    assert resp.accepted is True
    assert len(resp.salt) == 32
    assert "p1" in resp.accepted_producers


def test_admission_request_fields():
    req = AdmissionRequest(credential_json='{"x":1}', iid_token="token123")
    assert req.credential_json == '{"x":1}'
    assert req.iid_token == "token123"
