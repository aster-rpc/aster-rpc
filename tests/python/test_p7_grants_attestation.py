"""Plan P7 golden vectors -- attestation, identity-derived HPKE keys, sealed
grants and lease rules, all through the PyO3 wrappers of the ``aster`` crate:

- conformance/vectors/grants.json (R5): AAD, open / open_invalid, namespace
  grants, the role rule, the grant key and reconcile decisions;
- core/tests/fixtures/fory_golden_1_3_0.txt: the byte-stable attestation chain
  (deterministic Ed25519), HPKE envelope, namespace capabilities and grant
  records Rust core pins.

Offline, no nodes.
"""

from __future__ import annotations

import dataclasses
import datetime as dt
import json
from pathlib import Path

import pytest

from aster import (
    AttestationChain,
    AttestationError,
    Fence,
    FenceRejectedError,
    GrantContext,
    GrantOutcome,
    GrantRecord,
    HpkeEnvelope,
    LeaseOp,
    LeaseRowState,
    LeaseTransition,
    NamespaceCapability,
    NodeId,
    SecretKey,
    TransitionOutcome,
    VerifiedChain,
    attest_root_node,
    attestation_public_key,
    check_fence,
    check_grant_role,
    grant_aad,
    grant_key,
    hpke_open,
    hpke_public_key_from_private,
    hpke_seal,
    hpke_x25519_public_from_identity,
    hpke_x25519_public_from_node_id,
    hpke_x25519_secret_from_identity,
    lease_transition,
    open_grant,
    open_grant_with_key,
    open_namespace_grant,
    reconcile_grant,
    reconcile_namespace_grant,
    seal_grant,
    seal_grant_to_key,
    seal_namespace_grant,
    verify_chain,
)

ROOT = Path(__file__).resolve().parents[2]
GRANTS = json.loads((ROOT / "conformance/vectors/grants.json").read_text())
GOLDEN = dict(
    line.split(" ", 1)
    for line in (ROOT / "core/tests/fixtures/fory_golden_1_3_0.txt").read_text().splitlines()
    if line and not line.startswith("#")
)


def ctx_of(c: dict) -> GrantContext:
    return GrantContext(
        app=c["app"],
        granter=c["granter"],
        recipient=c["recipient"],
        resource=bytes.fromhex(c["resource_hex"]),
        path=c["path"],
        role=c["role"],
    )


def _named(section: str):
    return pytest.mark.parametrize("v", GRANTS[section], ids=lambda v: v.get("name", "?"))


# -- grants.json (R5 vectors) -------------------------------------------------


@_named("aad")
def test_aad(v):
    ctx = ctx_of(v["context"])
    assert grant_aad(ctx).hex() == v["aad_hex"]
    assert ctx.aad().hex() == v["aad_hex"]


def test_grant_key():
    for v in GRANTS["grant_key"]:
        assert grant_key(v["prefix"], v["recipient"]) == v["key"]
        assert grant_key(v["prefix"], NodeId(v["recipient"])) == v["key"]


def test_role_rule():
    for v in GRANTS["role_rule"]:
        if v["ok"]:
            check_grant_role(v["role"], v["can_write"])
        else:
            with pytest.raises(ValueError, match="role"):
                check_grant_role(v["role"], v["can_write"])


def _open(v):
    ctx = ctx_of(v["context"])
    env = bytes.fromhex(v["envelope_hex"])
    if v["scheme"] == "identity":
        return open_grant(v["recipient_key_hex"], ctx, env)
    return open_grant_with_key(bytes.fromhex(v["recipient_key_hex"]), ctx, env)


@_named("open")
def test_open(v):
    assert _open(v).hex() == v["plaintext_hex"]
    if v.get("recipient_public_hex"):
        priv = bytes.fromhex(v["recipient_key_hex"])
        assert hpke_public_key_from_private(priv).hex() == v["recipient_public_hex"]


@_named("open_invalid")
def test_open_invalid(v):
    with pytest.raises(ValueError):
        _open(v)


@_named("namespace_open")
def test_namespace_open(v):
    cap = open_namespace_grant(
        v["recipient_secret_hex"], ctx_of(v["context"]), bytes.fromhex(v["envelope_hex"])
    )
    assert cap.encode_fory().hex() == v["capability_hex"]
    assert cap.can_write is v["can_write"]


def test_namespace_open_invalid():
    for v in GRANTS["namespace_open_invalid"]:
        with pytest.raises(ValueError):
            open_namespace_grant(
                v["recipient_secret_hex"],
                ctx_of(v["context"]),
                bytes.fromhex(v["envelope_hex"]),
            )


@_named("reconcile")
def test_reconcile(v):
    ctx = ctx_of(v["context"])
    record = None if v.get("record_hex") is None else bytes.fromhex(v["record_hex"])
    want = v["outcome"]
    if v.get("payload") == "namespace":
        got = reconcile_namespace_grant(v["recipient_secret_hex"], ctx, record)
    else:
        got = reconcile_grant(v["recipient_secret_hex"], ctx, record)
    assert isinstance(got, GrantOutcome)
    assert got.kind == want["kind"]
    assert got.drops_capability is (want["kind"] in ("revoked", "unopenable"))
    if got.kind == "revoked":
        assert got.reason == want["reason"]
        assert got.role == want["role"]
    elif got.kind == "unopenable" and want.get("error_contains"):
        assert want["error_contains"] in got.error
    elif got.kind == "active":
        if v.get("payload") == "namespace":
            assert got.value.encode_fory().hex() == want["capability_hex"]
        else:
            assert got.value.hex() == want["plaintext_hex"]
    else:
        assert got.value is None


# -- sealed grants round trip -------------------------------------------------


@pytest.fixture
def keys():
    root, recipient = SecretKey.generate(), SecretKey.generate()
    ctx = GrantContext(
        app="py/test-grant/v1",
        granter=root.node_id(),
        recipient=recipient.node_id(),
        resource=bytes([7]) * 32,
        path=grant_key("/py/v1", recipient.node_id()),
        role="write",
    )
    return root, recipient, ctx


def test_seal_to_identity_key_open_with_identity_secret(keys):
    root, recipient, ctx = keys
    env = seal_grant(ctx, b"secret")
    assert open_grant(recipient, ctx, env) == b"secret"
    assert open_grant(recipient.to_bytes(), ctx, env) == b"secret"
    assert open_grant(recipient.to_hex(), ctx, env) == b"secret"
    # The same keys through the explicit-key path.
    assert open_grant_with_key(hpke_x25519_secret_from_identity(recipient), ctx, env) == b"secret"
    # Any context field change fails; so does the wrong identity.
    with pytest.raises(ValueError):
        open_grant(recipient, dataclasses.replace(ctx, role="read"), env)
    with pytest.raises(ValueError):
        open_grant(recipient, dataclasses.replace(ctx, path=ctx.path + "x"), env)
    with pytest.raises(ValueError):
        open_grant(root, ctx, env)


def test_seal_to_explicit_key(keys):
    _, recipient, ctx = keys
    key = hpke_x25519_public_from_node_id(ctx.recipient)
    env = seal_grant_to_key(key, ctx, bytes([1, 2, 3]))
    assert open_grant(recipient, ctx, env) == bytes([1, 2, 3])
    with pytest.raises(ValueError):
        seal_grant_to_key(b"short", ctx, b"x")


def test_namespace_grants_enforce_role_rule_both_ways(keys):
    _, recipient, ctx = keys
    ns_secret = bytes([9]) * 32
    write = NamespaceCapability.write(ns_secret)
    env = seal_namespace_grant(ctx, write)
    opened = open_namespace_grant(recipient, ctx, env)
    assert opened.can_write is True
    assert opened.namespace_secret() == ns_secret
    with pytest.raises(ValueError, match="role"):
        seal_namespace_grant(dataclasses.replace(ctx, role="read"), write)


def test_records_active_revoked_reconcile(keys):
    _, recipient, ctx = keys
    env = seal_namespace_grant(ctx, NamespaceCapability.write(bytes([3]) * 32))
    active = GrantRecord.active("write", 1, env)
    decoded = GrantRecord.decode(active.encode())
    assert decoded == active
    assert decoded.is_active and decoded.envelope == env
    out = reconcile_namespace_grant(recipient, ctx, active.encode())
    assert out.kind == "active" and out.value.can_write

    revoked = GrantRecord.revoked("write", "rotated")
    r = reconcile_namespace_grant(recipient, ctx, revoked.encode())
    assert (r.kind, r.reason, r.role, r.drops_capability) == ("revoked", "rotated", "write", True)
    assert reconcile_grant(recipient, ctx, None) == GrantOutcome(kind="absent")
    assert reconcile_grant(recipient, ctx, None).drops_capability is False
    with pytest.raises(ValueError):
        GrantRecord.active("write", 1, b"")
    with pytest.raises(ValueError):
        GrantRecord.decode(b"\x00garbage")
    # Frozen value types.
    with pytest.raises(dataclasses.FrozenInstanceError):
        active.role = "read"  # type: ignore[misc]


# -- identity-derived HPKE keys -----------------------------------------------


def test_public_from_identity_agrees_with_secret_from_identity():
    secret = SecretKey.generate()
    node_id = secret.node_id()
    from_public = hpke_x25519_public_from_node_id(node_id)
    assert hpke_x25519_public_from_node_id(str(node_id)) == from_public
    assert hpke_x25519_public_from_identity(node_id) == from_public
    assert hpke_x25519_public_from_identity(secret.public_key()) == from_public
    assert hpke_x25519_public_from_identity(node_id.to_bytes()) == from_public
    priv = hpke_x25519_secret_from_identity(secret)
    assert hpke_x25519_secret_from_identity(secret.to_bytes()) == priv
    assert hpke_public_key_from_private(priv) == from_public
    env = hpke_seal(from_public, b"\x09", b"\x04\x05")
    assert hpke_open(priv, b"\x09", env) == b"\x04\x05"


def test_invalid_identity_keys_rejected():
    with pytest.raises(ValueError):
        hpke_x25519_public_from_node_id("zz")
    with pytest.raises(ValueError):
        hpke_x25519_public_from_identity(bytes(31))
    # The identity point is small-order.
    with pytest.raises(ValueError):
        hpke_x25519_public_from_identity("01" + "00" * 31)


# -- Fory golden fixtures (core byte stability) -------------------------------

ROOT_SEED = "01" * 32
NODE_SEED = "02" * 32


def test_attestation_chain_is_byte_identical_and_verifies_offline():
    chain = attest_root_node(ROOT_SEED, NODE_SEED, epoch=7)
    assert chain.data.hex() == GOLDEN["attestation.chain.root_node"]
    assert bytes(chain) == chain.data
    # Typed secrets mint the same bytes.
    assert attest_root_node(SecretKey.from_hex(ROOT_SEED), bytes.fromhex(NODE_SEED), epoch=7) == chain

    root = attestation_public_key(ROOT_SEED)
    node = attestation_public_key(NODE_SEED)
    assert root == SecretKey.from_hex(ROOT_SEED).node_id()
    golden = AttestationChain.from_bytes(bytes.fromhex(GOLDEN["attestation.chain.root_node"]))
    v = verify_chain(golden, [root], node)
    assert isinstance(v, VerifiedChain)
    assert v.node == node and v.anchor == root
    assert v.depth == 1 and v.intermediates == ()
    assert v.role_of(root) == "anchor"
    assert v.role_of(node) == "node"
    assert v.role_of(node.to_bytes()) == "node"
    assert v.role_of(attestation_public_key("03" * 32)) is None
    assert v.is_anchor(str(root)) and not v.is_intermediate(root)


def test_chain_text_round_trip_and_failures_raise_attestation_error():
    chain = attest_root_node(ROOT_SEED, NODE_SEED, epoch=7)
    text = chain.to_text()
    assert text.startswith("aster.attestation.chain.v1:")
    assert AttestationChain.from_text(text) == chain
    assert hash(AttestationChain.from_text(text)) == hash(chain)

    root = attestation_public_key(ROOT_SEED)
    node = attestation_public_key(NODE_SEED)
    other = attestation_public_key("03" * 32)
    with pytest.raises(AttestationError):
        verify_chain(chain, [other], node)
    with pytest.raises(AttestationError):
        verify_chain(chain, [root], other)
    tampered = bytearray(chain.data)
    tampered[-1] ^= 1
    with pytest.raises(AttestationError):
        verify_chain(bytes(tampered), [root], node)
    with pytest.raises(AttestationError):
        AttestationChain.from_text("nope")
    with pytest.raises(ValueError):
        verify_chain(chain, [b"short"], node)


def test_expired_chains_fail():
    past = dt.datetime.fromtimestamp(1000, tz=dt.timezone.utc)
    chain = attest_root_node(ROOT_SEED, NODE_SEED, not_after=past)
    with pytest.raises(AttestationError):
        verify_chain(chain, [attestation_public_key(ROOT_SEED)], attestation_public_key(NODE_SEED))
    future = attest_root_node(ROOT_SEED, NODE_SEED, not_before=4_000_000_000)
    with pytest.raises(AttestationError):
        verify_chain(future, [attestation_public_key(ROOT_SEED)], attestation_public_key(NODE_SEED))


def test_hpke_envelope_namespace_capabilities_and_grant_records():
    env = HpkeEnvelope.decode_fory(bytes.fromhex(GOLDEN["hpke.envelope"]))
    assert env.encapped_key.hex() == "33" * 32
    assert env.ciphertext.hex() == "44" * 48
    assert NamespaceCapability.read(bytes([0x11]) * 32).encode_fory().hex() == GOLDEN["namespace.read"]
    assert NamespaceCapability.write(bytes([0x22]) * 32).encode_fory().hex() == GOLDEN["namespace.write"]
    assert GrantRecord.active("write", 1, env.encode_fory()).encode().hex() == GOLDEN["grant.active"]
    assert GrantRecord.revoked("read", "rotated").encode().hex() == GOLDEN["grant.revoked"]
    revoked = GrantRecord.decode(bytes.fromhex(GOLDEN["grant.revoked"]))
    assert revoked == GrantRecord("revoked", 1, "read", b"", "rotated")
    active = GrantRecord.decode(bytes.fromhex(GOLDEN["grant.active"]))
    assert active.state == "active" and active.envelope == env.encode_fory()


# -- lease rules (pure) -------------------------------------------------------

FREE = LeaseRowState()


def test_lease_transition_acquire_renew_release_revoke():
    a = lease_transition(FREE, False, LeaseOp.acquire("n1", 1.0), "r")
    assert isinstance(a, LeaseTransition)
    assert a.kind == "apply"
    assert a.next == LeaseRowState(epoch=1, holder="n1", held=True, ttl=1.0)
    assert a.outcome == TransitionOutcome(kind="granted", fence=Fence("r", 1, "n1"))
    assert a.reset_grant is True

    assert lease_transition(a.next, False, LeaseOp.acquire("n2", 1.0), "r") == LeaseTransition(
        kind="deny", deny_reason="held_by_other"
    )
    takeover = lease_transition(a.next, True, LeaseOp.acquire("n2", 1.0), "r")
    assert takeover.kind == "apply" and takeover.next.epoch == 2
    assert lease_transition(a.next, True, LeaseOp.renew("n1", 1), "r").deny_reason == "expired"
    rel = lease_transition(a.next, False, LeaseOp.release("n1", 1), "r")
    assert rel.next == LeaseRowState(epoch=2, holder=None, held=False, ttl=1.0)
    assert rel.outcome.kind == "released"
    rev = lease_transition(a.next, False, LeaseOp.revoke("op"), "r")
    assert rev.outcome.kind == "revoked"

    with pytest.raises(ValueError, match="epoch"):
        lease_transition(a.next, False, LeaseOp("renew", "n1"), "r")
    with pytest.raises(ValueError, match="unknown"):
        lease_transition(a.next, False, LeaseOp("steal", "n1"), "r")  # type: ignore[arg-type]


def test_check_fence():
    row = LeaseRowState(epoch=3, holder="n1", held=True, ttl=1.0)
    assert check_fence(row, Fence("r", 3, "n1"), "n1") is None
    stale = check_fence(row, Fence("r", 2, "n1"), "n1")
    assert isinstance(stale, FenceRejectedError)
    assert (stale.kind, stale.carried, stale.current) == ("stale_epoch", 2, 3)
    assert check_fence(row, Fence("r", 4, "n1"), "n1").kind == "future_epoch"
    assert check_fence(row, Fence("r", 3, "n1"), "n2").kind == "not_holder"
    unheld = dataclasses.replace(row, held=False)
    assert check_fence(unheld, Fence("r", 3, "n1"), "n1").kind == "not_held"
