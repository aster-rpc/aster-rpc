"""Coordination client (Rust ``aster::coordination::client``, wrapped -- never
ported; plan D7 / P9).

A durable, journaled client for a coordination group: fenced leases over small
registers, exact ACLs, and linearizable reads. Journaling, leader redirects,
retries and reply validation all run in Rust::

    client = await CoordinationClient.connect(
        node, group_id,
        seed_tickets=seed_tickets,               # routing hints only
        journal_dir="./state/coordination",      # persist with the node identity
    )
    async with client:
        # On startup, before any mutation: redeliver what a previous run resolved.
        pending = await client.unacknowledged() or await client.resume()
        if pending:
            await handle(pending.reply)
            await pending.acknowledge()

        resolved = await client.acquire(ResourceKey("app.head", b"w-1"), ttl=30)
        # fsynced before it returned; acknowledge once *you* durably handled it
        await resolved.acknowledge()

The delivery contract: every mutation returns a :class:`ResolvedMutation` that
was fsynced before the awaitable completed. Until it is acknowledged no new
mutation is permitted -- across calls and across restarts -- and
:meth:`CoordinationClient.unacknowledged` / :meth:`CoordinationClient.resume`
redeliver it. A typed rejection (``outcome == "rejected"``) is still a resolved
result and must be acknowledged too.

Resources are a :class:`ResourceKey` or a ``(namespace, key)`` tuple (a ``str``
key is UTF-8). Node ids are accepted as :class:`aster.NodeId`, 32 bytes or 64
hex characters, and returned as :class:`aster.NodeId`. Durations are seconds.

Not wrapped yet (needs the binding upcall mechanism, plan R6): the placement
*manager* with Python-implemented provisioner / policy / topology sources. The
pure planner is :func:`placement_plan`.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Dict, Literal, Optional, Tuple

from ._aster import (
    AuditStream,
    CoordinationClient,
    CoordinationLease,
    CoordinationReadClient,
    NodeId,
    ResolvedMutation,
    TopologySnapshot,
    _coordination_placement_plan,
)
from .lease import DenyReason, LeaseSnapshot

__all__ = [
    "AclGrant",
    "AclStatus",
    "AdminStatus",
    "AuditRecord",
    "AuditStream",
    "COORDINATION_OUTCOMES",
    "COORDINATION_REASONS",
    "ClientProbe",
    "CoordinationAwaitingAckError",
    "CoordinationClient",
    "CoordinationClosedError",
    "CoordinationError",
    "CoordinationFence",
    "CoordinationInvalidArgumentError",
    "CoordinationJournalFatalError",
    "CoordinationJournalLockedError",
    "CoordinationLease",
    "CoordinationLeaseOutcome",
    "CoordinationReadClient",
    "CoordinationRejectedError",
    "CoordinationRights",
    "CoordinationRpcError",
    "CoordinationUnavailableError",
    "CoordinationUnresolvedError",
    "ExportManifest",
    "GroupStatus",
    "LogId",
    "MAX_AUDIT_PAGE",
    "MemberSeed",
    "MembershipStatus",
    "MutationReply",
    "PlacementCandidate",
    "PlacementInput",
    "PlacementPlan",
    "PlacementPolicy",
    "PlacementRecommendation",
    "RecoveredFrom",
    "RecoveryExport",
    "ResolvedMutation",
    "ResourceKey",
    "ResourceSnapshot",
    "RetireReply",
    "SeparationSummary",
    "fence_of",
    "new_operation_id",
    "placement_plan",
]

# -- Codes ---------------------------------------------------------------------

#: Section 5.2 outcome codes (checked against the Rust enum by the tests).
COORDINATION_OUTCOMES: Dict[int, str] = {
    1: "granted",
    2: "renewed",
    3: "mutated",
    4: "released",
    5: "revoked",
    6: "grant_updated",
    7: "admin_updated",
    8: "membership_accepted",
    9: "client_retired",
    10: "membership_cancelled",
    100: "rejected",
}

#: Section 5.2 reason codes, ``0`` is none (checked against the Rust enum).
COORDINATION_REASONS: Dict[int, str] = {
    0: "none",
    1: "held_by_other",
    2: "not_held",
    3: "not_holder",
    4: "epoch_mismatch",
    5: "value_revision_mismatch",
    6: "acl_revision_mismatch",
    7: "admin_revision_mismatch",
    8: "sequence_too_old",
    9: "sequence_gap",
    10: "sequence_conflict",
    11: "client_retired",
    12: "permission_changed",
    13: "membership_revision_mismatch",
    14: "membership_intent_active",
    15: "counter_exhausted",
    16: "membership_cannot_cancel",
    17: "operation_id_conflict",
    18: "retained_session_cap_reached",
    19: "state_budget_exhausted",
    20: "intent_slots_exhausted",
    21: "active_client_cap",
    22: "resource_slots_exhausted",
    23: "acl_slots_exhausted",
    24: "last_administrator",
    25: "membership_learner_envelope",
    26: "root_authority_immutable",
}


class CoordinationRights:
    """Section 8.1 rights bits -- exact, no wildcards or inheritance."""

    READ = 0x01
    ACQUIRE = 0x02
    RENEW = 0x04
    MUTATE = 0x08
    RELEASE = 0x10
    REVOKE = 0x20
    #: READ | ACQUIRE | RENEW | MUTATE | RELEASE -- a register holder's set.
    HOLDER = 0x1F


#: The section 5.2 hard cap on one ``audit_stream`` request.
MAX_AUDIT_PAGE = 10_000

# -- Errors --------------------------------------------------------------------

#: What a :class:`CoordinationError` reports.
CoordinationErrorKind = Literal[
    "awaiting_ack",
    "unresolved",
    "journal_fatal",
    "journal_locked",
    "invalid",
    "rpc",
    "unavailable",
    "closed",
    "rejected",
    "other",
]


class CoordinationError(Exception):
    """Base class of every coordination failure; ``kind`` says which."""

    kind: str = "other"

    def __init__(self, message: str, *, kind: Optional[str] = None) -> None:
        super().__init__(message)
        if kind is not None:
            self.kind = kind

    @property
    def message(self) -> str:
        return str(self)

    def __repr__(self) -> str:
        return f"{type(self).__name__}(kind={self.kind!r}, message={str(self)!r})"


class CoordinationAwaitingAckError(CoordinationError):
    """A resolved result awaits acknowledgement before any new mutation;
    take ``unacknowledged()`` first."""

    kind = "awaiting_ack"


class CoordinationUnresolvedError(CoordinationError):
    """Another request is unresolved on this journal; call ``resume()``."""

    kind = "unresolved"


class CoordinationJournalFatalError(CoordinationError):
    """The journal is fatal (section 10 reason 18); no further mutation is
    possible."""

    kind = "journal_fatal"


class CoordinationJournalLockedError(CoordinationError):
    """Another process (or an unclosed client) holds this journal's lock."""

    kind = "journal_locked"


class CoordinationInvalidArgumentError(CoordinationError, ValueError):
    """A bad argument, or a request the client refused to journal."""

    kind = "invalid"


class CoordinationRpcError(CoordinationError):
    """The group answered with a non-OK RPC status (``code``, e.g. 7 for
    PERMISSION_DENIED)."""

    kind = "rpc"

    def __init__(self, message: str, *, code: int = 0) -> None:
        super().__init__(message)
        self.code = code


class CoordinationUnavailableError(CoordinationError):
    """No member answered, or the outcome is ambiguous -- safe to retry."""

    kind = "unavailable"


class CoordinationClosedError(CoordinationError):
    """The client (or this handle) is closed / already consumed."""

    kind = "closed"


class CoordinationRejectedError(CoordinationError):
    """A committed typed rejection (section 5.2 outcome 100). Carries the
    reason and the full reply (its post-decision snapshot is a back-off /
    retry hint)."""

    kind = "rejected"

    def __init__(self, reply: "MutationReply") -> None:
        super().__init__(
            f"coordination: rejected ({reply.reason}, reason {reply.reason_code})"
        )
        self.reply = reply
        self.reason = reply.reason
        self.reason_code = reply.reason_code


# -- Value types ---------------------------------------------------------------


@dataclass(frozen=True)
class ResourceKey:
    """An exact resource identity: ``namespace`` plus raw key bytes (a
    ``str`` key is accepted as UTF-8 wherever a resource is an argument)."""

    namespace: str
    key: bytes


@dataclass(frozen=True)
class CoordinationFence:
    """The ``(group, resource, epoch, holder)`` fence a mutation carries."""

    group_id: str
    resource: ResourceKey
    epoch: int
    holder: NodeId


@dataclass(frozen=True)
class LogId:
    """A Raft log id."""

    term: int
    leader_node_id: NodeId
    index: int


@dataclass(frozen=True)
class ResourceSnapshot:
    """One resource's committed row at the reply's cut."""

    group_id: str
    resource: ResourceKey
    epoch: int
    holder: Optional[NodeId]
    held: bool
    #: The row's lease TTL, seconds.
    ttl: float
    lease_revision: int
    value_revision: int
    acl_revision: int
    value: Optional[bytes]
    value_redacted: bool


@dataclass(frozen=True)
class MutationReply:
    """The exact reply a journaled mutation resolved to."""

    group_id: str
    client_id: str
    sequence: int
    request_hash: str
    #: Whether the sequence was consumed (``disposition_code == 1``).
    consumed: bool
    disposition_code: int
    #: A :data:`COORDINATION_OUTCOMES` name, or ``"unknown"``.
    outcome: str
    outcome_code: int
    #: A :data:`COORDINATION_REASONS` name, or ``"unknown"``.
    reason: str
    reason_code: int
    snapshot: Optional[ResourceSnapshot]
    acl_revision: int
    admin_revision: int
    operation_id: Optional[str]
    membership_log_id: Optional[LogId]


@dataclass(frozen=True)
class MemberSeed:
    """Canonical seed material for a new member."""

    node_id: NodeId
    ticket: str
    signed_join_permit: Optional[bytes] = None


@dataclass(frozen=True)
class RecoveredFrom:
    """Where a group founded by a section 13 recovery import came from."""

    source_group_id: str
    source_genesis_hash: str
    source_authority_public_key: str
    source_authority_domain: str
    export_hash: str
    source_last_applied: Optional[LogId]


@dataclass(frozen=True)
class GroupStatus:
    """The ``group_status`` cut (S4-STATUS-CUT). Advisory fields describe the
    answering node; replicated fields are the group's."""

    group_id: str
    lifecycle_code: int
    local_node_id: NodeId
    #: ``0`` none, ``1`` learner, ``2`` voter.
    local_role_code: int
    leader_node_id: Optional[NodeId]
    term: int
    last_log_id: Optional[LogId]
    committed_log_id: Optional[LogId]
    applied_log_id: Optional[LogId]
    deployment_profile_code: int
    membership_log_id: Optional[LogId]
    membership_joint: bool
    voters: Tuple[NodeId, ...]
    learners: Tuple[NodeId, ...]
    pending_operation_id: Optional[str]
    quorum_available: Optional[bool]
    fatal_code: int
    audit_floor: Optional[LogId]
    genesis_hash: str
    authority_public_key: str
    authority_domain: str
    recovered_from: Optional[RecoveredFrom]


@dataclass(frozen=True)
class AuditRecord:
    """One retained audit row (section 14)."""

    log_id: LogId
    actor_node_id: Optional[NodeId]
    #: 1-11 mutation method codes; 100 expire, 101 membership progress, 102
    #: genesis installation.
    operation_code: int
    resource: Optional[ResourceKey]
    result_code: int
    epoch: int
    value_revision: int


@dataclass(frozen=True)
class AclGrant:
    """One ACL row: a subject and its exact rights bits."""

    subject_node_id: NodeId
    rights: int


@dataclass(frozen=True)
class AclStatus:
    """The committed ACL of one resource."""

    resource: ResourceKey
    acl_revision: int
    grants: Tuple[AclGrant, ...]


@dataclass(frozen=True)
class AdminStatus:
    """The delegated administrator set (ROOT is static and not listed)."""

    admin_revision: int
    administrators: Tuple[NodeId, ...]


@dataclass(frozen=True)
class MembershipStatus:
    """One membership intent's status (section 5.2)."""

    operation_id: Optional[str]
    status_code: int
    expected_membership_log_id: Optional[LogId]
    observed_membership_log_id: Optional[LogId]
    desired_voters: Tuple[NodeId, ...]
    retained_learners: Tuple[NodeId, ...]
    error_code: int


@dataclass(frozen=True)
class ClientProbe:
    """``probe_client`` diagnostics -- never authority to reassign a
    sequence."""

    target_node_id: NodeId
    target_client_id: str
    exists: bool
    last_sequence: int
    request_hash: str
    disposition_code: int
    retired: bool


@dataclass(frozen=True)
class RetireReply:
    """``retire_client`` reply: ``(9, 0)`` retired, or ``(100, reason)``."""

    outcome_code: int
    reason_code: int


@dataclass(frozen=True)
class ExportManifest:
    """A verified recovery export's manifest."""

    total_len: int
    blake3_checksum: str
    applied_log_id: Optional[LogId]


@dataclass(frozen=True)
class RecoveryExport:
    """A recovery export held in memory."""

    manifest: ExportManifest
    body: bytes


@dataclass(frozen=True)
class CoordinationLeaseOutcome:
    """The result of :meth:`CoordinationClient.acquire_lease`: ``lease`` when
    ``granted``, else ``reason`` and ``snapshot``."""

    granted: bool
    lease: Optional[CoordinationLease] = None
    reason: Optional[DenyReason] = None
    snapshot: Optional[LeaseSnapshot] = None


def fence_of(snapshot: ResourceSnapshot) -> CoordinationFence:
    """The fence a granted / renewed reply's snapshot names -- what
    ``renew``, ``compare_and_set``, ``release`` and ``release_with_value``
    carry."""
    if not snapshot.held or snapshot.holder is None:
        raise CoordinationInvalidArgumentError(
            "coordination: the snapshot names no holder, so it carries no fence"
        )
    return CoordinationFence(
        group_id=snapshot.group_id,
        resource=snapshot.resource,
        epoch=snapshot.epoch,
        holder=snapshot.holder,
    )


def new_operation_id() -> str:
    """A fresh random 16-byte membership operation id (hex)."""
    return os.urandom(16).hex()


# -- Placement planner (pure) ----------------------------------------------------


@dataclass(frozen=True)
class PlacementCandidate:
    """One eligible node, optionally with the seed material to join it."""

    node_id: NodeId
    member_seed: Optional[MemberSeed] = None


@dataclass(frozen=True)
class PlacementPolicy:
    """The application-owned placement policy (section 7.7.1)."""

    revision: int
    #: BLAKE3 of the application's canonical policy record (hex).
    digest: str
    authority_public_key: str
    authority_domain: str
    target_voters: int
    eligible: Tuple[PlacementCandidate, ...]
    target_retained_learners: int = 0
    pinned_voters: Tuple[NodeId, ...] = ()
    excluded: Tuple[NodeId, ...] = ()
    minimum_separated_voter_pairs: int = 0
    #: Seconds.
    replacement_grace: float = 0.0
    allow_healthy_rebalance: bool = False


@dataclass(frozen=True)
class PlacementInput:
    """One planning input (membership facts usually from ``group_status()``)."""

    group_id: str
    genesis_hash: str
    deployment_profile_code: int
    current_voters: Tuple[NodeId, ...]
    policy: PlacementPolicy
    membership_log_id: Optional[LogId] = None
    current_learners: Tuple[NodeId, ...] = ()
    active_membership_intent: bool = False
    grace_crossed_members: Tuple[NodeId, ...] = ()


@dataclass(frozen=True)
class SeparationSummary:
    """Pairwise separation evidence for a selected voter set."""

    separated_pairs: int
    connected_pairs: int
    unknown_pairs: int
    total_pairs: int
    represented_clusters: int
    diversity_proved: bool


@dataclass(frozen=True)
class PlacementPlan:
    """A complete desired membership, ready for
    ``client.change_membership(new_operation_id(), plan)``."""

    desired_voters: Tuple[NodeId, ...]
    retained_learners: Tuple[NodeId, ...]
    member_seeds: Tuple[MemberSeed, ...]
    expected_membership_log_id: Optional[LogId]
    separation: SeparationSummary


@dataclass(frozen=True)
class PlacementRecommendation:
    """The planner's answer (section 7.7.2). Which fields are set depends on
    ``kind``: ``separation`` (stable / degraded), ``why`` (degraded /
    invalid_policy), ``missing`` (insufficient_evidence), ``candidates`` /
    ``desired_voters`` / ``retained_learners`` (needs_provisioning), ``plan``
    (plan)."""

    kind: Literal[
        "stable",
        "degraded",
        "converging",
        "needs_provisioning",
        "insufficient_evidence",
        "invalid_policy",
        "plan",
    ]
    why: Optional[str] = None
    missing: Optional[str] = None
    separation: Optional[SeparationSummary] = None
    candidates: Optional[Tuple[NodeId, ...]] = None
    desired_voters: Optional[Tuple[NodeId, ...]] = None
    retained_learners: Optional[Tuple[NodeId, ...]] = None
    plan: Optional[PlacementPlan] = None


def placement_plan(
    input: PlacementInput, topology: TopologySnapshot
) -> PlacementRecommendation:
    """Section 7.7.2's pure, deterministic voter placement planner: local
    advice only. ``topology`` comes from ``await node.topology().snapshot()``.
    Submit a ``plan`` with :meth:`CoordinationClient.change_membership` and a
    fresh :func:`new_operation_id`."""
    return _coordination_placement_plan(input, topology)
