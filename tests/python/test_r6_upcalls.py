"""Plan R6 -- Python-implemented Rust extension points over upcalls.

Rust emits a typed event, the ``aster.upcalls`` pump decodes it (in Rust)
into the usual dataclasses, runs the Python coroutine as a task and answers
by id. This file tests the Python mapping only; the upcall behaviour
(timeouts, overload, close-fails-pending, cache semantics, failure modes,
HTTP limits and relay failure codes) is tested once in Rust:
``aster/tests/it/upcalls.rs``, ``ffi/tests/upcalls.rs``,
``ffi/tests/expose.rs`` and ``aster/tests/it/upcall_vectors.rs``.
Covered here, each end to end through the Rust adapter:

- the UCv1 payload codec against ``conformance/vectors/upcalls.json`` (via
  the same PyO3 decode/encode the pump uses);
- the runtime: policies, error mapping, cancellation of the Python task;
- a Python ``LeaseSerializer`` (acquire / renew / release in Rust, served
  as a designated grantor over RPC), a Python lease authorizer, a Python
  ``FencedResource``;
- the topology ``admitted`` filter (sync, async and batch predicates);
- expose authorize, Python HTTP handlers (inline and streamed bodies,
  failures as 5xx, cancellation) and edge route policies (R9);
- Gate 0 decisions.

The placement manager with Python providers needs a live coordination
group: ``test_coordination_live.py``.
"""

from __future__ import annotations

import asyncio
import inspect
import json
import time
from pathlib import Path

import pytest
import pytest_asyncio

from aster import (
    AdmissionHandler,
    AdmittedFilter,
    AsterConfig,
    ClusterView,
    EdgeRouter,
    EndpointConfig,
    ExposeAuthorizer,
    HttpBodyError,
    HttpBodyStream,
    HttpHandler,
    HttpRelayBody,
    HttpRequest,
    HttpResponse,
    HttpUpload,
    RoutePolicy,
    RouteSpec,
    Fence,
    FenceOutcomeUnknownError,
    FenceRejectedError,
    FencedResource,
    LeaseAuthorizer,
    LeaseOp,
    LeaseOptions,
    LeaseSerializer,
    LeaseSnapshot,
    MemorySerializer,
    Node,
    NodeId,
    TopologySnapshot,
    TopologyView,
    TransitionOutcome,
    UpcallError,
    UpcallPolicy,
    UpcallRuntime,
    create_endpoint_with_config,
    lease_transition,
    LeaseRowState,
    check_fence,
    net_client,
    open_relay,
    relay_http,
    relay_http_stream,
    relay_http_streaming,
    request_route,
)

from aster import upcalls as up
from aster.coordination import (
    LogId,
    MemberSeed,
    PlacementCandidate,
    PlacementPolicy,
    ProvisionRequest,
)

from wait_budget import WAIT_BUDGET

REPO =Path(__file__).resolve().parents[2]
VECTORS = json.loads((REPO / "conformance/vectors/upcalls.json").read_text())

FAST = LeaseOptions(ttl=0.6, renew_interval=0.15, holder_margin=0.1)
A = "aa" * 32
B = "bb" * 32


def _offline(**kw) -> AsterConfig:
    return AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0", **kw)


async def _ready(node: Node) -> None:
    for _ in range(200):
        if node.node_addr_info().direct_addresses:
            return
        await asyncio.sleep(0.01)


async def _until(what: str, f, timeout: float = 20.0):
    deadline = time.monotonic() + timeout
    while True:
        v = f()
        if inspect.isawaitable(v):
            v = await v
        if v is not None and v is not False:
            return v
        if time.monotonic() > deadline:
            raise AssertionError(f"timed out waiting for {what}")
        await asyncio.sleep(0.05)


@pytest_asyncio.fixture
async def rt():
    """A dedicated runtime with short timeouts (objects pin it via
    ``upcall_runtime``)."""
    runtime = UpcallRuntime(default_policy=UpcallPolicy(timeout=0.3))
    runtime.start()
    yield runtime
    await runtime.aclose()


# -- UCv1 codec: conformance vectors through the PyO3 decode -----------------------


def _hex(v) -> str:
    return str(v)


def _render(ty: str, v):
    """The vectors' JSON rendering of a decoded Python value."""
    def op(o: LeaseOp):
        if o.kind == "acquire":
            return {"acquire": {"candidate": o.actor, "ttl_ms": round(o.ttl * 1000)}}
        if o.kind == "revoke":
            return {"revoke": {"authority": o.actor}}
        return {o.kind: {"holder": o.actor, "epoch": o.epoch}}

    def fence(f: Fence):
        return {"resource": f.resource, "epoch": f.epoch, "holder": f.holder}

    def snap(s: LeaseSnapshot):
        return {
            "resource": s.resource,
            "epoch": s.epoch,
            "holder": s.holder,
            "held": s.held,
            "remaining_ms": None if s.remaining is None else round(s.remaining * 1000),
        }

    def seed(s: MemberSeed):
        return {
            "node_id": _hex(s.node_id),
            "ticket": s.ticket,
            "signed_join_permit": None if s.signed_join_permit is None else s.signed_join_permit.hex(),
        }

    def log(lg):
        return None if lg is None else {
            "term": lg.term, "leader_node_id": _hex(lg.leader_node_id), "index": lg.index
        }

    if ty == "LeaseTransitionRequest":
        return {"resource": v[0], "op": op(v[1])}
    if ty == "TransitionOutcome":
        if v.kind == "granted":
            return {"granted": fence(v.fence)}
        if v.kind == "denied":
            return {"denied": {"reason": v.deny_reason, "snapshot": snap(v.snapshot)}}
        return v.kind
    if ty == "LeaseSnapshotRequest":
        return {"resource": v[0]}
    if ty == "LeaseSnapshot":
        return snap(v)
    if ty == "LeaseAuthorizeRequest":
        return {"op": op(v[0])}
    if ty == "FencedMutateRequest":
        return {"fence": fence(v[0]), "writer": v[1], "op_hex": v[2].hex()}
    if ty == "FencedMutateResponse":
        if isinstance(v, bytes):
            return {"applied": {"output_hex": v.hex()}}
        return {"rejected": {"reason": v.kind, "carried": v.carried or 0, "current": v.current or 0}}
    if ty == "Decision":
        return v
    if ty == "TopologyAdmittedRequest":
        return {"node_ids": [_hex(n) for n in v[0]]}
    if ty == "TopologyAdmittedResponse":
        return {"admitted": list(v)}
    if ty in ("TopologySnapshotRequest", "PlacementPolicyRequest"):
        assert v == ()
        return {}
    if ty == "TopologySnapshot":
        return {
            "clusters": [
                {
                    "members": [_hex(m) for m in c.members],
                    "bridge": _hex(c.bridge),
                    "witnesses": [_hex(w) for w in c.witnesses],
                }
                for c in v.clusters
            ],
            "far_pairs": [[_hex(a), _hex(b)] for a, b in v.far_pairs],
            "separation_coverage": v.separation_coverage,
        }
    if ty == "ExposeAuthorizeRequest":
        return {"service_id": v[0], "peer_id": _hex(v[1]), "metadata_hex": v[2].hex()}
    if ty == "AdmissionConnectRequest":
        return {"peer": _hex(v[0]), "alpn": v[1].decode()}
    if ty == "AdmissionHandshakeRequest":
        return {"peer": _hex(v[0]), "alpn": v[1].decode(), "is_alive": v[2]}
    if ty == "AdmissionHandshakeResponse":
        if v is True:
            return {"accept": True, "code": 0, "reason": ""}
        return {"accept": False, "code": v[0], "reason": v[1].decode()}
    if ty == "RpcAuthRequest":
        return dict(zip(("peer_id", "service", "method", "session_id", "metadata"), v))
    if ty == "RpcAuthResponse":
        return {"accepted": False} if v is None else {"accepted": True, "principal": v[0], "attributes": v[1]}
    if ty == "PlacementPolicy":
        return {
            "revision": v.revision,
            "digest": v.digest,
            "authority_public_key": v.authority_public_key,
            "authority_domain": v.authority_domain,
            "target_voters": v.target_voters,
            "target_retained_learners": v.target_retained_learners,
            "eligible": [
                {
                    "node_id": _hex(c.node_id),
                    "member_seed": None if c.member_seed is None else seed(c.member_seed),
                }
                for c in v.eligible
            ],
            "pinned_voters": [_hex(n) for n in v.pinned_voters],
            "excluded": [_hex(n) for n in v.excluded],
            "minimum_separated_voter_pairs": v.minimum_separated_voter_pairs,
            "replacement_grace_ms": round(v.replacement_grace * 1000),
            "allow_healthy_rebalance": v.allow_healthy_rebalance,
        }
    if ty == "PlacementProvisionRequest":
        r = v[0]
        assert isinstance(r, ProvisionRequest)
        return {
            "group_id": r.group_id,
            "policy_digest": r.policy_digest,
            "membership_log_id": log(r.membership_log_id),
            "node_id": _hex(r.node_id),
        }
    if ty == "MemberSeed":
        return seed(v)

    def hdrs(hs):
        out = []
        for name, value in hs:
            assert isinstance(value, bytes)
            try:
                out.append({"name": name, "value": value.decode("ascii")})
            except UnicodeDecodeError:
                out.append({"name": name, "value_hex": value.hex()})
        return out

    def body(b):
        if isinstance(b, bytes):
            return {"inline": b.decode()}
        assert b is None or isinstance(b, HttpBodyStream)
        return "stream"

    if ty == "ExposeHttpRequest":
        (r,) = v
        assert isinstance(r, HttpRequest) and not r.handle.attached
        return {
            "service_id": r.service_id,
            "peer_id": None if r.peer is None else _hex(r.peer),
            "stream_id": r.stream_id,
            "version": ["HTTP/0.9", "HTTP/1.0", "HTTP/1.1", "HTTP/2", "HTTP/3"].index(r.version),
            "method": r.method,
            "uri": r.uri,
            "headers": hdrs(r.headers),
            "body": body(r.body),
        }
    if ty == "ExposeHttpResponse":
        assert isinstance(v, HttpResponse)
        return {"status": v.status, "headers": hdrs(v.headers), "body": body(v.body)}
    if ty == "ExposeHttpTrailers":
        return hdrs(v)
    if ty == "ExposeRoutePolicyRequest":
        return {
            "peer_id": _hex(v[0]),
            "metadata_hex": v[1].hex(),
            "specs": [
                {"host": s.host, "port": s.port, "protocol": s.protocol, "service_id": s.service_id}
                for s in v[2]
            ],
        }
    if ty == "ExposeRouteGrant":
        return {"granted": list(v)}
    raise AssertionError(f"no renderer for {ty}")


def _canon(x):
    return json.loads(json.dumps(x, sort_keys=True))


@pytest.mark.parametrize("case", VECTORS["valid"], ids=lambda c: c["name"])
def test_ucv1_valid_vector_decodes_reencodes_and_renders(case):
    raw = bytes.fromhex(case["hex"])
    value = up.ucv1_decode(case["type"], raw)
    assert up.ucv1_encode(case["type"], value) == raw
    assert _canon(_render(case["type"], value)) == _canon(case["value"])


@pytest.mark.parametrize("case", VECTORS["invalid"], ids=lambda c: c["name"])
def test_ucv1_invalid_vector_is_refused(case):
    with pytest.raises(ValueError):
        up.ucv1_decode(case["type"], bytes.fromhex(case["hex"]))


def test_kind_tables_match_the_vectors():
    assert {k["name"]: k["code"] for k in VECTORS["kinds"]} == up.UPCALL_KINDS
    assert {k["name"] for k in VECTORS["kinds"] if k["decision"]} == set(up.DECISION_KINDS)
    assert [r["name"] for r in VECTORS["cancel_reasons"]] == ["timeout", "abandoned", "shutdown"]


def test_encode_refuses_wrong_python_values():
    with pytest.raises(ValueError):
        up.ucv1_encode("Decision", 1)  # not a bool
    with pytest.raises(ValueError):
        up.ucv1_encode("TopologyAdmittedResponse", [True, "yes"])
    with pytest.raises(ValueError):
        up.ucv1_encode("TransitionOutcome", TransitionOutcome("granted"))  # no fence
    with pytest.raises(ValueError):
        up.ucv1_encode("NoSuchMessage", None)


# -- the runtime -------------------------------------------------------------------


async def test_runtime_policies_and_default_runtime_per_loop():
    rt = UpcallRuntime(
        default_policy=UpcallPolicy(timeout=2.0),
        policies={"lease_authorize": UpcallPolicy(timeout=0.5, failure_mode="fail_open")},
    )
    assert rt.policy("lease_authorize") == UpcallPolicy(0.5, "fail_open")
    assert rt.policy("lease_transition") == UpcallPolicy(2.0, "fail_closed")
    assert rt.policy(up.UPCALL_KINDS["lease_authorize"]).failure_mode == "fail_open"
    with pytest.raises(ValueError):
        rt.set_policy("no_such_kind", UpcallPolicy())
    with pytest.raises(ValueError):
        rt.set_policy("lease_authorize", UpcallPolicy(timeout=0))
    with pytest.raises(ValueError):
        rt.set_policy("lease_authorize", UpcallPolicy(failure_mode="maybe"))
    assert not rt.running
    async with rt:
        assert rt.running and not rt.closed
    assert rt.closed
    with pytest.raises(UpcallError) as e:
        rt.start()
    assert e.value.kind == "closed"

    assert up.get_runtime() is up.get_runtime()
    mine = UpcallRuntime()
    up.set_runtime(mine)
    assert up.get_runtime() is mine
    await mine.aclose()
    assert up.get_runtime() is not mine  # a closed default is replaced


def test_binding_needs_a_running_loop():
    with pytest.raises(RuntimeError, match="running asyncio event loop"):
        MemorySerializer().set_authorizer(lambda op: True)


# -- Python LeaseSerializer --------------------------------------------------------


class RowSerializer(LeaseSerializer):
    """A Python-owned serialization point applying the normative state
    machine under its own lock (a dict standing in for a database row)."""

    def __init__(self, runtime=None):
        self.upcall_runtime = runtime
        self.rows = {}
        self.granted_at = {}
        self.ops = []
        self.lock = asyncio.Lock()

    async def transition(self, resource, op):
        self.ops.append(op.kind)
        async with self.lock:
            row = self.rows.get(resource, LeaseRowState())
            now = time.monotonic()
            expired = row.held and now - self.granted_at.get(resource, 0) >= row.ttl
            t = lease_transition(row, expired, op, resource)
            if t.kind == "deny":
                return TransitionOutcome(
                    "denied", deny_reason=t.deny_reason, snapshot=self._snap(resource, now)
                )
            self.rows[resource] = t.next
            if t.reset_grant:
                self.granted_at[resource] = now
            return t.outcome

    def _snap(self, resource, now):
        row = self.rows.get(resource, LeaseRowState())
        remaining = None
        if row.held:
            remaining = max(0.0, row.ttl - (now - self.granted_at.get(resource, now)))
        return LeaseSnapshot(resource, row.epoch, row.holder, row.held, remaining)

    async def snapshot(self, resource):
        return self._snap(resource, time.monotonic())


async def test_python_serializer_raw_transition_round_trips_every_outcome():
    ser = RowSerializer()
    granted = await ser.rust_transition("r", LeaseOp.acquire(A, 5.0))
    assert granted == TransitionOutcome("granted", fence=Fence("r", 1, A))
    assert (await ser.rust_transition("r", LeaseOp.renew(A, 1))).kind == "renewed"
    denied = await ser.rust_transition("r", LeaseOp.renew(B, 1))
    assert denied.kind == "denied" and denied.deny_reason == "not_holder"
    assert denied.snapshot.holder == A and 0 < denied.snapshot.remaining <= 5.0
    assert (await ser.rust_transition("r", LeaseOp.release(A, 1))).kind == "released"


class FlakySerializer(LeaseSerializer):
    def __init__(self, runtime, mode):
        self.upcall_runtime = runtime
        self.mode = mode
        self.cancelled = asyncio.Event()

    async def transition(self, resource, op):
        if self.mode == "raise":
            raise RuntimeError("backend down")
        if self.mode == "garbage":
            return "not an outcome"
        try:
            await asyncio.sleep(10)
        except asyncio.CancelledError:
            self.cancelled.set()
            raise

    async def snapshot(self, resource):
        return LeaseSnapshot(resource, 0)


async def test_serializer_faults_raise_upcall_errors_never_guessed_outcomes(rt):
    with pytest.raises(UpcallError) as e:
        await FlakySerializer(rt, "raise").acquire("r", A, FAST)
    assert e.value.kind == "foreign" and "backend down" in str(e.value)

    with pytest.raises(UpcallError) as e:
        await FlakySerializer(rt, "garbage").acquire("r", A, FAST)
    assert e.value.kind == "foreign" and "unusable value" in str(e.value)

    slow = FlakySerializer(rt, "slow")
    with pytest.raises(UpcallError) as e:
        await slow.acquire("r", A, FAST)
    assert e.value.kind == "timeout"
    # Rust stopped waiting: the handler task is cancelled.
    await asyncio.wait_for(slow.cancelled.wait(), WAIT_BUDGET)
    assert rt.pending == 0


async def test_missing_handler_method_is_unimplemented(rt):
    class NoSnapshot:
        upcall_runtime = rt

        async def transition(self, resource, op):
            raise AssertionError("not called")

    from aster.lease import _foreign_lease_snapshot

    with pytest.raises(UpcallError) as e:
        await _foreign_lease_snapshot(NoSnapshot(), "r")
    assert e.value.kind == "unimplemented"


async def test_unbind_closes_that_objects_channel(rt):
    ser = RowSerializer(rt)
    assert (await ser.rust_snapshot("r")).epoch == 0
    rt.unbind(ser)
    # A new binding opens a fresh channel.
    assert (await ser.rust_snapshot("r")).epoch == 0


# -- lease authorizer --------------------------------------------------------------


class Allowlist(LeaseAuthorizer):
    def __init__(self, allowed, runtime=None):
        self.allowed = set(allowed)
        self.upcall_runtime = runtime
        self.seen = []

    async def authorize(self, op):
        self.seen.append((op.kind, op.actor))
        return op.actor in self.allowed


async def test_lease_authorizer_gates_every_transition():
    store = MemorySerializer()
    auth = Allowlist({A})
    store.set_authorizer(auth)
    ok = await store.acquire("r", A, FAST)
    assert ok.granted
    refused = await store.acquire("r2", B, FAST)
    assert not refused.granted and refused.reason == "unauthorized"
    assert ("acquire", B) in auth.seen
    # Renewals asking the authorizer: Rust memory_serializer_with_foreign_authorizer.
    await ok.lease.release()

    # Plain callables (sync or async) work.
    store.set_authorizer(lambda op: op.actor == B)
    assert (await store.acquire("r3", B, FAST)).granted
    assert (await store.transition("r4", LeaseOp.acquire(A, 1.0))).deny_reason == "unauthorized"

    async def deny_all(op):
        return False

    store.set_authorizer(deny_all)
    assert (await store.revoke("r3", A)).kind == "denied"
    store.set_authorizer(None)
    assert (await store.revoke("r3", A)).kind == "revoked"


# -- grantor over a Python serializer ----------------------------------------------


async def test_python_serializer_serves_a_designated_grantor():
    grantor = await Node.start(_offline(), alpns=[b"aster/1"])
    a = await Node.start(_offline())
    try:
        for n in (grantor, a):
            await _ready(n)
        a.add_node_addr(grantor)
        ser = RowSerializer()
        server = grantor.serve_lease_grantor(ser)
        try:
            remote = await asyncio.wait_for(a.lease_grantor(grantor.id), 10)
            outcome = await remote.acquire("db/primary", FAST)
            assert outcome.granted
            assert outcome.lease.holder == a.id
            # The grant is the Python row, bound to the authenticated caller.
            assert ser.rows["db/primary"].holder == str(a.id)
            # Renewal over RPC: Rust rpc_lease.rs lease_handle_renews_over_rpc.
            await outcome.lease.release()
            assert ser.rows["db/primary"].held is False
        finally:
            server.close()
    finally:
        for n in (grantor, a):
            await n.shutdown()


# -- FencedResource ----------------------------------------------------------------


class Register(FencedResource):
    """A fenced register: the fence check and the write under one lock."""

    def __init__(self, row: LeaseRowState, runtime=None):
        self.row = row
        self.value = b""
        self.upcall_runtime = runtime
        self.lock = asyncio.Lock()

    async def mutate(self, fence, writer, op):
        async with self.lock:
            rejection = check_fence(self.row, fence, writer)
            if rejection is not None:
                raise rejection
            self.value += op
            return b"len=%d" % len(self.value)


async def test_fenced_resource_applies_rejects_and_reports_unknown(rt):
    row = LeaseRowState(epoch=7, holder=A, held=True, ttl=10.0)
    reg = Register(row)
    assert await reg.mutate_via_rust(Fence("tree/42", 7, A), A, b"abc") == b"len=3"
    assert reg.value == b"abc"

    with pytest.raises(FenceRejectedError) as e:
        await reg.mutate_via_rust(Fence("tree/42", 6, A), A, b"x")
    assert (e.value.kind, e.value.carried, e.value.current) == ("stale_epoch", 6, 7)
    with pytest.raises(FenceRejectedError) as e:
        await reg.mutate_via_rust(Fence("tree/42", 7, A), B, b"x")
    assert e.value.kind == "not_holder"
    with pytest.raises(FenceRejectedError) as e:
        await reg.mutate_via_rust(Fence("tree/42", 9, A), A, b"x")
    assert e.value.kind == "future_epoch"
    assert reg.value == b"abc"

    class Crashy(FencedResource):
        upcall_runtime = rt

        async def mutate(self, fence, writer, op):
            raise OSError("disk full")

    with pytest.raises(FenceOutcomeUnknownError) as e:
        await Crashy().mutate_via_rust(Fence("r", 1, A), A, b"x")
    assert e.value.kind == "foreign" and isinstance(e.value, UpcallError)
    # A hung mutate timing out as unknown: Rust
    # fenced_mutation_without_an_answer_times_out_as_unknown.


# -- topology admitted filter --------------------------------------------------------


async def test_admitted_filter_async_predicate_batch_and_prefetch(rt):
    async def pred(node_id):
        await asyncio.sleep(0)
        return str(node_id).startswith("aa")

    flt = AdmittedFilter(pred, runtime=rt)
    await flt.prefetch([A, B])
    assert flt.is_admitted(A) is True and flt.is_admitted(B) is False
    assert flt.stats().upcalls == 1

    class Batch:
        def __init__(self):
            self.batches = []

        def admitted_batch(self, node_ids):
            self.batches.append(tuple(map(str, node_ids)))
            return [True] * len(node_ids)

    b = Batch()
    flt2 = AdmittedFilter(b, runtime=rt)
    await flt2.prefetch([A, B, "cc" * 32])
    assert b.batches == [(A, B, "cc" * 32)]
    assert flt2.is_admitted("cc" * 32)


async def test_topology_snapshot_from_view():
    a, b, c = "0a" * 32, "0b" * 32, "0c" * 32
    view = TopologyView(
        clusters=(
            ClusterView(members=(NodeId(a), NodeId(b)), bridge=NodeId(b), witnesses=(NodeId(b),)),
            ClusterView(members=(NodeId(c),), bridge=NodeId(c), witnesses=(NodeId(c),)),
        ),
        far_pairs=((NodeId(a), NodeId(c)),),
        separation_coverage=1,
    )
    snap = TopologySnapshot.from_view(view)
    assert len(snap.clusters) == 2
    assert snap.cluster_of(a) == view.clusters[0]
    assert snap.separated(a, b) == "connected"
    assert snap.separated(a, c) == "separated"
    assert snap.separated(b, c) == "separated"  # cluster-level evidence
    assert snap.separated(a, "0d" * 32) == "unknown"
    # The same view round-trips through UCv1 as a TopologySource answer.
    assert up.ucv1_decode("TopologySnapshot", up.ucv1_encode("TopologySnapshot", view)) == view


# -- expose authorize ------------------------------------------------------------------


async def _http_backend():
    async def handle(reader, writer):
        data = b""
        while b"\r\n\r\n" not in data:
            chunk = await reader.read(4096)
            if not chunk:
                break
            data += chunk
        body = b"hello from python"
        writer.write(
            b"HTTP/1.1 200 OK\r\ncontent-length: %d\r\nconnection: close\r\n\r\n%s"
            % (len(body), body)
        )
        await writer.drain()
        writer.close()

    server = await asyncio.start_server(handle, "127.0.0.1", 0)
    return server, server.sockets[0].getsockname()[1]


async def _get(conn, service: str, metadata: bytes = b"") -> bytes:
    """One HTTP/1.1 GET over a raw relay stream; the response bytes (empty
    when the host refused the relay)."""
    send, recv = await asyncio.wait_for(open_relay(conn, service, metadata), 10)
    await send.write_all(b"GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
    await send.finish()
    try:
        return await asyncio.wait_for(recv.read_to_end(1 << 16), 10)
    except Exception:
        return b""


EXPOSE_ALPN = b"aster-expose/r6"


@pytest.mark.network
@pytest.mark.timeout(60)
async def test_expose_authorize_by_upcall():
    backend, port = await _http_backend()
    host = await Node.start(_offline(), alpns=[EXPOSE_ALPN])
    peer = await Node.start(_offline())
    rt = UpcallRuntime(policies={"expose_authorize": UpcallPolicy(timeout=0.5)})
    try:
        for n in (host, peer):
            await _ready(n)
        peer.add_node_addr(host)

        class Team(ExposeAuthorizer):
            upcall_runtime = rt

            def __init__(self):
                self.seen = []

            async def authorize(self, service_id, who, metadata):
                self.seen.append((service_id, who, metadata))
                return who == peer.id and metadata == b"token"

        team = Team()
        expose = host.expose()
        expose.expose_socket("api", f"127.0.0.1:{port}", authorize=team)
        expose.expose_socket("open", f"127.0.0.1:{port}")
        serving = asyncio.ensure_future(expose.serve([EXPOSE_ALPN]))

        conn = await asyncio.wait_for(net_client(peer).connect(host.node_id(), EXPOSE_ALPN), 10)
        resp = await _get(conn, "api", b"token")
        assert resp.startswith(b"HTTP/1.1 200") and resp.endswith(b"hello from python")
        assert team.seen == [("api", peer.id, b"token")]
        assert (await _get(conn, "open")).startswith(b"HTTP/1.1 200")
        # Refusals and the failure modes: Rust expose_authorize_upcalls_per_service,
        # socket_target_with_and_without_a_foreign_authorizer and
        # authorize_without_an_answer_follows_the_failure_mode (ffi/tests/expose.rs).
        serving.cancel()
        with pytest.raises(ValueError):
            expose.expose_socket("bad", "not-an-addr")
    finally:
        await rt.aclose()
        for n in (host, peer):
            await n.shutdown()
        backend.close()


# -- expose HTTP handlers and route policies (R9) ------------------------------------

HTTP_ALPN = b"aster-expose/r9"
BIG = bytes(range(256)) * 800  # 200 KiB: over inline_request_max, so it streams


class App(HttpHandler):
    def __init__(self, rt):
        self.upcall_runtime = rt
        self.seen = []

    async def handle(self, request):
        self.seen.append(request)
        if request.uri == "/hello":
            return HttpResponse(
                200,
                [("Content-Type", "text/plain"), ("x-peer", str(request.peer))],
                b"hi " + request.method.encode(),
            )
        if request.uri == "/echo":
            body = await request.read_body()
            return HttpResponse(201, {"x-len": str(len(body))}, body)
        if request.uri == "/stream":
            # Streamed request in, streamed response out (async generator).
            data = await request.read_body()

            async def chunks():
                for i in range(0, len(data), 50_000):
                    yield data[i : i + 50_000]

            return HttpResponse(200, [("x-sum", str(sum(data)))], chunks(), trailers=[("x-t", "1")])
        if request.uri == "/list":
            return HttpResponse(200, (), [b"a", "b", bytearray(b"c")])
        if request.uri == "/text":
            return "plain text"
        if request.uri == "/boom":
            raise RuntimeError("handler exploded")
        if request.uri == "/bad":
            return {"not": "a response"}
        if request.uri == "/midway":

            async def failing():
                yield b"partial"
                raise RuntimeError("mid-body failure")

            return HttpResponse(200, (), failing())
        return HttpResponse(404, (), b"")


async def _relay(conn, *a, **kw):
    return await asyncio.wait_for(relay_http(conn, "app", *a, **kw), 15)


@pytest.mark.network
@pytest.mark.timeout(90)
async def test_expose_http_handler_by_upcall():
    host = await Node.start(_offline(), alpns=[HTTP_ALPN])
    peer = await Node.start(_offline())
    rt = UpcallRuntime(policies={"expose_http": UpcallPolicy(timeout=1.0)})
    try:
        for n in (host, peer):
            await _ready(n)
        peer.add_node_addr(host)
        app = App(rt)

        class Team(ExposeAuthorizer):
            upcall_runtime = rt

            async def authorize(self, service_id, who, metadata):
                return who == peer.id

        expose = host.expose()
        expose.expose_http("app", app, authorize=Team(), max_request_body=1 << 20)
        serving = asyncio.ensure_future(expose.serve([HTTP_ALPN]))
        conn = await asyncio.wait_for(net_client(peer).connect(host.node_id(), HTTP_ALPN), 10)

        # Inline round trip: head, peer, headers (names lowercased).
        resp = await _relay(conn, "GET", "/hello", headers=[("x-q", "1")])
        assert isinstance(resp, HttpResponse)
        assert (resp.status, resp.body) == (200, b"hi GET")
        assert resp.header("content-type") == b"text/plain"
        assert resp.header("x-peer") == str(peer.id).encode()
        req = app.seen[-1]
        assert isinstance(req, HttpRequest)
        assert (req.service_id, req.peer, req.method, req.uri) == ("app", peer.id, "GET", "/hello")
        assert req.header("x-q") == b"1" and req.version == "HTTP/1.1"

        # A small declared body arrives inline.
        resp = await _relay(
            conn, "POST", "/echo", headers=[("content-length", "5")], body=b"hello"
        )
        assert (resp.status, resp.body, resp.header("x-len")) == (201, b"hello", b"5")
        assert app.seen[-1].body == b"hello"

        # A large body streams both ways (pulled reads, chunked writes).
        resp = await _relay(conn, "PUT", "/stream", body=BIG)
        assert isinstance(app.seen[-1].body, HttpBodyStream)
        assert resp.status == 200 and resp.body == BIG
        assert resp.header("x-sum") == str(sum(BIG)).encode()

        # A sync iterable of mixed chunks; a bare str is a 200.
        assert (await _relay(conn, "GET", "/list")).body == b"abc"
        resp = await _relay(conn, "GET", "/text")
        assert (resp.status, resp.body) == (200, b"plain text")

        # HEAD keeps the headers, drops the body.
        resp = await _relay(conn, "HEAD", "/hello")
        assert resp.status == 200 and resp.body == b""

        # Failures become 5xx; the Python message never reaches the peer.
        resp = await _relay(conn, "GET", "/boom")
        assert resp.status == 500 and b"exploded" not in resp.body
        assert (await _relay(conn, "GET", "/bad")).status == 500

        # A failure after a streamed head aborts the body: the relay resets
        # the stream, so the consumer sees a failed response, never the
        # truncated body as if it were complete.
        with pytest.raises(Exception) as failed:
            await _relay(conn, "GET", "/midway")
        assert not isinstance(failed.value, (AssertionError, asyncio.TimeoutError))

        # The 504 timeout, the 413 body cap, consumer-cancel and the
        # authorizer refusing a stranger are Rust behaviour
        # (ffi/tests/expose.rs: handler_failures_become_5xx_statuses,
        # request_body_limits, consumer_cancel_before_the_head_cancels_the_upcall,
        # authorize_deny_refuses_the_request_before_any_http_upcall).

        # A plain async function is a handler too.
        async def fn(request):
            return HttpResponse(202, (), request.uri)

        expose.expose_http("fn", fn)
        resp = await asyncio.wait_for(relay_http(conn, "fn", "GET", "/x"), 15)
        assert (resp.status, resp.body) == (202, b"/x")
        serving.cancel()
        with pytest.raises(ValueError):
            expose.expose_http("x", None)
    finally:
        await rt.aclose()
        for n in (host, peer):
            await n.shutdown()


class Streamer(HttpHandler):
    """Handler side of the streaming-relay tests: records what each request
    body did and how the response writes ended."""

    def __init__(self, rt):
        self.upcall_runtime = rt
        self.log = []
        self.started = asyncio.Event()

    async def handle(self, request):
        self.log.append(("start", request.uri))
        self.started.set()
        if request.uri == "/echo":
            try:
                data = await request.read_body()
            except HttpBodyError as exc:
                # Sticky: a retry fails too rather than ending cleanly. (The
                # task may be cancelled meanwhile; log the outcome either way.)
                again = "cancelled while retrying"
                try:
                    await request.body.read()
                    again = "ok"
                except HttpBodyError as exc2:
                    again = exc2.kind
                finally:
                    self.log.append(("body failed", exc.kind, again))
                raise
            except asyncio.CancelledError:
                # The consumer reset before the head: the task is cancelled.
                self.log.append(("body failed", "task cancelled", "task cancelled"))
                raise
            self.log.append(("body", len(data)))

            async def back():
                for i in range(0, len(data), 7000):
                    yield data[i : i + 7000]

            return HttpResponse(200, [("x-len", str(len(data)))], back())
        if request.uri == "/duplex":
            # Head first, then echo each request chunk as it arrives.
            log = self.log

            async def echo():
                outcome = ("duplex cancelled",)
                try:
                    async for chunk in request.body:
                        yield chunk
                    outcome = ("duplex done",)
                except HttpBodyError as exc:
                    outcome = ("duplex failed", exc.kind)
                    raise
                finally:
                    log.append(outcome)

            return HttpResponse(200, (), echo())
        return HttpResponse(404, (), b"")


async def _stream_pair(rt, *handlers):
    host = await Node.start(_offline(), alpns=[HTTP_ALPN])
    peer = await Node.start(_offline())
    for n in (host, peer):
        await _ready(n)
    peer.add_node_addr(host)
    expose = host.expose()
    for name, h in handlers:
        expose.expose_http(name, h)
    serving = asyncio.ensure_future(expose.serve([HTTP_ALPN]))
    conn = await asyncio.wait_for(net_client(peer).connect(host.node_id(), HTTP_ALPN), 10)
    return host, peer, serving, conn


@pytest.mark.network
@pytest.mark.timeout(90)
async def test_relay_http_stream_both_ways():
    rt = UpcallRuntime()
    app = Streamer(rt)
    host, peer, serving, conn = await _stream_pair(rt, ("app", app))
    try:
        # Async-generator upload; bounded reads of the streamed response.
        async def upload():
            for i in range(0, len(BIG), 30_000):
                yield BIG[i : i + 30_000]

        resp = await asyncio.wait_for(
            relay_http_stream(conn, "app", "PUT", "/echo", body=upload(), max_chunk=16384), 15
        )
        assert resp.status == 200 and resp.header("x-len") == str(len(BIG)).encode()
        assert isinstance(resp.body, HttpRelayBody) and resp.body.max_chunk == 16384
        got = bytearray()
        while (chunk := await resp.body.read(5000)) is not None:
            assert 0 < len(chunk) <= 5000
            got += chunk
        assert bytes(got) == BIG
        assert await resp.body.read() is None  # the end is sticky
        assert ("body", len(BIG)) in app.log

        # bytes, a sync iterable of mixed chunks, and no body; read_body().
        for body, want in ((b"abc", b"abc"), ([b"a", "b", bytearray(b"c")], b"abc"), (None, b"")):
            resp = await asyncio.wait_for(relay_http_stream(conn, "app", "POST", "/echo", body=body), 15)
            assert await resp.read_body() == want
        resp = await asyncio.wait_for(relay_http_stream(conn, "app", "POST", "/echo", body=BIG), 15)
        with pytest.raises(HttpBodyError) as big:
            await resp.read_body(limit=1000)
        assert big.value.kind == "too_large" and resp.body.closed

        # async for over the body.
        resp = await asyncio.wait_for(relay_http_stream(conn, "app", "POST", "/echo", body=BIG), 15)
        assert b"".join([c async for c in resp.body]) == BIG

        # The low-level form: write the upload yourself.
        upload_h, pending = relay_http_streaming(conn, "app", "POST", "/echo", {"x-a": "1"})
        assert isinstance(upload_h, HttpUpload) and not upload_h.closed
        await upload_h.write(b"hello ")
        assert upload_h.try_write("world") in (True, False)
        upload_h.finish()
        assert upload_h.closed
        with pytest.raises(HttpBodyError) as done:
            upload_h.finish()
        assert done.value.kind == "finished"
        resp = await asyncio.wait_for(pending, 15)
        assert (await resp.read_body()).startswith(b"hello ")

        with pytest.raises(ValueError):
            relay_http_streaming(conn, "app", "BAD METHOD", "/")
    finally:
        serving.cancel()
        await rt.aclose()
        for n in (host, peer):
            await n.shutdown()


@pytest.mark.network
@pytest.mark.timeout(90)
async def test_relay_http_stream_failures_never_truncate():
    rt = UpcallRuntime(policies={"expose_http": UpcallPolicy(timeout=10.0)})
    app = Streamer(rt)
    host, peer, serving, conn = await _stream_pair(rt, ("app", app))
    try:

        def body_failures():
            return [e for e in app.log if e[0] == "body failed"]

        # The upload iterator raising: its own exception propagates, and the
        # handler sees a failed request body (sticky), not a short one.
        async def broken():
            yield b"z" * 50_000
            await asyncio.wait_for(app.started.wait(), 10)
            raise KeyError("upload source failed")

        with pytest.raises(KeyError):
            await asyncio.wait_for(relay_http_stream(conn, "app", "PUT", "/echo", body=broken()), 15)
        await _until("handler saw the failed body", lambda: len(body_failures()) == 1)
        assert body_failures()[0][2] in (body_failures()[0][1], "cancelled while retrying")

        # An upload dropped without finish() aborts too.
        app.started.clear()
        upload_h, pending = relay_http_streaming(conn, "app", "PUT", "/echo")
        await upload_h.write(b"z" * 50_000)
        await asyncio.wait_for(app.started.wait(), 10)
        del upload_h
        with pytest.raises(Exception) as dropped:
            await asyncio.wait_for(pending, 15)
        assert not isinstance(dropped.value, asyncio.TimeoutError)
        await _until("dropped upload failed the body", lambda: len(body_failures()) == 2)

        # Cancelling the call mid-upload resets the request.
        app.started.clear()

        async def slow():
            yield b"z" * 50_000
            await asyncio.sleep(30)
            yield b"never"

        call = asyncio.ensure_future(relay_http_stream(conn, "app", "PUT", "/echo", body=slow()))
        await asyncio.wait_for(app.started.wait(), 10)
        call.cancel()
        with pytest.raises(asyncio.CancelledError):
            await call
        await _until("cancelled call failed the body", lambda: len(body_failures()) == 3)

        # The rest is Rust behaviour (ffi/tests/expose.rs):
        # relay_stream_failures_have_stable_codes (unknown service, reset
        # before the head), relay_stream_keeps_the_full_duplex_contract (a
        # response aborted mid-body fails the read; freeing the response
        # resets the upload) and consumer_cancel_mid_stream_makes_writes_cancelled.
        # No handler ever took a failed upload for a complete (short) one.
        assert not [e for e in app.log if e[0] == "body"]
    finally:
        serving.cancel()
        await rt.aclose()
        for n in (host, peer):
            await n.shutdown()


@pytest.mark.network
@pytest.mark.timeout(90)
async def test_relay_http_stream_response_proxies_through_a_handler():
    """An edge handler returns the relayed response as its own: bodies
    stream client -> edge -> origin and back without buffering."""
    rt = UpcallRuntime()
    origin = await Node.start(_offline(), alpns=[HTTP_ALPN])
    edge = await Node.start(_offline(), alpns=[HTTP_ALPN])
    client = await Node.start(_offline())
    serving = []
    try:
        for n in (origin, edge, client):
            await _ready(n)
        edge.add_node_addr(origin)
        client.add_node_addr(edge)
        origin_expose = origin.expose()
        origin_expose.expose_http("app", Streamer(rt))
        serving.append(asyncio.ensure_future(origin_expose.serve([HTTP_ALPN])))
        upstream = await asyncio.wait_for(
            net_client(edge).connect(origin.node_id(), HTTP_ALPN), 10
        )

        async def proxy(request):
            return await relay_http_stream(
                upstream, "app", request.method, request.uri, body=request.body
            )

        edge_expose = edge.expose()
        edge_expose.expose_http("proxy", proxy)
        serving.append(asyncio.ensure_future(edge_expose.serve([HTTP_ALPN])))
        conn = await asyncio.wait_for(net_client(client).connect(edge.node_id(), HTTP_ALPN), 10)
        resp = await asyncio.wait_for(relay_http_stream(conn, "proxy", "PUT", "/echo", body=BIG), 15)
        assert resp.status == 200 and resp.header("x-len") == str(len(BIG)).encode()
        assert await resp.read_body() == BIG
    finally:
        for t in serving:
            t.cancel()
        await rt.aclose()
        for n in (origin, edge, client):
            await n.shutdown()


@pytest.mark.network
@pytest.mark.timeout(90)
async def test_relay_http_stream_is_full_duplex():
    rt = UpcallRuntime(policies={"expose_http": UpcallPolicy(timeout=10.0)})
    app = Streamer(rt)
    host, peer, serving, conn = await _stream_pair(rt, ("app", app))
    try:
        # The early 413 during an endless upload, chunk-by-chunk ping-pong and
        # closing the response mid-upload: Rust
        # relay_stream_keeps_the_full_duplex_contract (ffi/tests/expose.rs).
        # Here: the Python mapping of a body generator driven by the echoes it
        # gets back, and of one that fails after the head.
        replies: asyncio.Queue = asyncio.Queue()

        async def chatty():
            for i in range(5):
                yield f"msg {i}".encode()
                await asyncio.wait_for(replies.get(), 15)

        resp = await asyncio.wait_for(
            relay_http_stream(conn, "app", "POST", "/duplex", body=chatty()), 15
        )
        got = b""
        async for chunk in resp.body:
            got += chunk
            replies.put_nowait(chunk)
        assert got == b"".join(f"msg {i}".encode() for i in range(5))

        # A body iterator failing after the head resets the request: the
        # handler's read fails and so does the response; the error is
        # logged since the call has already returned.
        n = len([e for e in app.log if e[0] == "duplex failed"])

        async def flaky():
            yield b"one"
            await asyncio.wait_for(replies.get(), 15)
            raise KeyError("source failed after the head")

        import logging

        warnings: list = []

        class Capture(logging.Handler):
            def emit(self, record):
                warnings.append(record.getMessage())

        capture = Capture(level=logging.WARNING)
        expose_log = logging.getLogger("aster.expose")
        expose_log.addHandler(capture)
        try:
            resp = await asyncio.wait_for(
                relay_http_stream(conn, "app", "POST", "/duplex", body=flaky()), 15
            )
            assert await resp.body.read() == b"one"
            replies.put_nowait(b"one")
            with pytest.raises(HttpBodyError) as failed:
                while await asyncio.wait_for(resp.body.read(), 15) is not None:
                    pass
            assert failed.value.kind == "read"
            await _until(
                "handler saw the failed body",
                lambda: len([e for e in app.log if e[0] == "duplex failed"]) == n + 1,
            )
            await _until(
                "failure logged",
                lambda: any("source failed after the head" in w for w in warnings),
            )
        finally:
            expose_log.removeHandler(capture)
        last = len(app.log) - 1 - app.log[::-1].index(("start", "/duplex"))
        assert ("duplex done",) not in app.log[last:]
    finally:
        serving.cancel()
        await rt.aclose()
        for n_ in (host, peer):
            await n_.shutdown()


def test_body_handle_detached_and_http_codec_values():
    (req,) = up.ucv1_decode(
        "ExposeHttpRequest",
        up.ucv1_encode(
            "ExposeHttpRequest",
            (
                HttpRequest(
                    service_id="s",
                    peer=None,
                    stream_id=9,
                    version="HTTP/2",
                    method="GET",
                    uri="/",
                    headers=(("x", b"1"),),
                    body=None,
                ),
            ),
        ),
    )
    assert isinstance(req.body, HttpBodyStream) and not req.body.attached
    assert req.body is req.handle and req.stream_id == 9
    with pytest.raises(HttpBodyError) as e:
        req.handle.finish()
    assert e.value.kind == "unknown_stream" and e.value.code == 1
    # Header names are lowercased on encode; status range is checked.
    raw = up.ucv1_encode("ExposeHttpResponse", HttpResponse(204, {"X-A": "b"}, ""))
    assert up.ucv1_decode("ExposeHttpResponse", raw).headers == (("x-a", b"b"),)
    for bad in (HttpResponse(99), HttpResponse(1000), "nope"):
        with pytest.raises(ValueError):
            up.ucv1_encode("ExposeHttpResponse", bad)
    # Grants are sorted and de-duplicated on encode, refused unsorted on decode.
    assert up.ucv1_decode("ExposeRouteGrant", up.ucv1_encode("ExposeRouteGrant", [2, 0, 2])) == (0, 2)
    with pytest.raises(ValueError):
        up.ucv1_encode("ExposeRouteGrant", [True])


EDGE_ALPN = b"aster-expose/r9-edge"


@pytest.mark.network
@pytest.mark.timeout(60)
async def test_edge_route_policy_by_upcall():
    edge = await Node.start(_offline(), alpns=[EDGE_ALPN])
    origin = await Node.start(_offline())
    rt = UpcallRuntime(policies={"expose_route_policy": UpcallPolicy(timeout=0.5)})
    try:
        for n in (edge, origin):
            await _ready(n)
        origin.add_node_addr(edge)

        class Domains(RoutePolicy):
            upcall_runtime = rt

            def __init__(self):
                self.seen = []

            async def grant(self, peer, metadata, specs):
                self.seen.append((peer, metadata, specs))
                if metadata == b"all":
                    return True
                return [s for s in specs if s.host.endswith(".example")]

        policy = Domains()
        router = EdgeRouter()
        expose = edge.expose()
        expose.serve_routes(router, policy)
        serving = asyncio.ensure_future(expose.serve([EDGE_ALPN]))
        conn = await asyncio.wait_for(net_client(origin).connect(edge.node_id(), EDGE_ALPN), 10)

        specs = [
            RouteSpec("a.example", 443, "http", "web"),
            RouteSpec("evil.test", 443, "http", "web"),
            RouteSpec("", 5432, "tcp", "db"),
        ]
        reg = await asyncio.wait_for(request_route(conn, specs, b"tok"), 10)
        assert reg.granted == (specs[0],)
        peer, meta, seen_specs = policy.seen[-1]
        assert (peer, meta, seen_specs) == (origin.id, b"tok", tuple(specs))
        hit = router.lookup("a.example", 443)
        assert hit is not None and hit[:2] == ("web", "http") and hit[2] == origin.id
        assert router.lookup("evil.test", 443) is None
        await reg.close()
        assert reg.closed

        reg2 = await asyncio.wait_for(request_route(conn, specs[1:], b"all"), 10)
        assert reg2.granted == tuple(specs[1:])
        # Eviction on close and the failure modes: Rust
        # cancelled_successful_route_registration_frees_handle_and_evicts_route
        # and route_policy_grant_and_deny (ffi/tests/expose.rs).
        serving.cancel()
    finally:
        await rt.aclose()
        for n in (edge, origin):
            await n.shutdown()


# -- Gate 0 over the upcall channel ----------------------------------------------------

APP_ALPN = b"test/r6/gate0"


async def _client():
    return await create_endpoint_with_config(EndpointConfig(alpns=[APP_ALPN]))


async def _accept_one(node: Node):
    return await asyncio.wait_for(node.accept_aster(), timeout=10)


@pytest.mark.network
@pytest.mark.timeout(60)
async def test_gate0_decided_by_python_handler_over_upcalls():
    node = await Node.start(AsterConfig(enable_hooks=True), alpns=[APP_ALPN])
    ep = await _client()
    seen = []

    class Handler(AdmissionHandler):
        async def handshake(self, peer, alpn, is_alive):
            seen.append((peer, alpn, is_alive))
            return True if len(seen) > 1 else (4040, b"custom reason")

    try:
        node.serve_admission(Handler())
        with pytest.raises(RuntimeError):
            node.admission()  # taken
        addr = node.node_addr_info()
        conn = await ep.connect_node_addr(addr, APP_ALPN)
        closed = await asyncio.wait_for(conn.closed(), timeout=10)
        assert closed["code"] == 4040 and closed["reason"] == b"custom reason"

        await ep.connect_node_addr(addr, APP_ALPN)
        alpn, _ = await _accept_one(node)
        assert alpn == APP_ALPN
        assert [s[0] for s in seen] == [ep.endpoint_id()] * 2
        assert all(s[1] == APP_ALPN and isinstance(s[2], bool) for s in seen)
    finally:
        await ep.close()
        await node.shutdown()


async def test_serve_admission_needs_hooks():
    node = await Node.start()
    try:
        with pytest.raises(RuntimeError):
            node.serve_admission(AdmissionHandler())
    finally:
        await node.shutdown()


# -- placement provider values -------------------------------------------------------


def test_placement_values_round_trip_through_ucv1():
    seed = MemberSeed(NodeId("01" * 32), "aster1ticket", b"\xde\xad")
    policy = PlacementPolicy(
        revision=3,
        digest="11" * 32,
        authority_public_key="22" * 32,
        authority_domain="example.org",
        target_voters=3,
        eligible=(
            PlacementCandidate(NodeId("01" * 32), seed),
            PlacementCandidate(NodeId("02" * 32)),
        ),
        target_retained_learners=1,
        pinned_voters=(NodeId("01" * 32),),
        excluded=(NodeId("03" * 32),),
        minimum_separated_voter_pairs=1,
        replacement_grace=600.0,
    )
    assert up.ucv1_decode("PlacementPolicy", up.ucv1_encode("PlacementPolicy", policy)) == policy
    assert up.ucv1_decode("MemberSeed", up.ucv1_encode("MemberSeed", seed)) == seed
    req = ProvisionRequest("33" * 32, "11" * 32, LogId(2, NodeId("01" * 32), 130), NodeId("02" * 32))
    assert up.ucv1_decode("PlacementProvisionRequest", up.ucv1_encode("PlacementProvisionRequest", (req,))) == (req,)


@pytest.mark.network
@pytest.mark.timeout(60)
async def test_expose_http_max_chunk_is_configurable():
    host = await Node.start(_offline(), alpns=[HTTP_ALPN])
    peer = await Node.start(_offline())
    rt = UpcallRuntime()
    sizes = []

    async def handler(request):
        if request.uri == "/up":
            while True:
                chunk = await request.handle.read(1 << 20)
                if chunk is None:
                    break
                sizes.append(len(chunk))
            return HttpResponse(200, (), str(sum(sizes)))
        # One write of the whole body: write() splits it at max_chunk.
        async def one():
            yield BIG

        return HttpResponse(200, (), one())

    try:
        for n in (host, peer):
            await _ready(n)
        peer.add_node_addr(host)
        expose = host.expose()
        for bad in (0, -1):
            with pytest.raises((ValueError, OverflowError)):
                expose.expose_http("bad", handler, max_chunk=bad)
        expose.expose_http("app", handler, max_chunk=1000, response_buffer=2)
        serving = asyncio.ensure_future(expose.serve([HTTP_ALPN]))
        conn = await asyncio.wait_for(net_client(peer).connect(host.node_id(), HTTP_ALPN), 10)

        # A large streamed response through a small max_chunk arrives intact.
        resp = await _relay(conn, "GET", "/down")
        assert resp.status == 200 and resp.body == BIG

        # A request-body read returns at most max_chunk bytes.
        resp = await _relay(conn, "PUT", "/up", body=BIG)
        assert resp.body == str(len(BIG)).encode()
        assert sizes and max(sizes) <= 1000 and sum(sizes) == len(BIG)
        serving.cancel()
    finally:
        await rt.aclose()
        for n in (host, peer):
            await n.shutdown()


@pytest.mark.network
@pytest.mark.timeout(60)
async def test_runtime_close_cancels_streamed_responses():
    host = await Node.start(_offline(), alpns=[HTTP_ALPN])
    peer = await Node.start(_offline())
    rt = UpcallRuntime(close_grace=0.5)
    started = asyncio.Event()
    finished = asyncio.Event()
    stubborn = asyncio.Event()
    stop = False

    async def forever():
        started.set()
        try:
            while True:
                yield b"x" * 1000
                await asyncio.sleep(0.01)
        finally:
            finished.set()

    class Stubborn:
        """An iterator that never gives way: it swallows cancellation, and
        its aclose() hangs until the test lets go."""

        def __aiter__(self):
            started.set()
            return self

        async def __anext__(self):
            await asyncio.sleep(0.01)
            return b"y" * 100

        async def aclose(self):
            stubborn.set()
            try:
                while not stop:
                    try:
                        await asyncio.sleep(0.01)
                    except asyncio.CancelledError:
                        pass
            finally:
                finished.set()

    def ignores_cancel():
        return Stubborn()

    class Streams(HttpHandler):
        def __init__(self, runtime, make):
            self.upcall_runtime = runtime
            self.make = make

        async def handle(self, request):
            return HttpResponse(200, (), self.make())

    rt2 = UpcallRuntime(close_grace=0.3)
    try:
        for n in (host, peer):
            await _ready(n)
        peer.add_node_addr(host)
        expose = host.expose()
        expose.expose_http("app", Streams(rt, forever))
        expose.expose_http("stubborn", Streams(rt2, ignores_cancel))
        serving = asyncio.ensure_future(expose.serve([HTTP_ALPN]))
        conn = await asyncio.wait_for(net_client(peer).connect(host.node_id(), HTTP_ALPN), 10)

        consumer = asyncio.ensure_future(relay_http(conn, "app", "GET", "/"))
        await asyncio.wait_for(started.wait(), 10)
        await asyncio.sleep(0.2)  # the head is out and the pump is writing
        assert not finished.is_set()
        rt.close()
        await asyncio.wait_for(finished.wait(), WAIT_BUDGET)  # the generator's finally ran
        await asyncio.wait_for(rt.aclose(), WAIT_BUDGET)
        consumer.cancel()

        # An iterator that ignores cancellation: aclose() is bounded.
        started.clear()
        finished.clear()
        consumer = asyncio.ensure_future(relay_http(conn, "stubborn", "GET", "/"))
        try:
            await asyncio.wait_for(started.wait(), 10)
            await asyncio.sleep(0.2)
            t0 = time.monotonic()
            await asyncio.wait_for(rt2.aclose(), WAIT_BUDGET)
            assert time.monotonic() - t0 < 2.5
            assert stubborn.is_set() and not finished.is_set()
        finally:
            stop = True
            await asyncio.wait_for(finished.wait(), WAIT_BUDGET)
            consumer.cancel()
        serving.cancel()
    finally:
        stop = True
        await rt.aclose()
        await rt2.aclose()
        for n in (host, peer):
            await n.shutdown()
