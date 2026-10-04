"""Plan P9 (coordination client) -- the parts that need no voter group: the
section 5.2 code tables against the Rust enums, the typed-error mapping (run
through the real Rust classifier), argument validation at the PyO3 boundary,
the journal lifecycle, and the pure placement planner. The live suite is
``test_coordination_live.py``.
"""

from __future__ import annotations

import dataclasses

import pytest

import aster
from aster import AsterConfig, Node, TopologyTiming
from aster._aster import (
    _coordination_error_for_testing,
    _coordination_outcome_codes,
    _coordination_reason_codes,
)
from aster.coordination import (
    COORDINATION_OUTCOMES,
    COORDINATION_REASONS,
    MAX_AUDIT_PAGE,
    CoordinationAwaitingAckError,
    CoordinationClient,
    CoordinationClosedError,
    CoordinationError,
    CoordinationInvalidArgumentError,
    CoordinationJournalFatalError,
    CoordinationJournalLockedError,
    CoordinationReadClient,
    CoordinationRejectedError,
    CoordinationRights,
    CoordinationRpcError,
    CoordinationUnavailableError,
    CoordinationUnresolvedError,
    MutationReply,
    PlacementCandidate,
    PlacementInput,
    PlacementPolicy,
    ResourceKey,
    ResourceSnapshot,
    fence_of,
    new_operation_id,
    placement_plan,
)

GROUP = "ab" * 32


def _offline(**kw) -> AsterConfig:
    return AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0", **kw)


# -- Code tables ---------------------------------------------------------------


def test_reason_table_matches_the_rust_enum():
    assert dict(_coordination_reason_codes()) == COORDINATION_REASONS


def test_outcome_table_matches_the_rust_enum():
    assert dict(_coordination_outcome_codes()) == COORDINATION_OUTCOMES


def test_rights_are_the_section_8_1_bits():
    r = CoordinationRights
    assert r.HOLDER == r.READ | r.ACQUIRE | r.RENEW | r.MUTATE | r.RELEASE
    assert r.REVOKE == 0x20
    assert MAX_AUDIT_PAGE == 10_000


def test_top_level_exports():
    assert aster.CoordinationClient is CoordinationClient
    assert aster.placement_plan is placement_plan
    assert aster.CoordinationRpcError is CoordinationRpcError


# -- Typed errors --------------------------------------------------------------


@pytest.mark.parametrize(
    "variant,message,cls,kind",
    [
        ("coordination:awaiting_ack", "held", CoordinationAwaitingAckError, "awaiting_ack"),
        ("coordination:unresolved", "held", CoordinationUnresolvedError, "unresolved"),
        ("coordination:journal_fatal", "held", CoordinationJournalFatalError, "journal_fatal"),
        ("coordination:journal_locked", "held", CoordinationJournalLockedError, "journal_locked"),
        ("coordination:unavailable", "budget spent", CoordinationUnavailableError, "unavailable"),
        ("coordination:closed", "closed", CoordinationClosedError, "closed"),
        ("invalid_argument", "bad hex", CoordinationInvalidArgumentError, "invalid"),
        # The kind is structural: a message that merely mentions a journal
        # state is not that state.
        ("invalid_argument", "a resolved result awaits acknowledgement", CoordinationInvalidArgumentError, "invalid"),
        ("rpc", "permission denied", CoordinationRpcError, "rpc"),
        ("transport", "no member answered", CoordinationUnavailableError, "unavailable"),
        ("connection", "something else", CoordinationError, "other"),
    ],
)
def test_rust_errors_map_to_typed_classes(variant, message, cls, kind):
    e = _coordination_error_for_testing(variant, message, 7)
    assert type(e) is cls
    assert isinstance(e, CoordinationError)
    assert e.kind == kind
    assert message in str(e)


def test_rpc_errors_carry_their_code_even_through_a_lease():
    direct = _coordination_error_for_testing("rpc", "denied", 7)
    assert isinstance(direct, CoordinationRpcError) and direct.code == 7
    # A lease failure keeps the structured kind of the RPC error underneath.
    wrapped = _coordination_error_for_testing(
        "coordination:rpc", "invalid argument: coordination lease: rpc error [5]: gone", 5
    )
    assert isinstance(wrapped, CoordinationRpcError) and wrapped.code == 5
    assert "coordination lease: rpc error [5]: gone" in str(wrapped)


def test_invalid_argument_is_also_a_value_error():
    assert issubclass(CoordinationInvalidArgumentError, ValueError)
    assert not issubclass(CoordinationRpcError, ValueError)


def _reply(**kw) -> MutationReply:
    base = dict(
        group_id=GROUP,
        client_id="00" * 16,
        sequence=3,
        request_hash="00" * 32,
        consumed=True,
        disposition_code=1,
        outcome="rejected",
        outcome_code=100,
        reason="held_by_other",
        reason_code=1,
        snapshot=None,
        acl_revision=0,
        admin_revision=0,
        operation_id=None,
        membership_log_id=None,
    )
    base.update(kw)
    return MutationReply(**base)


def test_a_rejection_carries_its_reason_and_reply():
    e = CoordinationRejectedError(_reply())
    assert e.kind == "rejected"
    assert e.reason == "held_by_other"
    assert e.reason_code == 1
    assert e.reply.sequence == 3


def test_fence_of_refuses_an_unheld_snapshot():
    snap = ResourceSnapshot(
        group_id=GROUP,
        resource=ResourceKey("n", b"\x01"),
        epoch=1,
        holder=None,
        held=False,
        ttl=0.0,
        lease_revision=0,
        value_revision=0,
        acl_revision=0,
        value=None,
        value_redacted=False,
    )
    with pytest.raises(CoordinationInvalidArgumentError):
        fence_of(snap)
    held = dataclasses.replace(snap, held=True, holder=aster.NodeId("cd" * 32))
    fence = fence_of(held)
    assert fence.epoch == 1 and fence.holder == "cd" * 32 and fence.group_id == GROUP


def test_new_operation_id_is_16_random_bytes():
    a = new_operation_id()
    assert len(a) == 32 and int(a, 16) >= 0
    assert new_operation_id() != a


# -- Client lifecycle without a group ------------------------------------------


@pytest.mark.network
async def test_client_lifecycle_without_a_group(tmp_path):
    node = await Node.start(_offline())
    try:
        with pytest.raises(CoordinationInvalidArgumentError):
            await CoordinationClient.connect(node, "nope", journal_dir=str(tmp_path))
        with pytest.raises(CoordinationError):
            await CoordinationClient.connect(
                node, GROUP, journal_dir=str(tmp_path), seed_tickets=["not-a-ticket"]
            )

        client = await CoordinationClient.connect(node, GROUP, journal_dir=tmp_path)
        assert client.group_id == GROUP
        assert client.local_id == node.id
        client_id = await client.client_id()
        assert len(client_id) == 32 and int(client_id, 16) >= 0
        assert await client.unacknowledged() is None
        assert await client.resume() is None

        # One lifetime lock per actor/group journal.
        with pytest.raises(CoordinationJournalLockedError):
            await CoordinationClient.connect(node, GROUP, journal_dir=str(tmp_path))

        # Bad arguments are refused before anything is journaled.
        with pytest.raises(CoordinationInvalidArgumentError):
            await client.set_grant(("n", "k"), "zz", CoordinationRights.READ, 0)
        with pytest.raises(CoordinationInvalidArgumentError):
            await client.acquire(("n", "k"), -1)
        with pytest.raises(CoordinationInvalidArgumentError):
            await client.compare_and_set(object(), 0, b"v")
        with pytest.raises(CoordinationInvalidArgumentError):
            await client.change_membership("00", [node.id])
        with pytest.raises(CoordinationInvalidArgumentError):
            client.reads().audit_stream(page_size=0)
        with pytest.raises(CoordinationInvalidArgumentError):
            client.reads().audit_stream(page_size=MAX_AUDIT_PAGE + 1)

        client.close()
        assert client.closed
        client.close()  # idempotent
        with pytest.raises(CoordinationClosedError):
            await client.acquire(ResourceKey("n", b"k"), 1.0)
        with pytest.raises(CoordinationClosedError):
            client.reads()

        # close() released the lock: the same journal reopens, and
        # `async with` closes it again.
        async with await CoordinationClient.connect(
            node, GROUP, journal_dir=str(tmp_path)
        ) as again:
            assert await again.unacknowledged() is None
        assert again.closed
        reopened = await CoordinationClient.connect(node, GROUP, journal_dir=str(tmp_path))
        reopened.close()

        reads = await CoordinationReadClient.connect(node, GROUP)
        assert "CoordinationReadClient" in repr(reads)
    finally:
        await node.shutdown()


# -- Placement planner (pure) --------------------------------------------------


@pytest.mark.network
async def test_placement_planner_judges_policy_and_membership():
    node = await Node.start(_offline(enable_monitoring=True))
    try:
        await node.topology().enable_shared(
            bytes([7] * 32), timing=TopologyTiming(min_hold=0, liveness_ttl=60)
        )
        snapshot = await node.topology().snapshot()
        ids = (node.id, "a1" * 32, "b2" * 32)
        policy = PlacementPolicy(
            revision=1,
            digest="22" * 32,
            authority_public_key="33" * 32,
            authority_domain="test.domain",
            target_voters=3,
            eligible=tuple(PlacementCandidate(i) for i in ids),
        )
        base = PlacementInput(
            group_id=GROUP,
            genesis_hash="11" * 32,
            deployment_profile_code=1,  # production: exactly 3 or 5 voters
            current_voters=ids,
            policy=policy,
        )

        bad = placement_plan(
            dataclasses.replace(base, policy=dataclasses.replace(policy, target_voters=4)),
            snapshot,
        )
        assert bad.kind == "invalid_policy" and bad.why

        zero = placement_plan(
            dataclasses.replace(base, policy=dataclasses.replace(policy, target_voters=0)),
            snapshot,
        )
        assert zero.kind == "invalid_policy"

        pinned = placement_plan(
            dataclasses.replace(
                base, policy=dataclasses.replace(policy, pinned_voters=("c3" * 32,))
            ),
            snapshot,
        )
        assert pinned.kind == "invalid_policy"

        converging = placement_plan(
            dataclasses.replace(base, active_membership_intent=True), snapshot
        )
        assert converging.kind == "converging"

        settled = placement_plan(base, snapshot)
        assert settled.kind in ("stable", "degraded")
        assert settled.separation is not None
        assert settled.separation.total_pairs == 3

        with pytest.raises(CoordinationInvalidArgumentError):
            placement_plan(base, object())
        with pytest.raises(CoordinationInvalidArgumentError):
            placement_plan(
                dataclasses.replace(
                    base, policy=dataclasses.replace(policy, replacement_grace=-1.0)
                ),
                snapshot,
            )
    finally:
        await node.shutdown()

