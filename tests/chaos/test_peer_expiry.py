"""
Tests for peer admission expiry and reconnection behavior.

Verifies that:
1. Admitted peers can reconnect without re-admission (attributes persist)
2. Expired credentials are rejected on reconnection
3. Server-side TTL (ASTER_PEER_TTL_S) is enforced
4. The reaper cleans up expired entries (no memory leak)
5. Gate 3 (capability checks) work correctly after reconnection
"""

from __future__ import annotations

import time

import pytest

from aster.peer_store import PeerAdmission, PeerAttributeStore, PEER_TTL_S
from aster.trust.hooks import MeshEndpointHook


# =============================================================================
# Test 1: Admitted peer persists across lookups (simulates reconnection)
# =============================================================================

def test_admitted_peer_attributes_persist():
    """Attributes remain available after admission (reconnection scenario)."""
    store = PeerAttributeStore()
    store.admit(PeerAdmission(
        endpoint_id="peer-aaa",
        attributes={"aster.role": "ops.status"},
        expires_at=time.time() + 3600,
    ))

    # First lookup (initial connection)
    assert store.get_attributes("peer-aaa") == {"aster.role": "ops.status"}

    # Second lookup (simulated reconnection)
    assert store.get_attributes("peer-aaa") == {"aster.role": "ops.status"}


# =============================================================================
# Test 2: Expired credential rejected on lookup
# =============================================================================

def test_expired_credential_rejected():
    """Peer with expired credential must get empty attributes."""
    store = PeerAttributeStore()
    store.admit(PeerAdmission(
        endpoint_id="peer-bbb",
        attributes={"aster.role": "admin"},
        expires_at=time.time() - 1,  # already expired
    ))

    # Lookup returns empty -- expired
    assert store.get_attributes("peer-bbb") == {}
    # Entry removed from store
    assert store.get("peer-bbb") is None


# =============================================================================
# Test 3: Server-side TTL enforced (ASTER_PEER_TTL_S)
# =============================================================================

def test_server_ttl_enforced():
    """Peer admitted long ago must be evicted even if credential hasn't expired."""
    store = PeerAttributeStore()
    store.admit(PeerAdmission(
        endpoint_id="peer-ccc",
        attributes={"aster.role": "admin"},
        expires_at=time.time() + 999999,  # credential valid for ages
        admitted_at=time.time() - PEER_TTL_S - 1,  # but admitted too long ago
    ))

    # TTL exceeded -- evicted
    assert store.get_attributes("peer-ccc") == {}
    assert store.get("peer-ccc") is None


# =============================================================================
# Test 4: Non-expired peer is allowed
# =============================================================================

def test_non_expired_peer_allowed():
    """Peer within TTL and credential validity must be allowed."""
    store = PeerAttributeStore()
    store.admit(PeerAdmission(
        endpoint_id="peer-ddd",
        attributes={"aster.role": "ops"},
        expires_at=time.time() + 3600,
        admitted_at=time.time(),
    ))

    admission = store.get("peer-ddd")
    assert admission is not None
    assert not admission.is_expired()
    assert store.get_attributes("peer-ddd") == {"aster.role": "ops"}


# =============================================================================
# Test 5: Reaper sweeps expired entries
# =============================================================================

def test_reaper_sweep():
    """sweep_expired() must remove expired entries and leave valid ones."""
    store = PeerAttributeStore()

    # Add 5 expired, 5 valid
    for i in range(5):
        store.admit(PeerAdmission(
            endpoint_id=f"expired-{i}",
            attributes={"aster.role": "old"},
            expires_at=time.time() - 1,
        ))
    for i in range(5):
        store.admit(PeerAdmission(
            endpoint_id=f"valid-{i}",
            attributes={"aster.role": "current"},
            expires_at=time.time() + 3600,
        ))

    assert store.peer_count == 10

    removed = store.sweep_expired()
    assert removed == 5
    assert store.peer_count == 5

    # Valid ones still accessible
    for i in range(5):
        assert store.get_attributes(f"valid-{i}") == {"aster.role": "current"}

    # Expired ones gone
    for i in range(5):
        assert store.get_attributes(f"expired-{i}") == {}


# =============================================================================
# Test 6: Gate 0 with peer store checks expiry
# =============================================================================

def test_gate0_checks_expiry_via_store():
    """MeshEndpointHook with peer_store must deny expired peers."""
    store = PeerAttributeStore()
    hook = MeshEndpointHook(peer_store=store)

    # Admit a peer that expires in the past
    store.admit(PeerAdmission(
        endpoint_id="peer-expired",
        attributes={"aster.role": "ops"},
        expires_at=time.time() - 1,
    ))
    hook.add_peer("peer-expired")

    # Gate 0 should deny (store check finds expiry)
    assert not hook.should_allow("peer-expired", b"aster/1")


def test_gate0_allows_valid_peer_via_store():
    """MeshEndpointHook with peer_store must allow valid peers."""
    store = PeerAttributeStore()
    hook = MeshEndpointHook(peer_store=store)

    store.admit(PeerAdmission(
        endpoint_id="peer-valid",
        attributes={"aster.role": "admin"},
        expires_at=time.time() + 3600,
    ))
    hook.add_peer("peer-valid")

    assert hook.should_allow("peer-valid", b"aster/1")


def test_gate0_always_allows_admission_alpn():
    """Admission ALPN must be allowed even for expired/unknown peers."""
    store = PeerAttributeStore()
    hook = MeshEndpointHook(peer_store=store)

    # Unknown peer on admission ALPN -- must be allowed
    assert hook.should_allow("unknown", b"aster.consumer_admission")

    # Expired peer on admission ALPN -- must be allowed (re-admission)
    store.admit(PeerAdmission(
        endpoint_id="peer-readmit",
        expires_at=time.time() - 1,
    ))
    assert hook.should_allow("peer-readmit", b"aster.consumer_admission")


# =============================================================================
# Test 7: Re-admission refreshes expiry
# =============================================================================

def test_readmission_refreshes_expiry():
    """A peer that re-admits gets fresh expiry -- not stuck on old one."""
    store = PeerAttributeStore()

    # First admission, expires soon
    store.admit(PeerAdmission(
        endpoint_id="peer-refresh",
        attributes={"aster.role": "viewer"},
        expires_at=time.time() + 1,
    ))
    assert store.get_attributes("peer-refresh") == {"aster.role": "viewer"}

    # Re-admission with new role and later expiry
    store.admit(PeerAdmission(
        endpoint_id="peer-refresh",
        attributes={"aster.role": "admin"},
        expires_at=time.time() + 7200,
    ))
    assert store.get_attributes("peer-refresh") == {"aster.role": "admin"}


# =============================================================================
# Test 8: No expires_at (0) uses server TTL only
# =============================================================================

def test_no_credential_expiry_uses_ttl():
    """When expires_at=0 (no credential expiry), server TTL is the only bound."""
    store = PeerAttributeStore()

    # expires_at=0, admitted just now -- should be valid
    store.admit(PeerAdmission(
        endpoint_id="peer-no-expiry",
        attributes={"aster.role": "ops"},
        expires_at=0,
        admitted_at=time.time(),
    ))
    assert store.get_attributes("peer-no-expiry") == {"aster.role": "ops"}

    # expires_at=0, admitted long ago -- TTL expired
    store.admit(PeerAdmission(
        endpoint_id="peer-old-no-expiry",
        attributes={"aster.role": "ops"},
        expires_at=0,
        admitted_at=time.time() - PEER_TTL_S - 1,
    ))
    assert store.get_attributes("peer-old-no-expiry") == {}


# =============================================================================
# Test 9: Memory growth capped by reaper
# =============================================================================

def test_memory_growth_capped():
    """Adding many expired peers then sweeping must free memory."""
    store = PeerAttributeStore()

    # Simulate 10K peers connecting over time, all expired
    for i in range(10000):
        store.admit(PeerAdmission(
            endpoint_id=f"peer-{i}",
            attributes={"aster.role": "ephemeral"},
            expires_at=time.time() - 1,
        ))

    assert store.peer_count == 10000

    removed = store.sweep_expired()
    assert removed == 10000
    assert store.peer_count == 0
