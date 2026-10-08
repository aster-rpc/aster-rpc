"""Plan P9 -- the wrapped coordination client against a **real** three-voter
group (production profile) over loopback Iroh.

The voters are Rust-only (plan D7), so the group runs in a child process:
``aster-coordination-fixture`` (``bindings/typescript/native/fixtures``), built
from ``aster::coordination::testing::GroupFixture`` -- the same fixture the
Rust client gates and the TypeScript suite use. Build it with::

    cargo build -p aster-transport-napi --features coordination-fixture \\
        --bin aster-coordination-fixture

or point ``ASTER_COORDINATION_FIXTURE`` at a binary. Without one the module is
skipped with that notice, never silently passed.
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

import pytest
import pytest_asyncio

from aster import (
    AsterConfig,
    ClusterView,
    LeaseOptions,
    Node,
    NodeId,
    TopologyTiming,
    TopologyView,
    UpcallError,
)
from aster.coordination import (
    CoordinationAwaitingAckError,
    CoordinationClient,
    CoordinationClosedError,
    CoordinationInvalidArgumentError,
    CoordinationJournalLockedError,
    CoordinationLease,
    CoordinationRejectedError,
    CoordinationRights,
    CoordinationRpcError,
    CoordinationUnresolvedError,
    MemberSeed,
    PlacementCandidate,
    PlacementInput,
    PlacementManager,
    PlacementPolicy,
    PlacementProvisioner,
    PlacementReport,
    ProvisionRequest,
    ResourceKey,
    fence_of,
    placement_plan,
)

REPO = Path(__file__).resolve().parents[2]
NS = "p9.coordination"


def _fixture_binary():
    exe = ".exe" if sys.platform == "win32" else ""
    target = Path(os.environ.get("CARGO_TARGET_DIR") or REPO / "target")
    candidates = [
        os.environ.get("ASTER_COORDINATION_FIXTURE"),
        target / "debug" / f"aster-coordination-fixture{exe}",
        target / "release" / f"aster-coordination-fixture{exe}",
    ]
    found = [Path(c) for c in candidates if c and Path(c).is_file()]
    # Prefer the freshest build.
    return max(found, key=lambda p: p.stat().st_mtime, default=None)


FIXTURE = _fixture_binary()

pytestmark = [
    pytest.mark.network,
    pytest.mark.timeout(180),
    pytest.mark.asyncio(loop_scope="module"),
    pytest.mark.skipif(
        FIXTURE is None,
        reason=(
            "no aster-coordination-fixture binary (cargo build -p aster-transport-napi "
            "--features coordination-fixture --bin aster-coordination-fixture, or set "
            "ASTER_COORDINATION_FIXTURE)"
        ),
    ),
]


def _offline(**kw) -> AsterConfig:
    return AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0", **kw)


def res(key: str) -> ResourceKey:
    return ResourceKey(NS, key.encode())


def _start_group(storage: Path, voters: int, seed: int):
    child = subprocess.Popen(
        [str(FIXTURE), str(storage), str(voters), str(seed)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    )
    line: list = []
    reader = threading.Thread(target=lambda: line.append(child.stdout.readline()), daemon=True)
    reader.start()
    reader.join(90)
    if not line or not line[0].strip():
        child.kill()
        raise RuntimeError(
            f"the voter group did not come up in 90 s (exit code {child.poll()})"
        )
    return child, json.loads(line[0])


def _stop_group(child: subprocess.Popen) -> None:
    if child.poll() is not None:
        return
    child.stdin.close()
    try:
        child.wait(15)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait()


class Group:
    """The running group plus the module's long-lived actors."""

    def __init__(self, info, tmp: Path):
        self.info = info
        self.tmp = tmp
        self.group_id = info["groupId"]
        self.authority_key = bytes.fromhex(info["authorityKey"])

    @property
    def seeds(self):
        return [v["ticket"] for v in self.info["voters"]]

    def journal(self, name: str) -> str:
        return str(self.tmp / name)

    async def actor(self, secret: bytes = None) -> Node:
        return await Node.start(
            _offline(secret_key=secret or os.urandom(32), root_pubkey=self.authority_key)
        )

    async def connect(self, node: Node, journal: str) -> CoordinationClient:
        return await CoordinationClient.connect(
            node, self.group_id, seed_tickets=self.seeds, journal_dir=self.journal(journal)
        )

    async def grant(self, key: str, node: Node) -> None:
        acl = await self.root.reads().inspect_acl(res(key))
        granted = await self.root.set_grant(
            res(key), node.id, CoordinationRights.HOLDER, acl.acl_revision
        )
        assert granted.outcome == "grant_updated", granted
        await granted.acknowledge()


@pytest_asyncio.fixture(scope="module", loop_scope="module")
async def group():
    tmp = Path(tempfile.mkdtemp(prefix="aster-p9-"))
    child, info = await asyncio.to_thread(_start_group, tmp / "voters", 3, 0xC7)
    g = Group(info, tmp)
    nodes = []
    try:
        # ROOT (the group's immutable super-administrator) is the endpoint
        # whose secret is the deployment ROOT key; it has no implicit
        # resource rights.
        g.root_node = await g.actor(bytes.fromhex(info["rootSecret"]))
        g.worker_node = await g.actor()
        g.contender_node = await g.actor()
        nodes = [g.root_node, g.worker_node, g.contender_node]
        g.root = await g.connect(g.root_node, "root")
        g.worker = await g.connect(g.worker_node, "worker")
        for key in ("head", "lease", "restart"):
            await g.grant(key, g.worker_node)
        await g.grant("lease", g.contender_node)
        yield g
    finally:
        for c in (getattr(g, "root", None), getattr(g, "worker", None)):
            if c is not None:
                c.close()
        for n in nodes:
            await n.shutdown()
        await asyncio.to_thread(_stop_group, child)
        shutil.rmtree(tmp, ignore_errors=True)


async def test_reports_a_production_group_status_cut(group):
    status = await group.root.reads().group_status()
    info = group.info
    assert status.group_id == info["groupId"]
    assert status.genesis_hash == info["genesisHash"]
    assert status.authority_domain == info["authorityDomain"]
    assert status.authority_public_key == info["authorityKey"]
    assert status.deployment_profile_code == 1
    assert set(status.voters) == {v["nodeId"] for v in info["voters"]}
    assert status.fatal_code == 0

    acl = await group.root.reads().inspect_acl(res("head"))
    assert [(g.subject_node_id, g.rights) for g in acl.grants] == [
        (group.worker_node.id, CoordinationRights.HOLDER)
    ]
    admins = await group.root.reads().inspect_admins()
    assert len(admins.administrators) > 0


async def test_mutations_are_resolved_and_acknowledged(group):
    worker, me = group.worker, group.worker_node.id
    acquired = await worker.acquire(res("head"), 30)
    assert acquired.outcome == "granted"
    assert acquired.reply.consumed
    assert acquired.snapshot.holder == me
    assert acquired.pending

    # The delivery contract: no new sequence until acknowledged.
    with pytest.raises(CoordinationAwaitingAckError):
        await worker.acquire(res("other"), 30)
    await acquired.acknowledge()
    assert not acquired.pending
    with pytest.raises(CoordinationClosedError):
        await acquired.acknowledge()

    fence = fence_of(acquired.snapshot)
    renewed = await worker.renew(fence)
    assert renewed.outcome == "renewed"
    assert renewed.sequence == acquired.sequence + 1
    await renewed.acknowledge()

    before = await worker.reads().read_linearizable(res("head"))
    written = await worker.compare_and_set(fence, before.value_revision, "v1")
    assert written.outcome == "mutated"
    await written.acknowledge()

    # A stale expected revision is a committed typed rejection: still a
    # resolved result that must be acknowledged.
    stale = await worker.compare_and_set(fence, before.value_revision, b"v2")
    assert stale.rejected
    assert stale.outcome == "rejected"
    assert stale.reason == "value_revision_mismatch"
    assert isinstance(stale.rejection(), CoordinationRejectedError)
    assert written.rejection() is None
    await stale.acknowledge()

    mine = await worker.reads().read_linearizable(res("head"))
    assert mine.value == b"v1"
    assert mine.held
    assert mine.epoch == fence.epoch
    assert mine.ttl > 0

    released = await worker.release(fence)
    assert released.outcome == "released"
    await released.acknowledge()
    assert not (await worker.reads().read_linearizable(res("head"))).held

    # A fence from the released epoch is refused, typed.
    late = await worker.renew(fence)
    assert late.rejected
    assert late.reason in ("not_held", "epoch_mismatch")
    await late.acknowledge()


async def test_a_lease_renews_in_rust_mutates_and_hands_off(group):
    worker = group.worker
    # Renewing every 0.1 s on purpose: the compare-and-set and the final
    # handoff below land while renewals are in flight, and ending the lease
    # mid-renewal must not strand the shared journal.
    outcome = await worker.acquire_lease(
        res("lease"), LeaseOptions(ttl=6.0, renew_interval=0.1, holder_margin=0.5)
    )
    assert outcome.granted, outcome.reason
    lease = outcome.lease
    assert isinstance(lease, CoordinationLease)
    assert lease.status == "held"
    first = lease.fence()
    assert first.holder == group.worker_node.id
    assert lease.resource.namespace == NS

    # Another entitled actor is refused while the lease is held.
    rival = await group.connect(group.contender_node, "contender")
    async with rival:
        contender = await rival.acquire_lease(res("lease"))
        assert not contender.granted
        assert contender.reason == "held_by_other"
        assert contender.snapshot.held
    assert rival.closed

    # Outlive the TTL: the renew loop keeps it.
    await asyncio.sleep(7)
    assert lease.status == "held"
    assert lease.fence().epoch == first.epoch

    current = await worker.reads().read_linearizable(res("lease"))
    mutated = await lease.compare_and_set(current.value_revision, "next")
    assert mutated.outcome == "mutated"
    with pytest.raises(CoordinationRejectedError) as stale:
        await lease.compare_and_set(current.value_revision, "stale")
    assert stale.value.reason == "value_revision_mismatch"

    handed = await lease.release_with_value(mutated.snapshot.value_revision, "final")
    assert handed.outcome == "released"
    assert lease.status == "released"
    assert lease.fence() is None
    await lease.release()  # idempotent
    with pytest.raises(CoordinationInvalidArgumentError):
        await lease.compare_and_set(0, "x")

    after = await worker.reads().read_linearizable(res("lease"))
    assert not after.held
    assert after.value == b"final"
    # The journal was left free by the auto-acknowledging lease path.
    assert await worker.unacknowledged() is None

    # `async with` releases on exit.
    again = await worker.acquire_lease(res("lease"))
    assert again.granted
    async with again.lease as held:
        assert held.status == "held"
    assert again.lease.status == "released"
    assert not (await worker.reads().read_linearizable(res("lease"))).held


async def test_a_permission_denial_is_a_typed_rpc_error(group):
    actor = await group.actor()
    client = await group.connect(actor, "ungranted")
    original_id = await client.client_id()
    try:
        # No ACL row for this actor: refused before proposal (PERMISSION_DENIED).
        with pytest.raises(CoordinationRpcError) as denied:
            await client.acquire(res("head"), 30)
        assert denied.value.code == 7
        # Nothing committed; the journaled request stays open until resumed
        # (the journal-blocking finding recorded by T8).
        with pytest.raises(CoordinationUnresolvedError):
            await client.acquire(res("lease"), 30)
        with pytest.raises(CoordinationRpcError):
            await client.resume()
        # The lease adapter surfaces the same typed status.
        second = await group.connect(actor, "ungranted-lease")
        try:
            with pytest.raises(CoordinationRpcError):
                await second.acquire_lease(res("head"))
        finally:
            second.close()

        # Restore permission while the journal is closed, then drive its saved
        # request rather than replacing the denied operation with a new one.
        client.close()
        await group.grant("head", actor)
        client = await group.connect(actor, "ungranted")
        resumed = await client.resume()
        assert resumed is not None and resumed.outcome == "granted"
        assert resumed.reply.consumed
        assert await client.client_id() == original_id
        assert resumed.reply.client_id == original_id
        assert resumed.snapshot.resource == res("head")
        replay = await client.resume()
        assert replay.reply == resumed.reply
        await resumed.acknowledge()
        released = await client.release(fence_of(resumed.snapshot))
        assert released.outcome == "released"
        assert released.sequence == resumed.sequence + 1
        assert released.reply.client_id == original_id
        await released.acknowledge()
    finally:
        client.close()
        await actor.shutdown()


async def test_redelivers_an_unacknowledged_result_after_a_restart(group):
    actor = await group.actor()
    try:
        await group.grant("restart", actor)

        first = await group.connect(actor, "restart-client")
        client_id = await first.client_id()
        resolved = await first.acquire(res("restart"), 30)
        assert resolved.outcome == "granted"
        saved = resolved.reply

        # One process owns the journal.
        with pytest.raises(CoordinationJournalLockedError):
            await group.connect(actor, "restart-client")

        # "Crash": close without acknowledging. The handle goes inert.
        first.close()
        assert not resolved.pending

        second = await group.connect(actor, "restart-client")
        assert await second.client_id() == client_id
        with pytest.raises(CoordinationAwaitingAckError):
            await second.acquire(res("other"), 30)

        redelivered = await second.unacknowledged()
        assert redelivered is not None
        assert redelivered.reply == saved
        # resume() redelivers the same awaiting result too.
        via_resume = await second.resume()
        assert via_resume.reply == saved
        await redelivered.acknowledge()
        # The other handle names the same, now-acknowledged entry: refused.
        with pytest.raises(Exception):
            await via_resume.acknowledge()

        assert await second.unacknowledged() is None
        assert await second.resume() is None

        released = await second.release(fence_of(saved.snapshot))
        assert released.outcome == "released"
        assert released.sequence == saved.sequence + 1
        assert released.reply.client_id == client_id
        await released.acknowledge()

        probe = await second.reads().probe_client(actor.id, client_id)
        assert probe.exists
        assert probe.last_sequence == released.sequence
        second.close()
    finally:
        await actor.shutdown()


async def test_streams_the_audit_log_in_ascending_paged_order(group):
    reads = group.root.reads()
    rows = [row async for row in reads.audit_stream()]
    assert len(rows) > 5
    indices = [r.log_id.index for r in rows]
    assert indices == sorted(set(indices))
    ops = {r.operation_code for r in rows}
    assert {1, 3, 7} <= ops  # acquire, compare_and_set, set_grant
    assert any(
        r.actor_node_id == group.worker_node.id
        and r.resource is not None
        and r.resource.namespace == NS
        for r in rows
    )

    # Small pages walk the same rows.
    paged = [r.log_id.index async for r in reads.audit_stream(page_size=3)]
    assert paged == indices

    # A cursor and a limit.
    tail = [r.log_id.index async for r in reads.audit_stream(after=rows[1].log_id, limit=2)]
    assert tail == indices[2:4]

    page = await reads.audit_page(None, 2)
    assert [r.log_id.index for r in page] == indices[:2]

    # The worker is no administrator.
    with pytest.raises(CoordinationRpcError):
        await group.worker.reads().audit_page(None, 10)
    with pytest.raises(CoordinationInvalidArgumentError):
        await reads.audit_page(None, 0)


async def test_exports_a_verified_recovery_snapshot(group):
    export = await group.root.reads().recovery_export()
    assert export.manifest.total_len == len(export.body)
    assert len(export.manifest.blake3_checksum) == 64
    target = group.tmp / "export.bin"
    manifest = await group.root.reads().recovery_export_to_file(target)
    assert target.stat().st_size == manifest.total_len
    assert not Path(f"{target}.partial").exists()


async def test_plans_placement_from_the_live_status_cut(group):
    status = await group.root.reads().group_status()
    monitor = await Node.start(_offline(enable_monitoring=True))
    try:
        await monitor.topology().enable_shared(
            os.urandom(32), timing=TopologyTiming(min_hold=0, liveness_ttl=60)
        )
        snapshot = await monitor.topology().snapshot()
        rec = placement_plan(
            PlacementInput(
                group_id=status.group_id,
                genesis_hash=status.genesis_hash,
                deployment_profile_code=status.deployment_profile_code,
                membership_log_id=status.membership_log_id,
                current_voters=status.voters,
                current_learners=status.learners,
                active_membership_intent=status.pending_operation_id is not None,
                policy=PlacementPolicy(
                    revision=1,
                    digest="44" * 32,
                    authority_public_key=status.authority_public_key,
                    authority_domain=status.authority_domain,
                    target_voters=3,
                    eligible=tuple(PlacementCandidate(v) for v in status.voters),
                ),
            ),
            snapshot,
        )
        assert rec.kind in ("stable", "degraded")
    finally:
        await monitor.shutdown()


# -- Placement manager with Python providers (plan R6 upcalls) -------------------


class _Policy:
    def __init__(self, policy):
        self.value = policy
        self.reads = 0

    def policy(self):
        self.reads += 1
        return self.value


class _Topo:
    def __init__(self, view):
        self.view = view
        self.snapshots = 0

    async def snapshot(self):
        self.snapshots += 1
        return self.view


def _policy_for(status, **kw) -> PlacementPolicy:
    fields = dict(
        revision=1,
        digest="44" * 32,
        authority_public_key=status.authority_public_key,
        authority_domain=status.authority_domain,
        target_voters=3,
        eligible=tuple(PlacementCandidate(v) for v in status.voters),
    )
    fields.update(kw)
    return PlacementPolicy(**fields)


def _one_cluster(ids) -> TopologyView:
    ids = tuple(sorted(ids, key=str))
    return TopologyView(
        clusters=(ClusterView(members=ids, bridge=ids[0], witnesses=ids[:1]),),
        separation_coverage=1,
    )


async def test_placement_manager_reconciles_with_python_providers(group):
    status = await group.root.reads().group_status()
    policy = _Policy(_policy_for(status))
    topo = _Topo(_one_cluster(status.voters))
    manager = PlacementManager(group.root, topology=topo, policy=policy)
    report = await asyncio.wait_for(manager.reconcile(), 60)
    assert isinstance(report, PlacementReport)
    assert report.policy_revision == 1 and report.policy_digest == "44" * 32
    assert set(report.current_voters) == set(status.voters)
    assert report.pending_operation_id is None
    assert report.reason
    assert policy.reads >= 1 and topo.snapshots >= 1

    # A fixed policy and the node's own topology: the live view is read
    # directly (here: not joined, so the pass stops with that error).
    fixed = PlacementManager(
        group.root, topology=group.root_node.topology(), policy=_policy_for(status)
    )
    with pytest.raises(CoordinationInvalidArgumentError, match="enable_shared"):
        await asyncio.wait_for(fixed.reconcile(), 60)


async def test_placement_manager_provider_failures_stop_the_pass(group):
    status = await group.root.reads().group_status()
    newcomer = NodeId(os.urandom(32).hex())
    voters = tuple(status.voters)
    # One voter excluded, a seedless newcomer eligible: the plan needs a
    # provisioned seed for the newcomer.
    policy = _policy_for(
        status,
        eligible=tuple(PlacementCandidate(v) for v in voters) + (PlacementCandidate(newcomer),),
        excluded=(voters[0],),
    )
    view = _one_cluster(voters + (newcomer,))
    asked = []

    class Refusing(PlacementProvisioner):
        async def provision(self, request):
            asked.append(request)
            raise RuntimeError("no capacity")

    manager = PlacementManager(
        group.root, topology=_Topo(view), policy=_Policy(policy), provisioner=Refusing()
    )
    with pytest.raises(UpcallError) as e:
        await asyncio.wait_for(manager.reconcile(), 60)
    assert e.value.kind == "foreign" and "no capacity" in str(e.value)
    assert len(asked) == 1
    req = asked[0]
    assert isinstance(req, ProvisionRequest)
    assert req.group_id == group.group_id and req.policy_digest == "44" * 32
    assert req.node_id == newcomer
    assert req.membership_log_id == status.membership_log_id

    class Wrong(PlacementProvisioner):
        async def provision(self, request):
            return MemberSeed(NodeId("ee" * 32), "aster1ticket")

    # A seed for a different node is refused before anything is submitted.
    wrong = PlacementManager(
        group.root, topology=_Topo(view), policy=_Policy(policy), provisioner=Wrong()
    )
    with pytest.raises(CoordinationInvalidArgumentError):
        await asyncio.wait_for(wrong.reconcile(), 60)
    after = await group.root.reads().group_status()
    assert set(after.voters) == set(status.voters) and after.pending_operation_id is None

    class BrokenTopology:
        async def snapshot(self):
            raise OSError("topology service down")

    broken = PlacementManager(group.root, topology=BrokenTopology(), policy=_Policy(policy))
    with pytest.raises(UpcallError) as e:
        await asyncio.wait_for(broken.reconcile(), 60)
    assert e.value.kind == "foreign"
