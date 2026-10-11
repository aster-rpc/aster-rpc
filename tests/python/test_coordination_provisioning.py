"""Portable authority helpers against shared Rust vectors and an external signer."""
from dataclasses import FrozenInstanceError, replace
import json
from pathlib import Path

import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

from aster import (
    GenesisSpec, GenesisVoter, JoinPermitSpec, ProvisioningError,
    canonical_candidate_ticket, canonical_genesis, canonical_join_permit,
    genesis_hash, genesis_signing_bytes, join_signing_bytes,
    signed_document_from_parts, verify_genesis, verify_join_permit_for_start,
)
from aster import _aster


@pytest.fixture
def vector():
    return json.loads((Path(__file__).resolve().parents[1] / "fixtures" / "coordination_provisioning_v1.json").read_text())


def genesis_spec(vector):
    request = dict(vector["genesis"]["request"])
    request["initial_voters"] = tuple(GenesisVoter(**voter) for voter in request["initial_voters"])
    return GenesisSpec(**request)


def signer(vector):
    return Ed25519PrivateKey.from_private_bytes(bytes.fromhex(vector["test_only_authority_secret"]))


def sign_genesis(vector, spec):
    document = canonical_genesis(spec)
    return signed_document_from_parts(
        document, bytes.fromhex(vector["authority_public_key"]),
        signer(vector).sign(genesis_signing_bytes(document)),
    )


def test_all_nine_helpers_match_rust_vectors_with_external_ed25519_signatures(vector):
    key = bytes.fromhex(vector["authority_public_key"])
    genesis, join = vector["genesis"], vector["join"]
    document = canonical_genesis(genesis_spec(vector))
    assert isinstance(document, bytes) and document.hex() == genesis["document_bytes"]
    assert genesis_signing_bytes(document).hex() == genesis["signing_bytes"]
    assert genesis_hash(document).hex() == genesis["genesis_hash"]
    signature = signer(vector).sign(genesis_signing_bytes(document))
    assert signature.hex() == genesis["signature"]
    envelope = signed_document_from_parts(document, key, signature)
    assert envelope.hex() == genesis["signed_document"]
    verified = verify_genesis(key, genesis["request"]["authority_domain"], envelope)
    assert verified.document_bytes == document
    assert verified.genesis_hash == genesis["verified"]["genesis_hash"]
    assert verified.group_id == genesis["verified"]["group_id"]
    assert verified.authority_public_key == genesis["verified"]["authority_public_key"]
    assert verified.authority_domain == genesis["verified"]["authority_domain"]
    assert verified.deployment_profile_code == genesis["verified"]["deployment_profile_code"]
    assert verified.initial_voters == tuple(GenesisVoter(**voter) for voter in genesis["verified"]["initial_voters"])
    assert verified.initial_admins == tuple(genesis["verified"]["initial_admins"])
    assert verified.recovery_export_hash == genesis["verified"]["recovery_export_hash"]
    permit = canonical_join_permit(JoinPermitSpec(**join["request"]))
    assert permit.hex() == join["document_bytes"]
    assert join_signing_bytes(permit).hex() == join["signing_bytes"]
    signature = signer(vector).sign(join_signing_bytes(permit))
    assert signature.hex() == join["signature"]
    envelope = signed_document_from_parts(permit, key, signature)
    assert envelope.hex() == join["signed_document"]
    candidate = bytes.fromhex(join["request"]["candidate_node_id"])
    assert canonical_candidate_ticket(candidate) == join["candidate_ticket"]
    verified_permit = verify_join_permit_for_start(key, join["request"]["authority_domain"], candidate, envelope)
    assert verified_permit.document_bytes == permit
    assert verified_permit.group_id == join["verified"]["group_id"]
    assert verified_permit.genesis_hash == join["verified"]["genesis_hash"]
    assert verified_permit.candidate_node_id == join["verified"]["candidate_node_id"]
    assert verified_permit.bootstrap_senders == tuple(join["verified"]["bootstrap_senders"])
    with pytest.raises(FrozenInstanceError):
        verified.group_id = "unverified replacement"


def test_genesis_pins_and_domain_separation_have_stable_native_codes(vector):
    key = bytes.fromhex(vector["authority_public_key"])
    document = bytes.fromhex(vector["genesis"]["document_bytes"])
    envelope = bytes.fromhex(vector["genesis"]["signed_document"])
    domain = vector["genesis"]["request"]["authority_domain"]
    for expected, supplied_key, supplied_domain, supplied_bytes in [
        ("wrong_authority_key", b"\x01" * 32, domain, envelope),
        ("wrong_domain", key, "another.test", envelope),
        ("malformed", key, domain, b"garbage"),
        ("bad_signature", key, domain, signed_document_from_parts(document, key, signer(vector).sign(join_signing_bytes(document)))),
    ]:
        with pytest.raises(ProvisioningError) as failure:
            verify_genesis(supplied_key, supplied_domain, supplied_bytes)
        assert failure.value.code == expected and failure.value.detail


def test_encoder_does_not_normalize_sets_or_establish_authority_validity(vector):
    spec = genesis_spec(vector)
    key = bytes.fromhex(vector["authority_public_key"])
    for invalid, code, detail in [
        (replace(spec, deployment_profile_code=99), "unknown_profile", "UnknownProfile(99)"),
        (replace(spec, initial_admins=[key.hex()]), "root_is_not_delegable", "RootIsNotDelegable"),
        (replace(spec, initial_admins=spec.initial_admins * 2), "initial_admins", "InitialAdmins"),
    ]:
        envelope = sign_genesis(vector, invalid)  # Canonical encoding itself is allowed.
        with pytest.raises(ProvisioningError) as failure:
            verify_genesis(key, spec.authority_domain, envelope)
        assert (failure.value.code, failure.value.detail) == (code, detail)


def test_startup_permit_extracts_group_and_genesis_but_checks_candidate_and_domain(vector):
    spec = replace(JoinPermitSpec(**vector["join"]["request"]), group_id="29" * 32, genesis_hash="30" * 32)
    key = bytes.fromhex(vector["authority_public_key"])
    candidate = bytes.fromhex(spec.candidate_node_id)
    document = canonical_join_permit(spec)
    envelope = signed_document_from_parts(document, key, signer(vector).sign(join_signing_bytes(document)))
    verified = verify_join_permit_for_start(key, spec.authority_domain, candidate, envelope)
    assert (verified.group_id, verified.genesis_hash) == (spec.group_id, spec.genesis_hash)
    for expected, supplied_candidate, domain in [
        ("wrong_node", b"\x01" * 32, spec.authority_domain),
        ("wrong_domain", candidate, "another.test"),
    ]:
        with pytest.raises(ProvisioningError) as failure:
            verify_join_permit_for_start(key, domain, supplied_candidate, envelope)
        assert failure.value.code == expected


def test_strict_dto_hex_unknown_fields_and_widths_are_native_errors(vector):
    spec = genesis_spec(vector)
    for bad_hash in ["short", "g" * 64, "é" * 32]:
        with pytest.raises(ProvisioningError) as failure:
            canonical_genesis(replace(spec, group_id=bad_hash))
        assert failure.value.code == "invalid_input"
    request = dict(vector["join"]["request"], candidate_ticket="caller supplied ticket")
    with pytest.raises(ValueError, match="invalid_input"):
        _aster._provisioning_canonical_join_permit(json.dumps(request))
    with pytest.raises(ValueError, match="invalid_input"):
        _aster._provisioning_canonical_genesis(" " * (1024 * 1024 + 1))
    for key, signature in [(b"\0" * 31, b"\0" * 64), (b"\0" * 32, b"\0" * 63)]:
        with pytest.raises(ProvisioningError) as failure:
            signed_document_from_parts(b"", key, signature)
        assert failure.value.code == "invalid_input"
    with pytest.raises(ProvisioningError) as failure:
        canonical_candidate_ticket(b"short")
    assert failure.value.code == "invalid_input"


def test_hash_and_preimage_helpers_do_not_claim_document_verification(vector):
    document = b"arbitrary opaque bytes\0\xff"
    assert isinstance(genesis_hash(document), bytes) and len(genesis_hash(document)) == 32
    assert genesis_signing_bytes(document) != join_signing_bytes(document)
    assert genesis_signing_bytes(document).endswith(document)
    assert join_signing_bytes(document).endswith(document)
    with pytest.raises(ProvisioningError):
        verify_genesis(bytes.fromhex(vector["authority_public_key"]), "provisioning.test", document)
