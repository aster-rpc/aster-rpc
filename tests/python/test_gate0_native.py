"""Gate 0 on the Rust implementation (plan P3).

Covers ``GatePolicy`` / ``PeerAdmissionStore`` / ``TrustMode`` (pure), a node
started with ``gate=`` (decisions made in Rust, reject code/reason seen by the
client), and ``node.admission()`` (decisions made in Python, as an async
iterator of requests).
"""

from __future__ import annotations

import asyncio
import time

import pytest

from aster import (
    ALPN_DELEGATED_ADMISSION,
    ALPN_PRODUCER_ADMISSION,
    AsterConfig,
    ConnectRequest,
    EndpointConfig,
    GatePolicy,
    HandshakeRequest,
    Node,
    NodeId,
    PeerAdmissionStore,
    TrustMode,
    create_endpoint_with_config,
)

PEER = "ab" * 32
OTHER = "cd" * 32
APP_ALPN = b"test/gate0/1"


# -- GatePolicy / PeerAdmissionStore (no network) ----------------------------


def test_default_policy_is_protected_with_admission_alpns_open():
    gate = GatePolicy()
    assert gate.trust_mode == TrustMode.PROTECTED
    assert {ALPN_PRODUCER_ADMISSION, ALPN_DELEGATED_ADMISSION} <= set(gate.admission_alpns)
    for alpn in gate.admission_alpns:
        assert gate.should_allow(PEER, alpn) is True
    assert gate.should_allow(PEER, b"aster/1") is False
    assert gate.reject_code == 403
    assert gate.reject_reason == b"peer not admitted"


def test_open_dev_allows_everything_and_mode_is_switchable():
    gate = GatePolicy.open_dev()
    assert gate.trust_mode == TrustMode.OPEN_DEV
    assert gate.should_allow(PEER, b"aster/1") is True
    gate.trust_mode = TrustMode.PROTECTED
    assert gate.should_allow(PEER, b"aster/1") is False
    assert GatePolicy.protected().trust_mode == TrustMode.PROTECTED


def test_admit_and_revoke():
    gate = GatePolicy()
    assert gate.admit(PEER) is None
    assert gate.is_admitted(PEER)
    assert gate.should_allow(PEER, b"aster/1") is True
    assert gate.should_allow(OTHER, b"aster/1") is False
    # Typed ids and upper-case hex name the same peer.
    assert gate.is_admitted(NodeId(PEER.upper()))
    removed = gate.revoke(PEER)
    assert removed is not None and removed.peer == PEER
    assert not gate.is_admitted(PEER)
    assert gate.should_allow(PEER, b"aster/1") is False
    assert gate.revoke(PEER) is None


def test_admission_record_fields_and_attributes():
    gate = GatePolicy()
    before = time.time()
    gate.admit(
        PEER,
        attributes={"aster.role": "reader"},
        ttl=60,
        source="credential",
        owner_id=OTHER,
        proof_hash=b"\x01\x02",
        policy_epoch=7,
    )
    rec = gate.store.get(PEER)
    assert rec.peer == PEER
    assert rec.attributes == {"aster.role": "reader"}
    assert rec.source == "credential"
    assert rec.owner_id == OTHER
    assert rec.proof_hash == b"\x01\x02"
    assert rec.policy_epoch == 7
    assert before - 1 <= rec.admitted_at <= time.time() + 1
    assert rec.expires_at == pytest.approx(rec.admitted_at + 60, abs=1)
    assert gate.attributes(PEER) == {"aster.role": "reader"}
    assert gate.attributes(OTHER) == {}
    # Re-admitting returns the replaced record.
    prev = gate.admit(PEER, source="my-app")
    assert prev.source == "credential"
    assert gate.store.get(PEER).source == "my-app"


def test_expired_admission_is_ignored_and_pruned():
    store = PeerAdmissionStore()
    gate = GatePolicy(store=store)
    gate.admit(PEER, expires_at=time.time() - 1, attributes={"k": "v"})
    assert not gate.is_admitted(PEER)
    assert gate.should_allow(PEER, b"aster/1") is False
    assert gate.attributes(PEER) == {}
    assert store.get(PEER) is None
    assert len(store) == 1
    assert store.prune_expired() == 1
    assert len(store) == 0


def test_store_is_shared_between_gate_and_handles():
    store = PeerAdmissionStore()
    gate = GatePolicy(TrustMode.PROTECTED, store=store)
    store.admit(PEER)
    assert gate.is_admitted(PEER)
    assert PEER in gate.store
    gate.store.revoke(PEER)
    assert PEER not in store
    # Setters keep the store.
    gate.admit(OTHER)
    gate.set_reject(4031, b"nope")
    gate.trust_mode = TrustMode.PROTECTED
    assert gate.is_admitted(OTHER)
    assert gate.reject_code == 4031 and gate.reject_reason == b"nope"


def test_custom_admission_alpns():
    gate = GatePolicy(admission_alpns=[b"custom.admit"])
    assert gate.should_allow(PEER, b"custom.admit") is True
    assert gate.should_allow(PEER, ALPN_PRODUCER_ADMISSION) is False
    gate.admission_alpns = [ALPN_PRODUCER_ADMISSION]
    assert gate.admission_alpns == [ALPN_PRODUCER_ADMISSION]
    with pytest.raises(ValueError):
        gate.admission_alpns = [b""]


def test_bad_arguments_raise_value_error():
    gate = GatePolicy()
    with pytest.raises(ValueError):
        gate.admit("not-a-node-id")
    with pytest.raises(ValueError):
        gate.admit(PEER, ttl=10, expires_at=time.time() + 10)
    with pytest.raises(ValueError):
        gate.admit(PEER, ttl=-1)


# -- Node with an attached gate (decided in Rust) ----------------------------


async def _client():
    return await create_endpoint_with_config(EndpointConfig(alpns=[APP_ALPN]))


async def _accept_one(node):
    alpn, conn = await asyncio.wait_for(node.accept_aster(), timeout=10)
    return alpn, conn


@pytest.mark.asyncio
@pytest.mark.timeout(30)
async def test_gate_denies_then_admits_then_revokes():
    gate = GatePolicy()
    gate.set_reject(4031, b"go away")
    node = await Node.start(AsterConfig(), alpns=[APP_ALPN], gate=gate)
    ep = await _client()
    try:
        assert node.has_hooks()
        assert node.gate is not None
        assert node.gate.reject_code == 4031
        addr = node.node_addr_info()
        peer = ep.endpoint_id()

        # 1. Not admitted: closed with the gate's code + reason.
        conn = await ep.connect_node_addr(addr, APP_ALPN)
        closed = await asyncio.wait_for(conn.closed(), timeout=10)
        assert closed["code"] == 4031
        assert closed["reason"] == b"go away"

        # 2. Admitted: the connection reaches the node.
        gate.admit(peer)
        conn = await ep.connect_node_addr(addr, APP_ALPN)
        alpn, server_conn = await _accept_one(node)
        assert alpn == APP_ALPN
        assert server_conn.remote_id() == peer
        conn.close(0, b"done")

        # 3. Revoked: denied again.
        gate.revoke(peer)
        conn = await ep.connect_node_addr(addr, APP_ALPN)
        closed = await asyncio.wait_for(conn.closed(), timeout=10)
        assert closed["code"] == 4031

        # The gate owns admission; Python can't take it.
        with pytest.raises(RuntimeError):
            node.admission()
    finally:
        await ep.close()
        await node.shutdown()


@pytest.mark.asyncio
@pytest.mark.timeout(30)
async def test_open_dev_gate_lets_unknown_peer_in():
    node = await Node.start(alpns=[APP_ALPN], gate=GatePolicy.open_dev())
    ep = await _client()
    try:
        await ep.connect_node_addr(node.node_addr_info(), APP_ALPN)
        alpn, _ = await _accept_one(node)
        assert alpn == APP_ALPN
    finally:
        await ep.close()
        await node.shutdown()


DOCS_ALPN = b"/iroh-sync/1"


@pytest.mark.asyncio
@pytest.mark.timeout(30)
async def test_gate_decides_this_nodes_own_dials():
    """Outbound handshakes bypass admission, so the gate decides this node's
    own dials at connect time: a peer outside the allowlist is not dialed (as
    docs sync to a swarm-learned peer would), an admitted one is."""
    from aster import net_client

    gate = GatePolicy()
    node = await Node.start(AsterConfig(), alpns=[APP_ALPN], gate=gate)
    peer = await Node.start(AsterConfig())
    node.add_node_addr(peer)
    try:
        with pytest.raises(Exception) as rejected:
            await asyncio.wait_for(
                net_client(node).connect(peer.node_id(), DOCS_ALPN), timeout=10
            )
        assert not isinstance(rejected.value, asyncio.TimeoutError)
        gate.admit(peer.id)
        conn = await asyncio.wait_for(
            net_client(node).connect(peer.node_id(), DOCS_ALPN), timeout=10
        )
        assert conn.remote_id() == peer.id
        conn.close(0, b"done")
    finally:
        await peer.shutdown()
        await node.shutdown()


# -- node.admission(): decided in Python -------------------------------------


@pytest.mark.asyncio
@pytest.mark.timeout(30)
async def test_admission_iterator_reject_code_reason_and_accept():
    node = await Node.start(AsterConfig(enable_hooks=True), alpns=[APP_ALPN])
    ep = await _client()
    admission = node.admission()
    with pytest.raises(RuntimeError):
        node.admission()  # one taker
    seen: list[HandshakeRequest] = []
    peer = ep.endpoint_id()

    async def decide():
        async for req in admission:
            assert isinstance(req, HandshakeRequest)
            assert req.kind == "handshake"
            seen.append(req)
            if len(seen) == 1:
                req.reject(4040, b"custom reason")
                with pytest.raises(RuntimeError):
                    req.accept()  # already answered
            else:
                req.accept()

    task = asyncio.create_task(decide())
    try:
        addr = node.node_addr_info()
        conn = await ep.connect_node_addr(addr, APP_ALPN)
        closed = await asyncio.wait_for(conn.closed(), timeout=10)
        assert closed["code"] == 4040
        assert closed["reason"] == b"custom reason"

        await ep.connect_node_addr(addr, APP_ALPN)
        alpn, _ = await _accept_one(node)
        assert alpn == APP_ALPN
        assert [r.peer for r in seen] == [peer, peer]
        assert all(r.alpn == APP_ALPN for r in seen)
        assert all(isinstance(r.is_alive, bool) for r in seen)
    finally:
        task.cancel()
        await ep.close()
        await node.shutdown()


@pytest.mark.asyncio
@pytest.mark.timeout(30)
async def test_admission_iterator_ends_when_node_closes():
    node = await Node.start(AsterConfig(enable_hooks=True))
    admission = node.admission()
    await node.shutdown()
    assert await asyncio.wait_for(admission.recv(), timeout=10) is None
    with pytest.raises(StopAsyncIteration):
        await asyncio.wait_for(admission.__anext__(), timeout=10)


@pytest.mark.asyncio
@pytest.mark.timeout(30)
async def test_admission_outbound_yields_connect_requests():
    server = await Node.start(AsterConfig(enable_hooks=True), alpns=[APP_ALPN])
    client = await Node.start(AsterConfig(enable_hooks=True), alpns=[APP_ALPN])
    server_adm = server.admission()
    client_adm = client.admission(outbound=True)
    client.add_node_addr(server)
    try:
        from aster import net_client

        dial = asyncio.ensure_future(
            net_client(client).connect(server.node_id(), APP_ALPN)
        )
        req = await asyncio.wait_for(client_adm.recv(), timeout=10)
        assert isinstance(req, ConnectRequest)
        assert req.kind == "connect"
        assert req.peer == server.id
        assert req.alpn == APP_ALPN
        req.accept()
        inbound = await asyncio.wait_for(server_adm.recv(), timeout=10)
        assert isinstance(inbound, HandshakeRequest)
        assert inbound.peer == client.id
        inbound.accept()
        await asyncio.wait_for(dial, timeout=10)
        alpn, _ = await _accept_one(server)
        assert alpn == APP_ALPN
    finally:
        await client.shutdown()
        await server.shutdown()


@pytest.mark.asyncio
async def test_admission_needs_hooks():
    node = await Node.start()
    try:
        assert not node.has_hooks()
        assert node.gate is None
        with pytest.raises(RuntimeError):
            node.admission()
    finally:
        await node.shutdown()
