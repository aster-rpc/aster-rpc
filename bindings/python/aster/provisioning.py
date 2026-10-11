"""Stateless coordination authority provisioning through the Rust protocol.

Signing remains external. Verification pins must come from trusted configuration,
and the returned immutable summaries are informational, not native authority seals.
"""
from __future__ import annotations

from dataclasses import asdict, dataclass
import json
from typing import Sequence

from . import _aster


class ProvisioningError(ValueError):
    """A stable native authority error ``code`` and its diagnostic ``detail``."""
    def __init__(self, code: str, detail: str):
        self.code, self.detail = code, detail
        super().__init__(f"{code}: {detail}")


@dataclass(frozen=True)
class GenesisVoter:
    node_id: str
    ticket: str


@dataclass(frozen=True)
class GenesisSpec:
    group_id: str
    authority_domain: str
    genesis_nonce: str
    deployment_profile_code: int
    initial_voters: Sequence[GenesisVoter]
    initial_admins: Sequence[str]
    recovery_export_hash: str | None = None


@dataclass(frozen=True)
class JoinPermitSpec:
    group_id: str
    authority_domain: str
    genesis_hash: str
    candidate_node_id: str
    bootstrap_senders: Sequence[str]
    join_nonce: str


@dataclass(frozen=True)
class VerifiedGenesis:
    document_bytes: bytes
    genesis_hash: str
    group_id: str
    authority_public_key: str
    authority_domain: str
    deployment_profile_code: int
    initial_voters: tuple[GenesisVoter, ...]
    initial_admins: tuple[str, ...]
    recovery_export_hash: str | None


@dataclass(frozen=True)
class VerifiedJoinPermit:
    document_bytes: bytes
    group_id: str
    genesis_hash: str
    candidate_node_id: str
    bootstrap_senders: tuple[str, ...]


def _call(function, *args):
    try:
        return function(*args)
    except ValueError as error:
        # This is a transport envelope for the shared Rust error, not a second
        # implementation of verification or canonical document rules.
        try:
            diagnostic = json.loads(str(error))
        except (ValueError, TypeError):
            raise error
        raise ProvisioningError(diagnostic["code"], diagnostic["detail"]) from error


def canonical_genesis(spec: GenesisSpec) -> bytes:
    """Encode canonical v1 Fory bytes, preserving supplied list order.

    Encoding does not establish authority validity or sort/deduplicate sets.
    Fixed byte fields are 64-character ASCII hex strings; nonces are caller-owned.
    """
    return _call(_aster._provisioning_canonical_genesis, json.dumps(asdict(spec)))


def canonical_join_permit(spec: JoinPermitSpec) -> bytes:
    """Encode v1 Fory bytes with the native-derived addressless candidate ticket."""
    return _call(_aster._provisioning_canonical_join_permit, json.dumps(asdict(spec)))


def genesis_signing_bytes(document_bytes: bytes) -> bytes:
    """Return the exact native genesis signing preimage, without validation."""
    return _aster._provisioning_genesis_signing_bytes(document_bytes)


def join_signing_bytes(document_bytes: bytes) -> bytes:
    """Return the exact native join signing preimage, without validation."""
    return _aster._provisioning_join_signing_bytes(document_bytes)


def genesis_hash(document_bytes: bytes) -> bytes:
    """Return the native 32-byte digest of supplied bytes; this is not verification."""
    return _aster._provisioning_genesis_hash(document_bytes)


def signed_document_from_parts(document_bytes: bytes, authority_public_key: bytes, signature: bytes) -> bytes:
    """Assemble the native Fory envelope around an external 64-byte signature."""
    return _call(_aster._provisioning_signed_document_from_parts, document_bytes, authority_public_key, signature)


def canonical_candidate_ticket(node_id: bytes) -> str:
    """Derive the canonical addressless Open ticket for an exact 32-byte node ID."""
    return _call(_aster._provisioning_canonical_candidate_ticket, node_id)


def verify_genesis(authority_public_key: bytes, authority_domain: str, signed_bytes: bytes) -> VerifiedGenesis:
    """Verify with independently trusted key/domain pins; return verified fields."""
    value = json.loads(_call(_aster._provisioning_verify_genesis, authority_public_key, authority_domain, signed_bytes))
    value["document_bytes"] = bytes.fromhex(value["document_bytes"])
    value["initial_voters"] = tuple(GenesisVoter(**voter) for voter in value["initial_voters"])
    value["initial_admins"] = tuple(value["initial_admins"])
    return VerifiedGenesis(**value)


def verify_join_permit_for_start(authority_public_key: bytes, authority_domain: str, candidate_node_id: bytes, signed_bytes: bytes) -> VerifiedJoinPermit:
    """Verify pins and local candidate; extract group/genesis from the permit.

    This startup check does not compare an independent expected group or genesis.
    A permit alone does not admit a member; current quorum governance still applies.
    """
    value = json.loads(_call(_aster._provisioning_verify_join_permit_for_start, authority_public_key, authority_domain, candidate_node_id, signed_bytes))
    value["document_bytes"] = bytes.fromhex(value["document_bytes"])
    value["bootstrap_senders"] = tuple(value["bootstrap_senders"])
    return VerifiedJoinPermit(**value)
