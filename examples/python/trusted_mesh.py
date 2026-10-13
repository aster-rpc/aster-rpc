"""
Full Trust Workflow -- Aster RPC Example (Enrollment).

Demonstrates the Aster trust model end to end, configured entirely through
``AsterConfig``: a root key, credentials issued by the ``aster`` CLI,
Enrollment on the server and the client, and roles that reach ``requires=``.

The trust model (docs/_internal/working_ideas/aster-enrollment.md, section 15):
  1. An **operator** holds the root key (``aster key root generate``) and a
     fleet policy namespace (``aster enroll namespace create``).
  2. The operator issues each node a credential for its node key
     (``aster enroll node --subject <node id> --audience <fleet>``), with roles
     in the ``aster.role`` claim.
  3. The **server** starts with the root public key, the fleet audience and
     namespace, and its own credential. Enrollment owns Gate 0: only nodes
     holding a credential from the root connect.
  4. The **client** enrolls with its credential (the handshake is
     reciprocal: each side verifies the other) and calls methods its roles
     allow.

One-time setup with the ``aster`` CLI (Rust, ``aster-cli``)::

    mkdir mesh && cd mesh
    aster key root generate
    ROOT=$(aster key root show --json | jq -r .root_pubkey)
    FLEET=$(aster enroll namespace create --out-secret fleet.secret --json | jq -r .namespace)
    SERVER=$(aster key node generate --out server.key --json | jq -r .node_id)
    CLIENT=$(aster key node generate --out client.key --json | jq -r .node_id)
    aster enroll node --subject $SERVER --audience trusted-mesh --out server.cred
    echo '{"aster.role":{"type":"string","value":"mesh.reader"}}' > reader.json
    aster enroll node --subject $CLIENT --audience trusted-mesh \\
      --attributes reader.json --out client.cred

Run (both nodes in this process)::

    ASTER_ROOT_PUBKEY=$ROOT ASTER_ENROLLMENT_FLEET_NAMESPACE=$FLEET \\
      python trusted_mesh.py mesh/
"""
from __future__ import annotations

import asyncio
import os
import sys
from dataclasses import dataclass

from aster import AsterClient, AsterConfig, AsterServer, EnrollmentConfig
from aster.codec import wire_type
from aster.decorators import rpc, service
from aster.runtime import AdmissionDeniedError

AUDIENCE = "trusted-mesh"


# ── Message types ────────────────────────────────────────────────────────────


@wire_type("example.trust/WhoAmIRequest")
@dataclass
class WhoAmIRequest:
    pass


@wire_type("example.trust/WhoAmIResponse")
@dataclass
class WhoAmIResponse:
    message: str = ""


@wire_type("example.trust/SecureDataRequest")
@dataclass
class SecureDataRequest:
    query: str = ""


@wire_type("example.trust/SecureDataResponse")
@dataclass
class SecureDataResponse:
    data: str = ""
    access_level: str = ""


# ── Service definition ───────────────────────────────────────────────────────


@service("TrustedService")
class TrustedService:
    """Reachable only by enrolled nodes; ``get_secure_data`` needs a role."""

    @rpc
    async def who_am_i(self, req: WhoAmIRequest) -> WhoAmIResponse:
        return WhoAmIResponse(message="You are an enrolled node with a valid credential.")

    @rpc(requires="mesh.reader")
    async def get_secure_data(self, req: SecureDataRequest) -> SecureDataResponse:
        return SecureDataResponse(
            data=f"Sensitive result for query: {req.query!r}",
            access_level="mesh.reader",
        )


# ── Main demo ────────────────────────────────────────────────────────────────


async def main(mesh: str) -> None:
    # Root public key and fleet namespace come from the environment
    # (ASTER_ROOT_PUBKEY, ASTER_ENROLLMENT_FLEET_NAMESPACE); each node adds
    # its own key and credential.
    base = AsterConfig.from_env()
    fleet = base.enrollment.fleet_namespace

    server_config = base.replace(
        secret_key_file=os.path.join(mesh, "server.key"),
        enrollment_credential_file=os.path.join(mesh, "server.cred"),
        enrollment=EnrollmentConfig(
            audience=AUDIENCE,
            fleet_namespace=fleet,
            state_path=os.path.join(mesh, "server-enrollment.json"),
        ),
    )
    client_config = base.replace(
        secret_key_file=os.path.join(mesh, "client.key"),
        enrollment_credential_file=os.path.join(mesh, "client.cred"),
        enrollment=EnrollmentConfig(audience=AUDIENCE, fleet_namespace=fleet),
    )

    async with AsterServer(services=[TrustedService()], config=server_config) as srv:
        print(f"server: {srv.address}  (enrollment: {srv.enrollment is not None})")

        async with AsterClient(config=client_config, address=srv.address) as client:
            print(f"client: enrolled, services {[s.name for s in client.services]}")
            trusted = await client.client(TrustedService)
            print("who_am_i ->", (await trusted.who_am_i(WhoAmIRequest())).message)
            resp = await trusted.get_secure_data(SecureDataRequest(query="latest metrics"))
            print(f"get_secure_data -> {resp.data} (role {resp.access_level})")

        # A node without a credential (no enrollment configured) cannot connect.
        try:
            await AsterClient(config=AsterConfig(), address=srv.address).connect()
            print("ERROR: should have been refused")
        except AdmissionDeniedError as exc:
            print(f"no credential -> refused:\n{exc}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__)
        sys.exit(2)
    asyncio.run(main(sys.argv[1]))
