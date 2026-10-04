"""Ownership attestations (Rust ``aster::attestation``).

An :class:`AttestationChain` is a portable, signed artifact proving that a
*root* identity owns a *node* identity. Verification is fully offline::

    chain = attest_root_node(root_secret, node.secret_key(), epoch=1)
    text = chain.to_text()            # "aster.attestation.chain.v1:..."

    verified = verify_chain(AttestationChain.from_text(text), [root_id], node.id)
    verified.role_of(root_id)         # "anchor"

Signing and verification run in Rust; the chain is opaque bytes here. Key
arguments take a ``SecretKey`` / ``PublicKey`` / ``NodeId``, 32 raw bytes, or
64 hex characters.
"""

from __future__ import annotations

import datetime as _dt
from dataclasses import dataclass
from typing import Iterable, Literal, Optional, Tuple, Union

from ._aster import (
    AttestationError,
    NodeId,
    PublicKey,
    SecretKey,
    _attest_root_node,
    _attestation_chain_from_text,
    _attestation_chain_to_text,
    _attestation_public_key,
    _attestation_verify_chain,
)

__all__ = [
    "AttestationChain",
    "AttestationError",
    "AttestationRole",
    "VerifiedChain",
    "attest_root_node",
    "attestation_public_key",
    "verify_chain",
]

#: An identity's role within a verified chain.
AttestationRole = Literal["node", "anchor", "intermediate"]

SecretInput = Union[SecretKey, bytes, str]
IdInput = Union[NodeId, PublicKey, bytes, str]
TimeInput = Union[int, float, _dt.datetime, None]


def _hex(id: IdInput) -> str:
    if isinstance(id, (bytes, bytearray)):
        return bytes(id).hex()
    if isinstance(id, PublicKey):
        return id.to_hex()
    return str(id).strip().lower()


def _unix(v: TimeInput) -> int:
    if v is None:
        return 0
    if isinstance(v, _dt.datetime):
        return int(v.timestamp())
    return int(v)


@dataclass(frozen=True)
class AttestationChain:
    """An ownership-attestation chain -- the opaque, portable wire artifact.

    Equality and hashing are byte-wise. ``bytes(chain)`` gives the wire bytes.
    """

    data: bytes

    @classmethod
    def from_bytes(cls, data: bytes) -> "AttestationChain":
        """Wrap raw chain bytes (e.g. received from a peer or storage)."""
        return cls(bytes(data))

    @classmethod
    def from_text(cls, text: str) -> "AttestationChain":
        """Decode ``aster.attestation.chain.v1:<base64url>`` text. Raises
        :class:`AttestationError` for malformed text."""
        return cls(_attestation_chain_from_text(text))

    def to_text(self) -> str:
        """Encode as ``aster.attestation.chain.v1:<base64url>`` text."""
        return _attestation_chain_to_text(self.data)

    def __bytes__(self) -> bytes:
        return self.data

    def __repr__(self) -> str:
        return f"AttestationChain({len(self.data)} bytes)"


@dataclass(frozen=True)
class VerifiedChain:
    """The outcome of a successful :func:`verify_chain`. Ids are node-level
    identities (Ed25519 transport keys) -- not docs authors."""

    #: The node the chain binds to (== the expected node).
    node: NodeId
    #: The trusted anchor the chain terminates at.
    anchor: NodeId
    #: Intermediates between node and anchor, node-ward first.
    intermediates: Tuple[NodeId, ...]
    #: Number of edges.
    depth: int

    def is_node(self, id: IdInput) -> bool:
        """Whether ``id`` is the leaf node this chain binds."""
        return self.node == _hex(id)

    def is_anchor(self, id: IdInput) -> bool:
        """Whether ``id`` is the trusted anchor."""
        return self.anchor == _hex(id)

    def is_intermediate(self, id: IdInput) -> bool:
        """Whether ``id`` is an intermediate delegating identity."""
        return _hex(id) in self.intermediates

    def role_of(self, id: IdInput) -> Optional[AttestationRole]:
        """The role of ``id``, or ``None`` if it is not a participant
        (``"node"`` wins over ``"anchor"``)."""
        if self.is_node(id):
            return "node"
        if self.is_anchor(id):
            return "anchor"
        if self.is_intermediate(id):
            return "intermediate"
        return None


def attest_root_node(
    root_secret: SecretInput,
    node_secret: SecretInput,
    *,
    epoch: int = 0,
    not_before: TimeInput = None,
    not_after: TimeInput = None,
) -> AttestationChain:
    """Mint a single-edge root<->node chain: the root attests it owns the node
    and the node co-signs. ``epoch`` is a caller-managed monotonic replay
    counter; ``not_before`` / ``not_after`` are unix seconds or datetimes
    (``None`` = unbounded)."""
    return AttestationChain(
        _attest_root_node(
            root_secret, node_secret, int(epoch), _unix(not_before), _unix(not_after)
        )
    )


def verify_chain(
    chain: Union[AttestationChain, bytes],
    trusted_anchors: Iterable[IdInput],
    expected_node: IdInput,
) -> VerifiedChain:
    """Verify ``chain`` end-to-end, offline: structure, the leaf binding to
    ``expected_node``, each edge, the top anchor being in ``trusted_anchors``,
    and every signature. Raises :class:`AttestationError` on the first
    failure."""
    data = chain.data if isinstance(chain, AttestationChain) else bytes(chain)
    node, anchor, intermediates, depth = _attestation_verify_chain(
        data, list(trusted_anchors), expected_node
    )
    return VerifiedChain(
        node=NodeId(node),
        anchor=NodeId(anchor),
        intermediates=tuple(NodeId(i) for i in intermediates),
        depth=depth,
    )


def attestation_public_key(secret: SecretInput) -> NodeId:
    """The Ed25519 public key (as a ``NodeId``) of a 32-byte secret -- the
    identity an attestation edge signs as."""
    return NodeId(_attestation_public_key(secret))
