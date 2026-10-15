"""Enrollment through the high-level path only: ``AsterServer`` and
``AsterClient`` configured by ``AsterConfig``'s ``[trust]`` and
``[enrollment]`` settings (design: "AsterServer integration", section 15 of
``docs/_internal/working_ideas/aster-enrollment.md``).

Port of ``aster/tests/enrollment_aster_server.rs``. The root is played by the
native module's test-only helpers (``_EnrollmentTestRoot``,
``_enrollment_test_credential``, ``_enrollment_test_delegation``), standing in
for ``aster enroll node``, ``aster enroll delegate`` and ``aster enroll revoke
--peer``; everything the application writes is configuration and
``requires=``.
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import AsyncIterator
from dataclasses import dataclass

import pytest

from aster import (
    AsterClient,
    AsterConfig,
    AsterServer,
    CallContext,
    EnrollmentConfig,
    SecretKey,
    any_of,
    client_stream,
    rpc,
    service,
    wire_type,
)
from aster._aster import (
    _EnrollmentTestRoot,
    _enrollment_test_credential,
    _enrollment_test_delegation,
)
from aster.runtime import AdmissionDeniedError
from aster.status import PermissionDeniedError

FLEET = "mission-control"

pytestmark = pytest.mark.timeout(120)


def seed(n: int) -> bytes:
    return bytes([n]) * 32


def node_id(n: int) -> str:
    return SecretKey.from_bytes(seed(n)).public_key().to_hex()


def credential(n: int, roles: str | None = None, *, lifetime: int = 3600) -> str:
    """What ``aster enroll node --subject <n> --audience mission-control
    [--attributes roles.json]`` writes, signed by the root (seed 1)."""
    return _enrollment_test_credential(
        seed(1), node_id(n), FLEET, roles=roles, lifetime_secs=lifetime
    )


@wire_type("test.enrollment/Empty")
@dataclass
class Empty:
    value: str = ""


@wire_type("test.enrollment/Role")
@dataclass
class RoleReply:
    role: str = ""


@service(name="Ops", version=1)
class Ops:
    """``status`` requires ``ops.status``; ``logs`` requires ``ops.logs`` or
    ``ops.admin``; ``ping`` is open. Each echoes the caller's ``aster.role``."""

    @rpc()
    async def ping(self, req: Empty, ctx: CallContext) -> RoleReply:
        return RoleReply(role=ctx.attributes.get("aster.role", ""))

    @rpc(requires="ops.status")
    async def status(self, req: Empty, ctx: CallContext) -> RoleReply:
        return RoleReply(role=ctx.attributes.get("aster.role", ""))

    @rpc(requires=any_of("ops.logs", "ops.admin"))
    async def logs(self, req: Empty, ctx: CallContext) -> RoleReply:
        return RoleReply(role=ctx.attributes.get("aster.role", ""))

    @client_stream(requires="ops.admin")
    async def upload(self, reqs: AsyncIterator[Empty]) -> RoleReply:
        count = 0
        async for _ in reqs:
            count += 1
        return RoleReply(role=str(count))


def base(n: int, **changes) -> AsterConfig:
    return AsterConfig(
        relay_mode="disabled",
        bind_addr="127.0.0.1:0",
        secret_key=seed(n),
        root_pubkey=bytes.fromhex(node_id(1)),
        **changes,
    )


def server_enrollment(root, tmp_path, **changes) -> EnrollmentConfig:
    fields = dict(
        audience=FLEET,
        fleet_namespace=root.namespace,
        state_path=str(tmp_path / "server-enrollment.json"),
        limits={"reconciliation_interval_ms": 200},
    )
    fields.update(changes)
    return EnrollmentConfig(**fields)


def server_config(root, tmp_path, **changes) -> AsterConfig:
    """The server: seed 2, with a root-issued credential."""
    return base(
        2,
        enrollment_credential_chain=credential(2),
        enrollment=server_enrollment(root, tmp_path, **changes),
    )


def client_config(root, n: int, chain: str | None) -> AsterConfig:
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


async def start_server(config: AsterConfig) -> AsterServer:
    srv = AsterServer(services=[Ops()], config=config)
    await srv.start()
    srv.serve()
    await wait_for_addr(srv.node)
    return srv


async def connect(srv: AsterServer, config: AsterConfig) -> tuple[AsterClient, object]:
    client = AsterClient(config=config, address=srv.address)
    await client.connect()
    return client, await client.client(Ops)


async def call(ops, method: str) -> str:
    reply = await asyncio.wait_for(getattr(ops, method)(Empty()), 10)
    return reply.role


@pytest.fixture
async def root():
    root = await _EnrollmentTestRoot.start(seed(1), FLEET)
    try:
        yield root
    finally:
        await root.close()


async def test_credential_roles_gate_requires_and_root_revocation_closes_the_connection(
    root, tmp_path
):
    """The roles in a client's credential are what ``requires`` checks, with
    no application code; a credential without the role is refused, and a
    node with no credential cannot reach the RPC service. A revocation the
    root publishes refuses the next call and closes the connection."""
    # No replicas: the server holds the fleet namespace and the root
    # publishes into it, as `aster enroll revoke --peer <server>` does.
    srv = await start_server(server_config(root, tmp_path))
    clients: list[AsterClient] = []
    try:
        assert srv.enrollment is not None
        assert srv.gate is None, "Enrollment owns Gate 0"

        operator, op = await connect(srv, client_config(root, 5, credential(5, "ops.status, ops.logs")))
        clients.append(operator)
        assert operator.enrollment is not None
        assert await call(op, "ping") == "ops.logs,ops.status"
        assert await call(op, "status") == "ops.logs,ops.status"
        assert await call(op, "logs") == "ops.logs,ops.status"
        # The Python server advertises its services to enrolled clients.
        assert [s.name for s in operator.services] == ["Ops"]

        ingest, ing = await connect(srv, client_config(root, 6, credential(6, "ops.ingest")))
        clients.append(ingest)
        assert await call(ing, "ping") == "ops.ingest"
        with pytest.raises(PermissionDeniedError):
            await call(ing, "status")
        with pytest.raises(PermissionDeniedError):
            await call(ing, "logs")

        # No enrollment at all: Gate 0 refuses the connection itself.
        stranger = AsterClient(config=base(8), address=srv.address)
        with pytest.raises(AdmissionDeniedError) as denied:
            await stranger.connect()
        assert not denied.value.had_credential
        await stranger.close()

        # The root enrolls with the server and delivers a revocation of the
        # operator into the fleet namespace the server holds.
        await root.revoke(srv.node.node_addr_info(), node_id(5))
        deadline = time.monotonic() + 20
        while True:
            try:
                await call(op, "status")
            except Exception:  # noqa: BLE001 -- refused or closed
                break
            assert time.monotonic() < deadline, "the server revokes the operator"
            await asyncio.sleep(0.2)
        with pytest.raises(Exception):
            await call(op, "ping")  # the connection was closed
        # Unaffected peers keep working.
        assert await call(ing, "ping") == "ops.ingest"
    finally:
        for client in clients:
            await client.close()
        await srv.close()


async def test_runtime_restriction_refuses_the_next_call(root, tmp_path):
    """The credential sets the maximum; runtime settings can only restrict.
    Restricting a credential's ``ops.status,ops.logs`` to ``ops.status``
    refuses ``logs`` on the next call; a restriction never grants a role the
    credential lacks; clearing restores the credential roles; a peer without
    a session cannot be restricted."""
    srv = await start_server(server_config(root, tmp_path))
    client = None
    try:
        client, ops = await connect(srv, client_config(root, 5, credential(5, "ops.status,ops.logs")))
        peer = node_id(5)
        assert await call(ops, "logs") == "ops.logs,ops.status"

        srv.restrict_peer_roles(peer, ["ops.status"])
        assert await call(ops, "status") == "ops.status"
        with pytest.raises(PermissionDeniedError):
            await call(ops, "logs")

        srv.restrict_peer_roles(peer, ["ops.admin"])
        with pytest.raises(PermissionDeniedError):
            await call(ops, "logs")

        assert srv.clear_peer_restriction(peer)
        assert await call(ops, "logs") == "ops.logs,ops.status"
        with pytest.raises(LookupError):
            srv.restrict_peer_roles(node_id(9), ["ops.status"])
    finally:
        if client is not None:
            await client.close()
        await srv.close()


async def test_client_stream_refusal_is_permission_denied(root, tmp_path):
    """A Gate-3 refusal of a client-stream call sending many items is
    PERMISSION_DENIED, not UNAVAILABLE: the server stops the request side
    before the client has sent everything, and the client reports the
    server's status rather than its own failed send."""

    async def items():
        for _ in range(100):
            yield Empty("x")

    srv = await start_server(server_config(root, tmp_path))
    clients: list[AsterClient] = []
    try:
        ingest, ing = await connect(srv, client_config(root, 6, credential(6, "ops.ingest")))
        clients.append(ingest)
        for _ in range(5):
            with pytest.raises(PermissionDeniedError):
                await asyncio.wait_for(ing.upload(items()), 10)
        admin, adm = await connect(srv, client_config(root, 5, credential(5, "ops.admin")))
        clients.append(admin)
        assert (await asyncio.wait_for(adm.upload(items()), 10)).role == "100"
    finally:
        for client in clients:
            await client.close()
        await srv.close()


async def test_config_driven_issuer_renews_client_credentials(root, tmp_path):
    """A server configured as a delegated issuer serves renewals; a client
    whose credential that server signed renews automatically and stays
    admitted, and its roles keep working, across several short lifetimes.
    The server syncs policy from a configured replica (the root)."""
    delegation = tmp_path / "issuer.chain"
    delegation.write_text(_enrollment_test_delegation(seed(1), node_id(2), FLEET) + "\n")
    srv = await start_server(
        server_config(
            root,
            tmp_path,
            replicas=(root.ticket,),
            issuer_delegation_file=str(delegation),
            issuer_lifetime_secs=6,
        )
    )
    client = None
    try:
        assert srv.enrollment.has_issuer
        issued = srv.enrollment.issue_for(node_id(7), attributes={"aster.role": "ops.status"})

        client, ops = await connect(srv, client_config(root, 7, issued))
        assert client.is_renewing, "the server signed it, so it renews"
        deadline = time.time() + 3 * 6 + 2
        while time.time() < deadline:
            assert await call(ops, "status") == "ops.status"
            await asyncio.sleep(0.5)
        assert client.enrollment.credential != issued, "the credential was renewed"
    finally:
        if client is not None:
            await client.close()
        await srv.close()


async def test_misconfiguration_fails_closed_with_a_clear_error(root, tmp_path):
    """Misconfiguration stops the start with an error that names the
    problem; nothing runs open."""

    async def refused(config: AsterConfig, expect: str) -> None:
        srv = AsterServer(services=[Ops()], config=config)
        try:
            with pytest.raises(ValueError) as error:
                await srv.start()
            assert expect in str(error.value), (expect, str(error.value))
        finally:
            await srv.close()

    good = dict(
        audience=FLEET,
        fleet_namespace=root.namespace,
        state_path=str(tmp_path / "server-enrollment.json"),
    )
    chain = credential(2)
    await refused(
        base(2, enrollment_credential_chain=chain, enrollment={**good, "audience": None}),
        "audience is required",
    )
    await refused(
        AsterConfig(
            relay_mode="disabled",
            secret_key=seed(2),
            enrollment_credential_chain=chain,
            enrollment=good,
        ),
        "no trusted root",
    )
    await refused(
        base(2, enrollment_credential_chain=chain, enrollment={**good, "state_path": None}),
        "keeps denials durably",
    )
    # A server with no credential, or one that does not verify for its key,
    # starts: only a root registration admits it (aster/tests/
    # enrollment_aster_server.rs registered_server_restarts_without_a_valid_credential).
    # A file path where the inline chain belongs.
    await refused(
        base(2, enrollment_credential_chain="/etc/aster/server.cred", enrollment=good),
        "ASTER_ENROLLMENT_CREDENTIAL)",
    )
    await refused(
        base(2, enrollment_credential_chain=chain, enrollment={**good, "policy": "allow_everyone"}),
        "policy",
    )
    await refused(
        base(2, enrollment_credential_chain=chain, enrollment={**good, "fleet_namespace": "not-hex"}),
        "fleet_namespace",
    )
    # Enabled with nothing else set: still refused, never open.
    await refused(
        base(2, enrollment_credential_chain=chain, enrollment={"enabled": True}),
        "audience is required",
    )
    # A credential file that does not exist.
    await refused(
        base(2, enrollment_credential_file=str(tmp_path / "missing.cred"), enrollment=good),
        "enrollment_credential",
    )
    # A custom Gate-0 policy and Enrollment are exclusive.
    from aster import GatePolicy

    srv = AsterServer(
        services=[Ops()], config=base(2, enrollment_credential_chain=chain, enrollment=good),
        gate=GatePolicy(),
    )
    with pytest.raises(ValueError, match="owns Gate 0"):
        await srv.start()
    await srv.close()

    # A client: no credential, and server-only issuer settings.
    srv = await start_server(base(2, enrollment_credential_chain=chain, enrollment=good))
    try:
        with pytest.raises(ValueError, match="no credential"):
            await AsterClient(config=client_config(root, 5, None), address=srv.address).connect()
        issuer_client = client_config(root, 5, credential(5)).replace(
            enrollment=EnrollmentConfig(
                audience=FLEET,
                fleet_namespace=root.namespace,
                issuer_delegation_file=str(tmp_path / "x"),
            )
        )
        with pytest.raises(ValueError, match="server-only"):
            await AsterClient(config=issuer_client, address=srv.address).connect()
    finally:
        await srv.close()


async def test_open_server_admits_any_client_and_advertises_services():
    """Without enrollment the server is open and the client needs no
    configuration beyond the address."""
    srv = await start_server(AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0"))
    try:
        assert srv.enrollment is None
        client = AsterClient(
            config=AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0"),
            address=srv.address,
        )
        await client.connect()
        try:
            assert client.open_gate
            assert client.enrollment is None
            assert [s.name for s in client.services] == ["Ops"]
            ops = await client.client(Ops)
            assert await call(ops, "ping") == ""
            with pytest.raises(PermissionDeniedError):
                await call(ops, "status")
        finally:
            await client.close()
    finally:
        await srv.close()


async def test_client_discovers_services_from_the_registry_namespace_attribute(monkeypatch):
    """Servers of every binding publish ``aster.registry_namespace`` in
    ``aster.ops.NodeInfo``; a Python client finds services through it when
    the server does not also advertise ``aster.services``."""
    from aster.runtime import _REGISTRY_ATTRIBUTE

    def registry_only(self):
        return [(_REGISTRY_ATTRIBUTE, self._registry_namespace)]

    monkeypatch.setattr(AsterServer, "_discovery_attributes", registry_only)
    srv = await start_server(AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0"))
    try:
        client = AsterClient(
            config=AsterConfig(relay_mode="disabled", bind_addr="127.0.0.1:0"),
            address=srv.address,
        )
        await client.connect()
        try:
            assert client.services == []
            assert client.registry_namespace == srv._registry_namespace != ""
            ops = await client.proxy("Ops")
            assert await ops.ping({}) == {"role": ""}
            assert [s.name for s in client.services] == ["Ops"]
        finally:
            await client.close()
    finally:
        await srv.close()
