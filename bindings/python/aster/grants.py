"""Sealed grants -- per-recipient capability distribution over public docs
(Rust ``aster::grants``).

A root writes, in a publicly replicated policy doc, one record per recipient
at ``<prefix>/grants/<node_id>``: a secret (usually a ``NamespaceCapability``)
HPKE-sealed to that node's identity-derived key, with a :class:`GrantContext`
bound into the AEAD associated data so it cannot be replayed at another node,
resource, role or path. Recipients read through a :class:`PolicyDoc` pinned
to the root author (HPKE Base has no sender authenticity) and decide with
:func:`reconcile_namespace_grant`::

    # root
    ctx = GrantContext(app="myapp/grant/v1", granter=root.id, recipient=member_id,
                       resource=ns_id, path=grant_key("/myapp/v1", member_id),
                       role="write")
    record = GrantRecord.active("write", 1, seal_namespace_grant(ctx, capability))
    await doc.set_bytes(author, ctx.path.encode(), record.encode())

    # recipient
    policy = await PolicyDoc.import_read_only(docs_client(node), ns_id, root_id)
    outcome = reconcile_namespace_grant(
        node.secret_key(), ctx, await policy.fetch_grant("/myapp/v1", node.id))
    match outcome.kind:
        case "active": use(outcome.value)
        case "revoked" | "unopenable": drop()
        case "absent": pass          # not revocation -- keep waiting

Revoke by writing :meth:`GrantRecord.revoked` -- never by deleting the key
(an absent key reads the same as "not synced yet").

The AAD layout, seal/open, the role rule, reconcile and the grant key all run
in Rust (vectors: ``conformance/vectors/grants.json``). Failures raise
``ValueError``.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Generic, Literal, Optional, TypeVar, Union

from ._aster import (
    NamespaceCapability,
    NodeId,
    PolicyDoc,
    SecretKey,
    _grant_aad,
    _grant_record_decode,
    _grant_record_encode,
    _open_grant,
    _open_grant_with_key,
    _open_namespace_grant,
    _reconcile_grant,
    _seal_grant,
    _seal_grant_to_key,
    _seal_namespace_grant,
    check_grant_role,
    grant_key,
)

__all__ = [
    "GRANT_AAD_DOMAIN",
    "GRANT_RECORD_VERSION",
    "GrantContext",
    "GrantOutcome",
    "GrantRecord",
    "GrantState",
    "PolicyDoc",
    "check_grant_role",
    "grant_aad",
    "grant_key",
    "open_grant",
    "open_grant_with_key",
    "open_namespace_grant",
    "reconcile_grant",
    "reconcile_namespace_grant",
    "seal_grant",
    "seal_grant_to_key",
    "seal_namespace_grant",
]

#: Domain label baked into every grant AAD.
GRANT_AAD_DOMAIN = "aster/sealed-grant/v1"
#: Grant record schema version.
GRANT_RECORD_VERSION = 1

SecretInput = Union[SecretKey, bytes, str]
T = TypeVar("T")


@dataclass(frozen=True)
class GrantContext:
    """Everything a sealed grant is bound to. Sealing and opening must use an
    identical context -- any mismatch makes opening fail. Vary one field with
    ``dataclasses.replace(ctx, role="read")``."""

    #: Versioned application label, e.g. ``"portal-sync/tree-grant/v1"``.
    app: str
    #: The sealing authority (root) node id, bound into the AAD verbatim.
    granter: Union[NodeId, str]
    #: The one node this grant is addressed to, bound verbatim.
    recipient: Union[NodeId, str]
    #: What the capability unlocks -- caller-chosen opaque bytes (e.g. the
    #: namespace id).
    resource: bytes
    #: The exact doc key the record is written at.
    path: str
    #: Grant role, e.g. ``"read"`` / ``"write"`` (free-form for other schemes).
    role: str

    def aad(self) -> bytes:
        """The canonical AAD for this context (see :func:`grant_aad`)."""
        return _grant_aad(self)


def grant_aad(ctx: GrantContext) -> bytes:
    """The canonical AAD for ``ctx``: the domain label and every field,
    each u32-BE length-prefixed."""
    return _grant_aad(ctx)


def seal_grant(ctx: GrantContext, plaintext: bytes) -> bytes:
    """Seal ``plaintext`` to ``ctx.recipient``'s identity-derived HPKE key.
    Returns the Fory-encoded envelope to put in a :class:`GrantRecord`."""
    return _seal_grant(ctx, bytes(plaintext))


def open_grant(identity_secret: SecretInput, ctx: GrantContext, envelope: bytes) -> bytes:
    """Open a :func:`seal_grant` envelope with the recipient's identity
    secret (``node.secret_key()``)."""
    return _open_grant(identity_secret, ctx, bytes(envelope))


def seal_grant_to_key(recipient_public_key: bytes, ctx: GrantContext, plaintext: bytes) -> bytes:
    """Seal to an explicit, published 32-byte X25519 public key (the rotation
    escape hatch). Keep the two schemes explicit."""
    return _seal_grant_to_key(bytes(recipient_public_key), ctx, bytes(plaintext))


def open_grant_with_key(recipient_private_key: bytes, ctx: GrantContext, envelope: bytes) -> bytes:
    """Open a :func:`seal_grant_to_key` envelope with an explicit 32-byte
    X25519 private key."""
    return _open_grant_with_key(bytes(recipient_private_key), ctx, bytes(envelope))


def seal_namespace_grant(ctx: GrantContext, capability: NamespaceCapability) -> bytes:
    """Seal a namespace capability; checks the role rule first."""
    return _seal_namespace_grant(ctx, capability)


def open_namespace_grant(
    identity_secret: SecretInput, ctx: GrantContext, envelope: bytes
) -> NamespaceCapability:
    """Open a namespace-capability grant, re-checking the role rule."""
    return _open_namespace_grant(identity_secret, ctx, bytes(envelope))


@dataclass(frozen=True)
class GrantOutcome(Generic[T]):
    """What a reconcile loop should do with the record (or absence of one)
    at a grant key.

    - ``"active"``: apply ``value`` (bytes, or a ``NamespaceCapability``);
    - ``"revoked"``: drop the capability (``reason``, ``role``);
    - ``"unopenable"``: drop and alert (``error``) -- fail closed;
    - ``"absent"``: **not** revocation -- keep waiting and keep what you hold.
    """

    kind: Literal["active", "revoked", "unopenable", "absent"]
    value: Optional[T] = None
    reason: Optional[str] = None
    role: Optional[str] = None
    error: Optional[str] = None

    @property
    def drops_capability(self) -> bool:
        """True for ``revoked`` and ``unopenable``."""
        return self.kind in ("revoked", "unopenable")


def _outcome(
    identity_secret: SecretInput,
    ctx: GrantContext,
    record: Optional[bytes],
    payload: str,
) -> GrantOutcome:
    kind, value, reason, role, error = _reconcile_grant(
        identity_secret, ctx, None if record is None else bytes(record), payload
    )
    return GrantOutcome(kind=kind, value=value, reason=reason, role=role, error=error)


def reconcile_grant(
    identity_secret: SecretInput, ctx: GrantContext, record: Optional[bytes]
) -> GrantOutcome[bytes]:
    """Decide on the record at ``ctx.path`` (``None`` = no record), opening
    the raw payload when active. With more than one AAD version, decode the
    record first (:meth:`GrantRecord.decode`), pick the context from
    ``context_version``, then pass the same bytes -- never trial-decrypt."""
    return _outcome(identity_secret, ctx, record, "raw")


def reconcile_namespace_grant(
    identity_secret: SecretInput, ctx: GrantContext, record: Optional[bytes]
) -> GrantOutcome[NamespaceCapability]:
    """:func:`reconcile_grant` for a namespace-capability payload."""
    return _outcome(identity_secret, ctx, record, "namespace")


#: Grant record lifecycle state.
GrantState = Literal["active", "revoked"]


@dataclass(frozen=True)
class GrantRecord:
    """The plaintext framing a sealed grant is published in: explicit state
    (revocation is a value, never a deleted key) and the context version."""

    state: GrantState
    #: Which context template the envelope was sealed under.
    context_version: int
    role: str
    #: The sealed envelope (empty when revoked).
    envelope: bytes = b""
    #: Revocation reason (empty when active).
    reason: str = ""

    @classmethod
    def active(cls, role: str, context_version: int, envelope: bytes) -> "GrantRecord":
        """An active record carrying ``envelope`` (from :func:`seal_grant` /
        :func:`seal_namespace_grant`)."""
        if not envelope:
            raise ValueError("active grant record must carry an envelope")
        return cls("active", int(context_version), role, bytes(envelope), "")

    @classmethod
    def revoked(cls, role: str, reason: str = "") -> "GrantRecord":
        """A revocation record -- write this at the grant key instead of
        deleting it."""
        return cls("revoked", GRANT_RECORD_VERSION, role, b"", reason)

    @classmethod
    def decode(cls, data: bytes) -> "GrantRecord":
        """Decode record bytes (validates version and the state/envelope
        rules). Raises ``ValueError``."""
        state, version, role, envelope, reason = _grant_record_decode(bytes(data))
        return cls(state, version, role, envelope, reason)

    @property
    def is_active(self) -> bool:
        return self.state == "active"

    def encode(self) -> bytes:
        """Fory XLANG bytes."""
        return _grant_record_encode(
            self.state, self.context_version, self.role, self.envelope, self.reason
        )

    def __repr__(self) -> str:
        return (
            f"GrantRecord({self.state}, role={self.role!r}, v{self.context_version}, "
            f"envelope={len(self.envelope)} bytes, reason={self.reason!r})"
        )
