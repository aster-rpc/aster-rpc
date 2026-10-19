"""Gateway identity on the high-level path only: ``AsterServer`` and
``AsterClient`` configured by ``AsterConfig``'s ``[identity]`` /
``[remote_config]`` settings and the credentials (design:
``docs/_internal/web_gateway/gateway-identity.md``).

Binding wiring only: config plumbing, the forwarded identity reaching
``CallContext``, refusals mapping to Python exceptions, and the NodeConfig
client. The behaviour is tested once in Rust:
``aster/tests/enrollment_gateway_identity.rs``,
``enrollment_provider_tokens.rs`` (provider tokens) and ``node_config.rs``
(NodeConfig hardening and admin checks). The root is the
native module's test-only ``_EnrollmentTestRoot``.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass

import pytest

from aster import (
    AsterClient,
    AsterConfig,
    AsterServer,
    CallContext,
    CallOptions,
    EnrollmentConfig,
    IdentityPublisherConfig,
    SecretKey,
    TrustedIssuer,
    bearer_token,
    rpc,
    service,
    wire_type,
)
from aster._aster import _EnrollmentTestRoot, _enrollment_test_credential
from aster.identity import (
    AMR_PASSWORD,
    AMR_WEBAUTHN,
    CONFIG_ADMIN_ROLE,
    GATEWAY_ROLE,
    ISSUABLE_ROLES_ATTRIBUTE,
    IdentityOverride,
)
from aster.status import PermissionDeniedError

FLEET = "mission-control"

pytestmark = pytest.mark.timeout(120)


def seed(n: int) -> bytes:
    return bytes([n]) * 32


def node_id(n: int) -> str:
    return SecretKey.from_bytes(seed(n)).public_key().to_hex()


def credential(n: int, roles: str, issuable: str | None = None) -> str:
    """A root-issued credential carrying ``roles`` in ``aster.role`` and,
    when given, an issuer delegation (``aster.identity.issuable_roles``)."""
    attributes = {ISSUABLE_ROLES_ATTRIBUTE: issuable} if issuable else None
    return _enrollment_test_credential(
        seed(1), node_id(n), FLEET, roles=roles or None, attributes=attributes
    )


@wire_type("test.gateway/WhoRequest")
@dataclass
class WhoRequest:
    n: int = 0


@wire_type("test.gateway/WhoReply")
@dataclass
class WhoReply:
    principal: str = ""
    peer: str = ""
    actor: str = ""
    amr: str = ""
    roles: str = ""
    issuer: str = ""
    tier: int = 0
    name: str = ""


def who(ctx: CallContext) -> WhoReply:
    obo = ctx.on_behalf_of
    return WhoReply(
        principal=ctx.principal or "",
        peer=ctx.peer or "",
        actor=ctx.actor or "",
        amr=",".join(ctx.auth_methods),
        roles=ctx.attributes.get("aster.role", ""),
        issuer=obo.issuer if obo is not None else "",
        tier=ctx.node_attribute("tier") or 0,
        name=ctx.node_properties.name,
    )


@service(name="Notes", version=1)
class Notes:
    """``whoami`` is open, ``read`` needs ``notes.read``, ``publish`` needs
    ``notes.read`` and a passkey sign-in."""

    @rpc()
    async def whoami(self, req: WhoRequest, ctx: CallContext) -> WhoReply:
        return who(ctx)

    @rpc(requires="notes.read")
    async def read(self, req: WhoRequest, ctx: CallContext) -> WhoReply:
        return who(ctx)

    @rpc(requires="notes.read")
    async def publish(self, req: WhoRequest, ctx: CallContext) -> WhoReply:
        ctx.require_auth_method(AMR_WEBAUTHN)
        return who(ctx)


def base(n: int, **changes) -> AsterConfig:
    return AsterConfig(
        relay_mode="disabled",
        bind_addr="127.0.0.1:0",
        secret_key=seed(n),
        root_pubkey=bytes.fromhex(node_id(1)),
        **changes,
    )


def server_config(root, tmp_path, **changes) -> AsterConfig:
    """The server: seed 2, accepting on-behalf-of calls."""
    changes.setdefault("identity_accept_on_behalf_of", True)
    return base(
        2,
        enrollment_credential_chain=credential(2, ""),
        enrollment=EnrollmentConfig(
            audience=FLEET,
            fleet_namespace=root.namespace,
            state_path=str(tmp_path / "server-enrollment.json"),
            limits={"reconciliation_interval_ms": 200},
        ),
        **changes,
    )


def client_config(root, n: int, chain: str) -> AsterConfig:
    return base(
        n,
        enrollment_credential_chain=chain,
        enrollment=EnrollmentConfig(audience=FLEET, fleet_namespace=root.namespace),
    )


async def wait_for_addr(node) -> None:
    for _ in range(100):
        if node.node_addr_info().direct_addresses:
            return
        await asyncio.sleep(0.02)


@pytest.fixture
async def root():
    root = await _EnrollmentTestRoot.start(seed(1), FLEET)
    try:
        yield root
    finally:
        await root.close()


@pytest.fixture
async def stack():
    """Servers and clients to close at the end, newest first."""
    opened: list = []
    try:
        yield opened
    finally:
        for item in reversed(opened):
            await item.close()


async def start_notes(stack, config: AsterConfig, **kwargs) -> AsterServer:
    srv = AsterServer(services=[Notes()], config=config, **kwargs)
    await srv.start()
    stack.append(srv)
    srv.serve()
    await wait_for_addr(srv.node)
    return srv


async def connect(stack, srv: AsterServer, config: AsterConfig):
    client = AsterClient(config=config, address=srv.address)
    await client.connect()
    stack.append(client)
    return client, await client.client(Notes)


async def call(notes, method: str, token: str | None = None) -> WhoReply:
    options = CallOptions(on_behalf_of=token) if token else None
    return await asyncio.wait_for(getattr(notes, method)(WhoRequest(n=1), options=options), 20)


# ── Configuration ───────────────────────────────────────────────────────────


def test_identity_config_from_env_round_trips(monkeypatch):
    monkeypatch.setenv("ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF", "true")
    monkeypatch.setenv(
        "ASTER_IDENTITY_TRUSTED_ISSUERS",
        '[{"iss":"https://idp.test","audiences":["notes"],"roles_claim":"groups",'
        '"role_map":{"readers":"notes.read"}}]',
    )
    monkeypatch.setenv("ASTER_IDENTITY_CLOCK_LEEWAY_SECS", "0")
    monkeypatch.setenv(
        "ASTER_IDENTITY_PUBLISHER",
        '{"issuers":["https://idp.test"],"jwks_url":{"https://idp.test":'
        '"https://idp.test/jwks"},"refresh_secs":600}',
    )
    monkeypatch.setenv("ASTER_REMOTE_CONFIG_LOCK", "identity")
    cfg = AsterConfig.from_env()
    assert cfg.identity_accept_on_behalf_of is True
    assert cfg.identity_trusted_issuers == (
        TrustedIssuer(
            iss="https://idp.test",
            audiences=("notes",),
            roles_claim="groups",
            role_map={"readers": "notes.read"},
        ),
    )
    assert cfg.identity_clock_leeway_secs == 0
    assert cfg.identity_edge_token_max_lifetime_secs == 300
    assert cfg.identity_publisher == IdentityPublisherConfig(
        issuers=("https://idp.test",),
        jwks_url={"https://idp.test": "https://idp.test/jwks"},
        refresh_secs=600,
    )
    assert cfg.remote_config_lock == ("identity",)
    assert cfg.remote_config_enabled is True
    assert cfg.setting_sources == {
        "identity.accept_on_behalf_of": "env",
        "identity.trusted_issuers": "env",
        "identity.clock_leeway_secs": "env",
    }

    # Round trip through the native config, sources included.
    back = AsterConfig._from_native(cfg._native())
    assert back.identity_trusted_issuers == cfg.identity_trusted_issuers
    assert back.identity_publisher == cfg.identity_publisher
    assert back.setting_sources == cfg.setting_sources
    assert cfg._native().to_dict()["setting_sources"] == cfg.setting_sources

    # A value changed in code no longer claims to come from the environment.
    changed = cfg.replace(identity_accept_on_behalf_of=False)
    assert "identity.accept_on_behalf_of" not in changed.setting_sources
    assert changed.setting_sources["identity.trusted_issuers"] == "env"
    assert changed._native().to_dict()["setting_sources"] == changed.setting_sources


def test_identity_config_from_toml_and_code(tmp_path, monkeypatch):
    for name in ("ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF", "ASTER_REMOTE_CONFIG_ENABLED"):
        monkeypatch.delenv(name, raising=False)
    path = tmp_path / "aster.toml"
    path.write_text(
        "[identity]\naccept_on_behalf_of = true\nmax_key_set_age_secs = 3600\n\n"
        "[[identity.trusted_issuers]]\niss = \"https://idp.test\"\naudiences = [\"notes\"]\n\n"
        "[remote_config]\nenabled = false\n"
    )
    cfg = AsterConfig.from_file(path)
    assert cfg.identity_accept_on_behalf_of is True
    assert cfg.identity_max_key_set_age_secs == 3600
    assert cfg.identity_trusted_issuers[0].audiences == ("notes",)
    assert cfg.remote_config_enabled is False
    assert cfg.setting_sources["identity.accept_on_behalf_of"] == "file"

    code = AsterConfig(
        identity_trusted_issuers=[{"iss": "https://idp.test", "audiences": "notes"}],
        identity_publisher={"issuers": ["https://idp.test"],
                            "jwks_url": {"https://idp.test": "https://idp.test/jwks"}},
        remote_config_lock="identity",
    )
    assert code.identity_trusted_issuers == (TrustedIssuer("https://idp.test", ("notes",)),)
    assert code.identity_publisher.refresh_secs == 3600
    assert code.remote_config_lock == ("identity",)
    assert code.setting_sources == {}
    with pytest.raises(TypeError):
        AsterConfig(identity_trusted_issuers=[{"iss": "x", "audiences": ["a"], "bogus": 1}])
    with pytest.raises(ValueError, match="unknown source"):
        AsterConfig(setting_sources={"identity.accept_on_behalf_of": "nowhere"})


def test_bearer_token():
    assert bearer_token("Bearer abc.def") == "abc.def"
    assert bearer_token("Basic x") is None


def test_call_options_on_behalf_of_replaces_the_entry():
    opts = CallOptions(metadata={"Aster-On-Behalf-Of": "old", "k": "v"}, on_behalf_of="new")
    assert opts.metadata == {"k": "v", "aster-on-behalf-of": "new"}
    merged = CallOptions(on_behalf_of="a").merged(CallOptions(on_behalf_of="b"))
    assert merged.metadata == {"aster-on-behalf-of": "b"}


async def test_unenrolled_server_refuses_trusted_issuers_and_client_publisher(root):
    srv = AsterServer(
        services=[Notes()],
        config=base(3, identity_trusted_issuers=[TrustedIssuer("https://idp.test", ("notes",))]),
    )
    with pytest.raises(ValueError, match="needs \\[enrollment\\]"):
        await srv.start()
    client = AsterClient(
        config=base(
            4,
            identity_publisher=IdentityPublisherConfig(
                issuers=("https://idp.test",),
                jwks_url={"https://idp.test": "https://idp.test/jwks"},
            ),
        ),
        address=root.ticket,
    )
    with pytest.raises(ValueError, match="needs \\[enrollment\\]"):
        await client.connect()


# ── Edge-minted tokens and NodeConfig ───────────────────────────────────────


async def test_edge_minted_token_and_node_config(root, tmp_path, stack):
    """An edge holding ``aster.gateway`` mints a token for a user; the
    handler sees the user, the edge as the actor and the passkey sign-in.
    The edge, holding ``aster.config-admin``, changes the server through
    NodeConfig: the handler sees the attributes, and turning
    ``accept_on_behalf_of`` off refuses the entry."""
    srv = await start_notes(stack, server_config(root, tmp_path))
    edge, notes = await connect(
        stack,
        srv,
        client_config(
            root, 5, credential(5, f"{GATEWAY_ROLE},{CONFIG_ADMIN_ROLE}", issuable="notes.read")
        ),
    )
    edge_id = node_id(5)

    me = await call(notes, "whoami")
    assert (me.principal, me.actor, me.amr) == (edge_id, "", "")
    with pytest.raises(PermissionDeniedError):
        await call(notes, "read")  # the edge itself lacks notes.read

    token = edge.mint_user_token("alice", ["notes.read"], [AMR_WEBAUTHN], ttl=60)
    alice = await call(notes, "publish", token)
    assert alice.principal == f"{edge_id}|alice"
    assert alice.peer == edge_id
    assert alice.actor == edge_id
    assert alice.amr == AMR_WEBAUTHN
    assert alice.roles == "notes.read"
    assert alice.issuer == edge_id

    pwd = edge.mint_user_token("bob", ["notes.read"], [AMR_PASSWORD], ttl=60)
    with pytest.raises(PermissionDeniedError, match="webauthn"):
        await call(notes, "publish", pwd)  # publish needs a passkey
    with pytest.raises(ValueError):
        edge.mint_user_token("bob", ["notes.admin"], [AMR_PASSWORD], ttl=60)
    assert not edge.enrollment.has_publisher
    with pytest.raises(LookupError):
        edge.enrollment.notice_kid("https://idp.test", "k1")

    # NodeConfig: get, set, ack, clear from the admin edge.
    admin = await edge.node_config()
    view = await admin.get()
    setting = view.setting("identity.accept_on_behalf_of")
    assert view.node_id == node_id(2)
    assert setting is not None and setting.value is True and setting.source == "code"

    ack = await admin.set(
        name="notes-1",
        tags=["eu"],
        attributes={"tier": 3},
        identity=IdentityOverride(accept_on_behalf_of=False),
    )
    assert ack.complete and ack.generation == 1
    me = await call(notes, "whoami")
    assert (me.tier, me.name) == (3, "notes-1")
    props = srv.node_properties()
    assert props.generation == 1 and props.has_tag("eu") and props.attribute("tier") == 3
    with pytest.raises(PermissionDeniedError):
        await call(notes, "read", token)  # accept_on_behalf_of = false refuses the entry
    assert (await admin.ack(1, timeout=1.0)).complete
    assert (await admin.clear()).complete
    alice = await call(notes, "read", token)
    assert alice.actor == edge_id and alice.tier == 0
